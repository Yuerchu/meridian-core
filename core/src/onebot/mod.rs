mod agent;
mod balance_watch;
mod capture;
mod command;
mod extract;
mod format;
mod handler;
mod media;
mod notice;
mod protocol;
mod qq_tools;
mod quote;
mod session;
mod stickers;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::db::DbPool;
use crate::listen_guard::{constant_time_eq, validate_listen_config};
use crate::services::Services;
use crate::turn::{Busy, TurnCoordinator, TurnLease, TurnOrigin};
use crate::util::{get_conn, now_ms};

use protocol::{OneBotAction, OneBotFrame, OneBotResponse};
pub use qq_tools::catalog as qq_tool_catalog;
use session::{SessionKey, SessionManager};

pub struct SharedState {
    pub services: Services,
    pub sessions: Mutex<SessionManager>,
    /// Tool calls waiting on a QQ reply. Behind its own `Arc` for the same
    /// reason as `session_states`: a turn that dies has to be able to retire
    /// its waiters from `Drop`, which cannot await.
    pub pending_approvals: Arc<PendingApprovals>,
    /// API calls waiting on an adapter's reply, by echo.
    ///
    /// A `std::sync::Mutex` rather than tokio's, and that is what lets a
    /// cancelled call clean up after itself: the waiter has to be removed from
    /// a `Drop`, which cannot await. Every critical section here is one map
    /// operation with nothing awaited inside.
    pub pending_api_responses: std::sync::Mutex<HashMap<String, PendingCall>>,
    pub pending_requests: Mutex<HashMap<u32, PendingRequest>>,
    pub request_seq: AtomicU32,
    pub ws_sinks: Mutex<HashMap<u64, mpsc::Sender<String>>>,
    /// 每条连接背后是哪个 QQ 账号。
    ///
    /// 采集要用它排除 bot 自己发的 TTS，而**只看当前这条连接的 self_id 不够**：
    /// bot A 发的语音会被同群的 bot B 当成真人语音收下来，合成音就这么从后门
    /// 进了真人语料。所以问的是"这个号是不是我们的某一个"，不是"是不是这一条
    /// 连接的"。
    ///
    /// 一条连接的身份认第一个报上来的，之后不再改：一条连接漂移到另一个账号
    /// 是适配器出了问题，跟着它改只会把两个账号的语料混起来。
    pub conn_identities: std::sync::Mutex<HashMap<u64, i64>>,
    pub connected_clients: AtomicU32,
    /// 这一代服务的关停信号。
    ///
    /// **在 state 上而不是只在 `OneBotServer` 上**，因为读它的是每一条连接的
    /// 读循环。清空 `ws_sinks` 只掐掉了写的那一半：`split()` 出来的两半共享底层
    /// 流，丢掉写的一半不会关闭套接字，读的那一半照常收事件、照常处理、照常带着
    /// 它启动时那套权限——重启一次是为了让新设置生效，结果是旧的那一代在新的
    /// 旁边继续跑。
    pub shutdown: watch::Sender<bool>,
    /// 每有一条连接彻底退出就 +1。`stop()` 等它，而不是轮询——与语料 drain 同
    /// 一个形状。
    pub conn_closed: watch::Sender<u64>,
    /// Session key → turn/inbox state. Behind its own `Arc` rather than inline:
    /// a running turn's guard has to hand the session back from `Drop`, and if
    /// that meant holding the whole server state, the turn machinery could not
    /// be exercised — or tested — without a websocket server and a provider
    /// standing behind it.
    pub session_states: Arc<SessionStates>,
    pub config: OneBotConfig,
    /// (session, user) → the memory ids their last listing showed, in the order
    /// it showed them. Numbers only mean something against the listing they came
    /// from; see `MemoryListing`.
    pub memory_listings: Mutex<HashMap<(String, i64), MemoryListing>>,
}

/// Every session's turn and inbox state, under one lock.
///
/// All turn-active/inbox transitions happen under it, which is what stops a
/// message racing past an active turn. A `std::sync::Mutex`, not tokio's: every
/// critical section is one map operation with nothing awaited inside, and
/// `SessionTurn::drop` has to be able to take it, which cannot await.
#[derive(Default)]
pub struct SessionStates(std::sync::Mutex<HashMap<String, SessionState>>);

impl SessionStates {
    /// Recovers from poisoning instead of propagating it. The critical sections
    /// are single map operations, so a panic elsewhere cannot leave the map
    /// half-written — whereas refusing the lock would take every later QQ
    /// message in the process down with it.
    pub fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionState>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A tool call sitting in front of a QQ user, waiting to be allowed or refused.
pub struct PendingApproval {
    /// Only the person who triggered the call may answer it — otherwise anyone
    /// in the group could approve someone else's command.
    pub initiator: i64,
    /// Which run of the turn asked. What lets a turn that died sweep its own
    /// waiters out in one pass, rather than leaving a sender in the map until
    /// somebody happens to reply to it or the next call overwrites it.
    pub turn_id: String,
    /// The prompt this is an answer to, so an answer can be recognised as one.
    ///
    /// Without it, any message from the initiator counts — and in a group the
    /// initiator is mostly talking to other people. A "y" meant for someone
    /// else approved whatever happened to be waiting.
    ///
    /// `None` when the send did not come back with an id: an adapter that
    /// answers nothing, or a call that timed out after the message went out.
    /// The rule then falls back to "anything from the initiator", because the
    /// alternative is an approval nobody can ever answer.
    pub prompt_message_id: Option<i64>,
    /// What kind of question is parked, so the acknowledgement can be worded
    /// without re-deriving it from a tool name this side no longer holds.
    pub kind: crate::onebot::agent::AskKind,
    /// What they typed, verbatim. Never logged, and read only by the adapter
    /// that knows which tool asked and therefore what the words mean.
    pub responder: oneshot::Sender<String>,
}

impl PendingApproval {
    /// Whether a message from the initiator is answering this, given what it
    /// quoted.
    pub fn answered_by(&self, reply_to: Option<i64>) -> bool {
        match self.prompt_message_id {
            Some(id) => reply_to == Some(id),
            None => true,
        }
    }
}

/// Session key → the call that session is waiting on an answer for.
///
/// A `std::sync::Mutex` for the same reason as `SessionStates`: the critical
/// sections are single map operations, and a dead turn has to be able to clear
/// its own entries from `Drop`.
#[derive(Default)]
pub struct PendingApprovals(std::sync::Mutex<HashMap<String, PendingApproval>>);

impl PendingApprovals {
    pub fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, PendingApproval>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Drop every waiter belonging to `turn_id`. Dropping the sender is what
    /// tells the waiting side that no answer is coming — though by the time
    /// this runs, on the path that matters, there is no waiting side left.
    pub fn retire_turn(&self, turn_id: &str) {
        self.lock().retain(|_, pending| pending.turn_id != turn_id);
    }
}

/// Which listing a set of numbers belongs to. Deleting resolves against the
/// matching kind only: the numbers a person saw for their own memories mean
/// something different from the ones they saw for the room's, and one slot
/// shared between them lets `/memory group` followed by `/memory forget 1`
/// delete a row the command never claimed to touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryListingKind {
    /// The caller's own memories (`/memory me`).
    Own,
    /// This room's memories (`/memory group`).
    Group,
    /// An operator view (`/memory user`, `/memory global`) — never a delete
    /// target for the numbered self-service commands.
    Operator,
}

/// A numbered listing shown to one person, so `/memory forget 2` can resolve
/// "2" to the row they actually saw.
///
/// Expires rather than falling back to the current order: between listing and
/// deleting, a memory can be added or removed, and silently renumbering would
/// delete something the person never chose.
pub struct MemoryListing {
    pub kind: MemoryListingKind,
    pub ids: Vec<String>,
    pub created_at: i64,
}

/// How long a numbered listing stays valid.
pub const MEMORY_LISTING_TTL_MS: i64 = 300 * 1000;

/// Cap on queued notice notes per session (user messages are not capped).
const NOTICE_INBOX_CAP: usize = 5;
/// Inbox items older than this are dropped instead of delivered.
const INBOX_EXPIRY_MS: i64 = 2 * 3600 * 1000;
/// How many OneBot message ids that entered the AI context to remember per
/// session (used to decide whether a recall is worth reporting).
const SEEN_IDS_CAP: usize = 200;

#[derive(Default)]
pub struct SessionState {
    pub turn_active: bool,
    pub inbox: Vec<InboxItem>,
    pub seen_message_ids: VecDeque<i64>,
    pub last_poke_reply_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboxKind {
    Notice,
    UserMessage,
}

/// Who sent a message, carried from the platform event through the queue, the
/// database and into the provider payload. Distinct from `is_admin` alone: that
/// gates tools, this attributes memories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SenderContext {
    pub user_id: i64,
    pub nickname: Option<String>,
    /// QQ group role (`owner` / `admin` / `member`), when the event carried one.
    /// Reaches the model through the `<people>` roster rather than the per-message
    /// marker: history rows carry no role, so a marker would show the same person
    /// holding rank this turn and none the turn before.
    pub role: Option<String>,
    /// The group's bespoke honorific, alongside `role` and by the same route.
    pub title: Option<String>,
    pub is_admin: bool,
    pub is_group: bool,
}

impl SenderContext {
    pub fn scope_id(&self) -> String {
        crate::db::models::memory::onebot_user_scope_id(self.user_id)
    }
}

impl From<&SenderContext> for crate::provider::SenderRef {
    fn from(s: &SenderContext) -> Self {
        // `role` and `title` are not copied: they reach the model on the
        // `<people>` roster, not on every message.
        crate::provider::SenderRef {
            user_id: s.user_id,
            nickname: s.nickname.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct InboxItem {
    pub text: String,
    pub kind: InboxKind,
    pub created_at: i64,
    /// `None` for notices, which nobody said. Queued user messages keep their
    /// own sender: merging them into one blob first would make the speakers
    /// unrecoverable.
    pub sender: Option<SenderContext>,
}

/// One inbound message for a turn. A turn can start with several (queued
/// messages from different people), and each keeps its own attribution rather
/// than being flattened into one string.
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub text: String,
    pub sender: Option<SenderContext>,
}

impl IncomingMessage {
    pub fn new(text: impl Into<String>, sender: Option<SenderContext>) -> Self {
        Self {
            text: text.into(),
            sender,
        }
    }
}

impl From<&InboxItem> for IncomingMessage {
    fn from(i: &InboxItem) -> Self {
        Self {
            text: i.text.clone(),
            sender: i.sender.clone(),
        }
    }
}

fn expire_inbox(inbox: &mut Vec<InboxItem>, now: i64) {
    inbox.retain(|i| now - i.created_at < INBOX_EXPIRY_MS);
}

/// Outcome of finishing a turn: either the session is idle again, or user
/// messages arrived too late for mid-turn injection and the caller must run
/// another turn with them.
///
/// The lease rides along on `Continue` rather than being released and retaken.
/// A follow-up round is the same turn continuing, and letting go of the
/// conversation in between would open exactly the gap this coordinator exists
/// to close — the desktop would be free to start a turn between two rounds of
/// one QQ answer.
pub enum TurnEnd {
    Done,
    Continue(SessionTurn, Vec<InboxItem>),
}

/// The session-local half of the above, before the lease is decided.
enum Finish {
    Done,
    Continue(Vec<InboxItem>),
}

impl SessionState {
    /// Queue `item` behind the running turn, or report that the session is free.
    ///
    /// Split from `activate` because taking the session is no longer the whole
    /// story: between the two the caller has to take the *conversation* from
    /// the coordinator, and that can be refused by a desktop turn — in which
    /// case nothing may be queued, since only the OneBot runner drains this
    /// inbox.
    fn queue_unless_free(&mut self, item: InboxItem) -> bool {
        if self.turn_active {
            self.inbox.push(item);
            false
        } else {
            true
        }
    }

    fn activate(&mut self) {
        self.turn_active = true;
    }

    /// User messages left in the inbox keep the turn active and are handed
    /// back for an immediate follow-up; notice-only leftovers stay queued.
    fn finish(&mut self, now: i64) -> Finish {
        expire_inbox(&mut self.inbox, now);
        if self.inbox.iter().any(|i| i.kind == InboxKind::UserMessage) {
            Finish::Continue(std::mem::take(&mut self.inbox))
        } else {
            self.turn_active = false;
            Finish::Done
        }
    }

    fn push_note(&mut self, text: String, now: i64) {
        expire_inbox(&mut self.inbox, now);
        let notice_count = self.inbox.iter().filter(|i| i.kind == InboxKind::Notice).count();
        if notice_count >= NOTICE_INBOX_CAP
            && let Some(pos) = self.inbox.iter().position(|i| i.kind == InboxKind::Notice)
        {
            self.inbox.remove(pos);
        }
        // Notices are ours, not anyone's utterance.
        self.inbox.push(InboxItem {
            text,
            kind: InboxKind::Notice,
            created_at: now,
            sender: None,
        });
    }

    fn record_seen(&mut self, message_id: i64) {
        if self.seen_message_ids.len() >= SEEN_IDS_CAP {
            self.seen_message_ids.pop_front();
        }
        self.seen_message_ids.push_back(message_id);
    }
}

/// A turn's claim on both things it had to take.
///
/// They live in different places — the conversation in the shared coordinator,
/// this session's `turn_active` flag in `session_states` — and giving back only
/// one of them is worse than giving back neither. Release just the conversation
/// and the session stays active forever: every later QQ message is queued into
/// an inbox with no runner left to drain it, and the session goes silent for
/// good. Release just the session and two runners write the same conversation.
///
/// So both are held by one value, and `Drop` covers every way a turn can leave
/// that is not the normal one — a cancelled task, a panic, an early return.
pub struct SessionTurn {
    states: Arc<SessionStates>,
    session: String,
    /// Copied out of the lease so they stay readable after it has been given
    /// back, and so neither accessor has to invent a value for a state that
    /// should not be observable.
    turn_id: String,
    cancel: CancellationToken,
    /// `None` once the turn has ended normally, which is what stops `Drop` from
    /// clearing a flag that `finish` already cleared — and, more to the point,
    /// one that a *later* turn may since have set.
    holding: Option<TurnLease>,
}

impl SessionTurn {
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    /// Hand both claims back inside one critical section.
    ///
    /// Not two. Anyone who can see this session as free had to take its lock to
    /// do it, so by the time they can, the conversation has to be free too.
    /// Leaving the lease to drop after the lock — which is what a plain field
    /// does, since fields are destroyed once `Drop::drop` has returned — opens
    /// a window where a QQ message finds an idle session and is then turned
    /// away by the coordinator, and the user is told the desktop is busy by a
    /// turn that had already ended.
    ///
    /// `watch` runs at the end of that section, which is the only place the
    /// difference between the two orders is visible.
    fn release(&mut self, watch: impl FnOnce()) {
        let Some(lease) = self.holding.take() else { return };
        let mut map = self.states.lock();
        if let Some(s) = map.get_mut(&self.session) {
            s.turn_active = false;
        }
        drop(lease);
        watch();
    }
}

impl Drop for SessionTurn {
    fn drop(&mut self) {
        self.release(|| ());
    }
}

/// Where a turn's terminal event goes. Injected rather than reached for through
/// an `AppHandle` so the ordering below can be watched from a test.
///
/// `Sync` as well as `Send` so that a `&RunningTurn` can be held across an
/// await. Without it the guard could only ever be used by value, which rules
/// out doing anything asynchronous through it — including opening the turn's
/// record, which has to happen *after* the guard exists.
pub type StopSink = Box<dyn Fn(crate::events::ChatStreamEvent) + Send + Sync>;

/// A turn while it is running, and the announcement it owes when it stops.
///
/// `SessionTurn` gives the session and the conversation back however the turn
/// dies. This is the other half: telling whoever is watching. Both have to
/// happen, in that order, on every exit — and "every exit" includes the ones
/// no code path passes through. An OneBot turn runs in a detached task; drop
/// that task mid-`await` (runtime shutdown) or let a tool panic, and the code
/// after the await never runs. Announcing from there covered the ordinary
/// endings and nothing else, and a desktop with that QQ conversation open would
/// stream for good.
///
/// So the duty lives in a value that is dropped either way.
pub struct RunningTurn {
    states: Arc<SessionStates>,
    session: SessionKey,
    /// Taken by `end_round` when the turn is over, and by `Drop` when nothing
    /// else got there first. `None` means the end has already been announced.
    turn: Option<SessionTurn>,
    /// Cleared of this turn's waiters on every exit. `make_approval_fn` parks
    /// on a 60-second timeout, and a task dropped mid-`await` never reaches the
    /// timeout branch — the sender would sit in the map until somebody replied
    /// to a question nobody is listening for, or the next call overwrote it.
    approvals: Arc<PendingApprovals>,
    sink: Option<StopSink>,
    conversation_id: String,
    turn_id: String,
    /// The last assistant row written, across rounds. A round that fails before
    /// writing one leaves the previous round's, which is closer to the truth
    /// than nothing.
    message_id: Option<String>,
    input_tokens: i32,
    output_tokens: i32,
}

impl RunningTurn {
    pub fn new(
        states: Arc<SessionStates>,
        approvals: Arc<PendingApprovals>,
        session: SessionKey,
        turn: SessionTurn,
        conversation_id: String,
        sink: Option<StopSink>,
    ) -> Self {
        let turn_id = turn.turn_id().to_string();
        Self {
            states,
            session,
            turn: Some(turn),
            approvals,
            sink,
            conversation_id,
            turn_id,
            message_id: None,
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.turn.as_ref().map(|t| t.cancel_token().clone()).unwrap_or_default()
    }

    /// Open this turn's durable record.
    ///
    /// A method on the guard rather than a free call, so it cannot be made
    /// before the guard exists. Recording first would leave a window — one
    /// `await` on a pooled connection — in which a dropped task handed both
    /// claims back and announced nothing, which is the state this value was
    /// written to make unreachable.
    ///
    /// `self_id` is the bot account the event arrived on. Taken as an argument
    /// rather than read from the config, because the config names the listener
    /// and the event names who answered — and those stop being the same thing
    /// the moment a second account connects to that listener.
    pub async fn open_record(&self, pool: &crate::db::DbPool, self_id: Option<i64>) -> Result<(), String> {
        crate::agent::turn_record::begin(pool, &self.turn_id, &self.conversation_id, TurnOrigin::OneBot, self_id).await
    }

    /// Fold a finished round's numbers in. Follow-up rounds are the same turn,
    /// so they add up rather than each reporting their own.
    pub fn record(&mut self, progress: agent::TurnProgress) {
        self.input_tokens += progress.input_tokens;
        self.output_tokens += progress.output_tokens;
        if progress.message_id.is_some() {
            self.message_id = progress.message_id;
        }
    }

    fn announce(&self, reason: crate::events::ChatStopReason) {
        let Some(sink) = self.sink.as_ref() else { return };
        sink(agent::turn_stop_event(
            &self.conversation_id,
            &self.turn_id,
            self.message_id.as_deref(),
            reason,
            self.input_tokens,
            self.output_tokens,
        ));
    }

    /// End a round. `None` means the turn is over and has been announced;
    /// `Some(items)` means it continues with those messages, and nothing has
    /// been announced because nothing has ended.
    pub fn end_round(&mut self, reason: crate::events::ChatStopReason) -> Option<Vec<InboxItem>> {
        let turn = self.turn.take().expect("a turn can only be ended once");
        // Ahead of the release, as on the desktop side: an answer arriving
        // after this has nobody to reach, and leaving the entry behind would
        // let the next call for this session inherit a stale one.
        self.approvals.retire_turn(&self.turn_id);
        match end_turn(&self.states, &self.session, turn, || self.announce(reason)) {
            TurnEnd::Done => None,
            TurnEnd::Continue(held, items) => {
                self.turn = Some(held);
                Some(items)
            }
        }
    }
}

impl Drop for RunningTurn {
    fn drop(&mut self) {
        // Ended normally: `end_round` took the turn and announced it.
        let Some(turn) = self.turn.take() else { return };
        // Anything still here means the runner died on its feet. It may have
        // died parked on an approval — `make_approval_fn` waits 60 seconds, and
        // a dropped task never reaches that timeout, so nothing else would ever
        // take the entry out.
        self.approvals.retire_turn(&self.turn_id);
        // Both claims back before the announcement: a stop is read as
        // permission to send, and the conversation has to actually be free by
        // the time it goes out.
        drop(turn);
        self.announce(crate::events::ChatStopReason::Error);
    }
}

/// What came of a session's attempt to start a turn.
pub enum TurnStart {
    /// This task owns the turn. Hold it for as long as the turn runs, follow-up
    /// rounds included.
    Started(SessionTurn),
    /// Another OneBot turn is running for this session and `item` went into its
    /// inbox — that turn will pick it up between tool rounds, or at its end.
    Queued,
    /// Something outside OneBot holds the conversation. Nothing was queued: the
    /// inbox is drained only by the OneBot runner, so a message parked there
    /// while the desktop is answering would sit until the next QQ message
    /// happened along.
    Elsewhere(Busy),
}

/// Try to start a turn for `session`, on the conversation it maps to.
///
/// Two things have to be taken, and they are not the same thing: the *session*,
/// which is what OneBot deduplicates and queues against, and the
/// *conversation*, which is the row set anyone might be writing. A QQ session's
/// conversation is an ordinary conversation the desktop can open and send to.
///
/// Lock order is `session_states` then the coordinator, here and everywhere.
/// The coordinator's critical sections never await and never reach back for
/// this lock, so the pair cannot invert.
pub fn try_begin_turn(
    states: &Arc<SessionStates>,
    coordinator: &Arc<TurnCoordinator>,
    session: &SessionKey,
    conversation_id: &str,
    item: InboxItem,
) -> TurnStart {
    let mut map = states.lock();
    let entry = map.entry(session.to_string()).or_default();
    if !entry.queue_unless_free(item) {
        return TurnStart::Queued;
    }
    match coordinator.try_acquire_turn(conversation_id, TurnOrigin::OneBot) {
        Ok(lease) => {
            entry.activate();
            TurnStart::Started(SessionTurn {
                states: Arc::clone(states),
                session: session.to_string(),
                turn_id: lease.turn_id().to_string(),
                cancel: lease.cancel_token().clone(),
                holding: Some(lease),
            })
        }
        // The session stays idle on purpose: it never became active, so the
        // next message can try again rather than queueing behind a turn that
        // does not exist.
        Err(busy) => TurnStart::Elsewhere(busy),
    }
}

/// Finish a turn. If the inbox holds user messages the session stays active,
/// the lease stays held, and they are handed back for an immediate follow-up
/// turn; notice-only leftovers stay queued for the next trigger.
///
/// `announce` is how the turn's end is told to anyone watching — the terminal
/// `chat-stream` event a desktop reads as permission to send again. It is taken
/// as a callback rather than left to the caller to run afterwards precisely so
/// the ordering is a property of *this* function: it runs only after both
/// claims have been handed back, and only on the path where the turn is
/// actually over. A `Continue` round is the same turn going round again, and
/// announcing an end there would invite a message the coordinator would refuse.
pub fn end_turn(
    states: &Arc<SessionStates>,
    session: &SessionKey,
    mut turn: SessionTurn,
    announce: impl FnOnce(),
) -> TurnEnd {
    let mut map = states.lock();
    match map.entry(session.to_string()).or_default().finish(now_ms()) {
        Finish::Done => {
            // `finish` has already cleared the flag, so the guard must not
            // clear it again — by the time this value is dropped another turn
            // may have set it. Taking the lease out here also releases the
            // conversation inside the same critical section: leaving it to the
            // drop after this returns would open an instant where the session
            // reads as free while the conversation was still taken, and a QQ
            // message landing there would be told "busy" by a turn that had
            // already finished.
            turn.holding = None;
            drop(map);
            announce();
            TurnEnd::Done
        }
        Finish::Continue(items) => TurnEnd::Continue(turn, items),
    }
}

/// Take everything queued for `session`; called by the agent loop between tool
/// rounds so events surface inside the running turn.
pub fn drain_inbox_mid_turn(states: &SessionStates, session: &SessionKey) -> Vec<InboxItem> {
    let mut map = states.lock();
    let Some(s) = map.get_mut(&session.to_string()) else {
        return vec![];
    };
    expire_inbox(&mut s.inbox, now_ms());
    std::mem::take(&mut s.inbox)
}

/// Queue a notice note for `session`; oldest notes are dropped past the cap.
pub fn push_notice_note(states: &SessionStates, session: &SessionKey, text: String) {
    states
        .lock()
        .entry(session.to_string())
        .or_default()
        .push_note(text, now_ms());
}

/// Record an OneBot message id that entered the AI context for `session`.
pub fn record_seen_message(states: &SessionStates, session: &SessionKey, message_id: i64) {
    states
        .lock()
        .entry(session.to_string())
        .or_default()
        .record_seen(message_id);
}

pub fn was_seen_message(states: &SessionStates, session: &SessionKey, message_id: i64) -> bool {
    states
        .lock()
        .get(&session.to_string())
        .is_some_and(|s| s.seen_message_ids.contains(&message_id))
}

/// Handle passed into the headless agent loop so it can pull queued events into
/// the running turn between tool rounds.
pub struct InboxHandle {
    states: Arc<SessionStates>,
    session: SessionKey,
}

impl InboxHandle {
    pub fn new(states: Arc<SessionStates>, session: SessionKey) -> Self {
        Self { states, session }
    }

    pub fn drain(&self) -> Vec<InboxItem> {
        drain_inbox_mid_turn(&self.states, &self.session)
    }
}

/// A friend request or group invite waiting for admin approval.
#[derive(Debug, Clone)]
pub struct PendingRequest {
    pub kind: RequestKind,
    pub flag: String,
    pub user_id: i64,
    pub group_id: Option<i64>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Friend,
    GroupAdd,
    GroupInvite,
}

/// 记下一条连接是哪个账号，并回答"这个号是我们自己的吗"。
///
/// 认第一个报上来的，之后不改——见 `conn_identities` 的说明。
pub fn note_identity(state: &Arc<SharedState>, conn_id: u64, self_id: i64) {
    if self_id == 0 {
        return;
    }
    if let Ok(mut identities) = state.conn_identities.lock() {
        identities.entry(conn_id).or_insert(self_id);
    }
}

impl SharedState {
    /// 哪条连接是这个账号的。发消息要走回它自己那条——广播会让两个号各发一遍。
    pub fn conn_for_self_id(&self, self_id: Option<i64>) -> Option<u64> {
        let want = self_id?;
        let identities = self.conn_identities.lock().ok()?;
        identities.iter().find_map(|(conn, id)| (*id == want).then_some(*conn))
    }
}

/// 这个号是不是本地的某个 bot 账号。
pub fn is_local_bot(state: &Arc<SharedState>, user_id: i64) -> bool {
    state
        .conn_identities
        .lock()
        .map(|identities| identities.values().any(|id| *id == user_id))
        .unwrap_or(false)
}

/// One API call waiting on a reply.
///
/// `expected_conn` is what makes a call *directed*: only that connection's
/// answer counts. `None` is the broadcast path, which has no predetermined
/// target and takes whichever adapter answers first.
pub struct PendingCall {
    pub expected_conn: Option<u64>,
    pub tx: oneshot::Sender<OneBotResponse>,
}

/// What became of a directed call.
///
/// Four states rather than `Result`, because "it timed out" and "it was
/// refused" are different facts and the caller acts on them differently. In
/// particular [`Self::DeliveryUnknown`] is **not** a failure: the frame is in
/// the connection's queue and may well have been acted on. Reporting it as an
/// error is how you get the same voice message sent twice.
#[derive(Debug)]
pub enum DirectedCallOutcome {
    /// Never left this process. Retrying is safe.
    NotDispatched(String),
    /// The adapter answered and said no. Retrying will not help.
    Refused { retcode: i32, message: String },
    /// The adapter took it. Not a promise that anyone read it.
    AdapterAccepted(serde_json::Value),
    /// Queued, but no answer came back. **It may have happened.**
    DeliveryUnknown,
}

/// Removes a waiter when its call goes away.
///
/// Not doable with cleanup on each return path: this future gets cancelled — a
/// turn that was stopped, a connection that dropped — and then no return path
/// runs at all. The keys are UUIDs, so a leaked entry is never overwritten by a
/// later call; it just accumulates for as long as the server is up.
struct WaiterGuard<'a> {
    state: &'a Arc<SharedState>,
    echo: String,
}

impl Drop for WaiterGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.state.pending_api_responses.lock() {
            pending.remove(&self.echo);
        }
    }
}

/// Send one action to one connection and wait for that connection's answer.
///
/// The broadcast path below sends to *every* adapter, which is wrong for
/// anything answering an event: two connected accounts means a second adapter
/// that has never heard of this `message_id` answers first with an error, and
/// the one waiter takes it. For an outbound message it is worse — both accounts
/// send it.
///
/// The echo is generated here rather than taken from the caller, because it is
/// what pairs the answer with this call and nothing else may share it.
pub async fn call_api_to_conn(
    state: &Arc<SharedState>,
    conn_id: u64,
    action: OneBotAction,
    timeout: std::time::Duration,
) -> DirectedCallOutcome {
    call_api_to_conn_within(state, conn_id, action, timeout, timeout).await
}

/// Same, with its own ceiling on how long the frame may sit in the queue.
///
/// The two halves are not the same kind of wait. Waiting for an *answer* is
/// waiting on the far end and there is nothing to decide; waiting for *room in
/// the queue* is time during which the caller's reason for sending may expire,
/// and it is time the caller could still take back. `send_voice` is why the
/// distinction exists: a permission checked immediately before the send is
/// worth nothing if the frame then sits behind a full queue for the whole
/// twenty seconds, and a voice reply that lands that late is wrong anyway.
///
/// Callers with nothing to revoke pass the same value twice.
pub async fn call_api_to_conn_within(
    state: &Arc<SharedState>,
    conn_id: u64,
    action: OneBotAction,
    enqueue_timeout: std::time::Duration,
    timeout: std::time::Duration,
) -> DirectedCallOutcome {
    let echo = uuid::Uuid::new_v4().to_string();
    let json = match serde_json::to_string(&action.with_echo(echo.clone())) {
        Ok(json) => json,
        Err(e) => return DirectedCallOutcome::NotDispatched(e.to_string()),
    };
    let Some(sink) = state.ws_sinks.lock().await.get(&conn_id).cloned() else {
        return DirectedCallOutcome::NotDispatched(format!("connection {conn_id} is gone"));
    };

    let (tx, rx) = oneshot::channel();
    {
        let Ok(mut pending) = state.pending_api_responses.lock() else {
            return DirectedCallOutcome::NotDispatched("pending table poisoned".into());
        };
        pending.insert(
            echo.clone(),
            PendingCall {
                expected_conn: Some(conn_id),
                tx,
            },
        );
    }
    let _guard = WaiterGuard { state, echo };

    // One deadline across both halves, so a full queue that eventually drains
    // does not get a fresh timeout to answer in. The enqueue may be given a
    // tighter one of its own; whichever comes first wins.
    let deadline = tokio::time::Instant::now() + timeout;
    let enqueue_by = (tokio::time::Instant::now() + enqueue_timeout).min(deadline);
    match tokio::time::timeout_at(enqueue_by, sink.send(json)).await {
        Err(_) => return DirectedCallOutcome::NotDispatched("the connection's queue stayed full".into()),
        Ok(Err(_)) => return DirectedCallOutcome::NotDispatched("the connection closed".into()),
        Ok(Ok(())) => {}
    }

    // Past this point the frame is queued, so nothing below may say it was not
    // dispatched.
    match tokio::time::timeout_at(deadline, rx).await {
        Ok(Ok(resp)) => match resp.retcode {
            Some(0) => DirectedCallOutcome::AdapterAccepted(resp.data.clone().unwrap_or(serde_json::Value::Null)),
            Some(retcode) => DirectedCallOutcome::Refused {
                retcode,
                message: resp.complaint().unwrap_or("no reason given").to_string(),
            },
            // An answer with no retcode has told us nothing. It is not a
            // refusal, and treating it as one would report a message that did
            // go out as one that did not.
            None => DirectedCallOutcome::DeliveryUnknown,
        },
        _ => DirectedCallOutcome::DeliveryUnknown,
    }
}

/// Broadcast a pre-serialized frame to all connected clients. Senders are
/// cloned out of the ws_sinks lock so a slow client only blocks this task
/// (never other lock users), and a momentarily full queue backpressures rather
/// than silently dropping the message.
async fn broadcast(state: &Arc<SharedState>, json: String) {
    let sinks: Vec<mpsc::Sender<String>> = state.ws_sinks.lock().await.values().cloned().collect();
    for sink in sinks {
        let _ = sink.send(json.clone()).await;
    }
}

/// Broadcast an action to all connected clients without waiting for a response.
pub async fn send_action_nowait(state: &Arc<SharedState>, action: &OneBotAction) {
    let Ok(json) = serde_json::to_string(action) else {
        return;
    };
    broadcast(state, json).await;
}

pub async fn call_api(state: &Arc<SharedState>, action: OneBotAction) -> Result<serde_json::Value, String> {
    call_api_with_timeout(state, action, std::time::Duration::from_secs(10)).await
}

pub async fn call_api_with_timeout(
    state: &Arc<SharedState>,
    action: OneBotAction,
    timeout: std::time::Duration,
) -> Result<serde_json::Value, String> {
    let echo = action.echo.clone().unwrap_or_default();
    // Serialize before inserting into pending so a serialization failure can't
    // leave an orphaned pending entry behind.
    let json = serde_json::to_string(&action).map_err(|e| e.to_string())?;
    let (tx, rx) = oneshot::channel();
    {
        let mut pending = state
            .pending_api_responses
            .lock()
            .map_err(|_| "pending table poisoned")?;
        pending.insert(
            echo.clone(),
            PendingCall {
                // No particular adapter: this one goes to all of them and the
                // first answer wins. See `call_api_to_conn` for why anything
                // answering a specific event should not use this path.
                expected_conn: None,
                tx,
            },
        );
    }
    let _guard = WaiterGuard {
        state,
        echo: echo.clone(),
    };
    broadcast(state, json).await;
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(resp)) => {
            if resp.retcode == Some(0) {
                Ok(resp.data.unwrap_or(serde_json::Value::Null))
            } else {
                Err(match resp.complaint() {
                    Some(why) => format!("API error: {why}"),
                    None => format!("API error: {:?}", resp.status),
                })
            }
        }
        _ => Err("API call timed out".into()),
    }
}

#[derive(Debug, Clone)]
pub struct OneBotConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub access_token: Option<String>,
    pub assistant_id: Option<String>,
    pub admin_users: Vec<i64>,
    /// QQ emoji id used to acknowledge group messages; empty or "0" disables.
    pub ack_emoji_id: String,
    /// Tell the admins when a provider's credit falls below this.
    ///
    /// `None` switches the watcher off entirely, which is the default: it makes
    /// periodic requests with the user's API keys, so it should exist because
    /// somebody asked for it rather than because they installed the app. `0`
    /// keeps the watcher but drops the early warning — the admins are told only
    /// when the upstream itself reports the account unusable.
    ///
    /// Compared per currency rather than against a sum; see
    /// `ProviderBalance::is_low`.
    pub balance_alert_threshold: Option<crate::decimal::Decimal>,
    /// 要留存入站语音的 `(bot 账号, 会话)`，写作 `<bot>@group:123`。
    ///
    /// 空是默认，意思是一个都不留。存的是真人声纹，所以这是**许可名单而不是
    /// 过滤器**：不在名单上的会话不产生任务、不落盘、不写行。
    ///
    /// 账号要写进去而不只是会话：两个 bot 各自被拉进同一个群，是两次独立的
    /// 同意。
    pub voice_capture_sessions: Vec<String>,
    /// 模型可不可以用语音回复。
    ///
    /// **默认关**：它要一个 Fish Audio 的 key 和一个音色，没配齐就把工具端上去
    /// 只会让模型反复调用一个必然失败的东西。
    pub voice_send_enabled: bool,
    /// 允许 bot 发语音的群，写作 `<bot>@group:123`。
    ///
    /// 私聊默认就开（一个对手方，屋主就是听的人），所以这里只列群——群是一间
    /// 屋子，发不发语音是屋主的决定。**与采集白名单是两份**：一个授权保存真人
    /// 声纹，一个授权 bot 说话。
    pub voice_send_groups: Vec<String>,
    /// Fish Audio 的型号。**默认空**——`s2.1-pro-free` 的官方免费期到
    /// 2026-08-31，把它设成永久默认就是给一个到期日安排一次集体失效。
    pub voice_tts_model: String,
    /// 固定音色。机器人的嗓音是身份，不是每次调用的选项。
    pub voice_tts_reference_id: String,
}

fn default_ack_emoji() -> String {
    "76".into()
}

impl Default for OneBotConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: "127.0.0.1".into(),
            port: 6700,
            access_token: None,
            assistant_id: None,
            admin_users: vec![],
            ack_emoji_id: default_ack_emoji(),
            balance_alert_threshold: None,
            voice_capture_sessions: vec![],
            voice_send_enabled: false,
            voice_send_groups: vec![],
            voice_tts_model: String::new(),
            voice_tts_reference_id: String::new(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct OneBotStatus {
    pub enabled: bool,
    pub running: bool,
    pub connected_clients: u32,
    pub host: String,
    pub port: u16,
}

fn parse_stored_bool(key: &str, raw: Option<String>, default: bool) -> Result<bool, String> {
    match raw.as_deref() {
        None => Ok(default),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(value) => Err(format!("preference {key} must be 'true' or 'false', got {value:?}")),
    }
}

fn parse_stored_port(key: &str, raw: Option<String>, default: u16) -> Result<u16, String> {
    let Some(raw) = raw else {
        return Ok(default);
    };
    let port = raw
        .parse::<u16>()
        .map_err(|error| format!("preference {key} has invalid port {raw:?}: {error}"))?;
    if port.to_string() != raw {
        return Err(format!(
            "preference {key} must use canonical decimal digits, got {raw:?}"
        ));
    }
    Ok(port)
}

fn parse_stored_json<T>(key: &str, raw: Option<String>) -> Result<T, String>
where
    T: serde::de::DeserializeOwned + Default,
{
    match raw {
        Some(raw) => serde_json::from_str(&raw).map_err(|error| format!("preference {key} has invalid JSON: {error}")),
        None => Ok(T::default()),
    }
}

fn validate_scope_list(key: &str, values: &[String], groups_only: bool) -> Result<(), String> {
    for raw in values {
        let (bot, session) = raw
            .split_once('@')
            .ok_or_else(|| format!("preference {key} contains invalid scope {raw:?}"))?;
        let bot_id = bot
            .parse::<i64>()
            .map_err(|error| format!("preference {key} contains invalid bot id in {raw:?}: {error}"))?;
        let (kind, source) = session
            .split_once(':')
            .ok_or_else(|| format!("preference {key} contains invalid session in {raw:?}"))?;
        if !matches!(kind, "group" | "private") || groups_only && kind != "group" {
            return Err(format!("preference {key} contains unsupported session kind in {raw:?}"));
        }
        let source_id = source
            .parse::<i64>()
            .map_err(|error| format!("preference {key} contains invalid session id in {raw:?}: {error}"))?;
        let canonical = format!("{bot_id}@{kind}:{source_id}");
        if canonical != *raw {
            return Err(format!("preference {key} contains non-canonical scope {raw:?}"));
        }
    }
    Ok(())
}

fn parse_stored_decimal(key: &str, raw: Option<String>) -> Result<Option<crate::decimal::Decimal>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Err(format!(
            "preference {key} must be absent or contain a canonical decimal"
        ));
    }
    let value = raw
        .parse::<crate::decimal::Decimal>()
        .map_err(|error| format!("preference {key} has invalid decimal {raw:?}: {error}"))?;
    if value.to_string() != raw {
        return Err(format!("preference {key} has non-canonical decimal {raw:?}"));
    }
    value
        .require_non_negative(key)
        .map(Some)
        .map_err(|error| error.to_string())
}

pub fn load_config(pool: &DbPool) -> Result<OneBotConfig, String> {
    let mut conn = get_conn(pool)?;
    let mut get = |key: &str| -> Result<Option<String>, String> {
        crate::db::ops::preference::get_preference(&mut conn, key)
            .map_err(|error| format!("failed to read preference {key}: {error}"))
    };

    let voice_capture_sessions: Vec<String> =
        parse_stored_json("onebot.voice_capture_sessions", get("onebot.voice_capture_sessions")?)?;
    validate_scope_list("onebot.voice_capture_sessions", &voice_capture_sessions, false)?;
    let voice_send_groups: Vec<String> =
        parse_stored_json("onebot.voice_send_groups", get("onebot.voice_send_groups")?)?;
    validate_scope_list("onebot.voice_send_groups", &voice_send_groups, true)?;

    Ok(OneBotConfig {
        enabled: parse_stored_bool("onebot.enabled", get("onebot.enabled")?, false)?,
        host: get("onebot.host")?.unwrap_or_else(|| "127.0.0.1".into()),
        port: parse_stored_port("onebot.port", get("onebot.port")?, 6700)?,
        access_token: get("onebot.access_token")?.filter(|s| !s.is_empty()),
        assistant_id: get("onebot.assistant_id")?.filter(|s| !s.is_empty()),
        admin_users: parse_stored_json("onebot.admin_users", get("onebot.admin_users")?)?,
        ack_emoji_id: get("onebot.ack_emoji_id")?.unwrap_or_else(default_ack_emoji),
        // Empty means off, which is why this is not `unwrap_or(0.0)`: zero is a
        // meaningful setting here — watch, but only alert when the upstream says
        // the account has stopped working.
        balance_alert_threshold: parse_stored_decimal(
            "onebot.balance_alert_threshold",
            get("onebot.balance_alert_threshold")?,
        )?,
        voice_capture_sessions,
        voice_send_enabled: parse_stored_bool("onebot.voice_send_enabled", get("onebot.voice_send_enabled")?, false)?,
        voice_send_groups,
        voice_tts_model: get("onebot.voice_tts_model")?.unwrap_or_default(),
        voice_tts_reference_id: get("onebot.voice_tts_reference_id")?.unwrap_or_default(),
    })
}

/// 写下整份配置。
///
/// **一个事务**，不是逐条写。中途失败会留下一个没人能解释的状态：UI 报了失败，
/// 内存里还是旧策略，而重启之后生效的却是写进去的那一半。对普通设置那是难看，
/// 对 `voice_capture_sessions` 那是"用户以为关掉了而它还在录"。
pub fn save_config(pool: &DbPool, config: &OneBotConfig) -> Result<(), String> {
    use diesel::connection::Connection;

    validate_scope_list("onebot.voice_capture_sessions", &config.voice_capture_sessions, false)?;
    validate_scope_list("onebot.voice_send_groups", &config.voice_send_groups, true)?;
    if config
        .balance_alert_threshold
        .as_ref()
        .is_some_and(|value| value.is_negative())
    {
        return Err("onebot.balance_alert_threshold must be non-negative".into());
    }
    let mut conn = get_conn(pool)?;
    let now = now_ms();
    let admin_users = serde_json::to_string(&config.admin_users)
        .map_err(|error| format!("could not serialize onebot.admin_users: {error}"))?;
    let voice_capture_sessions = serde_json::to_string(&config.voice_capture_sessions)
        .map_err(|error| format!("could not serialize onebot.voice_capture_sessions: {error}"))?;
    let voice_send_groups = serde_json::to_string(&config.voice_send_groups)
        .map_err(|error| format!("could not serialize onebot.voice_send_groups: {error}"))?;

    conn.transaction::<_, diesel::result::Error, _>(|conn| {
        match config.balance_alert_threshold.as_ref() {
            Some(value) => crate::db::ops::preference::set_preference(
                conn,
                "onebot.balance_alert_threshold",
                &value.to_string(),
                now,
            )?,
            None => crate::db::ops::preference::delete_preference(conn, "onebot.balance_alert_threshold")?,
        }

        let mut set =
            |key: &str, val: &str| crate::db::ops::preference::set_preference(conn, key, val, now).map(|_| ());

        set("onebot.enabled", if config.enabled { "true" } else { "false" })?;
        set("onebot.host", &config.host)?;
        set("onebot.port", &config.port.to_string())?;
        set("onebot.access_token", config.access_token.as_deref().unwrap_or(""))?;
        set("onebot.assistant_id", config.assistant_id.as_deref().unwrap_or(""))?;
        set("onebot.admin_users", &admin_users)?;
        set("onebot.ack_emoji_id", &config.ack_emoji_id)?;
        set("onebot.voice_capture_sessions", &voice_capture_sessions)?;
        set(
            "onebot.voice_send_enabled",
            if config.voice_send_enabled { "true" } else { "false" },
        )?;
        set("onebot.voice_send_groups", &voice_send_groups)?;
        set("onebot.voice_tts_model", &config.voice_tts_model)?;
        set("onebot.voice_tts_reference_id", &config.voice_tts_reference_id)?;
        Ok(())
    })
    .map_err(|e| e.to_string())
}

/// 把配置里那一行行文本解析成授权范围。
///
/// 配置在读取和保存时都已经校验；这里仍返回错误，避免未来新增调用方绕过边界后
/// 把坏条目静默丢掉。
fn capture_scopes(
    config: &OneBotConfig,
) -> Result<std::collections::HashSet<crate::voice_corpus::CaptureScope>, String> {
    config
        .voice_capture_sessions
        .iter()
        .map(|raw| {
            crate::voice_corpus::CaptureScope::parse(raw)
                .ok_or_else(|| format!("onebot.voice_capture_sessions contains invalid scope {raw:?}"))
        })
        .collect()
}

/// 把配置里的语音策略推给协调器，等在途采集结束。
///
/// 返回时"不再新增"已经成立。已经拿到 permit 的那些允许跑完——那是 permit 的
/// 正常语义，也是唯一能简单推理的：取消一个正在下载的任务，要么留下半个文件，
/// 要么要一整套取消传播。
pub async fn refresh_voice_policy(services: &Services, config: &OneBotConfig) -> Result<(), String> {
    let scopes = capture_scopes(config)?;
    let pool = services.db.clone();
    let secrets = services.secrets.clone();
    // keyring 是阻塞 IO，和 opt-out 的查询一起挪到 blocking 线程上。
    //
    // **读不到就报错，不能当作空名单。** 这张表通常是空的，所以"查询失败"和
    // "没人拒绝过"在结果上长得一模一样——而把失败读成后者，等于让一次瞬时的
    // 数据库错误重新开始录一个已经明确说过不要的人。
    let (optouts, key_fingerprint) = tokio::task::spawn_blocking(move || {
        let mut conn = get_conn(&pool)?;
        let optouts = crate::db::ops::voice_corpus::optouts(&mut conn).map_err(|e| e.to_string())?;
        Ok::<_, String>((optouts, fish_key_fingerprint(&secrets)))
    })
    .await
    .map_err(|e| e.to_string())??;

    services.corpus.apply(scopes, optouts.into_iter().collect()).await;

    // 四项凑齐才算就绪，缺一则工具从三处同时消失。key 也在其中——只监听
    // preference 变化会漏掉换 key，而那正是让一个"配好了"的会话开始失败的
    // 那种变更。
    let readiness = key_fingerprint
        .filter(|_| config.voice_send_enabled)
        .filter(|_| !config.voice_tts_model.trim().is_empty())
        .filter(|_| !config.voice_tts_reference_id.trim().is_empty())
        .map(|key_fingerprint| crate::voice_corpus::SendReadiness {
            model: config.voice_tts_model.trim().to_string(),
            reference_id: config.voice_tts_reference_id.trim().to_string(),
            key_fingerprint,
        });
    if config.voice_send_enabled && readiness.is_none() {
        // 开关开着而工具不出现，是这个功能唯一一种"什么都不说"的失败：用户会
        // 反复问助手为什么不会说话，而助手看不见这个工具，所以它自己也答不上来。
        tracing::warn!(
            has_model = !config.voice_tts_model.trim().is_empty(),
            has_reference = !config.voice_tts_reference_id.trim().is_empty(),
            "voice replies are switched on but one of the four is missing; send_voice stays hidden"
        );
    }
    let groups = config
        .voice_send_groups
        .iter()
        .map(|raw| {
            crate::voice_corpus::CaptureScope::parse(raw)
                .ok_or_else(|| format!("onebot.voice_send_groups contains invalid scope {raw:?}"))
        })
        .collect::<Result<_, _>>()?;
    services.corpus.apply_send_policy(groups, readiness);
    Ok(())
}

/// 出站语音差哪一项。设置页照着它说话——四项之中缺哪个，只有这一层知道。
#[derive(Debug, Clone, serde::Serialize)]
pub struct VoiceSendReadiness {
    pub enabled: bool,
    pub has_model: bool,
    pub has_reference_id: bool,
    pub has_api_key: bool,
    /// 四项齐全，`send_voice` 现在真的在工具表里。
    pub ready: bool,
}

pub fn voice_send_readiness(services: &Services, config: &OneBotConfig) -> VoiceSendReadiness {
    let has_api_key = fish_key_fingerprint(&services.secrets).is_some();
    let has_model = !config.voice_tts_model.trim().is_empty();
    let has_reference_id = !config.voice_tts_reference_id.trim().is_empty();
    VoiceSendReadiness {
        enabled: config.voice_send_enabled,
        has_model,
        has_reference_id,
        has_api_key,
        ready: config.voice_send_enabled && has_model && has_reference_id && has_api_key,
    }
}

/// key 的指纹，不是 key。策略要能回答"换过没有"，而把密钥抄进一个会被 Debug
/// 打印的结构里没有必要。`None` = 没配。
fn fish_key_fingerprint(secrets: &crate::secrets::SecretsManager) -> Option<String> {
    use sha2::Digest;
    let name = crate::secrets::SecretName::new(FISH_KEY_SECRET).ok()?;
    let key = secrets
        .get(&crate::secrets::SecretScope::Global, &name)
        .ok()
        .flatten()
        .filter(|k| !k.trim().is_empty())?;
    let digest = sha2::Sha256::digest(key.as_bytes());
    Some(digest[..8].iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{b:02x}");
        acc
    }))
}

/// Fish Audio 的 key 名。与 web_search 的那几个同一套命名
/// （`SERVICE_{X}_KEY`），所以前端用现成的 `setServiceKey('FISH_AUDIO', …)`。
pub const FISH_KEY_SECRET: &str = "SERVICE_FISH_AUDIO_KEY";

/// Manages the OneBot WS server lifecycle.
pub struct OneBotServer {
    state: Arc<SharedState>,
    running: Arc<std::sync::atomic::AtomicBool>,
}

impl OneBotServer {
    pub fn new(services: Services, config: OneBotConfig) -> Self {
        let (shutdown_tx, _) = watch::channel(false);
        let (conn_closed, _) = watch::channel(0);
        Self {
            state: Arc::new(SharedState {
                sessions: Mutex::new(SessionManager::new(services.db.clone())),
                pending_approvals: Arc::new(PendingApprovals::default()),
                pending_api_responses: std::sync::Mutex::new(HashMap::new()),
                pending_requests: Mutex::new(HashMap::new()),
                // Time-seeded so ids don't restart at 1 after a relaunch, which
                // would let a stale "同意 N" notification approve a new request.
                request_seq: AtomicU32::new((now_ms() / 1000 % 1_000_000) as u32),
                ws_sinks: Mutex::new(HashMap::new()),
                conn_identities: std::sync::Mutex::new(HashMap::new()),
                connected_clients: AtomicU32::new(0),
                shutdown: shutdown_tx,
                conn_closed,
                session_states: Arc::new(SessionStates::default()),
                memory_listings: Mutex::new(HashMap::new()),
                config,
                services,
            }),
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn status(&self) -> OneBotStatus {
        OneBotStatus {
            enabled: self.state.config.enabled,
            running: self.is_running(),
            connected_clients: self.state.connected_clients.load(Ordering::Relaxed),
            host: self.state.config.host.clone(),
            port: self.state.config.port,
        }
    }

    pub fn start(&self) -> Result<(), String> {
        if self.is_running() {
            return Err("OneBot server is already running".into());
        }
        // Sharper here than for the other two listeners: anyone who can reach
        // this socket can submit an event with an arbitrary `user_id`, i.e.
        // impersonate an admin.
        validate_listen_config(
            &self.state.config.host,
            self.state.config.access_token.as_deref(),
            "the OneBot access token",
        )?;

        let state = self.state.clone();
        let running = self.running.clone();
        let mut shutdown_rx = self.state.shutdown.subscribe();

        // Started below, once the port is actually bound. Spawning it here would
        // leave a watcher polling every six hours behind a server that never came
        // up — asking the provider for a balance it has nowhere to report.
        let watcher = (self.state.clone(), self.state.shutdown.subscribe());

        running.store(true, Ordering::Relaxed);

        tokio::spawn(async move {
            // 把这一代服务的语音策略交给协调器，**开门之前**。协调器活得比服务
            // 久（它在 `Services` 上），所以这是"换掉"而不是"初始化"：上一代的
            // 授权连同它的 generation 一起被这次调用作废。
            //
            // 顺序有两条约束，都不是偏好。恢复器在策略之前：它无条件清空
            // `.staging` 和所有 `pending` 行，靠的是"此刻没有 writer"——策略先
            // 装上，一个抢跑的采集就会跟清扫赛跑。bind 在两者之后：绑定在前的
            // 话，从开门到策略装好之间到达的语音事件对着一个空白名单被静默丢弃，
            // 而恢复器要哈希整个语料目录，这个窗口不是几毫秒。适配器是反向 WS，
            // 晚几秒开门只是让它多重试一次。
            //
            // 放在这里而不是 bootstrap：恢复要扫目录，没开 OneBot 的人不该为此
            // 等在应用启动上。
            //
            // **两套策略都要**。这里一度只推了采集白名单，于是出站那一套在一个
            // 新进程里永远是空的——四项填齐、保存过、重启一次，`send_voice` 就
            // 再也不出现，直到有人重新点一次保存。而它不出现的时候，助手自己也
            // 看不见它，所以问助手只会得到"我没有语音工具"。
            {
                let services = state.services.clone();
                let config = state.config.clone();
                let pool = services.db.clone();
                let data_dir = services.paths.data_dir.clone();
                let writable = services.corpus.writable();
                let recovered =
                    tokio::task::spawn_blocking(move || crate::voice_corpus::recover::run(&pool, &data_dir, writable))
                        .await;
                if let Ok(Err(error)) = recovered {
                    tracing::warn!(%error, "voice corpus recovery failed");
                }
                if let Err(error) = refresh_voice_policy(&services, &config).await {
                    // 采集与出站都没装上。说出来——静默的结果是一个开着的开关
                    // 什么也不做。
                    tracing::warn!(%error, "voice policy could not be applied; voice stays off this session");
                }
            }

            // 这一代在恢复期间就被 stop() 掉了。现在 bind 只会跟下一代抢端口——
            // 它自己的恢复也要几秒，两边谁先谁后没有保证。
            if *shutdown_rx.borrow() {
                running.store(false, Ordering::Relaxed);
                return;
            }

            let addr = format!("{}:{}", state.config.host, state.config.port);
            let listener = match TcpListener::bind(&addr).await {
                Ok(l) => {
                    tracing::info!("OneBot WS server listening on {addr}");
                    l
                }
                Err(e) => {
                    tracing::error!("Failed to bind OneBot WS server to {addr}: {e}");
                    running.store(false, Ordering::Relaxed);
                    return;
                }
            };

            // Its own task on the same shutdown signal: a six-hour timer has no
            // business inside the accept loop, and it has to stop when the
            // server does — a replaced server would otherwise leave a watcher
            // behind holding the outgoing generation's admin list.
            balance_watch::spawn(watcher.0, watcher.1);

            let mut conn_id_counter: u64 = 0;

            loop {
                tokio::select! {
                    changed = shutdown_rx.changed() => {
                        // Err = all senders dropped (this server was replaced);
                        // either case means stop, so don't hot-spin on a closed channel.
                        if changed.is_err() || *shutdown_rx.borrow() {
                            tracing::info!("OneBot WS server shutting down");
                            break;
                        }
                    }
                    result = listener.accept() => {
                        match result {
                            Ok((stream, peer)) => {
                                conn_id_counter += 1;
                                let conn_id = conn_id_counter;
                                let state = state.clone();

                                tokio::spawn(async move {
                                    use tokio_tungstenite::tungstenite::handshake::server::{
                                        ErrorResponse, Request, Response,
                                    };
                                    use tokio_tungstenite::tungstenite::http::StatusCode;

                                    let expected_token = state.config.access_token.clone();
                                    let callback = move |req: &Request, resp: Response|
                                        -> Result<Response, ErrorResponse> {
                                        let Some(ref expected) = expected_token else {
                                            return Ok(resp);
                                        };
                                        let auth = req.headers().get("authorization")
                                            .and_then(|v| v.to_str().ok());
                                        if token_matches(expected, auth, req.uri().query()) {
                                            Ok(resp)
                                        } else {
                                            let mut r = ErrorResponse::new(Some("Unauthorized".into()));
                                            *r.status_mut() = StatusCode::UNAUTHORIZED;
                                            Err(r)
                                        }
                                    };

                                    let ws_stream = match tokio_tungstenite::accept_hdr_async(stream, callback).await {
                                        Ok(ws) => ws,
                                        Err(e) => {
                                            tracing::warn!("WS handshake failed from {peer}: {e}");
                                            return;
                                        }
                                    };

                                    tracing::info!("OneBot client connected from {peer} (id={conn_id})");
                                    state.connected_clients.fetch_add(1, Ordering::Relaxed);

                                    handle_connection(ws_stream, conn_id, state.clone()).await;

                                    state.connected_clients.fetch_sub(1, Ordering::Relaxed);
                                    // Decrement first: `stop()` reads the count
                                    // and then waits for this to move, so a bump
                                    // ahead of the decrement would let it look
                                    // once more and see the connection still up.
                                    state.conn_closed.send_modify(|n| *n += 1);
                                    tracing::info!("OneBot client disconnected (id={conn_id})");
                                });
                            }
                            Err(e) => {
                                tracing::error!("Failed to accept connection: {e}");
                            }
                        }
                    }
                }
            }

            running.store(false, Ordering::Relaxed);
        });

        Ok(())
    }

    /// Stop accepting, close what is already connected, and wait for it.
    ///
    /// Three steps, and each one exists because the one before it is not enough.
    ///
    /// **The signal**, because dropping the sinks only closes the writing half:
    /// `split()` hands out two halves of one stream, so the reader keeps reading,
    /// keeps handling events and keeps whatever permissions it started with. A
    /// restart meant to apply new settings left the old generation running
    /// beside the new one — for anything the user revokes, that is the
    /// difference between a setting and a suggestion.
    ///
    /// **The wait**, because a signal nobody has acted on yet is not a stop. The
    /// caller's next move is to build the next generation, whose first act is a
    /// corpus recovery pass that clears every `.part` and every `pending` row
    /// unconditionally — sound only when there is no writer, which is exactly
    /// what this is waiting to become true.
    ///
    /// **The quiesce**, because a capture already under way holds a permit and
    /// is not on any connection. `granted_scopes` is emptied here rather than by
    /// the next `start()`: an adapter that reconnects in between would otherwise
    /// be recording under the outgoing generation's allowlist.
    ///
    /// Bounded, because the other end of this is a person pressing a button. A
    /// connection that will not close does not get to hold the settings page
    /// open; what it can no longer do is receive anything, since its sink is
    /// already gone.
    pub async fn stop(&self) {
        let _ = self.state.shutdown.send(true);
        self.running.store(false, Ordering::Relaxed);
        {
            let mut sinks = self.state.ws_sinks.lock().await;
            sinks.clear();
        }

        // Subscribed before the first read, so a connection closing between the
        // check and the wait cannot be missed.
        let mut closed = self.state.conn_closed.subscribe();
        let waited = tokio::time::timeout(CONNECTION_CLOSE_TIMEOUT, async {
            loop {
                if self.state.connected_clients.load(Ordering::Relaxed) == 0 {
                    break;
                }
                if closed.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
        if waited.is_err() {
            tracing::warn!(
                live = self.state.connected_clients.load(Ordering::Relaxed),
                "OneBot: a connection did not close in time; it can no longer send anything"
            );
        }

        self.state.services.corpus.quiesce().await;
    }
}

/// How long `stop()` waits for the readers.
///
/// Generous next to a socket close and short next to a person's patience. The
/// accept loop is already down and every sink is already gone, so what this
/// bounds is only how long the outgoing generation may keep *reading*.
const CONNECTION_CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Check an access token against the Authorization header (`Bearer <t>`,
/// `Token <t>`, or bare) or the `access_token` query parameter.
fn token_matches(expected: &str, auth_header: Option<&str>, query: Option<&str>) -> bool {
    if let Some(auth) = auth_header {
        let token = auth
            .strip_prefix("Bearer ")
            .or_else(|| auth.strip_prefix("Token "))
            .unwrap_or(auth)
            .trim();
        if constant_time_eq(token.as_bytes(), expected.as_bytes()) {
            return true;
        }
    }
    if let Some(q) = query {
        for kv in q.split('&') {
            if let Some(v) = kv.strip_prefix("access_token=") {
                let decoded = percent_encoding::percent_decode_str(v).decode_utf8_lossy();
                if constant_time_eq(decoded.as_bytes(), expected.as_bytes()) {
                    return true;
                }
            }
        }
    }
    false
}

/// Send actions to one connection. The sender is cloned out of the lock so a
/// slow client only blocks the calling task, never other ws_sinks users.
async fn send_to_conn(state: &Arc<SharedState>, conn_id: u64, actions: Vec<OneBotAction>) {
    let sink = state.ws_sinks.lock().await.get(&conn_id).cloned();
    let Some(sink) = sink else { return };
    for action in actions {
        if let Ok(json) = serde_json::to_string(&action) {
            let _ = sink.send(json).await;
        }
    }
}

async fn handle_connection(
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    conn_id: u64,
    state: Arc<SharedState>,
) {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (mut write, mut read) = ws.split();

    // Set up a channel for outbound messages (used by approval requests etc.)
    let (sink_tx, mut sink_rx) = mpsc::channel::<String>(64);
    {
        let mut sinks = state.ws_sinks.lock().await;
        sinks.insert(conn_id, sink_tx);
    }

    // Spawn outbound writer
    let write_handle = tokio::spawn(async move {
        while let Some(msg) = sink_rx.recv().await {
            if write.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    // Reading is what carries this connection's permissions, so it is what has
    // to stop. `next()` is poll-based and keeps its state in the stream, so
    // losing the race costs nothing.
    let mut shutdown = state.shutdown.subscribe();
    loop {
        if *shutdown.borrow() {
            break;
        }
        let next = tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            next = read.next() => next,
        };
        let Some(msg) = next else { break };
        let text = match msg {
            Ok(Message::Text(t)) => t.to_string(),
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
            Ok(Message::Close(_)) | Err(_) => break,
            _ => continue,
        };

        let frame = match protocol::parse_frame(&text) {
            Some(f) => f,
            None => {
                tracing::debug!("Failed to parse OneBot frame");
                continue;
            }
        };

        let event = match frame {
            OneBotFrame::Response(resp) => {
                if let Some(echo) = resp.echo.as_deref()
                    && let Ok(mut pending) = state.pending_api_responses.lock()
                {
                    // Compare the source connection *before* taking the waiter
                    // out. A directed call wants one adapter's answer, and with
                    // two accounts connected the other one answers first — with
                    // an error, since it has never heard of the message being
                    // asked about. Removing on the way past would leave the
                    // real answer with nobody waiting for it.
                    let ours = pending
                        .get(echo)
                        .is_some_and(|call| call.expected_conn.is_none_or(|want| want == conn_id));
                    if ours && let Some(call) = pending.remove(echo) {
                        let _ = call.tx.send(resp);
                    }
                }
                continue;
            }
            OneBotFrame::Event(e) => e,
        };

        // 每个事件都带 self_id，所以身份不需要单独的握手。
        if let Some(self_id) = event.self_id {
            note_identity(&state, conn_id, self_id);
        }

        match event.post_type.as_str() {
            "meta_event" => {
                // Heartbeat / lifecycle — just log
                if event.meta_event_type.as_deref() == Some("lifecycle") {
                    tracing::debug!("OneBot lifecycle event from conn {conn_id}");
                }
            }
            "message" => {
                let state = state.clone();
                tokio::spawn(async move {
                    let actions = handler::handle_message(&event, &state, conn_id).await;
                    send_to_conn(&state, conn_id, actions).await;
                });
            }
            "request" => {
                let state = state.clone();
                tokio::spawn(async move {
                    let actions = handler::handle_request(&event, &state).await;
                    send_to_conn(&state, conn_id, actions).await;
                });
            }
            "notice" => {
                let state = state.clone();
                tokio::spawn(async move {
                    let actions = notice::handle_notice(&event, &state, conn_id).await;
                    send_to_conn(&state, conn_id, actions).await;
                });
            }
            _ => {
                tracing::debug!("Unhandled OneBot event type: {}", event.post_type);
            }
        }
    }

    // Cleanup
    {
        let mut sinks = state.ws_sinks.lock().await;
        sinks.remove(&conn_id);
    }
    // Retire what was waiting on *this* adapter. Their answers are never
    // coming, and without this they sit until the timeout each — a call whose
    // connection is already gone has nothing to wait for. Broadcast waiters
    // (`expected_conn: None`) are left alone: another adapter may still answer.
    {
        if let Ok(mut pending) = state.pending_api_responses.lock() {
            pending.retain(|_, call| call.expected_conn != Some(conn_id));
        }
        if let Ok(mut identities) = state.conn_identities.lock() {
            identities.remove(&conn_id);
        }
    }
    write_handle.abort();
}

/// Start the server if the user has it enabled, and hand it back either way.
///
/// Returned rather than registered here: the caller is the shell, and where the
/// IPC commands look this up is its business. Nothing inside `OneBotServer`
/// knows a window exists, and this is the last place that could have.
pub async fn maybe_start(services: Services, config: OneBotConfig) -> AppOneBot {
    if !config.enabled {
        tracing::info!("OneBot server disabled, skipping auto-start");
        let server = OneBotServer::new(services, config);
        return AppOneBot(Arc::new(Mutex::new(server)));
    }

    let server = OneBotServer::new(services, config);
    if let Err(e) = server.start() {
        tracing::error!("Failed to auto-start OneBot server: {e}");
    }
    AppOneBot(Arc::new(Mutex::new(server)))
}

pub struct AppOneBot(pub Arc<Mutex<OneBotServer>>);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;
    use crate::turn::{Busy, TurnOrigin};

    #[test]
    fn stored_onebot_config_rejects_malformed_values() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        crate::db::ops::preference::set_preference(&mut conn, "onebot.admin_users", "not json", 1).unwrap();
        drop(conn);

        let error = load_config(&pool).expect_err("malformed stored JSON must fail config loading");
        assert!(error.contains("onebot.admin_users"), "{error}");
    }

    #[test]
    fn stored_onebot_scalars_use_exact_wire_spellings() {
        assert!(parse_stored_bool("onebot.enabled", Some("1".into()), false).is_err());
        assert!(parse_stored_port("onebot.port", Some("06700".into()), 6700).is_err());
        assert!(parse_stored_decimal("onebot.balance_alert_threshold", Some("1.0".into()),).is_err());
        assert!(parse_stored_decimal("onebot.balance_alert_threshold", Some("-1".into()),).is_err());
        assert!(parse_stored_decimal("onebot.balance_alert_threshold", Some(String::new()),).is_err());

        let mut config = OneBotConfig {
            balance_alert_threshold: Some("-1".parse().unwrap()),
            ..Default::default()
        };
        let pool = test_db();
        assert!(save_config(&pool, &config).is_err());
        config.balance_alert_threshold = Some("0".parse().unwrap());
        assert!(save_config(&pool, &config).is_ok());
        config.balance_alert_threshold = None;
        assert!(save_config(&pool, &config).is_ok());
        let mut conn = pool.get().unwrap();
        assert_eq!(
            crate::db::ops::preference::get_preference(&mut conn, "onebot.balance_alert_threshold").unwrap(),
            None
        );

        assert!(validate_scope_list("onebot.voice_capture_sessions", &["7@group:8".into()], false).is_ok());
        assert!(validate_scope_list("onebot.voice_capture_sessions", &["7@room:8".into()], false).is_err());
        assert!(validate_scope_list("onebot.voice_send_groups", &["7@private:8".into()], true).is_err());
    }

    fn item(kind: InboxKind, at: i64) -> InboxItem {
        InboxItem {
            text: "x".into(),
            kind,
            created_at: at,
            sender: None,
        }
    }

    /// Everything the turn/inbox transitions actually touch. Nothing here needs
    /// the websocket server, the database or a provider — which is the point of
    /// `SessionStates` being its own value rather than a field the guard has to
    /// reach through `SharedState` for.
    struct Fixture {
        states: Arc<SessionStates>,
        approvals: Arc<PendingApprovals>,
        coordinator: Arc<TurnCoordinator>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                states: Arc::new(SessionStates::default()),
                approvals: Arc::new(PendingApprovals::default()),
                coordinator: Arc::new(TurnCoordinator::new()),
            }
        }

        /// A tool call parked in front of the user, as `make_approval_fn`
        /// leaves it.
        fn park_approval(&self, key: &SessionKey, turn_id: &str) -> oneshot::Receiver<String> {
            let (tx, rx) = oneshot::channel();
            self.approvals.lock().insert(
                key.to_string(),
                PendingApproval {
                    initiator: 7,
                    turn_id: turn_id.into(),
                    prompt_message_id: Some(9001),
                    kind: crate::onebot::agent::AskKind::Permission,
                    responder: tx,
                },
            );
            rx
        }

        fn begin(&self, key: &SessionKey, conv: &str, at: i64) -> TurnStart {
            try_begin_turn(
                &self.states,
                &self.coordinator,
                key,
                conv,
                item(InboxKind::UserMessage, at),
            )
        }

        fn started(&self, key: &SessionKey, conv: &str, at: i64) -> SessionTurn {
            match self.begin(key, conv, at) {
                TurnStart::Started(turn) => turn,
                TurnStart::Queued => panic!("the session was supposed to be free"),
                TurnStart::Elsewhere(b) => panic!("the conversation was supposed to be free: {b}"),
            }
        }

        fn active(&self, key: &SessionKey) -> bool {
            self.states.lock()[&key.to_string()].turn_active
        }

        fn conversation_free(&self, conv: &str) -> bool {
            self.coordinator.try_acquire_turn(conv, TurnOrigin::Desktop).is_ok()
        }
    }

    /// The whole reason `SessionTurn` exists. A runner that is cancelled or
    /// panics never reaches `end_turn`, and the session's `turn_active` flag
    /// used to stay set for good — every later message then queued into an
    /// inbox with no runner left to drain it, and the session went silent.
    #[test]
    fn a_turn_that_dies_without_finishing_hands_the_session_back() {
        let f = Fixture::new();
        let key = SessionKey::group(1);
        let turn = f.started(&key, "conv-1", 1000);
        assert!(f.active(&key));

        // However the runner died: a cancelled task, a panic, an early return.
        drop(turn);

        assert!(!f.active(&key), "the session must be free again");
        drop(f.started(&key, "conv-1", 1001));
    }

    /// And gives them back together, not one and then the other.
    ///
    /// The probe runs at the end of the session's critical section — the only
    /// point where the two orders differ. Anyone who could see this session as
    /// free has to take that lock first, so if the conversation is still held
    /// here, there is a moment when a QQ message finds an idle session and is
    /// then refused by the coordinator: the user is told the desktop is busy by
    /// a turn that no longer exists.
    #[test]
    fn a_dying_turn_gives_both_back_before_anyone_can_look() {
        let f = Fixture::new();
        let key = SessionKey::group(1);
        let mut turn = f.started(&key, "conv-1", 1000);

        let mut conversation_free = None;
        turn.release(|| {
            conversation_free = Some(f.conversation_free("conv-1"));
        });

        assert_eq!(
            conversation_free,
            Some(true),
            "the conversation was still held when the session was already free",
        );
    }

    /// Both claims are given back, not just one. Releasing only the
    /// conversation would leave the session queueing into a dead inbox;
    /// releasing only the session would let two runners write one conversation.
    #[test]
    fn a_dead_turn_hands_the_conversation_back_too() {
        let f = Fixture::new();
        let key = SessionKey::group(1);
        let turn = f.started(&key, "conv-1", 1000);
        assert!(!f.conversation_free("conv-1"));

        drop(turn);

        assert!(f.conversation_free("conv-1"));
    }

    /// A panic is the case `Drop` exists for, so exercise it as a panic.
    #[test]
    fn a_panicking_runner_hands_both_back() {
        let f = Fixture::new();
        let key = SessionKey::group(1);
        let turn = f.started(&key, "conv-1", 1000);
        let _ = std::thread::spawn(move || {
            let _turn = turn;
            panic!("the runner died");
        })
        .join();

        assert!(!f.active(&key));
        assert!(f.conversation_free("conv-1"));
    }

    /// A follow-up round is the same turn continuing. Letting go of either
    /// claim in between is exactly the gap that lets the desktop start a turn
    /// between two rounds of one QQ answer.
    #[test]
    fn a_continuing_turn_keeps_both_claims() {
        let f = Fixture::new();
        let key = SessionKey::group(1);
        let turn = f.started(&key, "conv-1", 1000);
        // Arrived too late to be injected mid-turn. Stamped now, because
        // `end_turn` expires the inbox against the real clock.
        f.states
            .lock()
            .get_mut(&key.to_string())
            .unwrap()
            .inbox
            .push(item(InboxKind::UserMessage, now_ms()));

        let TurnEnd::Continue(held, items) = end_turn(&f.states, &key, turn, || ()) else {
            panic!("a queued user message must continue the turn")
        };

        assert_eq!(items.len(), 1);
        assert!(f.active(&key));
        assert_eq!(
            f.coordinator.try_acquire_turn("conv-1", TurnOrigin::Desktop).err(),
            Some(Busy::Turn(TurnOrigin::OneBot)),
        );
        drop(held);
    }

    /// The normal exit gives both back too — and the guard must not then clear
    /// a flag `finish` already cleared, which by the time it drops could belong
    /// to a later turn.
    #[test]
    fn a_finished_turn_gives_both_back() {
        let f = Fixture::new();
        let key = SessionKey::group(1);
        let turn = f.started(&key, "conv-1", 1000);

        assert!(matches!(end_turn(&f.states, &key, turn, || ()), TurnEnd::Done));

        assert!(!f.active(&key));
        assert!(f.conversation_free("conv-1"));
    }

    /// The ordering, watched from inside the announcement itself.
    ///
    /// The front end reads the terminal event as permission to send again, so
    /// by the time it goes out the conversation has to be free. Asserting on
    /// the state *after* `end_turn` returns cannot tell the two orders apart —
    /// this looks from where the difference is visible.
    #[test]
    fn the_end_is_announced_only_after_both_claims_are_back() {
        let f = Fixture::new();
        let key = SessionKey::group(1);
        let turn = f.started(&key, "conv-1", 1000);

        let mut seen: Option<(bool, bool)> = None;
        let announce = || {
            seen = Some((
                !f.states.lock()[&key.to_string()].turn_active,
                f.coordinator.try_acquire_turn("conv-1", TurnOrigin::Desktop).is_ok(),
            ));
        };
        assert!(matches!(end_turn(&f.states, &key, turn, announce), TurnEnd::Done));

        let (session_free, conversation_free) = seen.expect("the end must be announced");
        assert!(
            session_free,
            "the session was still active when the turn was announced over"
        );
        assert!(
            conversation_free,
            "the conversation was still held when the turn was announced over"
        );
    }

    /// A follow-up round is not an ending, so nothing may be announced. One
    /// used to go out between rounds, and a desktop watching the conversation
    /// took it as its cue to send — into a turn that still held it.
    #[test]
    fn a_continuing_turn_announces_nothing() {
        let f = Fixture::new();
        let key = SessionKey::group(1);
        let turn = f.started(&key, "conv-1", 1000);
        f.states
            .lock()
            .get_mut(&key.to_string())
            .unwrap()
            .inbox
            .push(item(InboxKind::UserMessage, now_ms()));

        let mut announced = false;
        let TurnEnd::Continue(held, _) = end_turn(&f.states, &key, turn, || announced = true) else {
            panic!("a queued user message must continue the turn")
        };

        assert!(!announced, "a round that is not the end must not announce one");
        drop(held);
    }

    /// And exactly once — the announcement is not repeated by whatever drops
    /// the guard afterwards.
    #[test]
    fn the_end_is_announced_once() {
        let f = Fixture::new();
        let key = SessionKey::group(1);
        let turn = f.started(&key, "conv-1", 1000);

        let mut count = 0;
        end_turn(&f.states, &key, turn, || count += 1);
        assert_eq!(count, 1);
    }

    /// The desktop is answering this conversation. Nothing may be queued: this
    /// inbox is drained only by the OneBot runner, so an item left here would
    /// wait for the next QQ message rather than for the desktop turn to end.
    #[test]
    fn a_conversation_held_by_the_desktop_queues_nothing() {
        let f = Fixture::new();
        let key = SessionKey::group(1);
        let _desktop = f
            .coordinator
            .try_acquire_turn("conv-1", TurnOrigin::Desktop)
            .expect("free");

        let start = f.begin(&key, "conv-1", 1000);
        assert!(matches!(start, TurnStart::Elsewhere(Busy::Turn(TurnOrigin::Desktop))));

        let states = f.states.lock();
        let s = &states[&key.to_string()];
        assert!(!s.turn_active, "a session we never took must not read as active");
        assert!(s.inbox.is_empty(), "nothing may queue behind a turn we do not own");
    }

    /// A second message for a session this runner *does* own still queues, as
    /// it always did.
    #[test]
    fn a_second_message_for_our_own_turn_still_queues() {
        let f = Fixture::new();
        let key = SessionKey::group(1);
        let _turn = f.started(&key, "conv-1", 1000);

        assert!(matches!(f.begin(&key, "conv-1", 1001), TurnStart::Queued));
        assert_eq!(f.states.lock()[&key.to_string()].inbox.len(), 1);
    }

    /// A `RunningTurn` whose announcements land somewhere a test can read, and
    /// which reports what the world looked like at the moment each went out.
    struct Watcher {
        stops: Arc<std::sync::Mutex<Vec<(serde_json::Value, bool, bool)>>>,
    }

    impl Watcher {
        fn attach(f: &Fixture, key: &SessionKey, turn: SessionTurn, conv: &str) -> (Self, RunningTurn) {
            let stops = Arc::new(std::sync::Mutex::new(Vec::new()));
            let recorded = Arc::clone(&stops);
            let states = Arc::clone(&f.states);
            let coordinator = Arc::clone(&f.coordinator);
            let session = key.to_string();
            let conversation = conv.to_string();
            let sink: StopSink = Box::new(move |event| {
                // Read from inside the announcement: after it returns, the two
                // orderings are indistinguishable.
                let session_free = !states.lock().get(&session).is_some_and(|s| s.turn_active);
                let conversation_free = coordinator.try_acquire_turn(&conversation, TurnOrigin::Desktop).is_ok();
                let payload = serde_json::to_value(event).expect("stop event must serialize");
                recorded
                    .lock()
                    .unwrap()
                    .push((payload, session_free, conversation_free));
            });
            let running = RunningTurn::new(
                Arc::clone(&f.states),
                Arc::clone(&f.approvals),
                key.clone(),
                turn,
                conv.to_string(),
                Some(sink),
            );
            (Self { stops }, running)
        }

        fn stops(&self) -> Vec<(serde_json::Value, bool, bool)> {
            self.stops.lock().unwrap().clone()
        }
    }

    /// The hole the old `ErrorStopGuard` covered and moving the emit to the
    /// caller did not: this task is detached, so dropping it mid-`await` — a
    /// runtime shutting down, a connection task torn down — runs none of the
    /// code after the await. Without this, a desktop with the QQ conversation
    /// open streams for good.
    #[test]
    fn a_runner_dropped_mid_turn_still_announces_the_end() {
        let f = Fixture::new();
        let key = SessionKey::group(1);
        let turn = f.started(&key, "conv-1", 1000);
        let (watch, running) = Watcher::attach(&f, &key, turn, "conv-1");

        // Whatever killed it, this is all that is left to run.
        drop(running);

        let stops = watch.stops();
        assert_eq!(stops.len(), 1, "a turn that died still owes a terminal event");
        assert_eq!(stops[0].0["reason"], "error");
        assert_eq!(stops[0].0["type"], "stop");
        assert!(stops[0].1, "the session was still active when the end was announced");
        assert!(stops[0].2, "the conversation was still held when the end was announced");
    }

    /// Same duty, reached by unwinding rather than by a drop.
    #[test]
    fn a_panicking_runner_announces_the_end() {
        let f = Fixture::new();
        let key = SessionKey::group(1);
        let turn = f.started(&key, "conv-1", 1000);
        let (watch, running) = Watcher::attach(&f, &key, turn, "conv-1");

        let _ = std::thread::spawn(move || {
            let _running = running;
            panic!("a tool blew up");
        })
        .join();

        let stops = watch.stops();
        assert_eq!(stops.len(), 1);
        assert_eq!(stops[0].0["reason"], "error");
        assert!(stops[0].2, "the conversation was still held when the end was announced");
    }

    /// And the normal path says it once, not twice — the guard must not add a
    /// second one on its way out.
    #[test]
    fn a_turn_that_ended_normally_announces_once() {
        let f = Fixture::new();
        let key = SessionKey::group(1);
        let turn = f.started(&key, "conv-1", 1000);
        let (watch, mut running) = Watcher::attach(&f, &key, turn, "conv-1");

        assert!(
            running.end_round(crate::events::ChatStopReason::EndTurn).is_none(),
            "nothing was queued"
        );
        assert_eq!(watch.stops().len(), 1);

        drop(running);
        assert_eq!(watch.stops().len(), 1, "the guard must not announce a second ending");
        assert_eq!(watch.stops()[0].0["reason"], "end_turn");
    }

    /// A round that continues has not ended, so it announces nothing — but the
    /// duty is still owed, and dying during the follow-up must still discharge
    /// it.
    #[test]
    fn a_continuing_round_announces_nothing_but_still_owes_one() {
        let f = Fixture::new();
        let key = SessionKey::group(1);
        let turn = f.started(&key, "conv-1", 1000);
        let (watch, mut running) = Watcher::attach(&f, &key, turn, "conv-1");
        f.states
            .lock()
            .get_mut(&key.to_string())
            .unwrap()
            .inbox
            .push(item(InboxKind::UserMessage, now_ms()));

        assert!(
            running.end_round(crate::events::ChatStopReason::EndTurn).is_some(),
            "a queued message continues the turn"
        );
        assert!(
            watch.stops().is_empty(),
            "a round that is not the end announces nothing"
        );

        // The follow-up round dies.
        drop(running);
        assert_eq!(watch.stops().len(), 1);
        assert_eq!(watch.stops()[0].0["reason"], "error");
    }

    /// What separates an answer from the rest of a group conversation. Without
    /// it the initiator's every message counted, and in a group most of them
    /// are addressed to other people: a "y" typed at a friend approved whatever
    /// happened to be waiting.
    #[test]
    fn only_a_reply_to_the_prompt_answers_it() {
        let asked = PendingApproval {
            initiator: 7,
            turn_id: "t1".into(),
            prompt_message_id: Some(42),
            kind: crate::onebot::agent::AskKind::Permission,
            responder: oneshot::channel().0,
        };
        assert!(asked.answered_by(Some(42)));
        assert!(!asked.answered_by(Some(41)), "answered a different message");
        assert!(!asked.answered_by(None), "answered nothing in particular");
    }

    /// And when the prompt never learnt its own id — a send that failed, or an
    /// adapter that returns nothing — the old rule stands. Worse, but an
    /// approval nobody can answer is worse still.
    #[test]
    fn a_prompt_that_does_not_know_its_own_id_takes_any_answer() {
        let asked = PendingApproval {
            initiator: 7,
            turn_id: "t1".into(),
            prompt_message_id: None,
            kind: crate::onebot::agent::AskKind::Permission,
            responder: oneshot::channel().0,
        };
        assert!(asked.answered_by(None));
        assert!(asked.answered_by(Some(42)));
    }

    /// The third thing a turn owes back. `make_approval_fn` parks on a
    /// 60-second timeout; a task dropped mid-`await` never reaches the timeout
    /// branch, so the sender used to sit in the map until somebody replied to a
    /// question nobody was listening for — or until the next call for the same
    /// session overwrote it.
    #[test]
    fn a_runner_dropped_while_waiting_on_an_approval_retires_it() {
        let f = Fixture::new();
        let key = SessionKey::group(1);
        let turn = f.started(&key, "conv-1", 1000);
        let turn_id = turn.turn_id().to_string();
        let (_watch, running) = Watcher::attach(&f, &key, turn, "conv-1");
        let mut waiting = f.park_approval(&key, &turn_id);

        drop(running);

        assert!(f.approvals.lock().is_empty(), "a dead turn leaves no waiters behind");
        // And the waiting side, if there still were one, is told rather than
        // left hanging.
        assert!(waiting.try_recv().is_err());
    }

    #[test]
    fn a_turn_that_ended_normally_retires_its_approvals_too() {
        let f = Fixture::new();
        let key = SessionKey::group(1);
        let turn = f.started(&key, "conv-1", 1000);
        let turn_id = turn.turn_id().to_string();
        let (_watch, mut running) = Watcher::attach(&f, &key, turn, "conv-1");
        f.park_approval(&key, &turn_id);

        assert!(running.end_round(crate::events::ChatStopReason::EndTurn).is_none());

        assert!(f.approvals.lock().is_empty());
    }

    /// By turn, not by session. A QQ session's key is stable across turns, so
    /// sweeping the session would take an approval the *next* turn had already
    /// registered — and that turn would then wait out its full minute for an
    /// answer that could no longer reach it.
    #[test]
    fn retiring_a_turn_leaves_another_turns_approval_alone() {
        let f = Fixture::new();
        let key = SessionKey::group(1);
        let other = SessionKey::group(2);
        let turn = f.started(&key, "conv-1", 1000);
        let turn_id = turn.turn_id().to_string();
        let (_watch, running) = Watcher::attach(&f, &key, turn, "conv-1");
        f.park_approval(&other, "some-other-turn");

        drop(running);

        assert_eq!(f.approvals.lock().len(), 1);
        assert_eq!(f.approvals.lock()[&other.to_string()].turn_id, "some-other-turn");
        let _ = turn_id;
    }

    /// Rounds add up: they are one turn, and the desktop gets one figure.
    #[test]
    fn a_turns_rounds_are_reported_together() {
        let f = Fixture::new();
        let key = SessionKey::group(1);
        let turn = f.started(&key, "conv-1", 1000);
        let (watch, mut running) = Watcher::attach(&f, &key, turn, "conv-1");
        f.states
            .lock()
            .get_mut(&key.to_string())
            .unwrap()
            .inbox
            .push(item(InboxKind::UserMessage, now_ms()));

        running.record(agent::TurnProgress {
            message_id: Some("msg-1".into()),
            input_tokens: 10,
            output_tokens: 1,
            aborted: false,
            steps: 1,
            ..Default::default()
        });
        assert!(running.end_round(crate::events::ChatStopReason::EndTurn).is_some());
        // The follow-up round never wrote a row, so the first round's stands.
        running.record(agent::TurnProgress {
            input_tokens: 5,
            output_tokens: 2,
            ..Default::default()
        });
        assert!(running.end_round(crate::events::ChatStopReason::EndTurn).is_none());

        let stops = watch.stops();
        assert_eq!(stops.len(), 1);
        assert_eq!(stops[0].0["input_tokens"], 15);
        assert_eq!(stops[0].0["output_tokens"], 3);
        assert_eq!(stops[0].0["message_id"], "msg-1");
    }

    /// Two sessions on two conversations do not contend at all.
    #[test]
    fn a_second_session_is_unaffected() {
        let f = Fixture::new();
        let _first = f.started(&SessionKey::group(1), "conv-1", 1000);
        drop(f.started(&SessionKey::private(2), "conv-2", 1000));
    }

    /// What `try_begin_turn` does when the coordinator hands the conversation
    /// over; the refusal path is covered by the coordinator's own tests.
    fn begin(s: &mut SessionState, item: InboxItem) -> bool {
        let free = s.queue_unless_free(item);
        if free {
            s.activate();
        }
        free
    }

    #[test]
    fn test_a_second_message_queues_behind_the_running_turn() {
        let mut s = SessionState::default();
        assert!(begin(&mut s, item(InboxKind::UserMessage, 1000)));
        assert!(s.turn_active);
        assert!(s.inbox.is_empty(), "starting item is not queued");
        // Second message while active gets queued instead of starting a turn.
        assert!(!begin(&mut s, item(InboxKind::UserMessage, 1001)));
        assert_eq!(s.inbox.len(), 1);
    }

    /// The session must not be marked active until the conversation is actually
    /// taken: a desktop turn can refuse in between, and an active session with
    /// no turn behind it would queue every later message forever.
    #[test]
    fn test_a_refused_conversation_leaves_the_session_idle() {
        let mut s = SessionState::default();
        assert!(s.queue_unless_free(item(InboxKind::UserMessage, 1000)));
        // The coordinator says no, so `activate` is never called.
        assert!(!s.turn_active);
        assert!(s.inbox.is_empty(), "nothing may be queued behind a turn we do not own");
        // The next message can still try.
        assert!(begin(&mut s, item(InboxKind::UserMessage, 1001)));
    }

    #[test]
    fn test_finish_continues_on_late_user_message() {
        let mut s = SessionState::default();
        assert!(begin(&mut s, item(InboxKind::UserMessage, 1000)));
        s.inbox.push(item(InboxKind::Notice, 1001));
        s.inbox.push(item(InboxKind::UserMessage, 1002));
        match s.finish(2000) {
            Finish::Continue(items) => {
                assert_eq!(items.len(), 2, "notices ride along with the user message");
                assert!(s.turn_active, "session stays active for the follow-up turn");
                assert!(s.inbox.is_empty());
            }
            Finish::Done => panic!("expected Continue"),
        }
    }

    #[test]
    fn test_finish_done_keeps_notice_queued() {
        let mut s = SessionState::default();
        assert!(begin(&mut s, item(InboxKind::UserMessage, 1000)));
        s.inbox.push(item(InboxKind::Notice, 1001));
        match s.finish(2000) {
            Finish::Done => {
                assert!(!s.turn_active);
                assert_eq!(s.inbox.len(), 1, "notice waits for the next trigger");
            }
            Finish::Continue(_) => panic!("expected Done"),
        }
    }

    #[test]
    fn test_finish_drops_expired_items() {
        let mut s = SessionState::default();
        assert!(begin(&mut s, item(InboxKind::UserMessage, 0)));
        s.inbox.push(item(InboxKind::UserMessage, 0));
        match s.finish(super::INBOX_EXPIRY_MS + 1) {
            Finish::Done => assert!(s.inbox.is_empty()),
            Finish::Continue(_) => panic!("expired item must not restart a turn"),
        }
    }

    #[test]
    fn test_push_note_cap_drops_oldest_notice() {
        let mut s = SessionState::default();
        for i in 0..(NOTICE_INBOX_CAP + 2) {
            s.push_note(format!("n{i}"), 1000 + i as i64);
        }
        let notices: Vec<&str> = s.inbox.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(notices.len(), NOTICE_INBOX_CAP);
        assert_eq!(notices.first(), Some(&"n2"), "oldest notes dropped first");
    }

    #[test]
    fn test_record_seen_ring() {
        let mut s = SessionState::default();
        for i in 0..(SEEN_IDS_CAP as i64 + 10) {
            s.record_seen(i);
        }
        assert_eq!(s.seen_message_ids.len(), SEEN_IDS_CAP);
        assert!(!s.seen_message_ids.contains(&5), "oldest ids evicted");
        assert!(s.seen_message_ids.contains(&(SEEN_IDS_CAP as i64 + 9)));
    }

    /// The dispatch rule for directed calls, as a pure decision.
    ///
    /// This mirrors the `ours` check in `handle_connection`: with two accounts
    /// connected, the adapter that was *not* asked answers first — it has never
    /// heard of the message in question, so it answers with an error. Taking the
    /// waiter out for that answer leaves the real one with nobody waiting.
    fn answers_us(expected_conn: Option<u64>, from_conn: u64) -> bool {
        expected_conn.is_none_or(|want| want == from_conn)
    }

    #[test]
    fn a_directed_call_only_takes_its_own_adapters_answer() {
        assert!(answers_us(Some(1), 1));
        assert!(!answers_us(Some(1), 2), "the other account must not be read as ours");
    }

    /// The broadcast path is unchanged: it has no predetermined target, so the
    /// first answer from anywhere is the answer.
    #[test]
    fn a_broadcast_call_still_takes_whoever_answers() {
        assert!(answers_us(None, 1));
        assert!(answers_us(None, 7));
    }

    #[test]
    fn test_token_matches_bearer_header() {
        assert!(token_matches("secret", Some("Bearer secret"), None));
        assert!(token_matches("secret", Some("Token secret"), None));
        assert!(token_matches("secret", Some("secret"), None));
        assert!(!token_matches("secret", Some("Bearer wrong"), None));
    }

    #[test]
    fn test_token_matches_query() {
        assert!(token_matches("secret", None, Some("access_token=secret")));
        assert!(token_matches("secret", None, Some("foo=1&access_token=secret")));
        assert!(!token_matches("secret", None, Some("access_token=wrong")));
    }

    #[test]
    fn test_token_matches_query_percent_encoded() {
        assert!(token_matches("s3cr:t/x", None, Some("access_token=s3cr%3At%2Fx")));
        assert!(!token_matches("s3cr:t/x", None, Some("access_token=s3cr%3At%2Fy")));
    }

    #[test]
    fn test_token_matches_none() {
        assert!(!token_matches("secret", None, None));
    }
}

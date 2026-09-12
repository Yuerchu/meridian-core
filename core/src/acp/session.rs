//! One hosted session: a conversation here, a session id there, and the turn
//! that connects them.
//!
//! The transcript is written with the same three functions a native turn uses
//! (`begin_assistant`, `complete_assistant`, `append_tool_result`), so an ACP
//! conversation is an ordinary row set that search, branching, compaction and
//! the transcript view all already understand. Nothing about it is a special
//! case below `chat-view`.
//!
//! **A turn is written round by round**, the way a native one is: the prose
//! that introduced a call stays on the row carrying that call, its result is
//! its own row, and whatever the agent says afterwards opens the next row.
//!
//! This used to be flattened onto a single assistant row, on the reasoning that
//! the shape was legal and only lost which text came before which call. It lost
//! more than that. `lib/turns.ts` takes the steps *after* the last tool call as
//! the turn's conclusion, so a flattened turn has none — its closing sentence
//! sits before the calls — and a turn with tools and no conclusion is drawn as
//! `interrupted`, with the whole answer folded away as process. Every finished
//! hosted turn that called a tool was reported as stopped.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::agent::engine::transcript::{append_tool_result, begin_assistant, complete_assistant, write_steering};
use crate::agent::tool_calls::serialize_tool_calls_openai;
use crate::db::models::acp_session_notice::AcpSessionNoticeInsert;
use crate::db::models::message::MessageUsage;
use crate::db::models::queue::QueuedPromptRow;
use crate::db::models::turn::{TurnPhase, TurnStatus};
use crate::events::{AcpNoticeSeverity, AcpSessionNoticeEvent, ChatStopReason, ChatStreamEvent, ToolOutcome};
use crate::provider;
use crate::services::Services;
use crate::turn::TurnOrigin;
use crate::util::{get_conn, now_ms};

use super::mapping::{self, Effect};
use super::peer::{Handler, Peer, PeerError};
use super::process::AdapterProcess;
use super::protocol::{self, SessionNotification};
use super::{AcpConfig, approvals, bridge, elicitation};

/// What this app calls itself when it introduces itself to the adapter.
const CLIENT_NAME: &str = "meridian";
/// Stands in until the agent says which model it is using, and means exactly
/// "it has not said". Not a model id, and nothing should treat it as one.
///
/// It usually does say: ACP carries the model as a session configuration option
/// (`category: "model"`), present in the `session/new` response and re-sent
/// whenever it changes, so a row normally records the real id. This is what a
/// row gets when the adapter is old enough, or quiet enough, not to report one.
const MODEL_LABEL: &str = "claude-code";
/// Copied onto every row beside the model label, the way a provider's name is.
///
/// `pub(super)` for the import path, which writes the same rows a live turn
/// does and must label them identically — two spellings of the provider in one
/// transcript would look like two providers.
pub(super) const PROVIDER_LABEL: &str = "Claude Code";

/// One assistant row, while it is still being written.
///
/// A row holds the prose that came *before* its tool calls, plus those calls.
/// Everything after their results belongs to the next row — which is how a
/// native turn writes a multi-round answer, and it is not cosmetic. The
/// transcript reader takes the steps after the last tool call as the turn's
/// conclusion (`lib/turns.ts`, `splitAtConclusion`); with every round crammed
/// into one row the final sentence sits *before* the calls instead of after
/// them, so there is no conclusion, and a finished turn is drawn as
/// `interrupted` with its whole answer folded away as process.
struct OpenRow {
    message_id: String,
    text: String,
    reasoning: String,
    tool_calls: Vec<provider::ToolCall>,
    /// `(call_id, output, outcome)`, in the order the calls finished.
    results: Vec<(String, String, ToolOutcome)>,
    /// Every call on this row has a result, so the next prose or a new call
    /// opens the next one. Not the first result: Claude Code runs calls in
    /// parallel, and thought can land between them.
    settled: bool,
    /// This row is already in the database and must not be written again.
    /// Only set when opening the next round failed part way — see
    /// [`Shared::open_round_if_settled`].
    written: bool,
}

impl OpenRow {
    fn new(message_id: String) -> Self {
        Self {
            message_id,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            results: Vec::new(),
            settled: false,
            written: false,
        }
    }

    /// Nothing worth a row of its own. Checked before opening a new round so a
    /// turn cannot end on an empty bubble.
    fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.reasoning.is_empty() && self.tool_calls.is_empty()
    }
}

/// What a `session/load`'s recital is for.
///
/// A load replays the whole conversation as ordinary `session/update`
/// notifications, and there are two entirely different reasons to ask for one.
///
/// It happens to be harmless without any of this — every branch of
/// [`Shared::absorb`] asks `with_turn` first and no turn is running during a
/// load, so the replay falls on the floor. But that is an accident of two
/// unrelated rules lining up, and two of the branches (`Plan`,
/// `ConfigOptions`) do not ask. Saying it out loud is also what makes the other
/// answer expressible: adopting a session this app did not start needs the
/// replay *written*, because there it is the only transcript there is.
enum Replay {
    /// No load in progress. Updates are the turn talking.
    No,
    /// Reopening a conversation this database already has the rows for. Every
    /// update is one of them said twice.
    Discard,
    /// Importing a session started somewhere else. Every update is a row
    /// nothing here has ever written, kept until the load is over and then
    /// turned into a transcript in one transaction.
    Collect(Vec<Recital>),
}

impl Replay {
    /// Whether an update belongs to the conversation happening now, rather than
    /// to a recital of one that already happened.
    fn is_live(&self) -> bool {
        matches!(self, Replay::No)
    }
}

/// One line of a recited conversation, kept in the order it arrived.
///
/// Deliberately the mapping's own vocabulary rather than a second one: the
/// difference between a live update and a replayed one is what is *done* with
/// it, and inventing a parallel type here would be somewhere for the two to
/// drift apart.
pub(super) type Recital = Effect;

/// A message the user put into a turn that was already running, which the
/// agent has taken and the transcript still owes a row.
struct Interjection {
    /// The queue item it came from, so the row can be named on it once written.
    queue_id: String,
    text: String,
}

/// The turn in flight, if there is one.
struct TurnState {
    turn_id: String,
    cancel: CancellationToken,
    /// The round being written.
    row: OpenRow,
    /// The last row landed in the database, which the next one hangs off.
    parent: String,
    /// Steered messages waiting for a round boundary to be written at.
    ///
    /// Not written when they are sent, which is the tempting thing to do and
    /// forks the transcript: the open row's children are its own tool results,
    /// and a user row landing beside them makes two branches out of one round.
    /// Written at the boundary instead, they sit exactly where they belong —
    /// after the round that was running when they were sent, before the round
    /// that answers them.
    interjected: Vec<Interjection>,
    /// The title of an `error`-severity incident the adapter reported while
    /// this turn ran, if any. Read by `finish`: the adapter answers the prompt
    /// with a plain `end_turn` once `sessionFailure` is declared, and this is
    /// what says the turn nevertheless failed.
    error_notice: Option<String>,
}

/// The half of a session the protocol handler needs.
///
/// Separate from [`AcpSession`] because the handler has to exist before the
/// peer does, and the session cannot exist before the peer. This is what both
/// of them hold.
struct Shared {
    services: Services,
    conversation_id: String,
    turn: Mutex<Option<TurnState>>,
    /// The agent's id for the model that is answering, once it has said.
    ///
    /// Kept on the session rather than the turn: it is a property of the
    /// session, arrives before the first turn, and can change under one.
    model: Mutex<Option<String>>,
    /// Every knob the agent exposes, as it last described them.
    ///
    /// Held whole rather than as the one value this app reads, because the
    /// composer offers them: what a `select` may be set to is the agent's to
    /// decide and changes under us — picking a model re-derives which modes
    /// exist. See [`Shared::merge_config`] for why this is merged rather than
    /// replaced.
    config: Mutex<Vec<protocol::SessionConfigOption>>,
    /// Whether a `session/load` is in progress, and what to do with what it
    /// recites. See [`Replay`].
    replay: Mutex<Replay>,
    /// Every `toolCallId` this session has ever announced.
    ///
    /// **On the session, because that is the scope the id is unique in** — and
    /// because the row it landed on is gone by the time the repeat arrives. The
    /// adapter announces a call twice, from two sources that can arrive in
    /// either order, and the second one can be overtaken by the first call's
    /// result *and* by the next call. Asked of the open row instead, that late
    /// repeat looks new: it is pushed onto whichever round is open by then,
    /// drawing a second card no result will ever close, and leaving that round
    /// with more calls than results — which is what
    /// [`Shared::absorb`]'s `ToolResult` arm reads to decide the turn has
    /// stopped running tools, so the phase stays at `RunningTool` for the rest
    /// of the turn.
    ///
    /// Only ever grows, and only during a live turn: the [`Replay`] gate at the
    /// top of `absorb` returns before this is reached, so a recital cannot fill
    /// it with ids belonging to rows this app already has.
    announced: Mutex<HashSet<String>>,
    /// Assistant rows this turn produced and could not store.
    ///
    /// A refused write leaves the answer on screen and absent from the
    /// database: well-formed, and missing a paragraph the moment anybody
    /// reloads. That is the same shape as an update the reader had to drop, and
    /// it gets the same treatment — [`AcpSession::finish`] refuses to call such
    /// a turn `Done`. Counted rather than returned because the two writers are
    /// `open_round_if_settled`, which cannot fail a turn from where it stands,
    /// and `finish` itself.
    unwritten_rows: std::sync::atomic::AtomicUsize,
    /// The agent is answering into a conversation it cannot see, and has not
    /// been told yet.
    ///
    /// Set when a session was opened for a conversation that already had a
    /// transcript and the agent's own memory of it could not be resumed. Read
    /// when a turn assembles its prompt and cleared only once that prompt has
    /// been delivered — the same rule the interrupted-turn report follows, for
    /// the same reason.
    memory_lost: Mutex<bool>,
    /// Whether this session was opened without its tool bridge.
    ///
    /// Same lifecycle as `memory_lost`: set at the open, read when a turn
    /// assembles its prompt, cleared only once that prompt came back. See
    /// [`NO_TOOLS`] for why the failure is visible rather than silent.
    tools_lost: Mutex<bool>,
    /// The transient ACP option sender for a durable plan review. The plan and
    /// decision live in SQLite; this only connects a still-running adapter to
    /// that state and may disappear on restart without retiring the review.
    plan_reviews: Arc<super::plan_review::ReviewControl>,
    /// The last title the agent gave this conversation, in this process.
    ///
    /// The agent's title is adopted only over a title nobody chose: the
    /// placeholder the conversation was created with, or the agent's own
    /// previous one. A title the user typed in the sidebar is theirs, and the
    /// adapter republishing its own at the next turn end must not take it
    /// back.
    agent_title: Mutex<Option<String>>,
    /// What the conversation was called before anybody named it — see
    /// [`super::title_for`]. Held here so `absorb` can tell a placeholder
    /// from a choice without a second lookup.
    placeholder_title: String,
}

impl Shared {
    fn emit(&self, event: ChatStreamEvent) {
        // Failure here is a window that has gone away. A native turn treats
        // that as fatal because its events *are* its answer; this one has
        // already written the row, and the adapter is mid-turn on the other
        // side of a pipe that cannot be rewound.
        if let Err(e) = self.services.events.emit_chat(event) {
            tracing::debug!(error = %e, "an ACP update reached no window");
        }
    }

    /// Run `f` against the turn in flight. `None` when nothing is running,
    /// which is how every update that arrives between turns is dropped.
    fn with_turn<T>(&self, f: impl FnOnce(&mut TurnState) -> T) -> Option<T> {
        let mut guard = self.turn.lock().ok()?;
        guard.as_mut().map(f)
    }

    /// Record a `toolCallId` as announced, answering whether that was the first
    /// time. `None` is a poisoned lock, treated the way [`Shared::with_turn`]
    /// treats one: the update is dropped rather than guessed at.
    ///
    /// See [`Shared::announced`] for why the session is the right scope.
    fn announce(&self, call_id: &str) -> Option<bool> {
        let mut seen = self.announced.lock().ok()?;
        Some(seen.insert(call_id.to_string()))
    }

    /// What to record as the model on a row written now.
    fn model(&self) -> String {
        self.model
            .lock()
            .ok()
            .and_then(|m| m.clone())
            .unwrap_or_else(|| MODEL_LABEL.to_string())
    }

    fn set_model(&self, model: String) {
        let Ok(mut slot) = self.model.lock() else { return };
        if slot.as_deref() == Some(model.as_str()) {
            return;
        }
        tracing::info!(model = %model, conversation_id = %self.conversation_id, "ACP session model");
        *slot = Some(model);
    }

    /// Take in a set of options the agent just described.
    ///
    /// **Merged, never replaced.** A `config_option_update` is allowed to carry
    /// only what changed, and an option in one is allowed to omit its `options`
    /// list — it is reporting a new `currentValue`, not redefining the knob. A
    /// wholesale replace would empty the picker the moment the user used it,
    /// which is the one moment they are looking at it.
    ///
    /// Also keeps the model in step: it is one of these options, and reading it
    /// from anywhere else would be a second source that could disagree.
    ///
    /// **And it announces, on every path.** The emit used to sit on the
    /// `config_option_update` branch alone, so a set established by
    /// `session/new` or `session/load` reached nobody: the composer fetches
    /// once when the conversation is opened, which for a session started
    /// lazily — every reopen after a restart — is *before* there is anything to
    /// fetch. The knobs then stayed missing until the agent happened to change
    /// one of its own accord. Announcing here is what makes that unforgettable,
    /// since there is no other way to change the set.
    /// **Except while a session is being imported**, where the conversation
    /// this would name does not exist yet — it is written in the same
    /// transaction as the transcript, once the recital is complete. The model
    /// is still merged, because that is how the imported rows learn which model
    /// answered; only the announcement is held back.
    fn merge_config(&self, incoming: Vec<protocol::SessionConfigOption>) {
        if let Some(model) = incoming.iter().find_map(|o| o.as_model()) {
            self.set_model(model.to_string());
        }
        if let Ok(mut held) = self.config.lock() {
            merge_options(&mut held, incoming);
        }
        if !self.importing() {
            self.emit(ChatStreamEvent::AcpConfig {
                conversation_id: self.conversation_id.clone(),
                config_options: self.config_options().into_iter().map(Into::into).collect(),
            });
        }
    }

    /// Whether this session exists only to be read into a transcript.
    ///
    /// Takes the `replay` lock, which is not reentrant: no caller may already
    /// hold it. Today none does — `absorb`'s gate is a `let` chain whose guard
    /// is dropped at the end of its `if`, and the `match` after it is a
    /// separate statement — but a `merge_config` moved *into* that gate block
    /// would deadlock rather than fail to compile.
    fn importing(&self) -> bool {
        self.replay.lock().is_ok_and(|r| matches!(*r, Replay::Collect(_)))
    }

    fn set_replay(&self, mode: Replay) {
        if let Ok(mut slot) = self.replay.lock() {
            *slot = mode;
        }
    }

    fn config_options(&self) -> Vec<protocol::SessionConfigOption> {
        self.config.lock().map(|c| c.clone()).unwrap_or_default()
    }

    async fn absorb(&self, notification: SessionNotification) {
        let effect = mapping::effect_of(notification.update);

        // The agent reciting rather than answering. Which of the two things
        // that means is [`Replay`]'s to say; this is the one place either is
        // acted on, and both of them end the update's journey here.
        if let Ok(mut replay) = self.replay.lock()
            && !replay.is_live()
        {
            if let Replay::Collect(recital) = &mut *replay
                && effect != Effect::Ignored
            {
                recital.push(effect);
            }
            return;
        }

        // Every branch below takes the lock, drops it, and only then emits.
        // Emitting under the lock would put a sink's latency inside a critical
        // section the reader is feeding.
        match effect {
            // The agent echoing back the prompt it was sent. This app wrote
            // that row before the prompt ever reached the adapter, so drawing
            // the echo would show the question twice. Dropped *here* rather
            // than in the mapping because it is a fact about this path: the
            // same update, arriving during an import, is the only record of the
            // question there is.
            Effect::UserText { .. } => {}
            Effect::Text { text: chunk, .. } => {
                // Prose arriving after a result is the next round talking, so it
                // gets a row of its own — see [`OpenRow`] for what depends on it.
                self.open_round_if_settled().await;
                let Some(message_id) = self.with_turn(|t| {
                    t.row.text.push_str(&chunk);
                    t.row.message_id.clone()
                }) else {
                    return;
                };
                self.emit(ChatStreamEvent::Text {
                    content: chunk,
                    message_id,
                    conversation_id: self.conversation_id.clone(),
                });
            }
            Effect::Reasoning { text: chunk, .. } => {
                self.open_round_if_settled().await;
                let Some(message_id) = self.with_turn(|t| {
                    t.row.reasoning.push_str(&chunk);
                    t.row.message_id.clone()
                }) else {
                    return;
                };
                self.emit(ChatStreamEvent::Reasoning {
                    content: chunk,
                    message_id,
                    conversation_id: self.conversation_id.clone(),
                });
            }
            Effect::ToolCall {
                call_id,
                tool_name,
                arguments,
            } => {
                // **Deduplicated before the round is rotated, and the order is
                // the whole of it.** The adapter announces a call twice, and
                // the second announcement can arrive *after* the result — at
                // which point the row is settled. Rotating first empties the
                // row this call is already on, so the repeat finds nothing to
                // match, opens a round of its own and draws a second card that
                // no result will ever close.
                //
                // So: a known id revises where it stands. Only an id nobody has
                // seen is allowed to start the next round — which it must,
                // because a round can end and the next one open with a call and
                // no prose at all. Left joined to the previous row the
                // transcript records `assistant(A, B) → result A → result B`:
                // two calls issued together, when B was in fact decided after
                // seeing A's result. A serial dependency persisted as a
                // parallel one.
                //
                // A `toolCallId` is unique within an ACP session, so the same id
                // twice is one call being announced twice — which the adapter
                // does, from two sources that can arrive in either order, and
                // older ones did without deduplicating at all.
                //
                // The front end deliberately does *not* dedupe by call id
                // (`handleToolCall` pushes regardless: OpenAI-compatible
                // gateways reuse "0" within a turn and two cards there are two
                // calls). That makes this the only place that can tell the
                // difference, and getting it wrong drew every shell command
                // twice — once as the placeholder, once as itself.
                //
                // **Asked of the session, not of the row.** The row is the wrong
                // scope by exactly one boundary: `Call(A) → Result(A) → Call(B)`
                // rotates the round, and a repeat of `A` arriving after that
                // finds a row holding only `B`. See [`Shared::announced`].

                // Nothing to land on, so nothing is recorded as announced
                // either — an id spent between turns must not suppress itself.
                if self.with_turn(|_| ()).is_none() {
                    return;
                }
                match self.announce(&call_id) {
                    // Where it still stands, this fills it in; where the round
                    // holding it has already been written out, `revise` reaches
                    // the stored row instead. That second case is the common
                    // one for the announcement that carries the real arguments,
                    // which is why it may not simply be dropped.
                    Some(false) => {
                        self.revise(&call_id, &tool_name, &arguments).await;
                        return;
                    }
                    Some(true) => {}
                    None => return,
                }

                // Only now, with the id known to be new.
                self.open_round_if_settled().await;

                let Some(message_id) = self.with_turn(|t| {
                    t.row.tool_calls.push(provider::ToolCall {
                        id: call_id.clone(),
                        name: tool_name.clone(),
                        arguments: arguments.clone(),
                    });
                    t.row.message_id.clone()
                }) else {
                    return;
                };
                self.emit(ChatStreamEvent::ToolCall {
                    call_id,
                    tool_name: tool_name.clone(),
                    arguments,
                    message_id,
                    conversation_id: self.conversation_id.clone(),
                });
                self.record_phase(TurnPhase::RunningTool, Some(&tool_name)).await;
            }
            Effect::ToolCallRevised {
                call_id,
                tool_name,
                arguments,
            } => self.revise(&call_id, &tool_name, &arguments).await,
            Effect::ToolResult {
                call_id,
                result,
                outcome,
            } => {
                let Some((message_id, quiet)) = self.with_turn(|t| {
                    t.row.results.push((call_id.clone(), result.clone(), outcome));
                    // Round is over only when every call on it has a result.
                    // Settling on the first one closed the row under a parallel
                    // sibling still outstanding, so a thought between A and B
                    // parked B's result on a new round that never asked for it.
                    let quiet = t.row.results.len() >= t.row.tool_calls.len();
                    t.row.settled = quiet;
                    (t.row.message_id.clone(), quiet)
                }) else {
                    return;
                };
                if quiet {
                    self.record_phase(TurnPhase::Streaming, None).await;
                }
                self.emit(ChatStreamEvent::ToolResult {
                    call_id,
                    result,
                    outcome,
                    message_id,
                    conversation_id: self.conversation_id.clone(),
                });
            }
            Effect::Plan(items) => self.write_plan(items).await,
            // Reported but not stored. `used`/`size` is how full the context is,
            // which is not what `input_tokens`/`output_tokens` mean, and writing
            // it into those columns would feed a wrong number to everything that
            // reads them — the usage report most of all.
            Effect::Usage { used, size } => {
                // Passed on, still not stored. It is how full the *agent's*
                // window is, which is the only honest thing to show for a
                // hosted conversation — this app's own estimate describes a
                // request it never makes, against a model that is not
                // answering and a limit that is not in force. Writing it into
                // `input_tokens`/`output_tokens` would still be wrong, which is
                // why it travels as an event rather than to a column.
                self.emit(ChatStreamEvent::AcpUsage {
                    conversation_id: self.conversation_id.clone(),
                    used,
                    size,
                });
            }
            // Nothing to do beyond merging: `merge_config` announces.
            Effect::ConfigOptions(options) => self.merge_config(options),
            // Not gated on a running turn: an incident noticed between turns
            // (a login that expired, a worker that died) is still one to keep,
            // filed against no turn.
            Effect::SessionNotice(record) => {
                let turn_id = self.with_turn(|t| {
                    if record.severity == AcpNoticeSeverity::Error {
                        t.error_notice = Some(record.title.clone());
                    }
                    t.turn_id.clone()
                });
                self.record_notice(record, turn_id).await;
            }
            Effect::SessionTitle(title) => self.adopt_title(title).await,
            Effect::Ignored => {}
        }
    }

    /// Keep an incident the adapter reported, and say so if it is new.
    ///
    /// `turn_id` is passed rather than read off the turn because one caller —
    /// `finish`, reading the record off the prompt's own reply — runs after
    /// the turn state has been taken, while the turn it belongs to is still
    /// known. `None` files a session-scoped incident.
    ///
    /// Announced only when the write changed something: a replayed revision
    /// is the adapter repeating itself, and the frontend already holds it.
    /// Held back during an import, where the conversation does not exist yet.
    async fn record_notice(&self, record: mapping::SessionNoticeRecord, turn_id: Option<String>) {
        let pool = self.services.db.clone();
        let conversation_id = self.conversation_id.clone();
        let written = tokio::task::spawn_blocking(move || {
            let mut conn = get_conn(&pool)?;
            let actions = serde_json::to_string(&record.actions).map_err(|e| e.to_string())?;
            let id = uuid::Uuid::new_v4().to_string();
            let now = now_ms();
            crate::db::ops::acp_session_notice::upsert_if_newer(
                &mut conn,
                AcpSessionNoticeInsert {
                    id: &id,
                    conversation_id: &conversation_id,
                    turn_id: turn_id.as_deref(),
                    notice_id: &record.notice_id,
                    revision: i32::try_from(record.revision).unwrap_or(i32::MAX),
                    category: record.category.as_str(),
                    severity: record.severity.as_str(),
                    title: &record.title,
                    details: record.details.as_deref(),
                    reason: record.reason.as_deref(),
                    actions: &actions,
                    created_at: now,
                    updated_at: now,
                },
            )
            .map_err(|e| e.to_string())
        })
        .await;
        let row = match written {
            Ok(Ok(Some(row))) => row,
            Ok(Ok(None)) => return,
            Ok(Err(error)) => {
                tracing::warn!(%error, conversation_id = %self.conversation_id, "could not record an ACP notice");
                return;
            }
            Err(error) => {
                tracing::warn!(%error, conversation_id = %self.conversation_id, "recording an ACP notice panicked");
                return;
            }
        };
        match AcpSessionNoticeEvent::try_from(row) {
            Ok(notice) => {
                tracing::info!(
                    conversation_id = %self.conversation_id,
                    notice_id = %notice.notice_id,
                    revision = notice.revision,
                    category = notice.category.as_str(),
                    severity = notice.severity.as_str(),
                    "ACP session notice"
                );
                if !self.importing() {
                    self.emit(ChatStreamEvent::AcpNotice {
                        conversation_id: self.conversation_id.clone(),
                        notice,
                    });
                }
            }
            Err(error) => tracing::warn!(%error, "an ACP notice was written but could not be read back"),
        }
    }

    /// Take the agent's name for this conversation — over a placeholder, or
    /// over its own earlier name, and over nothing else.
    ///
    /// The sidebar refetches on `conversation-updated`, which `finish` emits
    /// unconditionally at the end of every turn; the title lands before that
    /// because the adapter publishes it before it answers the prompt. Emitted
    /// here as well for the case where it does not.
    async fn adopt_title(&self, title: String) {
        // An import already took the title `session/list` reported, and the
        // conversation it would name does not exist yet.
        if self.importing() {
            return;
        }
        let previous = self
            .agent_title
            .lock()
            .ok()
            .and_then(|mut slot| slot.replace(title.clone()));
        let placeholder = self.placeholder_title.clone();
        let pool = self.services.db.clone();
        let conversation_id = self.conversation_id.clone();
        let written = tokio::task::spawn_blocking(move || {
            let mut conn = get_conn(&pool)?;
            let current = crate::db::ops::conversation::get_conversation(&mut conn, &conversation_id)
                .map_err(|e| e.to_string())?
                .title;
            let nobodys = match current.as_deref() {
                None => true,
                Some(current) => current == placeholder || Some(current) == previous.as_deref(),
            };
            if !nobodys || current.as_deref() == Some(title.as_str()) {
                return Ok::<bool, String>(false);
            }
            crate::db::ops::conversation::update_title(&mut conn, &conversation_id, &title, now_ms())
                .map_err(|e| e.to_string())?;
            Ok(true)
        })
        .await;
        match written {
            Ok(Ok(true)) => {
                tracing::info!(conversation_id = %self.conversation_id, "adopted the agent's title");
                let _ = self.services.events.emit_conversation_updated(&self.conversation_id);
            }
            Ok(Ok(false)) => {}
            Ok(Err(error)) => {
                tracing::warn!(%error, conversation_id = %self.conversation_id, "could not adopt the agent's title")
            }
            Err(error) => {
                tracing::warn!(%error, conversation_id = %self.conversation_id, "adopting the agent's title panicked")
            }
        }
    }

    /// Land a finished round: the assistant row, then its tool results.
    ///
    /// Returns the id of the last row written, which is what the next one hangs
    /// off. A failed tool-result write leaves the chain on the last row that did
    /// land, for the same reason a native turn does — the tool already ran, so
    /// the row is worth less than the turn.
    async fn write_row(&self, turn_id: &str, parent: &str, row: &OpenRow) -> String {
        let tool_calls_json = (!row.tool_calls.is_empty()).then(|| serialize_tool_calls_openai(&row.tool_calls));
        if let Err(e) = complete_assistant(
            &self.services.db,
            &row.message_id,
            &row.text,
            (!row.reasoning.is_empty()).then_some(row.reasoning.as_str()),
            tool_calls_json.as_deref(),
            None,
            // Nothing to declare, and nothing this path could declare: the
            // tokens went to whatever `claude` is signed in as and no figure
            // here would be about a request this app made. Written as the
            // default rather than five `None`s so that a column added to the
            // usage struct does not have to be echoed by a module that has no
            // opinion about any of them.
            MessageUsage::default(),
        )
        .await
        {
            tracing::error!(error = %e, "could not store an ACP assistant row");
            // Counted, not just logged. What the reader saw stream past is now
            // in no database, and every later row still lands — so the
            // transcript reloads well-formed with a paragraph missing and
            // nothing to say so. `finish` reads this and refuses to call the
            // turn `Done`, exactly as it does for a dropped update.
            self.unwritten_rows.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return parent.to_string();
        }

        let mut last = row.message_id.clone();
        for (call_id, output, outcome) in &row.results {
            if let Some(id) = append_tool_result(
                &self.services.db,
                &self.conversation_id,
                turn_id,
                call_id,
                output,
                outcome.as_str(),
                Some(&last),
            )
            .await
            {
                last = id;
            }
        }
        last
    }

    /// Write the rows owed to messages steered into this turn, and say what the
    /// next round hangs off.
    ///
    /// Called immediately after a round is written out, which is the only place
    /// in a turn where the chain has exactly one loose end. Each row also names
    /// itself on the queue item it came from — the item was settled when the
    /// agent said `injected`, minutes of tool call ago, and this is the second
    /// half of that record rather than the thing that settles it.
    async fn write_interjections(&self, turn_id: &str, parent: &str, interjected: &[Interjection]) -> String {
        let mut last = parent.to_string();
        for item in interjected {
            // The same write a native turn's steering uses, and for the same
            // reason: a user row, filed under the turn it was said *to* rather
            // than the one it causes, because that is where the agent read it.
            match write_steering(
                &self.services.db,
                &self.conversation_id,
                turn_id,
                &item.text,
                None,
                Some(&last),
            )
            .await
            {
                Ok(id) => {
                    let pool = self.services.db.clone();
                    let queue_id = item.queue_id.clone();
                    let message_id = id.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        let mut conn = get_conn(&pool)?;
                        crate::db::ops::queue::attach_message(&mut conn, &queue_id, &message_id)
                            .map_err(|e| e.to_string())
                    })
                    .await;
                    self.emit(ChatStreamEvent::UserMessage {
                        message_id: id.clone(),
                        content: item.text.clone(),
                        conversation_id: self.conversation_id.clone(),
                    });
                    last = id;
                }
                // The agent has it either way — this is the transcript's copy.
                // Losing it leaves an answer that changes direction for no
                // visible reason, which is worth a loud log and not worth
                // ending a turn over.
                Err(e) => tracing::error!(error = %e, "an interjection reached the agent but not the transcript"),
            }
        }
        last
    }

    /// Close the current round and open the next one, if the current one is
    /// finished.
    ///
    /// Called before prose is recorded, and does nothing until a result has
    /// landed — so a turn that never calls a tool stays one row, and one that
    /// does gets the row-per-round shape a native turn writes.
    async fn open_round_if_settled(&self) {
        // Decided and taken in one critical section: nothing may land on a row
        // that is already being written out.
        let taken = self
            .with_turn(|t| {
                if !t.row.settled || t.row.is_empty() {
                    return None;
                }
                let carried = OpenRow::new(t.row.message_id.clone());
                Some((
                    t.turn_id.clone(),
                    t.parent.clone(),
                    std::mem::replace(&mut t.row, carried),
                    std::mem::take(&mut t.interjected),
                ))
            })
            .flatten();
        let Some((turn_id, parent, finished, interjected)) = taken else {
            return;
        };

        let last = self.write_row(&turn_id, &parent, &finished).await;
        // A round boundary is the one place in a turn where the chain has a
        // single loose end, which is what a steered message needs to hang off.
        let last = self.write_interjections(&turn_id, &last, &interjected).await;

        match begin_assistant(
            &self.services.db,
            &self.conversation_id,
            &turn_id,
            (None, Some(PROVIDER_LABEL)),
            &self.model(),
            Some(&last),
        )
        .await
        {
            Ok(id) => {
                self.with_turn(|t| {
                    t.row = OpenRow::new(id.clone());
                    t.parent = last;
                });
                // Same event a native turn sends at the top of every round; it
                // is what makes the front end start a new bubble rather than
                // append to the one that just closed.
                self.emit(ChatStreamEvent::MessageStart {
                    message_id: id,
                    turn_id: turn_id.clone(),
                    conversation_id: self.conversation_id.clone(),
                });
            }
            // The database is not answering, which the rest of this turn is
            // going to keep discovering. Stop here rather than carry on writing
            // into a row that has already been completed — that would replace
            // what was just stored with what comes next. The turn is cancelled
            // so it ends down the ordinary path and reports the failure.
            Err(e) => {
                tracing::error!(error = %e, "could not open the next ACP round");
                self.with_turn(|t| {
                    t.parent = last;
                    t.row.written = true;
                    t.cancel.cancel();
                });
            }
        }
    }

    /// Record where in a turn this session is, for whoever finds the row after
    /// a crash.
    ///
    /// Written *before* the thing it describes, which is the whole point: what
    /// is stored when the process dies is where it died. `RunningTool` is the
    /// one that earns this — the agent had started a call and no result was
    /// recorded, so whatever it does may already be done. Without it a hosted
    /// turn killed mid-`Bash` reports as "stopped part way through writing a
    /// reply", the mildest of the four, when it is the most dangerous.
    ///
    /// The tool ran in the adapter rather than here, but the fact being
    /// recorded is the same one: a call was announced and never came back.
    async fn record_phase(&self, phase: TurnPhase, tool: Option<&str>) {
        let Some(turn_id) = self.with_turn(|t| t.turn_id.clone()) else {
            return;
        };
        let pool = self.services.db.clone();
        let tool = tool.map(str::to_string);
        let written = tokio::task::spawn_blocking(move || {
            let mut conn = get_conn(&pool)?;
            crate::db::ops::turn::set_phase(&mut conn, &turn_id, phase, tool.as_deref(), now_ms())
                .map_err(|e| e.to_string())
        })
        .await;
        // Logged, never fatal. A phase that did not land costs a vaguer warning
        // after a crash that may not happen; a turn ended over it costs the
        // answer somebody is reading.
        match written {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::debug!(error = %e, "could not record an ACP turn phase"),
            Err(e) => tracing::debug!(error = %e, "recording an ACP turn phase panicked"),
        }
    }

    /// Fill in a call that was announced before it knew what it was.
    ///
    /// Both the stored row and the card on screen: the row because that is what
    /// a reload rebuilds from, the card because otherwise it keeps saying
    /// "Terminal" with no arguments for the life of the conversation.
    ///
    /// Only ever *adds* information. A revision that arrived without arguments
    /// would otherwise blank the ones already shown — the adapter sends plain
    /// progress beats on the same shape, and those are the majority.
    ///
    /// **Two places to look, and the second one is not an edge case.** The open
    /// row is the fast path. But the two announcements come from two sources
    /// that arrive in either order, and `Call(A) {} → Result(A) → Call(B)`
    /// closes the round holding `A` — so the one carrying its real arguments
    /// can land after the row is already in the database. Stopping at the open
    /// row leaves `{}` in the transcript and in the audit copy for ever, which
    /// reads as a call that genuinely took no arguments.
    async fn revise(&self, call_id: &str, tool_name: &str, arguments: &str) {
        let name = (!tool_name.is_empty()).then_some(tool_name);
        let args = (!(arguments.trim().is_empty() || arguments == "{}")).then_some(arguments);
        if name.is_none() && args.is_none() {
            // An ordinary progress beat. Nothing to add, and nothing worth a
            // database scan — these arrive throughout a long call.
            return;
        }

        let updated = self.with_turn(|t| {
            let call = t.row.tool_calls.iter_mut().find(|c| c.id == call_id)?;
            if let Some(name) = name {
                call.name = name.to_string();
            }
            if let Some(args) = args {
                call.arguments = args.to_string();
            }
            Some((call.name.clone(), call.arguments.clone(), t.row.message_id.clone()))
        });

        let landed = match updated {
            Some(Some((tool_name, arguments, message_id))) => Some((message_id, tool_name, arguments)),
            // Either no turn is running, or the round this call belongs to has
            // been written out. Only the second is worth chasing, and it needs
            // the turn id to bound the search.
            _ => match self.with_turn(|t| t.turn_id.clone()) {
                Some(turn_id) => self.revise_stored(&turn_id, call_id, name, args).await,
                None => None,
            },
        };

        let Some((message_id, tool_name, arguments)) = landed else {
            return;
        };
        self.emit(ChatStreamEvent::ToolCallRevised {
            call_id: call_id.to_string(),
            tool_name,
            arguments,
            message_id,
            conversation_id: self.conversation_id.clone(),
        });
    }

    /// The half of [`Shared::revise`] that reaches a row already stored.
    async fn revise_stored(
        &self,
        turn_id: &str,
        call_id: &str,
        tool_name: Option<&str>,
        arguments: Option<&str>,
    ) -> Option<(String, String, String)> {
        let pool = self.services.db.clone();
        let (turn_id, call_id) = (turn_id.to_string(), call_id.to_string());
        let (name, args) = (tool_name.map(str::to_string), arguments.map(str::to_string));
        let found = tokio::task::spawn_blocking(move || {
            let mut conn = get_conn(&pool)?;
            crate::db::ops::message::revise_tool_call(&mut conn, &turn_id, &call_id, name.as_deref(), args.as_deref())
                .map_err(|e| e.to_string())
        })
        .await;

        // Logged, never fatal. What is lost is the arguments on one card, which
        // is what was already lost before this path existed.
        match found {
            Ok(Ok(row)) => row,
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "could not fill in a stored ACP tool call");
                None
            }
            Err(e) => {
                tracing::debug!(error = %e, "filling in a stored ACP tool call panicked");
                None
            }
        }
    }

    /// Mirror the agent's plan into the todo list.
    ///
    /// Goes straight to `replace_active_list` rather than through the
    /// `update_todos` tool: that one also retires an approved plan once every
    /// step is done, and an ACP session has no plan of this app's to retire.
    async fn write_plan(&self, items: Vec<mapping::PlanItem>) {
        use crate::db::models::todo::ItemStatus;
        use crate::db::ops::todo::TodoItemSpec;

        let pool = self.services.db.clone();
        let conversation_id = self.conversation_id.clone();
        let items: Vec<TodoItemSpec> = items
            .into_iter()
            .map(|item| TodoItemSpec {
                // ACP has no present-continuous form, and the todo bar shows
                // that one while a step runs. Repeating the content reads
                // slightly wrong; leaving it blank leaves the bar empty.
                active_form: item.content.clone(),
                content: item.content,
                status: ItemStatus::parse(&item.status).unwrap_or(ItemStatus::Pending),
            })
            .collect();
        if items.is_empty() {
            return;
        }

        let written = tokio::task::spawn_blocking(move || {
            let mut conn = get_conn(&pool)?;
            crate::db::ops::todo::replace_active_list(&mut conn, &conversation_id, "Claude Code", &items, now_ms())
                .map_err(|e| e.to_string())
        })
        .await;
        match written {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "could not store the agent's plan"),
            Err(e) => tracing::warn!(error = %e, "could not store the agent's plan (the write panicked)"),
        }
    }

    /// Where an inbound question can draw its card, if a turn is running.
    ///
    /// Both things the agent can stop to ask about — a permission and an
    /// elicitation — need the same three facts and neither may hold the lock
    /// across the wait that follows, which is minutes long.
    fn turn_context(&self) -> Option<approvals::TurnContext> {
        self.turn.lock().ok().and_then(|t| {
            t.as_ref().map(|t| approvals::TurnContext {
                turn_id: t.turn_id.clone(),
                assistant_message_id: t.row.message_id.clone(),
                cancel: t.cancel.clone(),
                plan_reviews: Arc::clone(&self.plan_reviews),
            })
        })
    }
}

#[async_trait::async_trait]
impl Handler for Shared {
    async fn notification(&self, method: String, params: serde_json::Value) {
        if method != "session/update" {
            tracing::debug!(method, "an ACP notification this client does not handle");
            return;
        }
        match serde_json::from_value::<SessionNotification>(params) {
            Ok(notification) => self.absorb(notification).await,
            Err(e) => tracing::debug!(error = %e, "could not read a session/update"),
        }
    }

    async fn request(&self, method: String, params: serde_json::Value) -> Result<serde_json::Value, String> {
        match method.as_str() {
            "session/request_permission" => {
                let params = serde_json::from_value(params).map_err(|e| e.to_string())?;
                match self.turn_context() {
                    Some(context) => approvals::ask(&self.services, &self.conversation_id, &context, params).await,
                    // A question with no turn behind it has nowhere to draw a
                    // card and nobody to answer it. Refusing beats hanging the
                    // adapter on a prompt that will never appear.
                    None => {
                        tracing::warn!("an ACP permission request arrived with no turn running");
                        Ok(protocol::permission_cancelled())
                    }
                }
            }
            // The agent asking a question rather than for permission. Drawn as
            // this app's own `ask_user` form — see `elicitation`, which also
            // explains why declaring the capability behind this is what makes
            // `AskUserQuestion` exist at all.
            "elicitation/create" => {
                let params = serde_json::from_value(params).map_err(|e| e.to_string())?;
                match self.turn_context() {
                    Some(context) => elicitation::ask(&self.services, &self.conversation_id, &context, params).await,
                    // A question nobody can be shown is one the agent should
                    // carry on without, rather than one it should abandon the
                    // turn over — the opposite of the arm above, and
                    // `elicitation` says why.
                    None => {
                        tracing::warn!("an ACP elicitation arrived with no turn running");
                        Ok(protocol::elicitation_declined())
                    }
                }
            }
            // `fs/*` and `terminal/*` land here while those capabilities are
            // declared unsupported. An agent should not send them; one that
            // does gets a refusal rather than silence.
            other => Err(format!("`{other}` is not supported by this client")),
        }
    }
}

/// What a turn is carrying that has to be settled once the agent has read it.
///
/// Held together because they settle together and on the same evidence — a
/// `session/prompt` that came back at all, whatever its stop reason. Anything
/// weaker settles nothing: a turn can assemble this and then die on a pipe that
/// closed, having told nobody. Anything stronger settles too little: a turn the
/// user stopped after two seconds still delivered the prompt that carried this.
#[derive(Default)]
struct Owed {
    turns: Option<crate::agent::interrupted::Report>,
    queued: Option<crate::agent::queue::Doubtful>,
    /// Shell output is already part of native provider history. Hosted ACP
    /// sessions keep their own history, so an undelivered result has to ride
    /// the next `session/prompt` and receive its own acknowledgement ledger.
    shell: Option<PendingShellContext>,
    /// The agent cannot see the conversation it is answering into.
    ///
    /// Not a ledger like the other two — there is nothing to write down, only
    /// something to say once. It settles with them because it settles on the
    /// same evidence and forgetting to clear it would repeat the notice on
    /// every turn for the life of the session.
    memory_lost: bool,
    /// The tool bridge was promised and is not there.
    ///
    /// Same shape as `memory_lost` and for the same reason: something to say
    /// exactly once, cleared only once a prompt carrying it came back.
    tools_lost: bool,
}

struct PendingShellContext {
    item_ids: Vec<String>,
    rendered: String,
}

const MAX_ACP_PENDING_SHELL_ITEMS: usize = 4;
const MAX_ACP_PENDING_SHELL_BYTES: usize = 128 * 1024;

fn bounded_pending_shell_context(
    candidates: &[crate::db::models::message_context_item::MessageContextItemRow],
) -> Result<Option<PendingShellContext>, String> {
    let mut item_ids = Vec::new();
    let mut rendered = Vec::new();
    let mut bytes = 0usize;
    for item in candidates {
        if item_ids.len() >= MAX_ACP_PENDING_SHELL_ITEMS {
            break;
        }
        let body = crate::workspace::reference::render_context_item(
            crate::workspace::reference::MessageContextKind::parse(&item.kind)?,
            item.display_path.as_deref(),
            item.line_start,
            item.line_end,
            &item.content,
            item.truncated != 0,
        );
        let message = provider::ChatMessage::user_provided_context(&body);
        let wire = provider::render_message(&message, provider::SenderRendering::Prefix)
            .expect("user-provided context renders without a content envelope")
            .content;
        let separator = usize::from(!rendered.is_empty()) * 2;
        if bytes.saturating_add(separator).saturating_add(wire.len()) > MAX_ACP_PENDING_SHELL_BYTES {
            // Preserve branch order. This item and everything after it remain
            // absent from the receipt ledger and are reconsidered next turn.
            break;
        }
        bytes += separator + wire.len();
        item_ids.push(item.id.clone());
        rendered.push(wire);
    }
    Ok((!item_ids.is_empty()).then(|| PendingShellContext {
        item_ids,
        rendered: rendered.join("\n\n"),
    }))
}

fn prompt_with_workspace_context(text: &str, context: &[crate::workspace::reference::PreparedContextItem]) -> String {
    let mut payload = if context.is_empty() {
        text.to_string()
    } else {
        let selected = context
            .iter()
            .filter_map(|item| {
                item.display_path
                    .as_ref()
                    .map(|path| crate::workspace::reference::WorkspaceReferenceRequest {
                        path: path.clone(),
                        line_start: item.line_start.map(|line| line as u32),
                        line_end: item.line_end.map(|line| line as u32),
                    })
            })
            .collect::<Vec<_>>();
        crate::workspace::reference::neutralise_reference_markers(text, &selected)
    };
    for item in context {
        let body = crate::workspace::reference::render_context_item(
            item.kind,
            item.display_path.as_deref(),
            item.line_start,
            item.line_end,
            &item.content,
            item.truncated != 0,
        );
        let context_message = provider::ChatMessage::user_provided_context(&body);
        payload.push_str("\n\n");
        payload.push_str(
            &provider::render_message(&context_message, provider::SenderRendering::Prefix)
                .expect("user-provided context renders without a content envelope")
                .content,
        );
    }
    payload
}

/// Whether the prompt carrying an [`Owed`] ever reached the adapter.
///
/// [`AcpSession::finish`] used to read this off the outcome — a reply means the
/// adapter took the prompt, and the prompt is what the explanations rode on.
/// That is true of every reply the *adapter* sends and false of the one this app
/// writes itself: a turn stopped before its request was first polled deliberately
/// does not send it, and is written up as `cancelled` from a synthesised success.
/// Settling there spends the interrupted-turn report, the in-doubt queue items
/// and the memory-loss notice on an agent that received none of them — and all
/// three clear exactly once, so the turn that does reach the adapter says
/// nothing about any of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptDelivery {
    Sent,
    NeverSent,
}

impl PromptDelivery {
    /// Whether a turn ending this way is proof the agent read what rode on the
    /// prompt.
    ///
    /// Both halves are needed and neither is enough. The reply is what says the
    /// adapter took it — a `cancelled` stop reason included, since the user
    /// stopped the work rather than the reading, while an `Err` may be a pipe
    /// that closed before the request went out. And `Sent` is what says the
    /// reply came from the adapter at all.
    fn read_by(self, outcome: &Result<serde_json::Value, PeerError>) -> bool {
        self == PromptDelivery::Sent && outcome.is_ok()
    }
}

/// What the agent is told when it has been given a conversation it has no
/// record of.
///
/// Worth saying plainly because the situation is worse than forgetting: a
/// hosted prompt carries only the new message, so this app's transcript never
/// enters the agent's context at all. It is not hazy about what came before —
/// it cannot see any of it, while the person it is talking to can see all of
/// it. Left unsaid, both sides spend a few turns confused about which of them
/// is being obtuse.
const NO_MEMORY: &str = "<no_session_memory>\n\
This conversation has a transcript above that you cannot see. The agent session \
it belonged to could not be resumed, so you are starting with no record of any \
of it — and a prompt carries only the newest message, so none of it will reach \
you later either. The user can see all of it.\n\
Do not answer as though you remember. Say plainly that this session lost its \
memory of the conversation, and ask them to restate whatever matters.\n\
</no_session_memory>";

/// What the agent is told when the tool bridge did not come up.
///
/// **The bridge fails visibly rather than open**, which is the opposite of
/// `hooks/`. That gate is an external review, and a review that does not run
/// costs one missed review. This is a set of capabilities already promised to
/// an agent: missing silently, it works around their absence, or — worse —
/// tells the user it saved a memory using a tool that was never there.
///
/// It rides `Owed` rather than getting a UI of its own for the same reason the
/// memory notice does: the agent's own first sentence lands exactly where the
/// confusion would have been.
const NO_TOOLS: &str = "<meridian_tools_unavailable>\n\
Meridian could not start the local tool bridge for this session, so its \
tools — reading this conversation's memories, its application log and its \
usage — are not available to you this time. Nothing else is affected.\n\
Mention this once, briefly, if the user asks for something that would have \
needed them. Do not claim to have used them.\n\
</meridian_tools_unavailable>";

impl Owed {
    /// The message with whatever has to be explained in front of it.
    ///
    /// A hosted prompt is one lump of text, so there is nowhere else to put
    /// this. Ahead of the message rather than behind it, because it is context
    /// for reading the message rather than a footnote to it.
    fn in_front_of(&self, text: &str) -> String {
        let mut parts: Vec<&str> = Vec::new();
        // First of the three. The other two describe things that happened
        // *within* a conversation the agent is assumed to be following, and
        // this one says it is not following any of it.
        if self.memory_lost {
            parts.push(NO_MEMORY);
        }
        // After the memory notice and before the two ledgers. It is about this
        // session's own capabilities rather than about anything that happened
        // in the conversation, which is what the ledgers are.
        if self.tools_lost {
            parts.push(NO_TOOLS);
        }
        if let Some(report) = &self.turns {
            parts.push(report.text());
        }
        if let Some(report) = &self.queued {
            parts.push(report.text());
        }
        if let Some(shell) = &self.shell {
            parts.push(&shell.rendered);
        }
        if parts.is_empty() {
            return text.to_string();
        }
        parts.push(text);
        parts.join("\n\n")
    }

    fn is_empty(&self) -> bool {
        self.turns.is_none() && self.queued.is_none() && self.shell.is_none() && !self.memory_lost && !self.tools_lost
    }

    /// Write the ledgers down, now that the agent has had them.
    async fn settle(self, services: &Services, shared: &Shared) {
        if let Some(report) = self.turns {
            crate::agent::interrupted::confirm_delivered(&services.db, report).await;
        }
        if let Some(report) = self.queued {
            crate::agent::queue::confirm_reported(services, report).await;
        }
        if let Some(shell) = self.shell {
            let pool = services.db.clone();
            let count = shell.item_ids.len();
            let settled = tokio::task::spawn_blocking(move || {
                let mut conn = get_conn(&pool)?;
                crate::db::ops::acp_context_delivery::mark_delivered(&mut conn, &shell.item_ids, now_ms())
                    .map_err(|e| e.to_string())
            })
            .await;
            match settled {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => tracing::warn!(error = %error, count, "could not settle ACP shell context"),
                Err(error) => tracing::warn!(error = %error, count, "ACP shell-context settlement task failed"),
            }
        }
        if self.memory_lost
            && let Ok(mut slot) = shared.memory_lost.lock()
        {
            *slot = false;
        }
        if self.tools_lost
            && let Ok(mut slot) = shared.tools_lost.lock()
        {
            *slot = false;
        }
    }
}

pub struct AcpSession {
    peer: Arc<Peer>,
    shared: Arc<Shared>,
    pub conversation_id: String,
    pub acp_session_id: String,
    pub cwd: String,
    /// Whether this adapter advertised `_session/steering`.
    ///
    /// Read once, at the handshake, because that is the only time it is said.
    /// An adapter without it cannot be steered at all — an interjection has to
    /// wait for the turn to end and go as an ordinary prompt, which is what
    /// Claude Code did before the extension existed.
    steering: bool,
    /// Whether the agent picked up the session it had rather than starting one.
    ///
    /// The caller writes the id back either way — a resume answers with
    /// whichever session the SDK actually recovered, which need not be the one
    /// asked for.
    pub resumed: bool,
    /// This session's tool bridge, when it came up.
    ///
    /// Held so a turn can open and shut its window, and so [`Self::close`] can
    /// take the port down with the process — a bridge outliving its session
    /// would be an open endpoint onto a conversation nothing is answering.
    bridge: Option<Arc<bridge::Bridge>>,
}

/// What the handshake settled.
struct Handshook {
    acp_session_id: String,
    steering: bool,
    resumed: bool,
}

/// How a session is being opened.
struct Opening<'a> {
    cwd: &'a str,
    /// The session to pick up, when there is one on record. Absent for a
    /// conversation being created, and for one from before there was anywhere
    /// to write the id down.
    resume: Option<&'a str>,
    /// Whether failing to resume is worth telling the agent about. False for a
    /// new conversation: there is no transcript above for it to be blind to.
    transcript_above: bool,
    /// Whether the recital a resume produces is something this app already has
    /// rows for, or the only copy of a conversation it has never seen.
    keep_recital: bool,
    /// Whether a resume that fails should start a fresh session instead.
    ///
    /// Right for a conversation being reopened — a session id outlives the
    /// session it names, and the answer to "that one is gone" is a new session
    /// rather than a dead conversation. Wrong for an import, where there is
    /// nothing to import from a session that was just created, and creating one
    /// leaves a stray session behind on the agent's disk every time somebody
    /// tries. It also buries the reason: "not found", "cwd does not exist" and
    /// "the adapter cannot load sessions" want three different responses and
    /// the fallback turns all three into the same one.
    fall_back_to_new: bool,
    /// Whether this session should be lent Meridian's own tools.
    ///
    /// False for an import, which opens a session only to read its recital: no
    /// turn ever runs on it, so the bridge would have no window to open and
    /// the conversation it would be scoped to does not exist in the database
    /// yet. A port bound for the length of a recital is a port bound for
    /// nothing.
    wants_tools: bool,
}

impl AcpSession {
    /// Start an adapter and open a session in `cwd`, resuming if asked to.
    ///
    /// The conversation must already exist: creating it is the caller's job
    /// because only the caller knows whether this is a new conversation or one
    /// being reopened, and a half-created conversation whose adapter failed to
    /// start is worse than none.
    async fn open_with(
        services: Services,
        config: &AcpConfig,
        conversation_id: String,
        opening: Opening<'_>,
    ) -> Result<Arc<Self>, String> {
        // Before the adapter, because its descriptor has to go in the very
        // first thing said to it. A failure here is *not* a failure to open —
        // see [`NO_TOOLS`] — but it does have to be visible, which is what
        // `tools_lost` is for.
        //
        // **A containerised adapter is not offered the bridge at all.** The
        // endpoint binds this host's loopback, and `127.0.0.1` inside the
        // container is the container — so a descriptor sent in would advertise
        // tools every call to which dials nowhere, and the model would keep
        // trying them or claim to have used them. Reaching through the
        // boundary (`host.docker.internal`, wider binds) is measured to work
        // only on Docker Desktop and weakens the loopback boundary elsewhere,
        // so until that is built the honest answer is the one `NO_TOOLS`
        // already says: the tools are missing, and the agent is told so.
        let containerised = super::process::launches_in_container(&config.command, &config.args);
        let bridge = if opening.wants_tools && !containerised {
            match Self::start_bridge(&services, &conversation_id).await {
                Ok(bridge) => Some(bridge),
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        conversation_id = %conversation_id,
                        "the tool bridge did not start; this session will have no Meridian tools"
                    );
                    None
                }
            }
        } else {
            if opening.wants_tools {
                tracing::warn!(
                    conversation_id = %conversation_id,
                    "the adapter runs in a container the bridge's loopback endpoint cannot reach; \
                     this session will have no Meridian tools"
                );
            }
            None
        };
        // Asked for and not got. An import asks for none, so its `None` is not
        // a loss and must not produce a notice — which is why this is the
        // conjunction rather than `bridge.is_none()`.
        let tools_lost = opening.wants_tools && bridge.is_none();

        // From the command the user configured, which is the only place the
        // mapping exists — see `cwd_for_agent`. Empty for every adapter that
        // is not containerised, which makes the translation the identity.
        let mounts = super::mounts::MountMap::from_command(&config.command, &config.args);

        let process = match AdapterProcess::spawn(&config.command, &config.args).await {
            Ok(process) => process,
            Err(e) => {
                // The bridge is already listening on a port with a task behind
                // it. Returning without this leaks both for every failed
                // launch, and failing to launch is the ordinary case on a
                // machine without node.
                if let Some(bridge) = &bridge {
                    bridge.stop();
                }
                return Err(e);
            }
        };

        let shared = Arc::new(Shared {
            services,
            conversation_id: conversation_id.clone(),
            turn: Mutex::new(None),
            model: Mutex::new(None),
            config: Mutex::new(Vec::new()),
            replay: Mutex::new(Replay::No),
            announced: Mutex::new(HashSet::new()),
            unwritten_rows: std::sync::atomic::AtomicUsize::new(0),
            memory_lost: Mutex::new(false),
            tools_lost: Mutex::new(tools_lost),
            plan_reviews: Arc::new(crate::acp::plan_review::ReviewControl::default()),
            agent_title: Mutex::new(None),
            placeholder_title: super::title_for(opening.cwd),
        });
        let peer = Peer::start(process, shared.clone() as Arc<dyn Handler>);

        // From here on the adapter is running, so every failure has to take it
        // down again. `?` alone would return leaving the peer's tasks holding a
        // live child nobody has a handle to any more — an orphaned node process
        // per failed attempt, and the usual reason to fail (not signed in) is
        // one the user retries.
        match Self::handshake(&peer, &shared, &opening, bridge.as_ref(), &mounts).await {
            Ok(Handshook {
                acp_session_id,
                steering,
                resumed,
            }) => {
                // Only now, because only now is it true. A conversation with a
                // transcript whose agent did not resume is answering blind.
                if opening.transcript_above
                    && !resumed
                    && let Ok(mut slot) = shared.memory_lost.lock()
                {
                    *slot = true;
                }
                tracing::info!(
                    conversation_id = %conversation_id,
                    acp_session_id = %acp_session_id,
                    steering,
                    resumed,
                    "ACP session opened"
                );
                Ok(Arc::new(Self {
                    peer,
                    shared,
                    conversation_id,
                    acp_session_id,
                    cwd: opening.cwd.to_string(),
                    steering,
                    resumed,
                    bridge,
                }))
            }
            Err(e) => {
                peer.stop().await;
                if let Some(bridge) = &bridge {
                    bridge.stop();
                }
                Err(e)
            }
        }
    }

    /// Bind this session's tool bridge, scoped to the conversation it serves.
    ///
    /// The project is read once, here, because it is what decides whether the
    /// memory tools are offered at all and it cannot change under a session.
    /// A conversation that has none is not an error — it gets the two tools
    /// that need no project.
    async fn start_bridge(services: &Services, conversation_id: &str) -> Result<Arc<bridge::Bridge>, String> {
        let pool = services.db.clone();
        let id = conversation_id.to_string();
        let project_id = tokio::task::spawn_blocking(move || {
            let mut conn = get_conn(&pool)?;
            crate::db::ops::conversation::get_conversation(&mut conn, &id)
                .map(|c| c.project_id)
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())??;

        bridge::Bridge::start(
            services.clone(),
            conversation_id,
            project_id.as_deref(),
            services.paths.data_dir.join("logs"),
        )
        .await
    }

    /// A session for a conversation being created. Nothing to resume, and
    /// nothing above for the agent to be blind to.
    pub async fn open(
        services: Services,
        config: &AcpConfig,
        conversation_id: String,
        cwd: String,
    ) -> Result<Arc<Self>, String> {
        Self::open_with(
            services,
            config,
            conversation_id,
            Opening {
                cwd: &cwd,
                resume: None,
                transcript_above: false,
                keep_recital: false,
                fall_back_to_new: true,
                wants_tools: true,
            },
        )
        .await
    }

    /// A session for a conversation that already exists, picking up `resume` if
    /// the agent still has it.
    pub async fn reopen(
        services: Services,
        config: &AcpConfig,
        conversation_id: String,
        cwd: String,
        resume: Option<String>,
        transcript_above: bool,
    ) -> Result<Arc<Self>, String> {
        Self::open_with(
            services,
            config,
            conversation_id,
            Opening {
                cwd: &cwd,
                resume: resume.as_deref(),
                transcript_above,
                keep_recital: false,
                fall_back_to_new: true,
                wants_tools: true,
            },
        )
        .await
    }

    /// A session opened only to be read.
    ///
    /// The conversation id is minted by the caller and does **not** exist in
    /// the database yet: nothing is written until the recital is complete, so
    /// that the conversation row, the `acp_sessions` row and every message land
    /// in one transaction or not at all. A half-imported conversation sitting
    /// in the sidebar is the failure this shape rules out.
    ///
    /// Fails rather than falling back when the session cannot be loaded, and
    /// the failure carries the agent's own words. For a reopen a fresh session
    /// is the right answer; here there is nothing to import from one, and
    /// making one anyway leaves a stray session on the agent's disk for every
    /// attempt while replacing three distinguishable reasons — gone, wrong
    /// directory, adapter cannot load — with one guess.
    pub(super) async fn open_for_import(
        services: Services,
        config: &AcpConfig,
        conversation_id: String,
        cwd: &str,
        session_id: &str,
    ) -> Result<Arc<Self>, String> {
        Self::open_with(
            services,
            config,
            conversation_id,
            Opening {
                cwd,
                resume: Some(session_id),
                transcript_above: false,
                keep_recital: true,
                fall_back_to_new: false,
                wants_tools: false,
            },
        )
        .await
    }

    /// The recited conversation, taken out of the session that collected it.
    ///
    /// Taken rather than borrowed: it is written once, and a second caller
    /// getting a second copy would be a second transcript.
    pub(super) fn take_recital(&self) -> Vec<Recital> {
        let Ok(mut replay) = self.shared.replay.lock() else {
            return Vec::new();
        };
        match std::mem::replace(&mut *replay, Replay::No) {
            Replay::Collect(recital) => recital,
            other => {
                *replay = other;
                Vec::new()
            }
        }
    }

    /// What the agent last said about its own knobs, which is where the model
    /// is. Read by an import to record which model wrote the rows.
    pub(super) fn model(&self) -> String {
        self.shared.model()
    }

    /// How many updates the reader had to throw away over this session's life.
    ///
    /// An import reads it across the load: a replay is a burst, the notify
    /// queue drops on overflow, and a transcript with an invisible hole in it
    /// is worse than no import at all.
    pub(super) fn dropped_notifications(&self) -> u64 {
        self.peer.dropped_notifications()
    }

    /// Greet the adapter and open a session in `cwd`.
    ///
    /// Split out so [`open_with`](Self::open_with) has exactly one failure path
    /// to clean up after, rather than four `?`s that each need remembering.
    async fn handshake(
        peer: &Arc<Peer>,
        shared: &Shared,
        opening: &Opening<'_>,
        bridge: Option<&Arc<bridge::Bridge>>,
        mounts: &super::mounts::MountMap,
    ) -> Result<Handshook, String> {
        let cwd = opening.cwd;
        let init = peer
            .request(
                "initialize",
                serde_json::to_value(protocol::InitializeParams {
                    protocol_version: protocol::PROTOCOL_VERSION,
                    // `fs` and `terminal` off, form elicitation on. Turning `fs`
                    // on means answering `fs/read_text_file` and
                    // `fs/write_text_file`, which is the step that would put
                    // every file the agent touches through this app. The
                    // elicitation half is not a nicety: the adapter withdraws
                    // `AskUserQuestion` from the model when it is missing.
                    client_capabilities: protocol::ClientCapabilities::default(),
                    client_info: protocol::Implementation {
                        name: CLIENT_NAME.into(),
                        title: Some("Meridian".into()),
                        version: env!("CARGO_PKG_VERSION").into(),
                    },
                })
                .map_err(|e| e.to_string())?,
            )
            .await
            .map_err(|e| describe(peer, e))?;

        let init: protocol::InitializeResult = serde_json::from_value(init).map_err(|e| e.to_string())?;
        let steering = init.steering_supported();
        tracing::info!(
            protocol_version = init.protocol_version,
            // The default command is an unpinned `npx -y`, so which adapter
            // answered is a fact about *today* and nothing in this repository
            // records it. Everything this client knows about replay shapes,
            // `messageId` stamping and what `session/list` returns was measured
            // against one version; when that stops being true the symptom will
            // be a transcript that is subtly wrong, and this line is the only
            // place that will say which build produced it.
            agent = init.agent_info.as_ref().map(|a| format!("{} {}", a.name, a.version)),
            load_session = init.agent_capabilities.load_session,
            lists_sessions = init.agent_capabilities.lists_sessions(),
            auth_method_count = init.auth_methods.len(),
            steering,
            "ACP adapter initialised"
        );

        // Picking the session back up, when there is one and the agent can.
        // Tried first and — for a reopen — allowed to fail: a session id
        // outlives the session it names, the user can delete it and `claude`
        // can prune it, and the right answer to "that one is gone" is a fresh
        // session rather than a dead conversation. `fall_back_to_new` is where
        // that stops being right.
        if let Some(resume) = opening.resume {
            let refused = (!init.agent_capabilities.load_session)
                .then(|| "this adapter cannot load existing sessions".to_string());
            let outcome = match refused {
                Some(why) => Err(why),
                None => Self::load(peer, shared, cwd, resume, opening.keep_recital, bridge, mounts).await,
            };
            match outcome {
                Ok(session) => {
                    return Ok(Handshook {
                        acp_session_id: session,
                        steering,
                        resumed: true,
                    });
                }
                // Nothing to fall back *to*: an import wants that session or
                // none, and a `session/new` here would create one on the
                // agent's disk that nobody asked for and nothing will use.
                Err(e) if !opening.fall_back_to_new => {
                    return Err(format!("could not open session `{resume}`: {e}"));
                }
                Err(e) => tracing::warn!(
                    error = %e,
                    resume,
                    conversation_id = %shared.conversation_id,
                    "could not resume the agent session; starting a new one"
                ),
            }
        }

        // No `authenticate` call. `authMethods` lists what the adapter *can*
        // do, not what it still needs — `claude-code-acp` reports several while
        // already signed in as whatever `claude` is signed in as, so treating a
        // non-empty list as "not authenticated" would refuse every working
        // setup. If it really is unauthenticated, `session/new` says so and
        // that message reaches the user unchanged.
        // Asked for once, given up on rather than insisted on. `_meta` carries
        // the thinking display down to a CLI this app does not pin, and a
        // `claude` that has never heard of the flag refuses it *before it runs*
        // — so without this second attempt one old binary means no hosted
        // session at all. See [`protocol::SessionMeta`] for the measurement.
        //
        // Inlined rather than calling `ask_for_the_thinking` because an
        // `AsyncFn` closure that captures `&peer` and `&shared` produces a
        // future whose `Send` bound Tauri's `#[tauri::command]` macro cannot
        // satisfy for arbitrary lifetimes.
        let new = |meta| new_session_params(cwd, bridge, meta, mounts);
        let session = match peer
            .request(
                "session/new",
                serde_json::to_value(new(Some(protocol::SessionMeta::default()))).map_err(|e| e.to_string())?,
            )
            .await
            .map_err(|e| describe(peer, e))
        {
            Ok(s) => s,
            Err(refused) => {
                let plain = peer
                    .request(
                        "session/new",
                        serde_json::to_value(new(None)).map_err(|e| e.to_string())?,
                    )
                    .await
                    .map_err(|e| describe(peer, e));
                if plain.is_ok() {
                    tracing::warn!(
                        error = %refused,
                        conversation_id = %shared.conversation_id,
                        "the agent refused the session options; opened without them, so no thinking will be shown"
                    );
                }
                plain?
            }
        };
        let session: protocol::NewSessionResult = serde_json::from_value(session).map_err(|e| e.to_string())?;
        // Known from the moment the session exists, so the first row of the
        // first turn records the real model rather than the placeholder, and
        // the composer has something to offer before anyone has typed. Re-sent
        // on every change after this, as a `config_option_update`.
        shared.merge_config(session.config_options);
        Ok(Handshook {
            acp_session_id: session.session_id,
            steering,
            resumed: false,
        })
    }

    /// Ask the agent to pick a session back up, and deal with the recital.
    ///
    /// A load replays the whole conversation as `session/update` notifications.
    /// `keep` says which kind of recital that is — [`Replay::Discard`] for a
    /// conversation this database already holds, [`Replay::Collect`] for one it
    /// has never seen. Either way the gate goes up before the request and comes
    /// down only after the reply *and* a drain: the reply travels a different
    /// route from the notifications and routinely overtakes them, which is the
    /// same reason `prompt` drains before it finishes a turn.
    ///
    /// The gate is also still up while the options are merged, because that is
    /// what keeps an import from announcing knobs for a conversation whose row
    /// has not been written yet.
    async fn load(
        peer: &Arc<Peer>,
        shared: &Shared,
        cwd: &str,
        resume: &str,
        keep: bool,
        bridge: Option<&Arc<bridge::Bridge>>,
        mounts: &super::mounts::MountMap,
    ) -> Result<String, String> {
        // One attempt, gate and all. Raised *inside* rather than around the two,
        // because a refused attempt can have recited before it failed and
        // `Replay::Collect` starts each one with an empty recital — a retry
        // appending to the first one's leavings would import the same rows
        // twice.
        //
        // `LoadSessionResult`, not `NewSessionResult`: the schema's load
        // response has no `sessionId` and no required field at all, so `{}` and
        // `null` are both conforming answers. Parsed as a new session they
        // would read as a failure, send a reopen down the `session/new`
        // fallback and lose the agent's memory of the conversation silently.
        // `null` reaches here as `Value::Null`, which deserialises to the
        // default rather than an error.
        // Same retry shape as `handshake`, inlined for the same `Send` reason.
        let load_params = |meta| load_session_params(resume, cwd, bridge, meta, mounts);
        let load_once = async |meta| {
            let params = serde_json::to_value(load_params(meta)).map_err(|e| e.to_string())?;
            shared.set_replay(if keep {
                Replay::Collect(Vec::new())
            } else {
                Replay::Discard
            });
            let answered = peer.request("session/load", params).await;
            peer.drain_notifications().await;
            answered
                .map_err(|e| describe(peer, e))
                .and_then(protocol::LoadSessionResult::read)
        };
        let loaded = match load_once(Some(protocol::SessionMeta::default())).await {
            Ok(session) => Ok(session),
            Err(refused) => {
                let plain = load_once(None).await;
                if plain.is_ok() {
                    tracing::warn!(
                        error = %refused,
                        conversation_id = %shared.conversation_id,
                        "the agent refused the session options; resumed without them, so no thinking will be shown"
                    );
                }
                plain
            }
        };

        let session = match loaded {
            Ok(session) => session,
            // The gate has to come down on the failure path as well. A load
            // that fails falls back to `session/new` on the same session, and
            // one still set to discard would swallow that session's first turn
            // — every chunk of it read as more recital.
            Err(e) => {
                shared.set_replay(Replay::No);
                return Err(e);
            }
        };
        shared.merge_config(session.config_options);
        // A collection stays up until the caller takes it with `take_recital`,
        // which is also what puts the session back to live.
        if !keep {
            shared.set_replay(Replay::No);
        }
        // The reply when it names one, because a resume can land on a different
        // session than the one asked for and storing the request would have the
        // next resume chase an id that never existed. Silence means it took the
        // one it was given — there is nothing else it could mean.
        Ok(session.session_id.unwrap_or_else(|| resume.to_string()))
    }

    pub fn is_alive(&self) -> bool {
        self.peer.is_alive()
    }

    /// Whether this adapter takes `_session/steering`, as it said at the
    /// handshake. An interjection to a session that answers `false` has to wait
    /// for the turn to end and go as an ordinary prompt.
    pub fn supports_steering(&self) -> bool {
        self.steering
    }

    /// The turn running right now, if there is one.
    ///
    /// What the queue runner asks to decide which of the two modes it may
    /// deliver, and what it records a steer against. Racy by nature — the turn
    /// can end in the gap — which is why nothing downstream trusts it: a steer
    /// that arrives too late is answered `promptRequired` and comes back for
    /// the other path.
    pub fn current_turn_id(&self) -> Option<String> {
        let guard = self.shared.turn.lock().ok()?;
        let turn = guard.as_ref()?;
        // A durable review deliberately releases the ordinary runner lease.
        // Reporting it as steerable would let the queue inject a prompt into
        // an adapter parked inside ExitPlanMode, bypassing the review barrier.
        (!self.shared.plan_reviews.is_waiting_turn(&turn.turn_id)).then(|| turn.turn_id.clone())
    }

    /// Continue a committed durable plan decision across the ACP boundary.
    ///
    /// A live ExitPlanMode is resumed by answering its exact one-shot option.
    /// Change feedback is steered into that turn first when the adapter can
    /// prove it was injected; otherwise the rejection is allowed to close the
    /// old turn and the same durable envelope is sent as a new prompt. After a
    /// restart there is no option sender to recover, so the explicit delivery
    /// likewise becomes a prompt on the resumed session.
    pub async fn deliver_plan_review(
        &self,
        services: &Services,
        delivery: super::AcpPlanReviewDelivery,
    ) -> super::AcpPlanReviewDeliveryOutcome {
        use super::plan_review::{
            AcpPlanReviewDeliveryOutcome as Outcome, DeliveryAction, DeliveryBoundary, ReviewDecisionAction,
        };

        let action = match super::plan_review::delivery_action(&delivery.payload_json) {
            Ok(action) => action,
            Err(error) => return Outcome::Held(error),
        };
        let prompt = super::plan_review::delivery_prompt(&delivery.payload_json, action);

        if let Some(identity) = self.shared.plan_reviews.identity(&delivery.review_id) {
            if identity.call_id != delivery.provider_call_id {
                return Outcome::Held(format!(
                    "ACP review call mismatch: live {}, stored {}",
                    identity.call_id, delivery.provider_call_id
                ));
            }
            if let Some(expected) = delivery.target_session_id.as_deref()
                && expected != identity.session_id
            {
                return Outcome::Held(format!(
                    "ACP review session mismatch: live {}, stored {expected}",
                    identity.session_id
                ));
            }
            if identity.turn_id != delivery.submitting_turn_id {
                return Outcome::Held(format!(
                    "ACP review turn mismatch: live {}, stored {}",
                    identity.turn_id, delivery.submitting_turn_id
                ));
            }

            let mut needs_prompt = false;
            // Once steering reports any success other than `promptRequired`,
            // the feedback may already be inside the adapter. If resolving the
            // parked ExitPlanMode option then fails, replaying this delivery is
            // unsafe: surface an in-doubt boundary instead of a retryable hold.
            let mut feedback_consumed = false;
            if action == DeliveryAction::RequestChanges {
                if self.steering {
                    let params =
                        match serde_json::to_value(protocol::SteerParams::text(self.acp_session_id.clone(), &prompt)) {
                            Ok(params) => params,
                            Err(error) => return Outcome::Held(error.to_string()),
                        };
                    match self.peer.request(protocol::STEER_METHOD, params).await {
                        Ok(value) => match serde_json::from_value::<protocol::SteerResult>(value) {
                            Ok(result) if result.outcome() == protocol::SteerOutcome::PromptRequired => {
                                // Definitively not consumed; safe to carry it
                                // as a normal prompt after rejection closes the
                                // ExitPlanMode turn.
                                needs_prompt = true;
                            }
                            // Injected, startedNewTurn and an unknown future
                            // success all mean the adapter says it consumed the
                            // message. An unreadable success reply is treated
                            // the same way: replaying would be the dangerous
                            // choice.
                            Ok(_) | Err(_) => feedback_consumed = true,
                        },
                        Err(PeerError::Rpc(_)) => {
                            // The adapter answered no, so nothing was consumed.
                            needs_prompt = true;
                        }
                        Err(PeerError::Dead(error)) => return Outcome::InDoubt(error),
                    }
                } else {
                    needs_prompt = true;
                }
            }

            let action = match action {
                DeliveryAction::Approve => ReviewDecisionAction::Approve,
                DeliveryAction::RequestChanges => ReviewDecisionAction::RequestChanges,
            };
            let boundary = match self.shared.plan_reviews.resolve(&delivery.review_id, action) {
                Ok(boundary) => boundary,
                Err(error) if feedback_consumed => return Outcome::InDoubt(error),
                Err(error) => return Outcome::Held(error),
            };
            match boundary.await {
                Ok(DeliveryBoundary::Acknowledged) => {}
                Ok(DeliveryBoundary::InDoubt(error)) => return Outcome::InDoubt(error),
                Err(_) => {
                    return Outcome::InDoubt("ACP review response ended without a prompt acknowledgement".into());
                }
            }

            if !needs_prompt {
                return Outcome::Acknowledged;
            }
            return match self.prompt_with(services, &prompt, None, None, Vec::new(), true).await {
                Ok(()) => Outcome::Acknowledged,
                Err(error) if !self.is_alive() => Outcome::InDoubt(error),
                Err(error) => Outcome::Held(error),
            };
        }

        // There is no recoverable JSON-RPC response sender after restart. An
        // explicit user continuation instead delivers the immutable envelope
        // to the resumed ACP session as a normal prompt. Never do this from a
        // startup pump: callers reach it only from decide/continue commands.
        if self.shared.turn.lock().is_ok_and(|turn| turn.is_some()) {
            return Outcome::Held("the ACP session is busy with another turn".into());
        }
        let outcome = match self.prompt_with(services, &prompt, None, None, Vec::new(), true).await {
            Ok(()) => Outcome::Acknowledged,
            Err(error) if !self.is_alive() => Outcome::InDoubt(error),
            Err(error) => Outcome::Held(error),
        };
        if matches!(outcome, Outcome::Acknowledged) {
            let pool = services.db.clone();
            let turn_id = delivery.submitting_turn_id;
            let written = tokio::task::spawn_blocking(move || {
                let mut conn = get_conn(&pool)?;
                crate::db::ops::turn::finish_waiting_review(&mut conn, &turn_id, TurnStatus::Done, None, now_ms())
                    .map_err(|error| error.to_string())
            })
            .await;
            match written {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => tracing::warn!(%error, "could not settle resumed ACP review turn"),
                Err(error) => tracing::warn!(%error, "settling resumed ACP review turn panicked"),
            }
        }
        outcome
    }

    /// Put a message into the turn that is already running.
    ///
    /// Returns what the agent did with it, and the caller has to look: only
    /// [`SteerOutcome::PromptRequired`] means the message was not taken, and it
    /// is the one answer that is safe to retry.
    ///
    /// The transcript row is *not* written here. It is owed to the turn's next
    /// round boundary — see [`TurnState::interjected`] — because the chain has
    /// two loose ends anywhere else and a row landing between them forks it.
    pub async fn steer(&self, queue_id: &str, text: &str) -> Result<protocol::SteerOutcome, String> {
        if !self.steering {
            return Err("this adapter does not support steering".into());
        }
        let params = serde_json::to_value(protocol::SteerParams::text(self.acp_session_id.clone(), text))
            .map_err(|e| e.to_string())?;

        // **Before the send, and taken back after.** The other order loses the
        // row outright in a window that is not hypothetical: the agent decides
        // whether to inject the moment the request arrives, and the turn can
        // reach its ending before the reply gets back here — at which point
        // `finish` has taken the turn state and a message the agent has already
        // absorbed has nowhere left to be written.
        self.remember_interjection(queue_id, text);

        let answered = match self.peer.request(protocol::STEER_METHOD, params).await {
            Ok(value) => value,
            // The agent refusing, or the pipe failing. Neither says the message
            // landed, and the item stays in doubt — where the ledger, not the
            // transcript, is what carries the text forward. A row here would
            // contradict the very report that is about to be made about it.
            Err(e) => {
                self.forget_interjection(queue_id);
                return Err(describe(&self.peer, e));
            }
        };

        // An unreadable reply is not a failure to deliver: the agent answered,
        // and every outcome it can name except one means the message landed.
        // Reading it as an error would put the item in doubt over a field this
        // build does not recognise.
        let outcome = match serde_json::from_value::<protocol::SteerResult>(answered) {
            Ok(result) => result.outcome(),
            Err(e) => {
                tracing::debug!(error = %e, "could not read the reply to a steer");
                protocol::SteerOutcome::Unknown("unreadable".into())
            }
        };

        if outcome == protocol::SteerOutcome::PromptRequired {
            self.forget_interjection(queue_id);
        }
        Ok(outcome)
    }

    fn remember_interjection(&self, queue_id: &str, text: &str) {
        if let Ok(mut slot) = self.shared.turn.lock()
            && let Some(state) = slot.as_mut()
        {
            state.interjected.push(Interjection {
                queue_id: queue_id.to_string(),
                text: text.to_string(),
            });
        }
    }

    fn forget_interjection(&self, queue_id: &str) {
        if let Ok(mut slot) = self.shared.turn.lock()
            && let Some(state) = slot.as_mut()
        {
            state.interjected.retain(|i| i.queue_id != queue_id);
        }
    }

    /// Every knob the agent exposes, as it last described them.
    pub fn config_options(&self) -> Vec<protocol::SessionConfigOption> {
        self.shared.config_options()
    }

    /// Set one of them.
    ///
    /// The reply carries the whole set back rather than the one option, because
    /// changing one reshapes others — picking a model re-derives which modes
    /// are available — so it is merged in exactly like a notification.
    pub async fn set_config_option(
        &self,
        config_id: &str,
        value: serde_json::Value,
    ) -> Result<Vec<protocol::SessionConfigOption>, String> {
        let params = serde_json::to_value(protocol::SetConfigOptionParams {
            session_id: self.acp_session_id.clone(),
            config_id: config_id.to_string(),
            value,
        })
        .map_err(|e| e.to_string())?;

        let answered = self
            .peer
            .request("session/set_config_option", params)
            .await
            .map_err(|e| describe(&self.peer, e))?;

        // A reply this app cannot read is not a failure to set: the agent said
        // yes. The notification that follows carries the same set, so the
        // option list catches up either way.
        match serde_json::from_value::<protocol::SetConfigOptionResult>(answered) {
            Ok(result) if !result.config_options.is_empty() => {
                self.shared.merge_config(result.config_options);
            }
            Ok(_) => {}
            Err(e) => tracing::debug!(error = %e, "could not read the reply to session/set_config_option"),
        }
        Ok(self.shared.config_options())
    }

    /// Send one prompt and run it to completion.
    ///
    /// Holds a turn lease for the whole of it, so the conversation shows as
    /// busy, the stop button reaches this turn, and nothing else can write to
    /// the conversation underneath it.
    ///
    /// `turn_id` comes from the caller for the same reason `chat` takes one: the
    /// composer locks on the id the moment the user presses send, and the stop
    /// event it is waiting for has to carry that same id. An id minted here
    /// would not exist until the adapter had been reached, and everything
    /// arriving in the gap would be measured against nothing.
    pub async fn prompt(&self, services: &Services, text: &str, turn_id: Option<String>) -> Result<(), String> {
        self.prompt_with(services, text, turn_id, None, Vec::new(), false).await
    }

    /// The same turn with user-selected workspace snapshots. `text` remains
    /// the clean transcript body; `context` is committed beside its row and is
    /// appended only to the ACP payload.
    pub async fn prompt_with_context(
        &self,
        services: &Services,
        text: &str,
        turn_id: Option<String>,
        context: Vec<crate::workspace::reference::PreparedContextItem>,
    ) -> Result<(), String> {
        self.prompt_with(services, text, turn_id, None, context, false).await
    }

    /// Deliver a queued item as a turn of its own.
    ///
    /// The item is settled in the same transaction that writes its message row
    /// and its turn row, and there is no in-doubt window here for the same
    /// reason a native turn has none: what happens after that transaction is a
    /// *recorded turn*, which either answers or is written down as having
    /// failed. Marking it in doubt instead would warn the next agent about a
    /// message sitting in plain sight a few rows above.
    pub async fn deliver_queued(&self, services: &Services, item: &QueuedPromptRow) -> Result<(), String> {
        let pool = services.db.clone();
        let queue_id = item.id.clone();
        let context = tokio::task::spawn_blocking(move || {
            let mut conn = get_conn(&pool)?;
            crate::db::ops::queued_prompt_context_item::list_prepared(&mut conn, &queue_id).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())??;
        self.prompt_with(services, &item.content, None, Some(&item.id), context, false)
            .await
    }

    async fn prompt_with(
        &self,
        services: &Services,
        text: &str,
        turn_id: Option<String>,
        queued: Option<&str>,
        context: Vec<crate::workspace::reference::PreparedContextItem>,
        bypass_plan_review_barrier: bool,
    ) -> Result<(), String> {
        // Usually the coordinator lease is the whole concurrency guard. A
        // waiting_review turn has intentionally released that lease while its
        // ACP request is still alive, so the session-local state is the second
        // half: no new prompt may overwrite the parked turn before its durable
        // decision has crossed the adapter boundary.
        if self.shared.turn.lock().is_ok_and(|turn| turn.is_some()) {
            return Err("Claude Code is still finishing the previous plan review decision.".into());
        }
        let turn_id = match turn_id {
            Some(raw) => uuid::Uuid::parse_str(&raw)
                .map_err(|_| "turn id must be a uuid".to_string())?
                .to_string(),
            None => uuid::Uuid::new_v4().to_string(),
        };
        // The peer's drop counter never resets, so what this turn lost is the
        // difference across it rather than the total.
        let dropped_before = self.peer.dropped_notifications();
        let cancel = CancellationToken::new();
        let mut lease = Some(
            Arc::clone(&services.turns)
                .try_acquire_turn_with(
                    &self.conversation_id,
                    TurnOrigin::ClaudeCode,
                    turn_id.clone(),
                    cancel.clone(),
                )
                .map_err(|busy| busy.to_string())?,
        );
        if !bypass_plan_review_barrier
            && crate::agent::queue::has_plan_review_barrier(services, &self.conversation_id).await?
        {
            return Err(
                "This conversation is waiting for plan review or its continuation. Finish it before sending another ACP prompt."
                    .into(),
            );
        }
        // Subscribed before the prompt is sent. ExitPlanMode can arrive as the
        // adapter's first act, and the pause notification must not be lost in
        // the gap between durable submission and entering the select loop.
        let mut review_pause = self.shared.plan_reviews.subscribe();

        let user_message_id = self
            .write_prompt_row(services, &turn_id, text, queued, &context)
            .await?;

        let assistant_message_id = begin_assistant(
            &services.db,
            &self.conversation_id,
            &turn_id,
            (None, Some(PROVIDER_LABEL)),
            &self.shared.model(),
            Some(&user_message_id),
        )
        .await?;

        // The bridge's window, opened here and shut in `finish`. Before the
        // prompt goes out, because the agent may call a tool as its first act.
        if let Some(bridge) = &self.bridge {
            bridge.begin_turn(&turn_id, Some(&assistant_message_id), cancel.clone());
        }

        if let Ok(mut slot) = self.shared.turn.lock() {
            *slot = Some(TurnState {
                turn_id: turn_id.clone(),
                cancel: cancel.clone(),
                row: OpenRow::new(assistant_message_id.clone()),
                // The question. Every row this turn writes chains from it.
                parent: user_message_id.clone(),
                interjected: Vec::new(),
                error_notice: None,
            });
        }

        self.shared.emit(ChatStreamEvent::MessageStart {
            message_id: assistant_message_id,
            turn_id: turn_id.clone(),
            conversation_id: self.conversation_id.clone(),
        });

        // Read after the turn record exists, so `asking` can exclude it, and
        // sent in front of the message rather than stored: this is background
        // the agent needs for *this* answer, not something anybody said.
        let owed = self.owed_explanations(services, &turn_id).await?;
        let payload_text = prompt_with_workspace_context(text, &context);
        let params = serde_json::to_value(protocol::PromptParams {
            session_id: self.acp_session_id.clone(),
            prompt: vec![protocol::ContentBlock::text(owed.in_front_of(&payload_text))],
        })
        .map_err(|e| e.to_string())?;

        // No timeout: a turn legitimately runs for as long as the work takes,
        // and the stop button is the bound.
        //
        // Stopping does **not** abandon this request. `session/cancel` is a
        // notification, and the spec has the agent answer the prompt it
        // interrupts with `stopReason: cancelled` — so the reply still comes,
        // and waiting for it is what lets the turn end down the ordinary path
        // with whatever text had already been written. Dropping the future here
        // instead would leave the adapter mid-turn with nobody reading, and the
        // next prompt would collide with it.
        let prompt = self.peer.request("session/prompt", params);
        tokio::pin!(prompt);
        // Already stopped before the request was ever polled. Sending it now
        // and cancelling afterwards is a race nobody wins: the cancel would
        // reach an adapter with no turn to cancel and be ignored, and the
        // prompt behind it would then run to completion with `cancel_sent`
        // blocking any second attempt. Not sending it at all is both cheaper
        // and the thing the user asked for — the turn is written up as
        // cancelled by `finish`, down the ordinary path.
        //
        // `NeverSent` is the whole of what makes that safe. The reply below is
        // written here rather than by the adapter, so it is not evidence that
        // anything was read — see [`PromptDelivery`].
        if cancel.is_cancelled() {
            tracing::debug!(
                conversation_id = %self.conversation_id,
                "the turn was stopped before its prompt went out"
            );
            return self
                .finish(
                    services,
                    &turn_id,
                    Ok(serde_json::json!({ "stopReason": "cancelled" })),
                    lease,
                    dropped_before,
                    owed,
                    PromptDelivery::NeverSent,
                )
                .await;
        }
        let mut cancel_sent = false;
        let outcome = loop {
            tokio::select! {
                // **Biased, so the prompt is polled first.** `request` is lazy
                // — nothing is written until its first poll — and with a random
                // order a token cancelled a moment ago can win the very first
                // pass, putting `session/cancel` on the wire ahead of the
                // prompt it is meant to stop. The check above covers a stop
                // that arrived before the loop; this covers one that arrives
                // during it.
                biased;
                result = &mut prompt => break result,
                // The guard is what keeps this from spinning: a cancelled token
                // stays cancelled, so without it this arm would be ready for
                // ever and starve the one that matters.
                _ = cancel.cancelled(), if !cancel_sent => {
                    cancel_sent = true;
                    let _ = self.peer.notify(
                        "session/cancel",
                        serde_json::json!({ "sessionId": self.acp_session_id }),
                    ).await;
                }
                changed = review_pause.changed(), if lease.is_some() => {
                    if changed.is_ok()
                        && review_pause.borrow().as_deref() == Some(turn_id.as_str())
                    {
                        // SQLite is already at waiting_review. Releasing this
                        // lease is what makes the boundary survive as durable
                        // state rather than as a runner that merely happens to
                        // be blocked on a person.
                        drop(lease.take());
                    }
                }
            }
        };

        // Before `finish`, which takes the state the updates are recorded into.
        // The reply and the updates travel by different routes and the reply is
        // the faster one, so the last few `session/update`s of a turn are
        // routinely still queued at this point — the tool result and the
        // closing sentence among them. See `Peer::drain_notifications`.
        self.peer.drain_notifications().await;

        self.finish(
            services,
            &turn_id,
            outcome,
            lease,
            dropped_before,
            owed,
            PromptDelivery::Sent,
        )
        .await
    }

    /// What this session still owes the agent an explanation for.
    ///
    /// Two ledgers, one message. A hosted session has never carried either:
    /// `load_block` was called from the desktop path alone, so a Claude Code
    /// conversation whose app was killed mid-tool started its next turn as if
    /// nothing had happened — which is the case the warning exists for, since
    /// the adapter's own memory of that turn died with the process while
    /// whatever the tool did to the disk did not.
    async fn owed_explanations(&self, services: &Services, turn_id: &str) -> Result<Owed, String> {
        Ok(Owed {
            turns: crate::agent::interrupted::load_block(&services.db, &services.turns, &self.conversation_id, turn_id)
                .await?,
            queued: crate::agent::queue::owed(services, &self.conversation_id).await,
            shell: self.pending_shell_context(services).await?,
            // Read, not taken. A turn can assemble this and then die before a
            // byte leaves; clearing it here would spend the one chance to say
            // it on a prompt nobody received.
            memory_lost: self.shared.memory_lost.lock().is_ok_and(|slot| *slot),
            tools_lost: self.shared.tools_lost.lock().is_ok_and(|slot| *slot),
        })
    }

    /// Shell results on the active branch that this hosted session has never
    /// received. Read, not taken: only an adapter reply settles the receipt, so
    /// a pipe failure or a stop before first poll leaves them for the next
    /// prompt instead of spending them on nobody.
    async fn pending_shell_context(&self, services: &Services) -> Result<Option<PendingShellContext>, String> {
        let pool = services.db.clone();
        let conversation_id = self.conversation_id.clone();
        tokio::task::spawn_blocking(move || -> Result<Option<PendingShellContext>, String> {
            let mut conn = get_conn(&pool)?;
            let conversation = crate::db::ops::conversation::get_conversation(&mut conn, &conversation_id)
                .map_err(|e| e.to_string())?;
            let history =
                crate::db::ops::message::list_messages(&mut conn, &conversation_id).map_err(|e| e.to_string())?;
            let context = crate::db::ops::message::active_context(&history, conversation.head_message_id.as_deref());
            let message_ids = context.path.iter().map(|row| row.id.clone()).collect::<Vec<_>>();
            let mut by_message = crate::db::ops::message_context_item::list_for_messages(&mut conn, &message_ids)
                .map_err(|e| e.to_string())?;

            // Match native context construction: a denied sandbox attempt and
            // its approved host retry are both retained for diagnosis, but only
            // the final attempt is evidence for the next model turn.
            let mut candidates = Vec::new();
            for message in &context.path {
                let Some(items) = by_message.remove(&message.id) else {
                    continue;
                };
                for item in &items {
                    crate::workspace::reference::MessageContextKind::parse(&item.kind)?;
                }
                if let Some(item) = items
                    .into_iter()
                    .filter(|item| item.kind == "shell_output")
                    .max_by_key(|item| item.position)
                {
                    candidates.push(item);
                }
            }
            let ids = candidates.iter().map(|item| item.id.clone()).collect::<Vec<_>>();
            let delivered =
                crate::db::ops::acp_context_delivery::delivered(&mut conn, &ids).map_err(|e| e.to_string())?;
            candidates.retain(|item| !delivered.contains(&item.id));
            if candidates.is_empty() {
                return Ok(None);
            }

            bounded_pending_shell_context(&candidates)
        })
        .await
        .map_err(|error| format!("ACP shell-context load task failed: {error}"))?
    }

    /// Write the user's row and the turn record — and, when this prompt came
    /// off the queue, settle the item too — in one transaction.
    ///
    /// All or none: a turn row without its message is a run that reports
    /// progress on nothing, a message without its turn row is invisible to
    /// startup reconciliation, and a queue item settled without either is one
    /// that has been consumed and produced nothing.
    async fn write_prompt_row(
        &self,
        services: &Services,
        turn_id: &str,
        text: &str,
        queued: Option<&str>,
        context: &[crate::workspace::reference::PreparedContextItem],
    ) -> Result<String, String> {
        use crate::db::models::message::MessageInsert;
        use diesel::Connection;

        let pool = services.db.clone();
        let conversation_id = self.conversation_id.clone();
        let turn_id = turn_id.to_string();
        let message_id = uuid::Uuid::new_v4().to_string();
        let returned = message_id.clone();
        let content = text.to_string();
        let queued = queued.map(str::to_string);
        let context = context.to_vec();

        tokio::task::spawn_blocking(move || {
            let mut conn = get_conn(&pool)?;
            let now = now_ms();
            conn.transaction::<_, diesel::result::Error, _>(|conn| {
                let head = crate::db::ops::conversation::get_conversation(conn, &conversation_id)
                    .ok()
                    .and_then(|c| c.head_message_id);

                crate::db::ops::message::append_message(
                    conn,
                    &MessageInsert {
                        id: &message_id,
                        conversation_id: &conversation_id,
                        role: "user",
                        content: &content,
                        provider_id: None,
                        model_id: None,
                        input_tokens: None,
                        output_tokens: None,
                        tool_calls: None,
                        tool_call_id: None,
                        sort_order: 0,
                        created_at: now,
                        reasoning_content: None,
                        rating: None,
                        schema_version: 2,
                        is_compact_summary: 0,
                        sender_id: None,
                        parent_id: head.as_deref(),
                        compact_anchor_id: None,
                        source: None,
                        turn_id: Some(&turn_id),
                        tool_outcome: None,
                        cache_read_tokens: None,
                        cache_write_tokens: None,
                        server_tool_calls: None,
                        provider_name: None,
                    },
                    head.as_deref(),
                )?;

                let context_rows = context
                    .iter()
                    .enumerate()
                    .map(
                        |(position, item)| crate::db::models::message_context_item::MessageContextItemInsert {
                            id: &item.id,
                            message_id: &message_id,
                            position: position as i32,
                            kind: item.kind.as_str(),
                            content: &item.content,
                            display_path: item.display_path.as_deref(),
                            line_start: item.line_start,
                            line_end: item.line_end,
                            content_hash: &item.content_hash,
                            byte_count: item.byte_count,
                            line_count: item.line_count,
                            token_count: item.token_count,
                            truncated: item.truncated,
                            metadata: item.metadata.as_deref(),
                            created_at: now,
                        },
                    )
                    .collect::<Vec<_>>();
                crate::db::ops::message_context_item::insert_many(conn, &context_rows)?;

                crate::db::ops::turn::begin(conn, &turn_id, &conversation_id, TurnOrigin::ClaudeCode, None, now)?;
                if let Some(queued) = &queued {
                    // Refuses an item somebody has already taken, and rolls the
                    // whole thing back rather than writing a second row for it.
                    // The turn lease makes that all but impossible — two pumps
                    // cannot both hold the conversation — and "all but" is the
                    // wrong guarantee for a message that says "delete the old
                    // migration".
                    // `None`: this is a turn of its own, and with nothing
                    // running every mode is deliverable — an `interject` left
                    // over from a turn that has ended is still the next thing
                    // the user meant to happen.
                    if crate::db::ops::queue::mark_dispatched(conn, &conversation_id, queued, None, &turn_id, now)? == 0
                    {
                        return Err(diesel::result::Error::RollbackTransaction);
                    }
                    crate::db::ops::queue::mark_settled(conn, queued, Some(&message_id), now)?;
                    crate::db::ops::queued_prompt_context_item::delete_for_queue(conn, queued)?;
                }
                Ok(())
            })
            .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())??;

        Ok(returned)
    }

    /// Land the transcript and report how the turn ended.
    ///
    /// Runs on every path out of `prompt`, success or not: a turn that failed
    /// half way still wrote text worth keeping, and the row it wrote is already
    /// on screen.
    async fn finish(
        &self,
        services: &Services,
        turn_id: &str,
        outcome: Result<serde_json::Value, PeerError>,
        lease: Option<crate::turn::TurnLease>,
        dropped_before: u64,
        owed: Owed,
        sent: PromptDelivery,
    ) -> Result<(), String> {
        // Before anything else can return early. The evidence is a reply the
        // *adapter* sent: `session/prompt` answering at all means it took the
        // prompt, and the prompt is where this was written. A `cancelled` stop
        // reason still means it was read — the user stopped the work, not the
        // reading — while an `Err` is a pipe that may have closed before the
        // request went out, and leaves both ledgers owing.
        //
        // `sent` is the half the outcome cannot carry, and getting it from the
        // outcome alone was a real defect: see [`PromptDelivery`].
        if sent.read_by(&outcome) && !owed.is_empty() {
            owed.settle(services, &self.shared).await;
        }

        // Above the early return below, because the window has to shut on every
        // path out of a turn and the one where the state is already gone is the
        // one most likely to leave something running. Keyed on the turn id, so
        // this cannot shut the *next* turn's window if it lands late — and
        // cheap for a session that has no bridge.
        if let Some(bridge) = &self.bridge {
            bridge.end_turn(turn_id);
        }

        let state = self.shared.turn.lock().ok().and_then(|mut slot| slot.take());
        let Some(state) = state else {
            drop(lease);
            self.retire_approvals(services, turn_id);
            return Err("the ACP turn lost its state".into());
        };
        // Present only after a durable review decision was handed back to the
        // adapter. A process dying while the review is still pending has no
        // boundary here, so the waiting_review row is deliberately left
        // untouched for restart recovery.
        let review_boundary = self.shared.plan_reviews.take_boundary(turn_id);

        // Before anything else, and on every path out of a turn. A question can
        // still be on screen when the turn ends — the adapter died with one
        // outstanding, or the reply came back an error — and nothing else will
        // ever answer it: the task waiting on it is parked, the entry stays in
        // the register, `all_pending_approvals` keeps handing it out, and a
        // reload draws a card for a turn that stopped minutes ago.
        //
        // Cancelling first is what releases that task, which then removes its
        // own entry; the sweep below is for whatever it did not reach. The
        // desktop does the same two things in `TurnGuard::drop`, for the same
        // reason, and this path had neither.
        state.cancel.cancel();
        self.retire_approvals(services, turn_id);

        // The last round. Earlier ones were written as they closed, each by the
        // prose that opened the next — so this is only ever the tail.
        //
        // Skipped when the row has nothing on it, which happens on exactly one
        // path: the next round was opened and then failed to, leaving a carried
        // id that already holds the previous round's answer. Completing it again
        // would replace that answer with nothing.
        let last_row = state.row;
        let last = if last_row.written {
            state.parent.clone()
        } else {
            self.shared.write_row(turn_id, &state.parent, &last_row).await
        };
        // Whatever was steered into the final round, which has no boundary of
        // its own to land at. Owed even on a failed turn: the agent took the
        // message, so the transcript has to show it was said.
        self.shared
            .write_interjections(turn_id, &last, &state.interjected)
            .await;

        // What the reply says about itself, before `stopReason` is believed.
        // Declaring `sessionFailure` at `initialize` changed the shape of a
        // failed prompt: the adapter answers `end_turn` and puts a typed
        // record in `_meta` instead of rejecting the request. Read only
        // `stopReason` and every such failure is a finished turn.
        let air_record = match &outcome {
            Ok(value) => value
                .get("_meta")
                .and_then(|m| serde_json::from_value::<protocol::AirMetaEnvelope>(m.clone()).ok())
                .and_then(|m| m.session_failure().and_then(mapping::notice_of)),
            Err(_) => None,
        };
        // Either the reply's own record, or one that arrived as a
        // `session_info_update` while the turn ran — the `auth_required` case,
        // which the adapter still rejects with a JSON-RPC error and reports
        // beside it. The reply's record wins when both exist: it is the later
        // word.
        let mut notice_error = state.error_notice.clone();
        if let Some(record) = air_record {
            if record.severity == AcpNoticeSeverity::Error {
                notice_error = Some(record.title.clone());
            }
            self.shared.record_notice(record, Some(turn_id.to_string())).await;
        }
        let carried_by_notice = notice_error.is_some();
        let (status, reason, error) = classify(&outcome, notice_error.as_deref());

        // An update the reader could not queue is a piece of this answer that
        // was never written down, and the rows above have already been saved
        // without it. Nothing downstream can tell: the transcript is
        // well-formed, just missing a paragraph or a tool's result.
        //
        // So a turn that lost one does not get to say it finished. Reporting
        // `Done` here is the failure mode with no symptom at all — the user
        // reads a truncated answer as the whole answer. Overriding a real
        // failure would be worse, so this only demotes success.
        //
        // A row the database refused is the same failure arriving from the
        // other side — the answer streamed, and it is not stored — so the two
        // are counted together rather than given separate rules.
        let lost = self.peer.dropped_notifications().saturating_sub(dropped_before);
        let unwritten = self.shared.unwritten_rows.swap(0, std::sync::atomic::Ordering::Relaxed);
        let (status, reason, error) = if (lost > 0 || unwritten > 0) && status == TurnStatus::Done {
            tracing::error!(
                lost,
                unwritten,
                conversation_id = %self.conversation_id,
                "an ACP turn's transcript is incomplete"
            );
            let why = if unwritten > 0 {
                format!("{unwritten} part(s) of this answer could not be saved, so it is incomplete.")
            } else {
                format!("{lost} update(s) from Claude Code were dropped, so this answer is incomplete.")
            };
            (TurnStatus::Failed, ChatStopReason::Error, Some(why))
        } else {
            (status, reason, error)
        };

        if review_boundary.is_some() {
            let pool = services.db.clone();
            let id = turn_id.to_string();
            let stored_error = error.clone();
            let written = tokio::task::spawn_blocking(move || {
                let mut conn = get_conn(&pool)?;
                crate::db::ops::turn::finish_waiting_review(&mut conn, &id, status, stored_error.as_deref(), now_ms())
                    .map_err(|error| error.to_string())
            })
            .await;
            match written {
                Ok(Ok(1)) => {}
                Ok(Ok(_)) => tracing::warn!(turn_id, "ACP review decision found no waiting turn to settle"),
                Ok(Err(error)) => tracing::warn!(%error, turn_id, "could not settle ACP waiting review turn"),
                Err(error) => tracing::warn!(%error, turn_id, "settling ACP waiting review turn panicked"),
            }
        } else {
            crate::agent::turn_record::finish(&services.db, turn_id, status, error.as_deref()).await;
        }

        self.shared.emit(ChatStreamEvent::Stop {
            reason,
            message_id: Some(last_row.message_id),
            turn_id: turn_id.to_string(),
            conversation_id: self.conversation_id.clone(),
            input_tokens: Some(0),
            output_tokens: Some(0),
        });
        // The sidebar refetches on this; without it the conversation's preview
        // and timestamp stay at whatever they were before the turn.
        let _ = services.events.emit_conversation_updated(&self.conversation_id);

        drop(lease);

        // Now, and not before: the queue's next item wants a turn of its own,
        // and the lease it needs is the one that has just been dropped.
        //
        // A turn that did not reach an ending stops the queue instead. The
        // instructions behind a failure rest on the step that failed — "now
        // rename that function" means nothing if the function was never
        // created — so what happens next is a person's decision, not ours.
        match status {
            TurnStatus::Done => crate::agent::queue::pump_later(services, &self.conversation_id),
            _ => crate::agent::queue::hold(services, &self.conversation_id).await,
        }

        if let Some(boundary) = review_boundary {
            boundary.complete(match &outcome {
                // An answer to session/prompt is the first protocol evidence
                // that the adapter consumed our permission response. Its stop
                // reason may still describe a failed turn; delivery happened.
                Ok(_) => super::plan_review::DeliveryBoundary::Acknowledged,
                Err(error) => super::plan_review::DeliveryBoundary::InDoubt(error.to_string()),
            });
        }

        // A failure the adapter reported as a typed record is already in the
        // transcript, durably, with its category and what to do about it. The
        // composer's rejection path draws a transient bubble from the same
        // text, so that one is told the turn ended and nothing more.
        match outcome {
            // The caller hears about lost updates too. `acp_send` is awaited by
            // the composer, and a rejection is what unlocks it with an error
            // rather than with a tick.
            Ok(_) => match error {
                Some(e) if !carried_by_notice => Err(e),
                _ => Ok(()),
            },
            Err(_) if carried_by_notice => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Drop every question this turn left unanswered.
    ///
    /// Keyed by turn rather than by conversation: a later turn in the same
    /// conversation may already have questions of its own outstanding, and
    /// clearing those would strand *it* instead.
    fn retire_approvals(&self, services: &Services, turn_id: &str) {
        // Through `approval::retire_turn` rather than a bare `retain`, so a
        // question already claimed by its own deadline is not accounted for a
        // second time here.
        let retired = crate::approval::retire_turn(services, turn_id, crate::approval::RetireCause::TurnGone);
        if retired > 0 {
            tracing::debug!(
                retired,
                turn_id,
                conversation_id = %self.conversation_id,
                "dropped approvals nobody was left to answer"
            );
        }
    }

    /// Stop whatever this session is doing, without closing it.
    pub async fn cancel(&self) {
        if let Ok(slot) = self.shared.turn.lock()
            && let Some(state) = slot.as_ref()
        {
            state.cancel.cancel();
        }
        let _ = self
            .peer
            .notify(
                "session/cancel",
                serde_json::json!({ "sessionId": self.acp_session_id }),
            )
            .await;
    }

    /// End the session and the process behind it.
    pub async fn close(&self) {
        self.cancel().await;
        self.peer.stop().await;
        // With it, not after it. The bridge is a listening port and a task, and
        // both are meaningless once the agent that was given the address is
        // gone — what would be left is an open endpoint onto a conversation
        // nothing is answering.
        if let Some(bridge) = &self.bridge {
            bridge.stop();
        }
    }
}

/// What goes in `mcpServers`, for whichever way a session is being opened.
///
/// Empty when there is no bridge, which the field requires anyway: the spec
/// makes it mandatory even when there is nothing in it.
fn advertised(bridge: Option<&Arc<bridge::Bridge>>) -> Vec<serde_json::Value> {
    bridge.map(|b| vec![b.descriptor()]).unwrap_or_default()
}

/// The directory to tell the agent about, which is not always the one this app
/// holds.
///
/// An adapter launched with `docker run -v C:\work\repo:/repo …` is on the far
/// side of a wall: `C:\work\repo` names nothing it can reach. Sending it
/// anyway is not a subtle failure — `session/new` refuses a directory that does
/// not exist, so a containerised adapter simply never opens a session, and the
/// message says the path is wrong rather than that it is in the wrong
/// coordinate system.
///
/// **The mounts come from the command the user already wrote.** A mount list
/// configured beside it is one that can disagree with it, and the disagreement
/// looks exactly like this failure. See [`bridge::mounts`].
///
/// A path that maps nowhere is left alone rather than guessed at: the adapter's
/// own error about a directory it cannot find is more use than one this app
/// invented, and an empty map — every non-container adapter — is the identity.
///
/// [`bridge::mounts`]: super::mounts
fn cwd_for_agent(cwd: &str, mounts: &super::mounts::MountMap) -> String {
    if mounts.is_empty() {
        return cwd.to_string();
    }
    match mounts.to_container(std::path::Path::new(cwd)) {
        Some(inside) => {
            tracing::debug!(inside = %inside, "translated the working directory for a containerised adapter");
            inside
        }
        None => {
            tracing::warn!(
                "this conversation's directory is not inside any of the adapter's mounts; \
                 sending it unchanged, which the agent will probably refuse"
            );
            cwd.to_string()
        }
    }
}

/// The two openers' parameters, built where a test can reach them.
///
/// **There are two ways into a session and they are easy to get out of step.**
/// A resumed session builds its query through `session/load`, so a bridge
/// advertised only at `session/new` gives a conversation its tools until the
/// app is next restarted and none afterwards — a feature that appears to break
/// itself overnight, on a path nothing else exercises.
///
/// These are free functions rather than the closures they replaced because a
/// test asserting on `advertised` alone proves nothing about either caller:
/// mutating `session/load` back to `Vec::new()` left such a test green. The
/// seam has to be where the parameters are actually assembled.
fn new_session_params(
    cwd: &str,
    bridge: Option<&Arc<bridge::Bridge>>,
    meta: Option<protocol::SessionMeta>,
    mounts: &super::mounts::MountMap,
) -> protocol::NewSessionParams {
    protocol::NewSessionParams {
        cwd: cwd_for_agent(cwd, mounts),
        mcp_servers: advertised(bridge),
        meta,
    }
}

fn load_session_params(
    resume: &str,
    cwd: &str,
    bridge: Option<&Arc<bridge::Bridge>>,
    meta: Option<protocol::SessionMeta>,
    mounts: &super::mounts::MountMap,
) -> protocol::LoadSessionParams {
    protocol::LoadSessionParams {
        session_id: resume.to_string(),
        cwd: cwd_for_agent(cwd, mounts),
        mcp_servers: advertised(bridge),
        meta,
    }
}

/// Turn a peer failure into something worth showing a user.
///
/// A dead peer's message already carries the adapter's own stderr; an RPC
/// refusal does not, and the adapter's last words are usually the whole
/// explanation ("not logged in", "no such directory").
fn describe(peer: &Peer, error: PeerError) -> String {
    match error {
        PeerError::Dead(m) => m,
        PeerError::Rpc(m) if peer.is_alive() => m,
        PeerError::Rpc(m) => format!("{m} (the adapter has since stopped)"),
    }
}

/// Fold a freshly described set of config options into the one being held.
///
/// Free of the session so the rule can be stated on its own, because it is not
/// the obvious one: an option in an update may carry only a new `currentValue`
/// and omit the values it accepts. It is reporting a change, not redefining the
/// knob. Replacing wholesale — or even replacing one option wholesale — empties
/// the picker at the exact moment somebody is using it.
fn merge_options(held: &mut Vec<protocol::SessionConfigOption>, incoming: Vec<protocol::SessionConfigOption>) {
    for option in incoming {
        match held.iter_mut().find(|o| o.id == option.id) {
            Some(existing) => {
                // Keep what the update did not restate.
                let previous = std::mem::take(&mut existing.options);
                let keep_previous = option.options.is_empty();
                *existing = option;
                if keep_previous {
                    existing.options = previous;
                }
            }
            // A knob that did not exist a moment ago. Agents add them when a
            // model changes, so this is ordinary rather than exceptional.
            None => held.push(option),
        }
    }
}

/// How a turn ended, from the reply to `session/prompt` and from what the
/// adapter said about the turn while it ran.
///
/// `notice_error` is the title of an `error`-severity AIR incident recorded
/// for this turn — off the reply's own `_meta`, or off a `session_info_update`
/// that arrived mid-turn — and it wins over the reply: with `sessionFailure`
/// declared, the adapter reports a failed prompt as `stopReason: end_turn`
/// with the record beside it, so the stop reason alone reads a failure as a
/// finished turn. A warning is not passed here; it changes nothing.
///
/// Pure, and separate from `finish`, so the four cases can be pinned without a
/// database: a typed failure on a clean reply, a warning on one, a cancel with
/// a warning, and a JSON-RPC rejection with a session-scoped record beside it.
fn classify(
    outcome: &Result<serde_json::Value, PeerError>,
    notice_error: Option<&str>,
) -> (TurnStatus, ChatStopReason, Option<String>) {
    if let Some(title) = notice_error {
        return (TurnStatus::Failed, ChatStopReason::Error, Some(title.to_string()));
    }
    match outcome {
        Ok(value) => match value.get("stopReason").and_then(|v| v.as_str()) {
            Some(raw) => match ChatStopReason::try_from(raw) {
                Ok(reason @ ChatStopReason::Cancelled) => (TurnStatus::Cancelled, reason, None),
                Ok(reason @ (ChatStopReason::Error | ChatStopReason::LoopDetected)) => {
                    (TurnStatus::Failed, reason, None)
                }
                Ok(reason) => (TurnStatus::Done, reason, None),
                Err(error) => (TurnStatus::Failed, ChatStopReason::Error, Some(error)),
            },
            None => (
                TurnStatus::Failed,
                ChatStopReason::Error,
                Some("ACP prompt response is missing `stopReason`".to_string()),
            ),
        },
        Err(e) => (TurnStatus::Failed, ChatStopReason::Error, Some(e.to_string())),
    }
}

#[cfg(test)]
mod classify_tests {
    use super::*;

    fn ended(stop: &str) -> Result<serde_json::Value, PeerError> {
        Ok(serde_json::json!({ "stopReason": stop }))
    }

    /// The whole reason `sessionFailure` had to be read and not merely
    /// declared: a typed failure arrives on an `end_turn`.
    #[test]
    fn a_typed_failure_on_a_clean_reply_fails_the_turn_with_its_title() {
        let (status, reason, error) = classify(&ended("end_turn"), Some("Rate limit reached."));
        assert_eq!(status, TurnStatus::Failed);
        assert_eq!(reason, ChatStopReason::Error);
        assert_eq!(error.as_deref(), Some("Rate limit reached."));
    }

    /// A warning is not an error and never reaches `classify`; the reply
    /// stands on its own.
    #[test]
    fn a_reply_with_no_error_notice_is_read_off_its_stop_reason() {
        assert_eq!(classify(&ended("end_turn"), None).0, TurnStatus::Done);
        assert_eq!(classify(&ended("cancelled"), None).0, TurnStatus::Cancelled);
        assert_eq!(classify(&ended("max_tokens"), None).0, TurnStatus::Done);
        let (status, _, error) = classify(&Ok(serde_json::json!({})), None);
        assert_eq!(status, TurnStatus::Failed);
        assert!(error.unwrap().contains("stopReason"));
    }

    /// The `auth_required` shape: the adapter still rejects, and the typed
    /// record that arrived beside the rejection is the better message.
    #[test]
    fn a_rejection_with_a_session_scoped_record_reports_the_record() {
        let outcome = Err(PeerError::Rpc("Authentication required".into()));
        let (status, reason, error) = classify(&outcome, Some("Sign in to continue using Claude."));
        assert_eq!(status, TurnStatus::Failed);
        assert_eq!(reason, ChatStopReason::Error);
        assert_eq!(error.as_deref(), Some("Sign in to continue using Claude."));

        let (status, _, error) = classify(&outcome, None);
        assert_eq!(status, TurnStatus::Failed);
        assert!(error.unwrap().contains("Authentication required"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::protocol::{ConfigOptionValue, SessionConfigOption};

    /// The retry logic that `handshake` and `load` inline. Extracted here so
    /// the contract — one attempt with, one without, the right error reported —
    /// can be stated once and tested without a child process or a `Send` bound.
    ///
    /// **Not used outside `#[cfg(test)]`.** An `AsyncFn` closure that captures
    /// references produces a future whose `Send` bound Tauri's macro cannot
    /// satisfy for arbitrary lifetimes, so the two call sites inline this
    /// shape instead. Moving it here keeps it testable without infecting the
    /// shell crate's compilation.
    async fn ask_for_the_thinking<T>(
        conversation_id: &str,
        what_happened: &str,
        open: impl AsyncFn(Option<protocol::SessionMeta>) -> Result<T, String>,
    ) -> Result<T, String> {
        match open(Some(protocol::SessionMeta::default())).await {
            Ok(session) => Ok(session),
            Err(refused) => {
                let plain = open(None).await;
                if plain.is_ok() {
                    tracing::warn!(
                        error = %refused,
                        conversation_id = %conversation_id,
                        "the agent refused the session options; {what_happened} without them, so no thinking will be shown"
                    );
                }
                plain
            }
        }
    }

    fn select(id: &str, current: &str, values: &[&str]) -> SessionConfigOption {
        SessionConfigOption {
            id: id.into(),
            name: id.into(),
            description: None,
            category: Some(id.into()),
            kind: Some("select".into()),
            current_value: Some(serde_json::Value::String(current.into())),
            options: values
                .iter()
                .map(|v| ConfigOptionValue {
                    value: (*v).into(),
                    name: (*v).into(),
                    description: None,
                })
                .collect(),
        }
    }

    /// The whole reason this is a merge.
    ///
    /// An update that reports a new `currentValue` need not restate what the
    /// knob accepts. Taking it at face value would leave the picker with one
    /// entry and no way back — and it would happen on the very update that
    /// follows the user changing the model.
    #[test]
    fn an_update_that_omits_its_values_keeps_the_ones_already_known() {
        let mut held = vec![select("model", "sonnet", &["sonnet", "opus"])];

        let mut narrowed = select("model", "opus", &[]);
        narrowed.options.clear();
        merge_options(&mut held, vec![narrowed]);

        assert_eq!(held.len(), 1);
        assert_eq!(held[0].current_str(), Some("opus"), "the change is taken");
        assert_eq!(
            held[0].options.len(),
            2,
            "and what it may be set to survives: {:?}",
            held[0].options
        );
    }

    /// **An agent that cannot take the options still gets a session.**
    ///
    /// The ask reaches the user's own `claude` as a command-line flag and this
    /// app pins neither the CLI nor the adapter: an unknown option is not
    /// ignored, it exits 1 before the process runs. Insisted on, that would be
    /// no hosted session at all rather than a session without visible thinking —
    /// on every conversation, for a setting nobody chose.
    #[tokio::test]
    async fn a_session_that_cannot_take_the_options_is_opened_without_them() {
        let asked = std::sync::Mutex::new(Vec::new());
        let record = |meta: &Option<protocol::SessionMeta>| asked.lock().unwrap().push(meta.is_some());

        // The old binary: everything is fine except the flag.
        let opened = ask_for_the_thinking("c1", "opened", async |meta| {
            record(&meta);
            match meta {
                Some(_) => Err("unknown option '--thinking-display'".to_string()),
                None => Ok("sess-1"),
            }
        })
        .await;
        assert_eq!(opened, Ok("sess-1"), "the session is worth more than the display");
        assert_eq!(
            &*asked.lock().unwrap(),
            &[true, false],
            "asked once with the options and once without, in that order"
        );

        // The ordinary one: asked once, and never asked again.
        asked.lock().unwrap().clear();
        let opened = ask_for_the_thinking("c1", "opened", async |meta| {
            record(&meta);
            Ok::<_, String>("sess-2")
        })
        .await;
        assert_eq!(opened, Ok("sess-2"));
        assert_eq!(
            &*asked.lock().unwrap(),
            &[true],
            "a working agent must not pay for a second round trip"
        );

        // Not signed in, which has nothing to do with the options. The retry
        // happens anyway — there is no way to tell the two apart from here —
        // but what the user is shown is the failure of the attempt that asked
        // for nothing extra, not one that can be blamed on a flag.
        asked.lock().unwrap().clear();
        let refused = ask_for_the_thinking("c1", "opened", async |meta| {
            record(&meta);
            Err::<&str, _>(match meta {
                Some(_) => "unknown option '--thinking-display'".to_string(),
                None => "not authenticated".to_string(),
            })
        })
        .await;
        assert_eq!(
            refused,
            Err("not authenticated".to_string()),
            "the real reason survives, rather than being masked by the options"
        );
        assert_eq!(&*asked.lock().unwrap(), &[true, false], "and it stops at two");
    }

    /// When an update *does* restate them, it wins — an agent that re-derives
    /// which modes exist for a newly chosen model is telling us the old list is
    /// wrong, and keeping it would offer a mode that no longer applies.
    #[test]
    fn an_update_that_restates_its_values_replaces_them() {
        let mut held = vec![select("mode", "code", &["code", "plan", "bypass"])];
        merge_options(&mut held, vec![select("mode", "code", &["code"])]);

        assert_eq!(held[0].options.len(), 1);
        assert_eq!(held[0].options[0].value, "code");
    }

    /// A hosted prompt is one lump of text, so anything that has to be
    /// explained goes in front of the message rather than beside it — and when
    /// there is nothing to explain, the message is passed through untouched
    /// rather than wrapped in an empty frame.
    #[test]
    fn what_is_owed_goes_in_front_of_the_message_and_nothing_else_does() {
        let plain = Owed::default();
        assert!(plain.is_empty());
        assert_eq!(plain.in_front_of("do the thing"), "do the thing");

        let mut conn = crate::db::test_db().get().unwrap();
        crate::db::ops::conversation::create_conversation(&mut conn, "c1", Some("t"), None, None, 0).unwrap();
        crate::db::ops::turn::begin(&mut conn, "dead", "c1", crate::turn::TurnOrigin::ClaudeCode, None, 1000).unwrap();
        crate::db::ops::turn::set_phase(&mut conn, "dead", TurnPhase::RunningTool, Some("Bash"), 1001).unwrap();

        let owed = Owed {
            turns: crate::agent::interrupted::block(
                &mut conn,
                &crate::turn::TurnCoordinator::new(),
                "c1",
                Some("asking"),
            )
            .unwrap(),
            queued: None,
            shell: None,
            memory_lost: false,
            tools_lost: false,
        };
        assert!(!owed.is_empty(), "a turn killed inside a tool is owed an explanation");

        let sent = owed.in_front_of("carry on");
        assert!(sent.starts_with("<interrupted_turn>"), "{sent}");
        assert!(sent.ends_with("carry on"), "{sent}");
        assert!(
            sent.contains("Bash") && sent.contains("may have taken effect"),
            "a hosted turn caught inside a tool says the dangerous thing, not the mild one: {sent}"
        );

        // And the blindness goes first. The other two describe things that
        // happened inside a conversation the agent is assumed to be following;
        // this one says it is following none of it, which changes how the rest
        // should be read.
        let blind = Owed {
            turns: crate::agent::interrupted::block(
                &mut conn,
                &crate::turn::TurnCoordinator::new(),
                "c1",
                Some("asking"),
            )
            .unwrap(),
            queued: None,
            shell: None,
            memory_lost: true,
            tools_lost: false,
        };
        let sent = blind.in_front_of("carry on");
        assert!(sent.starts_with("<no_session_memory>"), "{sent}");
        assert!(
            sent.find("<no_session_memory>") < sent.find("<interrupted_turn>"),
            "{sent}"
        );
        assert!(sent.ends_with("carry on"));

        // On its own it is still worth saying, and still nothing more than a
        // prefix — the message itself is untouched.
        let alone = Owed {
            memory_lost: true,
            ..Owed::default()
        };
        assert!(!alone.is_empty());
        assert!(alone.in_front_of("hello").ends_with("\n\nhello"));

        let shell = Owed {
            shell: Some(PendingShellContext {
                item_ids: vec!["item-1".into()],
                rendered: "<untrusted_context>\ncommand output\n</untrusted_context>".into(),
            }),
            ..Owed::default()
        };
        assert!(!shell.is_empty());
        let sent = shell.in_front_of("explain the result");
        assert!(sent.starts_with("<untrusted_context>"), "{sent}");
        assert!(sent.ends_with("explain the result"), "{sent}");
    }

    /// A capability that was promised and is missing has to be *said*, not
    /// logged. The inverse of the `hooks/` rule and for a stated reason: a
    /// missed review costs one review, while an agent that cannot see its tools
    /// works around them or claims to have used them.
    #[test]
    fn a_missing_tool_bridge_is_something_the_agent_is_told() {
        let lost = Owed {
            tools_lost: true,
            ..Owed::default()
        };
        assert!(!lost.is_empty(), "a session with no tools has something to say");

        let sent = lost.in_front_of("what do you remember?");
        assert!(sent.contains("<meridian_tools_unavailable>"), "{sent}");
        assert!(sent.ends_with("what do you remember?"), "{sent}");
        // Explicit, because a model told only that something failed tends to
        // apologise for the app rather than get on with the question.
        assert!(sent.contains("Do not claim to have used them"), "{sent}");
    }

    /// And it goes behind the memory notice: that one says the agent cannot see
    /// the conversation at all, which changes how everything after it reads.
    #[test]
    fn the_blindness_notice_still_comes_before_the_tools_one() {
        let both = Owed {
            memory_lost: true,
            tools_lost: true,
            ..Owed::default()
        };
        let sent = both.in_front_of("hello");
        assert!(
            sent.find("<no_session_memory>") < sent.find("<meridian_tools_unavailable>"),
            "{sent}"
        );
    }

    /// A session that never asked for tools is not missing any. An import opens
    /// a session only to read its recital, and a notice there would be an
    /// apology for a capability nobody wanted.
    #[test]
    fn a_session_that_wanted_no_tools_says_nothing_about_them() {
        let quiet = Owed::default();
        assert!(quiet.is_empty());
        assert_eq!(quiet.in_front_of("hello"), "hello");
    }

    /// Both ways of opening a session advertise the same thing.
    ///
    /// The failure this is about does not look like a bug at first: a bridge
    /// advertised at `session/new` and forgotten at `session/load` gives a
    /// conversation its tools until the app restarts and none afterwards.
    #[tokio::test]
    async fn a_resumed_session_advertises_the_same_bridge_as_a_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let services = bare_services(dir.path());
        let bridge = bridge::Bridge::start(services, "c-1", None, dir.path().join("logs"))
            .await
            .expect("the bridge binds");

        // Through the constructors the openers actually use. Asserting on
        // `advertised` instead left this green while `session/load` was mutated
        // back to `Vec::new()` — the assertion was beside the defect.
        let opened = new_session_params("/repo", Some(&bridge), None, &mounts::MountMap::default());
        let resumed = load_session_params("s-1", "/repo", Some(&bridge), None, &mounts::MountMap::default());

        assert_eq!(opened.mcp_servers.len(), 1, "a new session was given no tools");
        assert_eq!(
            opened.mcp_servers, resumed.mcp_servers,
            "a resumed session would have lost its tools"
        );
        assert_eq!(opened.mcp_servers[0]["name"], serde_json::json!("meridian"));

        bridge.stop();
    }

    /// And a session opened without one advertises nothing rather than a
    /// half-filled descriptor. The field is mandatory even when empty.
    /// **What a containerised adapter is told the directory is.**
    ///
    /// Not a display nicety: `session/new` refuses a directory that does not
    /// exist, and `C:\work\repo` does not exist inside the container. Sent
    /// unchanged, the session never opens and the message says the path is
    /// wrong rather than that it is in the wrong coordinate system.
    ///
    /// Through the constructors both openers use, for the reason the bridge
    /// test above records: asserting on the translation helper alone stayed
    /// green while one of the two call sites was mutated away.
    #[test]
    fn a_containerised_adapter_is_told_the_path_it_can_reach() {
        let mounts = mounts::MountMap::from_command(
            "docker",
            &["run", "-v", "C:\\work\\repo:/repo", "img"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
        );

        let opened = new_session_params("C:\\work\\repo", None, None, &mounts);
        assert_eq!(opened.cwd, "/repo");
        let resumed = load_session_params("s-1", "C:\\work\\repo", None, None, &mounts);
        assert_eq!(
            resumed.cwd, "/repo",
            "a resumed session was sent a path the container cannot reach"
        );
    }

    /// The ordinary adapter shares this filesystem, so the translation is the
    /// identity and the host path goes through untouched.
    #[test]
    fn an_ordinary_adapter_is_told_the_host_path() {
        let none = mounts::MountMap::default();
        assert_eq!(
            new_session_params("C:\\work\\repo", None, None, &none).cwd,
            "C:\\work\\repo"
        );
        assert_eq!(
            load_session_params("s-1", "/home/me/repo", None, None, &none).cwd,
            "/home/me/repo"
        );
    }

    /// A directory outside every mount is sent as it stands rather than
    /// guessed at. The adapter's own "no such directory" names the path it
    /// actually looked for, which is more use than one this app invented.
    #[test]
    fn a_directory_outside_the_mounts_is_not_invented() {
        let mounts = mounts::MountMap::from_command(
            "docker",
            &["run", "-v", "/home/me/repo:/repo", "img"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            new_session_params("/somewhere/else", None, None, &mounts).cwd,
            "/somewhere/else"
        );
    }

    #[test]
    fn a_session_with_no_bridge_advertises_an_empty_list() {
        assert!(
            new_session_params("/repo", None, None, &mounts::MountMap::default())
                .mcp_servers
                .is_empty()
        );
        assert!(
            load_session_params("s-1", "/repo", None, None, &mounts::MountMap::default())
                .mcp_servers
                .is_empty()
        );
        let encoded =
            serde_json::to_value(new_session_params("/repo", None, None, &mounts::MountMap::default())).unwrap();
        assert_eq!(encoded["mcpServers"], serde_json::json!([]), "{encoded}");
    }

    /// Only what the update mentions is touched, and a knob it has never
    /// mentioned before is added rather than ignored.
    #[test]
    fn options_the_update_does_not_mention_are_left_alone() {
        let mut held = vec![
            select("model", "sonnet", &["sonnet"]),
            select("mode", "code", &["code"]),
        ];
        merge_options(&mut held, vec![select("effort", "high", &["low", "high"])]);

        assert_eq!(held.len(), 3);
        assert_eq!(held[0].current_str(), Some("sonnet"));
        assert_eq!(held[1].current_str(), Some("code"));
        assert_eq!(held[2].id, "effort");
    }

    /// What an ended turn proves about the explanations it carried.
    ///
    /// The regression this pins is the third row: a turn stopped before its
    /// request was first polled is written up from a reply this app composes
    /// itself, so `outcome.is_ok()` says nothing about whether anything was
    /// read. Settling on it spends the interrupted-turn report, the in-doubt
    /// queue items and the memory-loss notice on an agent that never saw them,
    /// and all three clear exactly once.
    #[test]
    fn only_a_reply_the_adapter_actually_sent_settles_what_is_owed() {
        let ended = |stop: &str| Ok(serde_json::json!({ "stopReason": stop }));
        let died = || Err(PeerError::Dead("the ACP adapter exited with code 1".into()));

        assert!(PromptDelivery::Sent.read_by(&ended("end_turn")));
        // The user stopped the work, not the reading: the prompt went out and
        // came back, so what rode on it was read.
        assert!(PromptDelivery::Sent.read_by(&ended("cancelled")));

        assert!(
            !PromptDelivery::NeverSent.read_by(&ended("cancelled")),
            "a reply this app wrote itself is not evidence about the agent"
        );
        assert!(
            !PromptDelivery::Sent.read_by(&died()),
            "a pipe that closed may have closed before the request went out"
        );
        assert!(!PromptDelivery::NeverSent.read_by(&died()));
    }

    #[test]
    fn current_workspace_snapshot_rides_the_acp_prompt_without_a_live_at_trigger() {
        let context = crate::workspace::reference::PreparedContextItem {
            id: "ctx".into(),
            kind: crate::workspace::reference::MessageContextKind::ProjectFile,
            content: "frozen bytes".into(),
            display_path: Some("src/lib.rs".into()),
            line_start: None,
            line_end: None,
            content_hash: "hash".into(),
            byte_count: 12,
            line_count: 1,
            token_count: 3,
            truncated: 0,
            metadata: None,
        };

        let payload = prompt_with_workspace_context("inspect @src/lib.rs", &[context]);

        assert!(payload.starts_with("inspect `src/lib.rs`"));
        assert!(!payload.contains("@src/lib.rs"));
        assert!(payload.contains("<untrusted_context>"));
        assert!(payload.contains("frozen bytes"));
    }

    #[test]
    fn pending_shell_output_is_placed_before_the_next_acp_prompt() {
        let owed = Owed {
            shell: Some(PendingShellContext {
                item_ids: vec!["item".into()],
                rendered: "<untrusted_context>\ncommand result\n</untrusted_context>".into(),
            }),
            ..Owed::default()
        };

        let payload = owed.in_front_of("next question");

        assert!(payload.starts_with("<untrusted_context>\ncommand result"));
        assert!(payload.ends_with("next question"));
    }

    #[test]
    fn pending_shell_context_batches_items_and_receipts_only_what_was_injected() {
        let item = |id: &str, content: String| crate::db::models::message_context_item::MessageContextItemRow {
            id: id.into(),
            message_id: format!("message-{id}"),
            position: 0,
            kind: "shell_output".into(),
            content,
            display_path: None,
            line_start: None,
            line_end: None,
            content_hash: "hash".into(),
            byte_count: 0,
            line_count: 1,
            token_count: 1,
            truncated: 0,
            metadata: None,
            created_at: 1,
        };
        let repeat = crate::workspace::reference::MAX_MODEL_SHELL_CONTEXT_BYTES;
        let candidates = vec![
            item("first", "FIRST".repeat(repeat)),
            item("deferred", "DEFERRED".repeat(repeat)),
        ];

        let batch = bounded_pending_shell_context(&candidates)
            .unwrap()
            .expect("one item fits");

        assert_eq!(batch.item_ids, vec!["first".to_string()]);
        assert!(batch.rendered.len() <= MAX_ACP_PENDING_SHELL_BYTES);
        assert!(batch.rendered.contains("FIRST"));
        assert!(!batch.rendered.contains("DEFERRED"));

        let small = (0..6)
            .map(|index| item(&format!("item-{index}"), "ok".into()))
            .collect::<Vec<_>>();
        let batch = bounded_pending_shell_context(&small).unwrap().expect("small items fit");
        assert_eq!(
            batch.item_ids,
            vec![
                "item-0".to_string(),
                "item-1".to_string(),
                "item-2".to_string(),
                "item-3".to_string(),
            ]
        );
    }

    use super::super::mounts;
    use crate::services::bare_services;

    fn update(json: serde_json::Value) -> SessionNotification {
        serde_json::from_value(serde_json::json!({ "sessionId": "s", "update": json })).expect("a session update")
    }

    fn call(id: &str, name: &str, args: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": id,
            "_meta": { "claudeCode": { "toolName": name } },
            "rawInput": args,
        })
    }

    /// The repeat announcement that arrives after the round it belonged to has
    /// closed.
    ///
    /// The adapter announces a call twice — once when it knows one is coming,
    /// once when the input has streamed — from two sources that can arrive in
    /// either order. `Call(A) → Result(A) → Call(B)` rotates the round, so by
    /// the time the second `A` lands the open row holds only `B`.
    ///
    /// Deduplicated against that row, as this did, the repeat reads as new: a
    /// second card for `A` that no result will ever close, and a round whose
    /// `results.len() >= tool_calls.len()` can never come true — which is the
    /// test that puts the turn's phase back to `Streaming`, so it sits at
    /// `RunningTool` for the rest of the turn and a crash there is reported as
    /// "a tool may already have run".
    #[tokio::test]
    async fn a_repeated_tool_call_is_not_a_second_card_once_its_round_has_closed() {
        let dir = std::env::temp_dir().join(format!("meridian-acp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let services = bare_services(&dir);

        {
            let mut conn = services.db.get().unwrap();
            crate::db::ops::conversation::create_conversation(&mut conn, "c1", Some("t"), None, None, 0).unwrap();
            crate::db::ops::turn::begin(&mut conn, "t1", "c1", TurnOrigin::ClaudeCode, None, 1000).unwrap();
        }
        let first = begin_assistant(
            &services.db,
            "c1",
            "t1",
            (None, Some(PROVIDER_LABEL)),
            "claude-code",
            None,
        )
        .await
        .unwrap();

        let shared = Shared {
            services: services.clone(),
            conversation_id: "c1".into(),
            turn: Mutex::new(Some(TurnState {
                turn_id: "t1".into(),
                cancel: CancellationToken::new(),
                row: OpenRow::new(first.clone()),
                parent: first.clone(),
                interjected: Vec::new(),
                error_notice: None,
            })),
            model: Mutex::new(None),
            config: Mutex::new(Vec::new()),
            replay: Mutex::new(Replay::No),
            announced: Mutex::new(HashSet::new()),
            unwritten_rows: std::sync::atomic::AtomicUsize::new(0),
            memory_lost: Mutex::new(false),
            tools_lost: Mutex::new(false),
            plan_reviews: Arc::new(crate::acp::plan_review::ReviewControl::default()),
            agent_title: Mutex::new(None),
            placeholder_title: String::new(),
        };

        // The first announcement is the placeholder one: the adapter knows a
        // call is coming and not yet what it is.
        shared.absorb(update(call("A", "Bash", serde_json::json!({})))).await;
        shared
            .absorb(update(serde_json::json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "A",
                "status": "completed",
                "content": [{ "type": "content", "content": { "type": "text", "text": "a.txt" } }],
            })))
            .await;
        // Opens the next round: the row holding `A` is written out here.
        shared
            .absorb(update(call("B", "Read", serde_json::json!({ "file_path": "a.txt" }))))
            .await;
        // And now the adapter's other source finally gets round to `A`, with
        // the arguments it did not have the first time.
        shared
            .absorb(update(call("A", "Bash", serde_json::json!({ "command": "ls" }))))
            .await;

        let calls = shared.with_turn(|t| t.row.tool_calls.clone()).unwrap();
        assert_eq!(
            calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["B"],
            "the late repeat of A landed on B's round"
        );

        // Not dropped either. The row it belongs to is in the database by now,
        // and `{}` left there is indistinguishable from a call that took no
        // arguments — in the transcript and in the audit copy alike.
        let stored = {
            let mut conn = services.db.get().unwrap();
            crate::db::ops::message::list_messages(&mut conn, "c1")
                .unwrap()
                .into_iter()
                .filter_map(|m| m.tool_calls)
                .flat_map(|json| crate::agent::tool_calls::parse_openai_tool_calls(Some(&json)).unwrap())
                .find(|c| c.id == "A")
                .expect("A's row was written")
        };
        assert_eq!(
            stored.arguments, r#"{"command":"ls"}"#,
            "the late arguments reached the row"
        );

        // And the round can still settle, which is the half that decides the
        // turn phase. With A wrongly on it, one result never reaches two calls.
        shared
            .absorb(update(serde_json::json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "B",
                "status": "completed",
                "content": [{ "type": "content", "content": { "type": "text", "text": "hello" } }],
            })))
            .await;
        let (results, calls) = shared
            .with_turn(|t| (t.row.results.len(), t.row.tool_calls.len()))
            .unwrap();
        assert!(results >= calls, "{results} result(s) for {calls} call(s)");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Parallel calls share a round. Settling on the first result used to
    /// rotate as soon as any prose arrived, so B's result landed on a new row
    /// that had never asked for it.
    #[tokio::test]
    async fn a_thought_between_parallel_results_stays_on_the_same_round() {
        let dir = std::env::temp_dir().join(format!("meridian-acp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let services = bare_services(&dir);

        {
            let mut conn = services.db.get().unwrap();
            crate::db::ops::conversation::create_conversation(&mut conn, "c1", Some("t"), None, None, 0).unwrap();
            crate::db::ops::turn::begin(&mut conn, "t1", "c1", TurnOrigin::ClaudeCode, None, 1000).unwrap();
        }
        let first = begin_assistant(
            &services.db,
            "c1",
            "t1",
            (None, Some(PROVIDER_LABEL)),
            "claude-code",
            None,
        )
        .await
        .unwrap();

        let shared = Shared {
            services: services.clone(),
            conversation_id: "c1".into(),
            turn: Mutex::new(Some(TurnState {
                turn_id: "t1".into(),
                cancel: CancellationToken::new(),
                row: OpenRow::new(first.clone()),
                parent: first.clone(),
                interjected: Vec::new(),
                error_notice: None,
            })),
            model: Mutex::new(None),
            config: Mutex::new(Vec::new()),
            replay: Mutex::new(Replay::No),
            announced: Mutex::new(HashSet::new()),
            unwritten_rows: std::sync::atomic::AtomicUsize::new(0),
            memory_lost: Mutex::new(false),
            tools_lost: Mutex::new(false),
            plan_reviews: Arc::new(crate::acp::plan_review::ReviewControl::default()),
            agent_title: Mutex::new(None),
            placeholder_title: String::new(),
        };

        shared.absorb(update(call("A", "Bash", serde_json::json!({})))).await;
        shared.absorb(update(call("B", "Read", serde_json::json!({})))).await;
        shared
            .absorb(update(serde_json::json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "A",
                "status": "completed",
                "content": [{ "type": "content", "content": { "type": "text", "text": "a" } }],
            })))
            .await;
        shared
            .absorb(update(serde_json::json!({
                "sessionUpdate": "agent_thought_chunk",
                "content": { "type": "text", "text": "hmm" },
            })))
            .await;
        shared
            .absorb(update(serde_json::json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "B",
                "status": "completed",
                "content": [{ "type": "content", "content": { "type": "text", "text": "b" } }],
            })))
            .await;

        let (calls, results, reasoning, settled) = shared
            .with_turn(|t| {
                (
                    t.row.tool_calls.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
                    t.row.results.iter().map(|(id, _, _)| id.clone()).collect::<Vec<_>>(),
                    t.row.reasoning.clone(),
                    t.row.settled,
                )
            })
            .unwrap();
        assert_eq!(calls, vec!["A".to_string(), "B".to_string()]);
        assert_eq!(results, vec!["A".to_string(), "B".to_string()]);
        assert!(reasoning.contains("hmm"), "{reasoning}");
        assert!(settled);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

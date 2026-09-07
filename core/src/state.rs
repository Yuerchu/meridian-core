use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agent::engine::ApprovalDecision;

/// A tool call sitting in front of the user, waiting to be allowed or refused.
///
/// Identified by an `approval_id` we mint, not by the provider's tool call id.
/// Those arrive off the wire unvalidated and OpenAI-compatible gateways
/// routinely reuse `"0"` from one call to the next; keyed by that, a second
/// call would overwrite the first one's sender, the first would read the
/// resulting `RecvError` as "the user said nothing", and its cleanup would then
/// delete the *second* one's entry — leaving that turn waiting on an answer
/// that could no longer reach it.
pub struct PendingApproval {
    /// Where the call is happening. For a delegated run that is the sub-agent's
    /// own conversation, which is *not* where the question is asked — see
    /// `bubble`.
    pub conversation_id: String,
    /// Which run of the turn asked. What the turn guard sweeps by when a turn
    /// ends: an approval whose turn is gone has nobody left to answer it.
    pub turn_id: String,
    /// The assistant row this call hangs off. Needed to place the card: the
    /// provider call id alone does not name one once it repeats within a turn.
    pub assistant_message_id: String,
    /// What the model called it, and what the tool result must be sent back
    /// under. Never used to look an approval up.
    pub provider_call_id: String,
    /// The call this one is a second attempt at. Set only for sandbox
    /// escalations. It currently equals `provider_call_id` — the retry reuses
    /// the id — but the two mean different things, and recording it keeps the
    /// front end from having to know that they coincide.
    pub origin_call_id: Option<String>,
    pub tool_name: String,
    /// What the tool was called with.
    ///
    /// Recoverable from the transcript for an ordinary approval — the call is
    /// written on the assistant row the card hangs off. Not for a bubbled one:
    /// that row lives in the sub-agent's conversation, and the parent, which is
    /// where the card is, has no way to reach it. Without this, reloading the
    /// parent would turn "run `cargo test --all`?" into an unlabelled yes/no.
    /// Kept for both kinds so that redrawing a card does not depend on which it
    /// is.
    pub arguments: String,
    /// Why a sandbox-blocked command is asking to run again without the
    /// sandbox. Present exactly when this approval is such a retry.
    pub retry_reason: Option<String>,
    /// Set when this call belongs to a delegated run.
    pub bubble: Option<Bubble>,
    /// When this stops standing, if it ever does.
    ///
    /// **Not what ends the wait** — the waiter's own timer is, and it removes
    /// the entry. This is here so a listing can decline to hand back a card in
    /// the moment between the deadline and that timer's next tick; see
    /// `crate::approval::is_expired`. `None` is a question with no deadline,
    /// which is a real setting.
    pub expires_at: Option<std::time::Instant>,
    pub sender: oneshot::Sender<ApprovalDecision>,
}

/// Where a delegated run's approvals are drawn and answered.
///
/// A sub-agent's tool call happens in its own conversation, but nobody is
/// watching that one — the person is looking at the card that spawned it. So the
/// question surfaces there instead, and this says where "there" is.
#[derive(Clone)]
pub struct Bubble {
    /// The parent conversation: where the card is drawn and the answer comes
    /// from.
    pub conversation_id: String,
    /// The parent's assistant row carrying the `run_agent` call.
    pub assistant_message_id: String,
    /// The `run_agent` call itself. Together with the row above it names one
    /// card, which the call id alone does not — provider call ids repeat within
    /// a conversation.
    pub parent_call_id: String,
    /// Where the run is happening, so the card can offer a way in.
    pub sub_conversation_id: String,
}

/// Keyed by `approval_id`.
///
/// A `std::sync::Mutex` rather than tokio's: every critical section is one map
/// operation with nothing awaited inside, and a turn has to be able to clear
/// its own entries from `Drop`, which cannot await.
pub struct ApprovalWaiters(std::sync::Mutex<HashMap<String, PendingApproval>>);

impl ApprovalWaiters {
    pub(crate) fn new() -> Self {
        ApprovalWaiters(std::sync::Mutex::new(HashMap::new()))
    }

    /// Recovers from poisoning instead of propagating it. The critical sections
    /// only insert and remove, so a panic elsewhere cannot leave the map
    /// half-written — whereas refusing the lock would take every later approval
    /// in the app down with it.
    pub fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, PendingApproval>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take a question out, if it is still there.
    ///
    /// **The `Some` is the right to act on it, not merely the entry.** Removing
    /// a `PendingApproval` drops its sender and so ends the wait, which means
    /// "who removes it" and "who ends it" are one question — and approve, deny
    /// and expiry all race for it. A caller that gets `None` lost the race and
    /// must do nothing at all, because whoever holds the `Some` is already
    /// accounting for it.
    ///
    /// Named for that rather than left as `remove`: the previous name invited
    /// `let _ = ...remove(id)`, which is exactly the call that produces two
    /// accounts of one question.
    pub fn claim(&self, approval_id: &str) -> Option<PendingApproval> {
        self.lock().remove(approval_id)
    }
}

/// Voice input state. The engine slot has its own lock so that a slow model
/// load (~3.6s) naturally deduplicates: the prewarm task holds the lock while
/// loading and a concurrent transcribe call just waits on it, then hits the
/// cache instead of loading again.
pub struct VoiceState {
    pub inner: Arc<Mutex<VoiceInner>>,
    pub engine: Arc<Mutex<Option<Arc<crate::voice::engine::Engine>>>>,
}

#[derive(Default)]
pub struct VoiceInner {
    /// Desktop only: Android records in the WebView and posts the finished
    /// samples over, so there is no open capture session to hold on to.
    #[cfg(not(target_os = "android"))]
    pub session: Option<crate::voice::capture::RecordingSession>,
    pub download: Option<CancellationToken>,
}

impl VoiceState {
    pub(crate) fn new() -> Self {
        VoiceState {
            inner: Arc::new(Mutex::new(VoiceInner::default())),
            engine: Arc::new(Mutex::new(None)),
        }
    }
}

/// Where a message typed into a running sub-agent's conversation waits.
///
/// Keyed by the sub-agent's conversation, because that is what the person is
/// looking at when they type. Only the lifecycle lives here for now — registered
/// when a delegated run starts, closed and removed however it ends. The queue
/// protocol that makes "accepted" and "closed" mutually exclusive comes with the
/// steering work; until then every inbox is empty and closing one returns
/// nothing.
#[derive(Default)]
pub struct AppSubAgentInboxes(std::sync::Mutex<HashMap<String, Arc<SubAgentInbox>>>);

#[derive(Default)]
pub struct SubAgentInbox {
    /// `None` once closed. Taking the queue out is what makes closing final:
    /// there is no state where something can still be added and nobody will
    /// read it.
    queue: std::sync::Mutex<Option<Vec<crate::agent::engine::Steered>>>,
}

/// What became of a message handed to an inbox.
///
/// The two are decided under the same lock as the close that races them, which
/// is the point: without that, a command can look the inbox up, find it, append
/// to it, and return `Ok` to somebody whose message nobody will ever read. Every
/// message is therefore either queued — and then owed an account of itself — or
/// refused with its own text handed straight back.
pub enum Accept {
    Queued,
    /// Nobody is reading any more. The text comes back so the caller can say so
    /// rather than pretend it went somewhere — the one caller today declines to,
    /// which is its choice rather than this type's.
    Closed(#[allow(dead_code)] String),
}

impl SubAgentInbox {
    /// Hand a message to a run, if there is still a run to hand it to.
    pub(crate) fn append(&self, text: String) -> Accept {
        let mut guard = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(queue) => {
                queue.push(crate::agent::engine::Steered::typed(
                    text,
                    // Typed into the window by the person watching. They have no
                    // chat identity, which is not the same as there being nobody
                    // — see `SteeredOrigin`.
                    crate::agent::engine::SteeredOrigin::User(None),
                ));
                Accept::Queued
            }
            None => Accept::Closed(text),
        }
    }

    /// Everything still waiting, and no more will be taken.
    ///
    /// Whatever comes back was accepted from the user and never delivered, so
    /// the caller owes them an account of it — dropping it on the floor is the
    /// one outcome that must not happen.
    pub(crate) fn close(&self) -> Vec<crate::agent::engine::Steered> {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap_or_default()
    }
}

/// The inbox *is* the port. A wrapper type would only exist to hold a reference
/// to this one and forward a single method.
#[async_trait::async_trait]
impl crate::agent::engine::Steering for SubAgentInbox {
    async fn drain(&self) -> Vec<crate::agent::engine::Steered> {
        match self.queue.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            Some(queue) => std::mem::take(queue),
            None => Vec::new(),
        }
    }
}

impl AppSubAgentInboxes {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<SubAgentInbox>>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Open one for a run that is starting. Replaces any leftover entry: the
    /// conversation is minted per run, so a collision would mean a previous run
    /// failed to clean up, and the new run is the one people are typing at.
    pub fn open(&self, conversation_id: &str) -> Arc<SubAgentInbox> {
        let inbox = Arc::new(SubAgentInbox {
            queue: std::sync::Mutex::new(Some(Vec::new())),
        });
        self.lock().insert(conversation_id.to_string(), Arc::clone(&inbox));
        inbox
    }

    /// Stop taking messages for this conversation, and hand back anything that
    /// was accepted but never delivered.
    pub fn close(&self, conversation_id: &str) -> Vec<crate::agent::engine::Steered> {
        match self.lock().remove(conversation_id) {
            Some(inbox) => inbox.close(),
            None => Vec::new(),
        }
    }

    /// Give a message to whatever is running on this conversation.
    ///
    /// The `Arc` is taken under the map's lock and the decision made under the
    /// inbox's own, so a run that ends in between refuses rather than accepting
    /// into something nobody will read. A conversation with no run at all is the
    /// same answer: there is nothing to steer.
    pub fn append(&self, conversation_id: &str, text: String) -> Accept {
        let inbox = self.lock().get(conversation_id).map(Arc::clone);
        match inbox {
            Some(inbox) => inbox.append(text),
            None => Accept::Closed(text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(conversation_id: &str, call_id: &str) -> (PendingApproval, oneshot::Receiver<ApprovalDecision>) {
        let (tx, rx) = oneshot::channel();
        let entry = PendingApproval {
            conversation_id: conversation_id.into(),
            turn_id: "turn-1".into(),
            assistant_message_id: "msg-1".into(),
            provider_call_id: call_id.into(),
            origin_call_id: None,
            tool_name: "read_file".into(),
            arguments: "{}".into(),
            retry_reason: None,
            bubble: None,
            expires_at: None,
            sender: tx,
        };
        (entry, rx)
    }

    /// The reason approvals are not keyed by the provider's call id. Two
    /// conversations both handed `"0"` used to collide: the second insert
    /// dropped the first sender, the first turn read that as "no answer", and
    /// its cleanup then removed the second turn's entry.
    #[test]
    fn two_calls_sharing_a_provider_id_do_not_displace_each_other() {
        let waiters = ApprovalWaiters::new();
        let (a, mut rx_a) = pending("conv-1", "0");
        let (b, mut rx_b) = pending("conv-2", "0");
        waiters.lock().insert("appr-1".into(), a);
        waiters.lock().insert("appr-2".into(), b);
        assert_eq!(waiters.lock().len(), 2);

        let answered = waiters
            .lock()
            .remove("appr-1")
            .expect("first approval still registered");
        answered
            .sender
            .send(ApprovalDecision::Approved)
            .expect("its turn is still listening");

        assert!(rx_a.try_recv().is_ok());
        assert!(rx_b.try_recv().is_err(), "the other turn must not have been answered");
        assert!(waiters.lock().contains_key("appr-2"));
    }

    #[test]
    fn an_answered_approval_cannot_be_answered_twice() {
        let waiters = ApprovalWaiters::new();
        let (p, _rx) = pending("conv-1", "c1");
        waiters.lock().insert("appr-1".into(), p);

        assert!(waiters.lock().remove("appr-1").is_some());
        // The command layer turns this `None` into an error, which is what
        // tells the front end to retire the card rather than spin on it.
        assert!(waiters.lock().remove("appr-1").is_none());
    }

    #[test]
    fn listing_is_scoped_to_one_conversation() {
        let waiters = ApprovalWaiters::new();
        let (a, _rx_a) = pending("conv-1", "c1");
        let (b, _rx_b) = pending("conv-2", "c1");
        waiters.lock().insert("appr-1".into(), a);
        waiters.lock().insert("appr-2".into(), b);

        let mine: Vec<String> = waiters
            .lock()
            .iter()
            .filter(|(_, p)| p.conversation_id == "conv-1")
            .map(|(id, _)| id.clone())
            .collect();
        assert_eq!(mine, vec!["appr-1".to_string()]);
    }

    /// The two answers are mutually exclusive under one lock, which is the
    /// whole point: a message is either queued — and then owed an account of
    /// itself — or handed straight back. What must not exist is a third state
    /// where the command says `Ok` and nobody is reading.
    #[test]
    fn a_message_is_either_taken_or_handed_back() {
        let inboxes = AppSubAgentInboxes::default();
        inboxes.open("sub-1");

        assert!(matches!(inboxes.append("sub-1", "keep going".into()), Accept::Queued));
        // A conversation with no run at all is the same answer as one that ended.
        assert!(matches!(inboxes.append("nowhere", "hello".into()), Accept::Closed(t) if t == "hello"));

        let leftover = inboxes.close("sub-1");
        assert_eq!(leftover.len(), 1, "what was taken comes back rather than vanishing");
        assert_eq!(leftover[0].text, "keep going");

        // And after closing, the same conversation refuses.
        assert!(matches!(
            inboxes.append("sub-1", "too late".into()),
            Accept::Closed(t) if t == "too late",
        ));
    }

    /// Typed into the window by the person watching. They have no chat
    /// identity, which is not the same as there being nobody — as
    /// `system_context` would have said.
    #[test]
    fn what_a_desktop_user_types_is_a_user_talking() {
        let inbox = SubAgentInbox {
            queue: std::sync::Mutex::new(Some(Vec::new())),
        };
        inbox.append("use the other approach".into());

        let taken = inbox.close();
        assert!(matches!(
            taken[0].origin,
            crate::agent::engine::SteeredOrigin::User(None),
        ));
    }

    /// Draining is what the loop does between rounds; closing is the end. A
    /// drained inbox still takes messages, a closed one never does again.
    #[tokio::test]
    async fn draining_is_not_closing() {
        use crate::agent::engine::Steering;
        let inbox = SubAgentInbox {
            queue: std::sync::Mutex::new(Some(Vec::new())),
        };
        inbox.append("first".into());

        assert_eq!(inbox.drain().await.len(), 1);
        assert!(inbox.drain().await.is_empty());
        assert!(matches!(inbox.append("second".into()), Accept::Queued));

        assert_eq!(inbox.close().len(), 1);
        assert!(matches!(inbox.append("third".into()), Accept::Closed(_)));
        assert!(
            inbox.drain().await.is_empty(),
            "a closed inbox has nothing left to give the loop"
        );
    }

    /// One turn panicking while holding the lock must not take every later
    /// approval in the app down with it.
    #[test]
    fn a_poisoned_lock_still_hands_out_the_map() {
        let waiters = Arc::new(ApprovalWaiters::new());
        let poisoner = Arc::clone(&waiters);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock();
            panic!("a turn died holding the lock");
        })
        .join();

        assert!(waiters.lock().is_empty());
    }
}

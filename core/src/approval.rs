//! How long a question may stand, and who is allowed to end it.
//!
//! Three askers register into `services.approvals` and wait on the answer —
//! the desktop's tool approvals, ACP's permission requests, and ACP's
//! elicitation forms. All three had written the same `select!` by hand, and the
//! wait below is that select in one place. Not tidiness: a deadline arm added
//! to two of three copies is a card that expires in some conversations and
//! hangs in others, and nothing about the code would say which.
//!
//! # One terminal owner
//!
//! **A `PendingApproval` leaving the register is what ends its wait**, because
//! dropping the sender wakes the receiver with an error. So "who removes it" is
//! the same question as "who ends this", and the answer has to be exactly one
//! party — approve, deny and expiry all race, and two of them acting produces
//! two accounts of one question.
//!
//! [`ApprovalWaiters::claim`] is that answer. It is an atomic take, and the
//! `Some` is the *right to act* rather than a convenience: every caller has to
//! look, and a caller holding `None` must do nothing at all, because whoever
//! holds the `Some` is already doing it.
//!
//! **There is no sweeper.** An earlier sketch had three places end a wait — a
//! timer, a filter over the list, and a background pass over the map — which is
//! three competing removers for one entry, and the removal itself already wakes
//! the waiter. The waiter is the one party that is by definition present and
//! already waiting, so it owns its own deadline. The filter in the list survives
//! only as belt and braces, and it hides rather than removes.
//!
//! # Timing out is not refusing
//!
//! [`crate::agent::engine::Approvals`] says `Ok(None)` means nobody answered,
//! which is not the same as a refusal: the tool does not run, and the turn goes
//! on to say so rather than being told the user said no. Expiry produces
//! exactly that. Reading it as a denial would put words in somebody's mouth,
//! and reading it as consent would be worse.
//!
//! # Which runner, which number
//!
//! One mechanism, one value per runner. The desktop and ACP take
//! [`TTL_PREFERENCE`], defaulting to thirty minutes — long enough to make a cup
//! of tea, short enough that a turn abandoned overnight is not still holding a
//! conversation in the morning. Zero disables it, which is a real setting and
//! not a broken one.
//!
//! **OneBot is deliberately not on this.** Its approvals are a different
//! register (`onebot::PendingApprovals`), keyed by chat session rather than by
//! approval id, answered with typed text rather than an `ApprovalDecision`, and
//! its sixty-second `tokio::time::timeout` is already the sole owner of that
//! wait. Sharing this would mean rewriting that register, not adding a timeout
//! to it — so the "no nested timers" rule holds there by construction rather
//! than by this module. Moving it here is a change to the OneBot approval
//! model and belongs in its own change.

use std::time::Duration;

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::agent::engine::ApprovalDecision;
use crate::events::ChatStreamEvent;
use crate::services::Services;
use crate::state::PendingApproval;

/// How long a card may sit unanswered, in minutes. `0` means for ever.
pub const TTL_PREFERENCE: &str = "approvals.ttl_minutes";
pub const DEFAULT_TTL_MINUTES: u64 = 30;

/// Why a question stopped standing.
///
/// Only [`Self::Expired`] is announced. The other two happen as a turn ends,
/// and the turn's own `stop` already tells every client that its questions are
/// over — a second event would be one more thing to keep in step with it. What
/// the cause earns is the log line saying which of them it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetireCause {
    /// Nobody answered inside the deadline.
    Expired,
    /// The turn was stopped while the card was up.
    Cancelled,
    /// The turn ended and swept up after itself.
    TurnGone,
}

impl RetireCause {
    fn as_str(self) -> &'static str {
        match self {
            RetireCause::Expired => "expired",
            RetireCause::Cancelled => "cancelled",
            RetireCause::TurnGone => "turn_gone",
        }
    }
}

/// What the deadline is for this app right now, or `None` for no deadline.
///
/// Read per question rather than held: it is one indexed row and a card is a
/// human-scale event, while a cached copy would mean the setting takes effect
/// at some point nobody can name.
pub fn ttl(services: &Services) -> Result<Option<Duration>, String> {
    let mut conn = services.db.get().map_err(|error| error.to_string())?;
    let stored = crate::db::ops::preference::get_preference(&mut conn, TTL_PREFERENCE)
        .map_err(|error| format!("failed to read preference {TTL_PREFERENCE}: {error}"))?;
    let minutes = match stored {
        None => DEFAULT_TTL_MINUTES,
        Some(raw) => {
            let minutes = raw
                .parse::<u64>()
                .map_err(|error| format!("preference {TTL_PREFERENCE} has invalid minutes {raw:?}: {error}"))?;
            if minutes.to_string() != raw {
                return Err(format!(
                    "preference {TTL_PREFERENCE} must use canonical decimal digits, got {raw:?}"
                ));
            }
            minutes
        }
    };
    // Zero is somebody saying "never expire", which is different from somebody
    // not having said anything.
    if minutes == 0 {
        return Ok(None);
    }
    let seconds = minutes
        .checked_mul(60)
        .ok_or_else(|| format!("preference {TTL_PREFERENCE} is too large"))?;
    Ok(Some(Duration::from_secs(seconds)))
}

/// Register a question, and wait for it to be answered, stopped, or to expire.
///
/// The caller inserts the entry and draws the card; this owns everything after
/// that. It returns `None` for all three of the ways a question can end without
/// an answer, because [`crate::agent::engine::Approvals`] draws no distinction
/// between them — what differs is only what gets said about it, which happens
/// here.
pub async fn wait(
    services: &Services,
    approval_id: &str,
    rx: oneshot::Receiver<ApprovalDecision>,
    cancel: &CancellationToken,
    ttl: Option<Duration>,
) -> Option<ApprovalDecision> {
    // `pending()` rather than a long sleep for the no-deadline case: a sleep
    // long enough to stand in for "never" is still a timer the runtime has to
    // carry for every card in the app.
    let deadline = async {
        match ttl {
            Some(ttl) => tokio::time::sleep(ttl).await,
            None => std::future::pending().await,
        }
    };

    tokio::select! {
        answered = rx => {
            // The entry is gone either way. `Ok` is somebody's decision;
            // `Err` is the sender dropped, which is a sweep that has already
            // claimed this and said whatever there was to say.
            answered.ok()
        }
        _ = cancel.cancelled() => {
            retire(services, approval_id, RetireCause::Cancelled);
            None
        }
        _ = deadline => {
            retire(services, approval_id, RetireCause::Expired);
            None
        }
    }
}

/// End a question that nobody answered, at most once.
///
/// Returns whether *this* call is the one that ended it. A `false` means the
/// race was lost and there is nothing to do — not an error, and not a reason to
/// try again.
pub fn retire(services: &Services, approval_id: &str, cause: RetireCause) -> bool {
    let Some(pending) = services.approvals.claim(approval_id) else {
        return false;
    };

    tracing::debug!(
        approval_id,
        cause = cause.as_str(),
        tool = %pending.tool_name,
        conversation_id = %pending.conversation_id,
        "a question was retired unanswered"
    );

    // Only expiry is announced — see [`RetireCause`]. Nothing else would tell a
    // client about it: the turn is still running, the card is still on screen,
    // and a card whose answer can no longer reach anybody is worse than no card.
    if cause == RetireCause::Expired {
        // Where the *card* is, which is the parent for a delegated run. The same
        // choice the request event makes, for the same reason: a question
        // announced on the parent has to be withdrawn from the parent.
        let (conversation_id, message_id) = match &pending.bubble {
            Some(b) => (&b.conversation_id, &b.assistant_message_id),
            None => (&pending.conversation_id, &pending.assistant_message_id),
        };
        let _ = services.events.emit_chat(ChatStreamEvent::ToolApprovalExpired {
            approval_id: approval_id.to_string(),
            call_id: pending.provider_call_id.clone(),
            tool_name: pending.tool_name.clone(),
            message_id: message_id.clone(),
            conversation_id: conversation_id.clone(),
        });
    }
    true
}

/// Retire every question belonging to a turn that has ended.
///
/// Keyed by turn rather than by conversation: a later turn in the same
/// conversation may have questions of its own outstanding, and clearing those
/// would strand it instead.
///
/// Goes through [`retire`] rather than a bare `retain`, so a turn ending while
/// one of its cards is expiring cannot have both of them account for it.
pub fn retire_turn(services: &Services, turn_id: &str, cause: RetireCause) -> usize {
    // The ids first, under the lock; the retiring after it. `retain` with a
    // closure that emits would hold the register while an event goes out to
    // every sink, and one of those is a window that may be gone.
    let ids: Vec<String> = services
        .approvals
        .lock()
        .iter()
        .filter(|(_, pending)| pending.turn_id == turn_id)
        .map(|(id, _)| id.clone())
        .collect();

    ids.iter().filter(|id| retire(services, id, cause)).count()
}

/// Whether an entry is past its deadline.
///
/// Belt and braces for the listing paths, and nothing more: the waiter's own
/// timer is what actually ends a question, and it will have done so within a
/// tick of this returning true. What this closes is the window between the two
/// — a list request landing inside it would otherwise hand back a card whose
/// buttons are about to stop meaning anything.
pub fn is_expired(pending: &PendingApproval) -> bool {
    pending.expires_at.is_some_and(|at| at <= std::time::Instant::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::bare_services;
    use crate::state::Bubble;

    fn register(services: &Services, approval_id: &str, turn_id: &str) -> oneshot::Receiver<ApprovalDecision> {
        let (tx, rx) = oneshot::channel();
        services.approvals.lock().insert(
            approval_id.to_string(),
            PendingApproval {
                conversation_id: "c-1".into(),
                turn_id: turn_id.into(),
                assistant_message_id: "m-1".into(),
                provider_call_id: "call-1".into(),
                origin_call_id: None,
                tool_name: "run_command".into(),
                arguments: "{}".into(),
                retry_reason: None,
                bubble: None,
                expires_at: None,
                sender: tx,
            },
        );
        rx
    }

    fn services() -> (tempfile::TempDir, Services) {
        let dir = tempfile::tempdir().unwrap();
        let services = bare_services(dir.path());
        (dir, services)
    }

    fn set_ttl(services: &Services, raw: &str) {
        let mut conn = services.db.get().unwrap();
        crate::db::ops::preference::set_preference(&mut conn, TTL_PREFERENCE, raw, 1).unwrap();
    }

    #[test]
    fn stored_ttl_is_strict_and_errors_are_not_defaulted() {
        let (_dir, services) = services();
        assert_eq!(ttl(&services).unwrap(), Some(Duration::from_secs(30 * 60)));

        set_ttl(&services, "0");
        assert_eq!(ttl(&services).unwrap(), None);

        for raw in [" 5", "05", "five", "18446744073709551615"] {
            set_ttl(&services, raw);
            let error = ttl(&services).expect_err("invalid stored TTL must fail");
            assert!(error.contains(TTL_PREFERENCE), "{raw:?}: {error}");
        }
    }

    /// Keeps the payloads, which is what these tests are about — the recorder in
    /// `events` keeps only the channel name.
    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<serde_json::Value>>);

    impl crate::events::EventSink for Recorder {
        fn emit(&self, _channel: &str, payload: &serde_json::Value) -> Result<(), String> {
            self.0.lock().unwrap().push(payload.clone());
            Ok(())
        }
    }

    /// Registered non-critical, so a test recorder cannot decide whether a turn
    /// succeeded.
    fn recording(services: &Services) -> std::sync::Arc<Recorder> {
        let recorder = std::sync::Arc::new(Recorder::default());
        services.events.register(recorder.clone(), false);
        recorder
    }

    /// An answer is an answer, and the deadline never fires.
    #[tokio::test(start_paused = true)]
    async fn an_answered_question_comes_back_with_its_decision() {
        let (_dir, services) = services();
        let rx = register(&services, "a-1", "t-1");

        let answered = services.approvals.claim("a-1").expect("still registered");
        answered.sender.send(ApprovalDecision::Approved).unwrap();

        let got = wait(
            &services,
            "a-1",
            rx,
            &CancellationToken::new(),
            Some(Duration::from_secs(60)),
        )
        .await;
        assert!(matches!(got, Some(ApprovalDecision::Approved)));
    }

    /// The contract the whole feature rests on: nobody answering is `None`, and
    /// `None` is not a denial. A `Denied` here would tell the model the user
    /// refused, which nobody did.
    #[tokio::test(start_paused = true)]
    async fn expiring_produces_no_answer_rather_than_a_refusal() {
        let (_dir, services) = services();
        let rx = register(&services, "a-1", "t-1");

        let got = wait(
            &services,
            "a-1",
            rx,
            &CancellationToken::new(),
            Some(Duration::from_secs(60)),
        )
        .await;

        assert!(got.is_none(), "an unanswered question must not become a decision");
        assert!(
            services.approvals.lock().is_empty(),
            "the waiter owns the removal, so nothing is left behind"
        );
    }

    /// No deadline means no deadline. Time moves a long way and the wait is
    /// still there.
    #[tokio::test(start_paused = true)]
    async fn a_zero_ttl_means_the_question_stands() {
        let (_dir, services) = services();
        let rx = register(&services, "a-1", "t-1");
        let cancel = CancellationToken::new();

        let waiting = tokio::spawn({
            let services = services.clone();
            let cancel = cancel.clone();
            async move { wait(&services, "a-1", rx, &cancel, None).await }
        });

        tokio::time::advance(Duration::from_secs(60 * 60 * 24)).await;
        assert!(!waiting.is_finished(), "a day passed and the card is still standing");

        cancel.cancel();
        assert!(waiting.await.unwrap().is_none());
    }

    /// Stopping the turn ends the wait, and the entry goes with it — otherwise
    /// a late answer lands on a turn that has moved on.
    #[tokio::test(start_paused = true)]
    async fn cancelling_the_turn_ends_the_wait() {
        let (_dir, services) = services();
        let rx = register(&services, "a-1", "t-1");
        let cancel = CancellationToken::new();
        cancel.cancel();

        assert!(wait(&services, "a-1", rx, &cancel, None).await.is_none());
        assert!(services.approvals.lock().is_empty());
    }

    /// The single-owner rule, as behaviour. Two parties try to end one
    /// question; exactly one of them gets to.
    #[test]
    fn only_one_party_can_retire_a_question() {
        let (_dir, services) = services();
        let _rx = register(&services, "a-1", "t-1");

        assert!(retire(&services, "a-1", RetireCause::Expired));
        assert!(
            !retire(&services, "a-1", RetireCause::TurnGone),
            "the second attempt must not act, or the question is accounted for twice"
        );
        assert!(!retire(&services, "never-existed", RetireCause::Expired));
    }

    /// And the sweep is the same rule at scale: only this turn's questions, and
    /// only the ones nobody has already claimed.
    #[test]
    fn a_turn_sweep_takes_its_own_and_leaves_the_rest() {
        let (_dir, services) = services();
        let _a = register(&services, "a-1", "t-1");
        let _b = register(&services, "a-2", "t-1");
        let _c = register(&services, "a-3", "t-2");

        // One of them is claimed first, as an expiry landing mid-sweep would.
        assert!(retire(&services, "a-1", RetireCause::Expired));

        assert_eq!(
            retire_turn(&services, "t-1", RetireCause::TurnGone),
            1,
            "the already-retired one must not be counted a second time"
        );
        assert_eq!(services.approvals.lock().len(), 1, "another turn's question stands");
        assert!(services.approvals.lock().contains_key("a-3"));
    }

    /// The listing guard. It is not what ends a question — the waiter is — but
    /// between the deadline and the waiter's next tick a list must not hand back
    /// a card whose buttons are about to stop working.
    #[test]
    fn a_question_past_its_deadline_reads_as_expired() {
        let (_dir, services) = services();
        let _rx = register(&services, "a-1", "t-1");

        let mut map = services.approvals.lock();
        let entry = map.get_mut("a-1").unwrap();
        assert!(!is_expired(entry), "no deadline is not an expired deadline");

        entry.expires_at = Some(std::time::Instant::now() + Duration::from_secs(60));
        assert!(!is_expired(entry));

        entry.expires_at = std::time::Instant::now().checked_sub(Duration::from_secs(1));
        assert!(is_expired(entry));
    }

    /// A delegated run's question is asked on the parent, so it has to be
    /// withdrawn from the parent. Announcing the sub-agent's conversation would
    /// leave the card that is actually on screen untouched.
    #[test]
    fn an_expiry_is_announced_where_the_card_was_drawn() {
        let (_dir, services) = services();
        let (tx, _rx) = oneshot::channel();
        services.approvals.lock().insert(
            "a-1".into(),
            PendingApproval {
                conversation_id: "sub-1".into(),
                turn_id: "t-1".into(),
                assistant_message_id: "child-row".into(),
                provider_call_id: "call-1".into(),
                origin_call_id: None,
                tool_name: "run_command".into(),
                arguments: "{}".into(),
                retry_reason: None,
                bubble: Some(Bubble {
                    conversation_id: "parent-1".into(),
                    assistant_message_id: "parent-row".into(),
                    parent_call_id: "call-run-agent".into(),
                    sub_conversation_id: "sub-1".into(),
                }),
                expires_at: None,
                sender: tx,
            },
        );

        let seen = recording(&services);
        assert!(retire(&services, "a-1", RetireCause::Expired));

        let events = seen.0.lock().unwrap();
        let expiry = events
            .iter()
            .find(|e| e["type"] == "tool_approval_expired")
            .expect("the expiry was announced");
        assert_eq!(expiry["conversation_id"], "parent-1");
        assert_eq!(expiry["message_id"], "parent-row");
        // And it names the call, because a conversation can have several cards
        // up and only one of them is the one that went.
        assert_eq!(expiry["call_id"], "call-1");
    }

    /// Only expiry is announced. The other two ride the turn's own `stop`, and
    /// a second event would be one more thing to keep in step with it.
    #[test]
    fn ending_with_the_turn_is_not_separately_announced() {
        let (_dir, services) = services();
        let seen = recording(&services);

        let _a = register(&services, "a-1", "t-1");
        assert!(retire(&services, "a-1", RetireCause::Cancelled));
        let _b = register(&services, "a-2", "t-1");
        assert!(retire(&services, "a-2", RetireCause::TurnGone));

        assert!(
            seen.0.lock().unwrap().is_empty(),
            "a turn ending already tells every client its questions are over"
        );
    }
}

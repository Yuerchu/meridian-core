//! Delivering the prompt queue.
//!
//! `db::ops::queue` decides *what* may go next; this decides *when*, and hands
//! it to whichever runner owns the conversation. One function does the work
//! ([`pump`]) and everything else here is about when it is allowed to run.
//!
//! **Nothing pumps at startup, and that is the rule the whole module is shaped
//! around.** A queue is a list of instructions a person wrote for an agent they
//! were watching; finding one on disk says only that the app died before it was
//! finished. Delivering it into an empty room — where the next thing that
//! happens is a tool call nobody approved — is the failure this feature must not
//! have. So there are exactly two things that make the queue move, and both are
//! somebody being there: an item being added, and a turn reaching its ending.
//!
//! **The two runners take the queue by opposite routes, and neither is an
//! accident.** A hosted session is asked — `_session/steering` and
//! `session/prompt` are requests, and what comes back is the only evidence
//! there is. A native turn is *offered*: [`Interjections`] is the `Steering`
//! port the loop drains between rounds, so the queue never pushes and never has
//! to know where the turn has got to.
//!
//! That difference is the same one the ledger is built around. A native turn
//! resends its whole history, so a row in the transcript is delivery — and
//! [`db::ops::queue::take_next`](crate::db::ops::queue::take_next) makes the row
//! and the item's removal one transaction, which leaves no in-doubt state to
//! report. A hosted turn's history lives in the adapter, so a row here proves
//! nothing and the doubt is real.

use crate::db::models::queue::{Delivery, QueuedPromptRow};
use crate::db::models::turn::TurnStatus;
use crate::services::Services;
use crate::util::{get_conn, now_ms};

mod native;

pub use native::{Announcing, Interjections};

/// Deliver whatever the queue owes this conversation, if anything can go now.
///
/// Returns without doing anything at all when there is nothing to deliver,
/// which is the ordinary case: this runs after every turn of every hosted
/// conversation, and most of them have an empty queue.
pub async fn pump(services: &Services, conversation_id: &str) {
    // A durable plan review is a conversation barrier, not merely a missing
    // in-memory lease.  The planning worker deliberately releases its lease at
    // `exit_plan`; without this row check the next queued prompt could start in
    // the gap and bypass the decision.  Read failures are fail-closed for the
    // same reason.
    match has_plan_review_barrier(services, conversation_id).await {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(%error, conversation_id, "could not verify the plan-review barrier; leaving the queue alone");
            return;
        }
    }
    // **Which runner is a property of the conversation, not of what happens to
    // be running.** Asking the registry answers "is an adapter alive right
    // now", and the two questions come apart exactly when it matters: after a
    // restart, or when the adapter died, a hosted conversation has no live
    // session — and the queue would then be delivered by `native::pump`, which
    // runs a turn with the user's own provider and this app's tool set and
    // appends it to a Claude Code transcript. The message would be answered by
    // the wrong agent, billed to the wrong account, and written into a
    // conversation whose agent has no idea it happened.
    //
    // A session is a child process, so Android has none, and `agent_kind` can
    // never be `claude_code` there.
    #[cfg(not(target_os = "android"))]
    if is_hosted(services, conversation_id).await {
        return hosted::pump(services, conversation_id).await;
    }
    native::pump(services, conversation_id).await;
}

/// Read the durable plan-review/continuation barrier off the database.
///
/// Public because writes outside the queue (manual compaction in both native
/// shells) take the same mutation lease and must make the same check before
/// changing the transcript. Keeping one async bridge avoids one caller
/// accidentally checking only for a pending review and forgetting the
/// unacknowledged delivery states.
pub async fn has_plan_review_barrier(services: &Services, conversation_id: &str) -> Result<bool, String> {
    let pool = services.db.clone();
    let conversation_id = conversation_id.to_string();
    tokio::task::spawn_blocking(move || {
        let mut conn = get_conn(&pool)?;
        crate::db::ops::plan_review::has_conversation_barrier(&mut conn, &conversation_id)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

/// Whether this conversation belongs to a hosted agent, from the row rather
/// than from the registry.
#[cfg(not(target_os = "android"))]
async fn is_hosted(services: &Services, conversation_id: &str) -> bool {
    let pool = services.db.clone();
    let id = conversation_id.to_string();
    // Every failure has to stay distinguishable from "this is not hosted", so
    // the errors are carried rather than flattened with `.ok()?`. Written the
    // short way, a busy pool or a transient query error read exactly like an
    // ordinary conversation — and the recovery from a transient error is a
    // Claude Code queue answered by the user's own provider.
    let kind: Result<Result<Option<String>, String>, _> = tokio::task::spawn_blocking(move || {
        let mut conn = crate::util::get_conn(&pool)?;
        crate::db::ops::conversation::get_conversation(&mut conn, &id)
            .map(|c| c.agent_kind)
            .map_err(|e| e.to_string())
    })
    .await;

    match kind {
        Ok(Ok(kind)) => kind.as_deref() == Some(crate::acp::AGENT_KIND),
        // Nothing was learned, and the two answers are not symmetrical:
        // `hosted::pump` with no session does nothing and the queue waits,
        // while `native::pump` starts a turn. So an unanswered question is
        // answered "hosted".
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "could not tell which runner owns this queue; leaving it alone");
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, "the runner lookup panicked; leaving the queue alone");
            true
        }
    }
}

/// The same, later and elsewhere.
///
/// For callers inside the thing they are pumping — a turn that has just ended
/// is still inside `prompt`, and the next item wants a `prompt` of its own,
/// with the turn lease this one has not finished dropping.
pub fn pump_later(services: &Services, conversation_id: &str) {
    let services = services.clone();
    let conversation_id = conversation_id.to_string();
    tokio::spawn(async move { pump(&services, &conversation_id).await });
}

/// What the end of a turn does to the queue, for both runners.
///
/// A turn that reached an ending lets the next item go; anything else stops the
/// whole queue. The instructions behind a failure rest on the same assumption
/// the failed step broke — "now rename that function" means nothing if the
/// function was never created — and a run the user stopped is them saying so
/// out loud.
pub async fn after_turn(services: &Services, conversation_id: &str, status: Option<TurnStatus>) {
    match status {
        Some(TurnStatus::Done) => pump_later(services, conversation_id),
        // exit_plan is a durable pause, not a failed premise. The review row
        // blocks delivery until its continuation is acknowledged; marking the
        // prompt queue held here would survive that acknowledgement and require
        // an unrelated manual queue release.
        Some(TurnStatus::WaitingReview) => {}
        _ => hold(services, conversation_id).await,
    }
}

/// The same, for a caller that has a turn id rather than a verdict.
///
/// The record is the source of truth about how a turn ended, and reading it
/// back is cheaper than threading the answer out through every early return of
/// a function that has a dozen.
pub async fn after_recorded_turn(services: &Services, conversation_id: &str, turn_id: &str) -> Result<(), String> {
    let pool = services.db.clone();
    let id = turn_id.to_string();
    let status = tokio::task::spawn_blocking(move || -> Result<Option<TurnStatus>, String> {
        let mut conn = get_conn(&pool)?;
        let turn = crate::db::ops::turn::get(&mut conn, &id).map_err(|error| error.to_string())?;
        turn.map(|row| row.status()).transpose()
    })
    .await
    .map_err(|error| error.to_string())??;
    after_turn(services, conversation_id, status).await;
    Ok(())
}

/// Stop the queue, because the turn in front of it did not finish.
///
/// Everything still waiting, not just the head: the instructions were written
/// as a sequence and the ones after a failure rest on the same assumption the
/// failed step broke.
pub async fn hold(services: &Services, conversation_id: &str) {
    let pool = services.db.clone();
    let id = conversation_id.to_string();
    let held = tokio::task::spawn_blocking(move || {
        let mut conn = get_conn(&pool)?;
        crate::db::ops::queue::hold_all(&mut conn, &id, now_ms()).map_err(|e| e.to_string())
    })
    .await;

    match held {
        Ok(Ok(0)) => {}
        Ok(Ok(count)) => {
            tracing::info!(
                count,
                conversation_id,
                "the queue is held: the turn before it did not finish"
            );
            announce(services, conversation_id);
        }
        Ok(Err(e)) => tracing::warn!(error = %e, conversation_id, "could not hold the queue"),
        Err(e) => tracing::warn!(error = %e, conversation_id, "could not hold the queue (the write panicked)"),
    }
}

/// Tell the window the queue has moved. It reads the rows back rather than the
/// event, so this carries only which conversation to re-read and the required
/// delivery marker used to decide whether the transcript moved too.
pub fn announce(services: &Services, conversation_id: &str) {
    let _ = services
        .events
        .emit_queue_updated(&crate::events::QueueUpdatedEvent::new(conversation_id, false));
}

/// The same, for the moment an item stops being queued and becomes a message.
///
/// Separate because it is the only one that changes the *transcript*, and the
/// listener has to be able to tell: re-reading a conversation on every enqueue
/// and every drag would be a snapshot of a running turn per keystroke.
///
/// Both halves are needed and neither works alone. Without the announcement the
/// front end keeps the item's last known state — still `queued`, still stacked
/// above the composer for the length of the turn — while the backend has
/// already spent it, so pressing the delete it is still offering answers that
/// the message has been sent. Without the transcript half, the row leaves the
/// queue on `settled_message_id` and the message it became is not on screen
/// either, which is the one state this must never produce.
pub fn announce_delivered(services: &Services, conversation_id: &str) {
    emit_delivered(&services.events, conversation_id);
}

/// The bus alone, for the port wrapper — everything it needs to do its job, and
/// the difference between a decorator that can be built in a test and one that
/// needs a data directory.
pub(super) fn emit_delivered(events: &crate::events::EventBus, conversation_id: &str) {
    let _ = events.emit_queue_updated(&crate::events::QueueUpdatedEvent::new(conversation_id, true));
}

/// How many doubtful items one message may describe.
///
/// Rarely more than one — an in-doubt item stops the queue, so a second can
/// only appear after somebody released it — and nothing beyond the cap is
/// dropped. It stays unreported and comes back on the next message, exactly as
/// an unreported turn does.
const AT_MOST: usize = 3;

/// What the agent is told about queued messages that may or may not have
/// reached it, and which items that settles.
///
/// The same shape as `interrupted::Report`, and settled by the same rule:
/// reading it is not telling anyone, so the two travel together and only a
/// reply read to the end writes anything down.
pub struct Doubtful {
    text: String,
    ids: Vec<String>,
}

impl Doubtful {
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// What to tell the agent about queued messages whose delivery is unknown.
///
/// Reading this settles nothing — see [`confirm_reported`].
pub async fn owed(services: &Services, conversation_id: &str) -> Option<Doubtful> {
    let pool = services.db.clone();
    let id = conversation_id.to_string();
    let items = tokio::task::spawn_blocking(move || {
        let mut conn = pool.get().ok()?;
        crate::db::ops::queue::unreported_in_doubt(&mut conn, &id).ok()
    })
    .await
    .ok()
    .flatten()?;

    let items: Vec<QueuedPromptRow> = items.into_iter().take(AT_MOST).collect();
    if items.is_empty() {
        return None;
    }
    Some(Doubtful {
        text: describe(&items),
        ids: items.into_iter().map(|i| i.id).collect(),
    })
}

/// Record that the agent has now been told about these.
///
/// Called at the one moment that proves it, for the same reason
/// `interrupted::confirm_delivered` is: reading the record is not telling
/// anyone, and a turn can read it and then die before a byte leaves. Every way
/// of getting this wrong repeats the warning rather than losing it, including a
/// failed write, and that is the direction to fail in.
pub async fn confirm_reported(services: &Services, report: Doubtful) {
    let pool = services.db.clone();
    let written = tokio::task::spawn_blocking(move || {
        let mut conn = get_conn(&pool)?;
        crate::db::ops::queue::mark_reported(&mut conn, &report.ids, now_ms()).map_err(|e| e.to_string())
    })
    .await;
    match written {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "could not record that a queued message was reported"),
        Err(e) => tracing::warn!(error = %e, "recording a reported queue item panicked"),
    }
}

/// Says what is not known, and says plainly that it will not be retried.
///
/// The temptation is to have the agent re-do the instruction to be safe, and
/// that is precisely the failure this design exists to prevent: the message may
/// have been "delete the old migration", and doing it twice is not the same as
/// doing it once. So the text asks for the state to be checked rather than for
/// the work to be repeated, and it never says which of the two happened —
/// because nothing here knows.
fn describe(items: &[QueuedPromptRow]) -> String {
    // Verbatim, in a tag of its own. A queued message is the user's own words
    // and gets the same treatment an ordinary prompt does — quoting it into one
    // line would fold a multi-line instruction into `\n`s and escaped quotes,
    // which is worse to read and no safer.
    let quoted: Vec<String> = items
        .iter()
        .map(|i| format!("<message>\n{}\n</message>", i.content))
        .collect();
    let opening = match items.len() {
        1 => "A message you had queued was sent to you and never acknowledged, so it may have \
              reached you or may not have. It said:"
            .to_string(),
        n => format!(
            "{n} messages you had queued were sent to you and never acknowledged, so they may \
             have reached you or may not have. Oldest first:"
        ),
    };
    format!(
        "<undelivered_queue>\n{opening}\n{}\n\nThey have not been sent again. If one did arrive, \
         you may already have acted on it, and doing the work a second time is not the same as \
         doing it once. Check the current state before assuming either way, and say what you find \
         rather than silently repeating anything.\n</undelivered_queue>",
        quoted.join("\n")
    )
}

/// The next item, asked for the way the runner's state allows.
///
/// `steerable` narrows to interjections; idle takes the front of the queue
/// whatever mode it is in, because with no turn to interrupt the distinction
/// has nothing to refer to.
async fn read(services: &Services, conversation_id: &str, steerable: bool) -> Option<QueuedPromptRow> {
    let pool = services.db.clone();
    let id = conversation_id.to_string();
    let found = tokio::task::spawn_blocking(move || {
        let mut conn = get_conn(&pool)?;
        if steerable {
            crate::db::ops::queue::next_deliverable(&mut conn, &id, Delivery::Interject).map_err(|e| e.to_string())
        } else {
            crate::db::ops::queue::next_pending(&mut conn, &id).map_err(|e| e.to_string())
        }
    })
    .await;

    match found {
        Ok(Ok(item)) => item,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, conversation_id, "could not read the queue");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, conversation_id, "could not read the queue (it panicked)");
            None
        }
    }
}

/// One small write against the queue, off the async runtime.
// Android has no runner to deliver to: a hosted session is a child process, and
// the native path is not connected yet.
#[cfg_attr(target_os = "android", allow(dead_code, reason = "nothing delivers there yet"))]
async fn write<F, T>(services: &Services, id: String, f: F) -> Result<T, String>
where
    F: FnOnce(&mut diesel::SqliteConnection, String) -> diesel::QueryResult<T> + Send + 'static,
    // Generic in the result so a caller can read the affected-row count back.
    // Most of these writes are notes and `()` is all there is to say; a
    // `mark_dispatched` is a claim, and the count is the claim's answer.
    T: Send + 'static,
{
    let pool = services.db.clone();
    tokio::task::spawn_blocking(move || {
        let mut conn = get_conn(&pool)?;
        f(&mut conn, id).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct CountingStarter(std::sync::atomic::AtomicUsize);

    #[async_trait::async_trait]
    impl crate::services::StartTurn for CountingStarter {
        async fn start(
            &self,
            _conversation_id: &str,
            _queued: &crate::db::models::queue::QueuedPromptRow,
        ) -> Result<(), String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_plan_review_blocks_then_acknowledgement_starts_the_queued_follow_up() {
        let dir = tempfile::tempdir().unwrap();
        let services = crate::services::bare_services(dir.path());
        let starter = std::sync::Arc::new(CountingStarter::default());
        services.turn_starter.set(starter.clone()).ok().unwrap();
        {
            let mut conn = services.db.get().unwrap();
            crate::db::ops::conversation::create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
            let document = crate::db::ops::plan_review::create_or_resume_document(&mut conn, "c1", 2).unwrap();
            let appended = crate::db::ops::plan_review::append_assistant_revision(
                &mut conn,
                &crate::db::ops::plan_review::PlanRevisionAppend {
                    document_id: &document.id,
                    expected_generation: 0,
                    expected_head_sha256: None,
                    content_markdown: "# Plan\n",
                    patch: "*** Add File: plan.md",
                    source_message_id: None,
                    source_call_id: None,
                    responding_to_suggestion_revision_id: None,
                    now: 3,
                },
            )
            .unwrap();
            crate::db::ops::plan_review::mark_materialization_applied(&mut conn, &appended.materialization.id, 4)
                .unwrap();
            crate::db::ops::turn::begin(&mut conn, "t1", "c1", crate::turn::TurnOrigin::Desktop, None, 5).unwrap();
            crate::db::ops::plan_review::submit_native_head_for_review(
                &mut conn,
                &crate::db::ops::plan_review::PlanReviewSubmit {
                    document_id: &document.id,
                    expected_generation: 1,
                    expected_head_sha256: &appended.revision.content_sha256,
                    turn_id: Some("t1"),
                    assistant_message_id: None,
                    provider_call_id: None,
                    provider_kind: crate::db::models::plan_review::PlanReviewProviderKind::Native,
                    now: 6,
                },
                &crate::db::models::plan_review::NativePlanReviewRuntimeConfig::fixture(),
            )
            .unwrap();
            crate::db::ops::queue::enqueue(&mut conn, "q1", "c1", "bypass the review", Delivery::FollowUp, 7).unwrap();
        }

        assert!(
            has_plan_review_barrier(&services, "c1").await.unwrap(),
            "desktop and OneBot compaction share this durable guard after taking their mutation lease"
        );
        pump(&services, "c1").await;

        assert_eq!(starter.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        let queued = crate::db::ops::queue::list(&mut services.db.get().unwrap(), "c1").unwrap();
        assert_eq!(queued[0].settled_at, None, "the prompt remains durable and undelivered");
        assert_eq!(queued[0].state(), crate::db::models::queue::QueueState::Queued);

        after_recorded_turn(&services, "c1", "t1").await.unwrap();
        let queued = crate::db::ops::queue::list(&mut services.db.get().unwrap(), "c1").unwrap();
        assert_eq!(
            queued[0].state(),
            crate::db::models::queue::QueueState::Queued,
            "WaitingReview is a pause and must not hold the follow-up queue"
        );

        {
            let mut conn = services.db.get().unwrap();
            let review = crate::db::ops::plan_review::get_pending_review_for_conversation(&mut conn, "c1")
                .unwrap()
                .unwrap();
            let bundle = crate::db::ops::plan_review::get_review_bundle(&mut conn, &review.id).unwrap();
            let decided = crate::db::ops::plan_review::decide_review(
                &mut conn,
                &crate::db::ops::plan_review::PlanReviewDecision {
                    review_id: &review.id,
                    decision_id: "approve-1",
                    expected_lock_version: review.lock_version,
                    expected_draft_generation: bundle.draft.generation,
                    expected_draft_sha256: &bundle.draft.draft_sha256,
                    action: crate::db::ops::plan_review::PlanReviewDecisionAction::Approve,
                    decision_summary: None,
                    delivery_target: Some(crate::db::models::plan_review::PlanDeliveryTarget::Native),
                    target_session_id: None,
                    target_turn_id: Some("continuation-1"),
                    now: 8,
                },
            )
            .unwrap();
            let delivery = decided.delivery.unwrap();
            crate::db::ops::plan_review::mark_delivery_dispatched(&mut conn, &delivery.id, "attempt-1", 9).unwrap();
            crate::db::ops::plan_review::mark_delivery_acknowledged(&mut conn, &delivery.id, "attempt-1", 10).unwrap();
        }

        pump(&services, "c1").await;
        assert_eq!(
            starter.0.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "an acknowledged plan continuation releases the durable barrier and starts the follow-up"
        );
    }

    #[tokio::test]
    async fn waiting_review_never_marks_an_ordinary_queued_prompt_held() {
        let dir = tempfile::tempdir().unwrap();
        let services = crate::services::bare_services(dir.path());
        {
            let mut conn = services.db.get().unwrap();
            crate::db::ops::conversation::create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
            crate::db::ops::queue::enqueue(&mut conn, "q1", "c1", "after review", Delivery::FollowUp, 2).unwrap();
        }

        after_turn(&services, "c1", Some(TurnStatus::WaitingReview)).await;
        let queued = crate::db::ops::queue::list(&mut services.db.get().unwrap(), "c1").unwrap();
        assert_eq!(queued[0].state(), crate::db::models::queue::QueueState::Queued);
    }

    fn doubtful(content: &str) -> QueuedPromptRow {
        QueuedPromptRow {
            id: format!("q-{content}"),
            conversation_id: "c1".into(),
            content: content.into(),
            delivery: "interject".into(),
            position: 0,
            created_at: 0,
            dispatched_at: Some(1),
            dispatched_turn_id: Some("t1".into()),
            settled_at: None,
            settled_message_id: None,
            held_at: None,
            reported_at: None,
        }
    }

    /// The one thing this text must not do is ask for the work to be redone.
    /// "It may not have arrived, so do it again" is how a queue deletes a
    /// migration twice — and the agent cannot check the claim, because nothing
    /// here knows which of the two happened.
    #[test]
    fn the_warning_asks_for_the_state_to_be_checked_not_for_a_retry() {
        let text = describe(&[doubtful("delete the old migration")]);
        assert!(text.contains("delete the old migration"), "it quotes the message");
        assert!(text.contains("may have reached you or may not have"));
        assert!(text.contains("have not been sent again"));
        assert!(text.contains("Check the current state"));
    }

    /// A queued message is often several lines, and it has to survive as
    /// several lines: an instruction folded into escaped `\n`s is harder to
    /// read and no safer, since these are the user's own words either way.
    #[test]
    fn a_multi_line_message_stays_multi_line() {
        let text = describe(&[doubtful("do this\nthen that")]);
        assert!(text.contains("do this\nthen that"), "{text}");
        assert!(!text.contains("\\n"), "nothing is escaped into one line: {text}");
    }

    /// More than one reads as a list, and says so — a single sentence naming
    /// three messages would read as one message in three parts.
    #[test]
    fn several_are_listed_oldest_first() {
        let text = describe(&[doubtful("first"), doubtful("second")]);
        assert!(text.contains("2 messages"));
        assert!(text.contains("Oldest first"));
        assert!(text.find("first") < text.find("second"));
    }
}

/// Delivering into a session running in an adapter, over ACP.
///
/// Separate from the rest because everything in it is a child process, which
/// Android does not have — and because the two delivery modes are two different
/// methods here, where a native turn has one port for both.
#[cfg(not(target_os = "android"))]
mod hosted {
    use std::sync::Arc;

    use super::{announce, read, write};
    use crate::acp::AcpSession;
    use crate::acp::protocol::SteerOutcome;
    use crate::db::models::queue::{Delivery, QueuedPromptRow};
    use crate::services::Services;
    use crate::util::now_ms;

    pub(super) async fn pump(services: &Services, conversation_id: &str) {
        // Only a live session. A conversation whose adapter died with the last
        // run of the app is exactly the "empty room" the module header is
        // about: reopening it would start a process and a turn nobody asked
        // for, in a directory nobody has looked at since.
        let Some(session) = services.acp.get(conversation_id).filter(|s| s.is_alive()) else {
            return;
        };

        // Which of the two modes may go depends on whether there is a turn to
        // interject into — and the answer can change under us, which is why
        // nothing below trusts it. A steer that arrives after the turn has
        // ended comes back `promptRequired` and takes the other path in the
        // same call.
        let steerable = session.supports_steering().then(|| session.current_turn_id()).flatten();
        let Some(next) = read(services, conversation_id, steerable.is_some()).await else {
            return;
        };

        let next_delivery = match next.delivery() {
            Ok(delivery) => delivery,
            Err(error) => {
                tracing::error!(%error, conversation_id, queue_id = %next.id, "queued prompt has an invalid delivery mode");
                return;
            }
        };

        let next = match steerable.filter(|_| next_delivery == Delivery::Interject) {
            None => next,
            Some(turn_id) => match steer(services, &session, &next, &turn_id).await {
                // Delivered, and the running turn is already adapting.
                // Whatever is behind it waits for that turn to end.
                Steered::Delivered => return,
                // The turn ended in the gap, or another pump had the item.
                // Either way what was read before the claim is stale by exactly
                // the window the claim was open — and that window is where a
                // hold lands: the turn this was meant to interrupt can fail
                // while the adapter is being asked, and `hold_all` reaches the
                // claimed row so that `undispatch` returns it *held* rather than
                // ready. Asking again is what reads that. Reusing `next` would
                // hand `deliver_queued` a row the queue has since stopped, and
                // leave it to `mark_dispatched` to refuse the claim and roll the
                // turn back — correct, and reported as a failure rather than as
                // the barrier it is.
                Steered::NotTaken => match read(services, conversation_id, false).await {
                    Some(again) => again,
                    None => return,
                },
                // In doubt, and the queue is stopped behind it until somebody
                // is told. Retrying is the one thing this design refuses.
                Steered::Unknown => return,
            },
        };

        // A turn of its own. Awaited rather than spawned: the caller is either
        // a command the user is waiting on or an already-detached task, and
        // awaiting is what keeps a second pump from taking an item for a turn
        // the lease is about to refuse.
        if let Err(e) = session.deliver_queued(services, &next).await {
            tracing::warn!(error = %e, conversation_id, "a queued prompt failed as a turn");
        }
    }

    /// What became of a steer, reduced to the three things the caller does
    /// about it. The protocol's outcomes collapse here because `injected`,
    /// `startedNewTurn` and anything newer all mean the agent has it.
    enum Steered {
        Delivered,
        NotTaken,
        Unknown,
    }

    async fn steer(services: &Services, session: &Arc<AcpSession>, item: &QueuedPromptRow, turn_id: &str) -> Steered {
        // The record of the attempt goes down *before* the attempt, and this is
        // the whole reason the ledger exists. Killed in the gap, the agent may
        // already have run a command — and a command's effects outlive both
        // this process and the adapter's memory of having asked for it.
        //
        // **And it is a claim, not a note.** `mark_dispatched` re-checks that
        // this row is still the deliverable front, and its affected-row count
        // is what says whether *this* caller won it. Discarded, two pumps that
        // both read the same item — a turn ending as the user adds one, a
        // window and a phone — would both go on to `session.steer`, and "delete
        // the old migration" would be sent twice.
        //
        // The gap this closes is not hypothetical here: between the read in
        // `pump` and this line there is a round trip to a child process, and
        // the user can hold the queue, drag the row out of first place or
        // switch it to `follow_up` in that time. `Some(Interject)` is what
        // makes the last of those void the claim.
        //
        // In a transaction of its own because the read and the update have to
        // be one step, and unlike `write_prompt_row` and `run_turn` this claim
        // is not already inside one.
        let turn = turn_id.to_string();
        let conversation = session.conversation_id.clone();
        let claimed = write(services, item.id.clone(), move |conn, id| {
            conn.immediate_transaction(|conn| {
                crate::db::ops::queue::mark_dispatched(
                    conn,
                    &conversation,
                    &id,
                    Some(Delivery::Interject),
                    &turn,
                    now_ms(),
                )
            })
        })
        .await;
        match claimed {
            Ok(1..) => {}
            // Somebody else has it. Not a failure and not in doubt: whoever
            // took it is delivering it, and this pump has nothing to report.
            Ok(_) => {
                tracing::debug!(item = %item.id, "a queued item was taken by another pump");
                return Steered::NotTaken;
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not record a steer before making it");
                return Steered::Unknown;
            }
        }
        announce(services, &session.conversation_id);

        let outcome = session.steer(&item.id, &item.content).await;
        announce(services, &session.conversation_id);

        match outcome {
            // The agent says it did not take the message — evidence about the
            // delivery rather than the absence of it — so the item goes back.
            Ok(SteerOutcome::PromptRequired) => {
                let _ = write(services, item.id.clone(), |conn, id| {
                    crate::db::ops::queue::undispatch(conn, &id).map(|_| ())
                })
                .await;
                Steered::NotTaken
            }
            Ok(outcome) => {
                if outcome == SteerOutcome::StartedNewTurn {
                    // Only reachable from an adapter that ignored the `_meta`
                    // we send. There is now a turn narrating itself into this
                    // session that this app did not open and cannot stop.
                    tracing::warn!(
                        conversation_id = %session.conversation_id,
                        "a steer started a detached turn; this app has no lease on it"
                    );
                }
                let _ = write(services, item.id.clone(), |conn, id| {
                    crate::db::ops::queue::mark_settled(conn, &id, None, now_ms()).map(|_| ())
                })
                .await;
                announce(services, &session.conversation_id);
                Steered::Delivered
            }
            // Left dispatched and unsettled on purpose. It is never delivered
            // again; it is reported to the agent instead.
            Err(e) => {
                tracing::warn!(error = %e, conversation_id = %session.conversation_id, "a steer failed");
                Steered::Unknown
            }
        }
    }
}

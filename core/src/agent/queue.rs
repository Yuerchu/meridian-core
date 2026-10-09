//! Delivering the prompt queue.
//!
//! `db::sea::ops::queue` decides *what* may go next; this decides *when*, and hands
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
//! [`db::sea::ops::queue::take_next`](crate::db::sea::ops::queue::take_next) makes the row
//! and the item's removal one transaction, which leaves no in-doubt state to
//! report. A hosted turn's history lives in the adapter, so a row here proves
//! nothing and the doubt is real.

use crate::db::entity::queued_prompt;
use crate::db::models::queue::Delivery;
use crate::db::models::turn::TurnStatus;
use crate::db::sea::DbErr;
use crate::db::sea::ops::queue as queue_ops;
use crate::services::Services;
use crate::util::now_ms;

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
    services
        .sea
        .read(async |tx| crate::db::sea::ops::plan_review::has_conversation_barrier(tx, conversation_id).await)
        .await
        .map_err(|error: DbErr| error.to_string())
}

/// Whether this conversation belongs to a hosted agent, from the row rather
/// than from the registry.
#[cfg(not(target_os = "android"))]
async fn is_hosted(services: &Services, conversation_id: &str) -> bool {
    // Every failure has to stay distinguishable from "this is not hosted", so
    // the errors are carried rather than flattened with `.ok()?`. Written the
    // short way, a busy pool or a transient query error read exactly like an
    // ordinary conversation — and the recovery from a transient error is a
    // Claude Code queue answered by the user's own provider.
    match crate::db::sea::ops::conversation::get_conversation(&services.sea, conversation_id).await {
        Ok(Some(conversation)) => conversation.agent_kind.as_deref() == Some(crate::acp::AGENT_KIND),
        // Nothing was learned, and the two answers are not symmetrical:
        // `hosted::pump` with no session does nothing and the queue waits,
        // while `native::pump` starts a turn. So an unanswered question is
        // answered "hosted". A conversation that is gone has nothing to pump.
        Ok(None) => {
            tracing::warn!(conversation_id, "the queue's conversation is gone; leaving it alone");
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not tell which runner owns this queue; leaving it alone");
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
    let status = crate::db::sea::ops::turn::get(&services.sea, turn_id)
        .await
        .map_err(|error| error.to_string())?
        .map(|row| row.status);
    after_turn(services, conversation_id, status).await;
    Ok(())
}

/// Stop the queue, because the turn in front of it did not finish.
///
/// Everything still waiting, not just the head: the instructions were written
/// as a sequence and the ones after a failure rest on the same assumption the
/// failed step broke.
pub async fn hold(services: &Services, conversation_id: &str) {
    let held = services
        .sea
        .write(async |tx| queue_ops::hold_all(tx, conversation_id, now_ms()).await)
        .await;

    match held {
        Ok(0) => {}
        Ok(count) => {
            tracing::info!(
                count,
                conversation_id,
                "the queue is held: the turn before it did not finish"
            );
            announce(services, conversation_id);
        }
        Err(e) => tracing::warn!(error = %e, conversation_id, "could not hold the queue"),
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
    let items = queue_ops::unreported_in_doubt(&services.sea, conversation_id)
        .await
        .ok()?;

    let items: Vec<queued_prompt::Model> = items.into_iter().take(AT_MOST).collect();
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
    let written = services
        .sea
        .write(async |tx| queue_ops::mark_reported(tx, &report.ids, now_ms()).await)
        .await;
    if let Err(e) = written {
        tracing::warn!(error = %e, "could not record that a queued message was reported");
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
fn describe(items: &[queued_prompt::Model]) -> String {
    // Verbatim, in a tag of its own. A queued message is the user's own words
    // and gets the same treatment an ordinary prompt does — quoting it into one
    // line would fold a multi-line instruction into `\n`s and escaped quotes,
    // which is worse to read and no safer.
    let quoted: Vec<String> = items
        .iter()
        .map(|i| format!("<message>\n{}\n</message>", spoken(&i.content)))
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

/// What a queued message said, as the person would describe it: the text, and
/// what was attached by name.
///
/// Not the stored envelope. That is JSON carrying local `file:///` paths, which
/// is neither what anybody wrote nor anything the agent should be handed as
/// though it were; an attachment it may already have seen is named so that it
/// can be recognised, not sent again.
fn spoken(content: &str) -> String {
    use crate::provider::MessageContentPart;
    match crate::provider::decode_message_parts(content) {
        Ok(None) => content.to_string(),
        Ok(Some(parts)) => {
            let mut text = Vec::new();
            let mut attached = Vec::new();
            for part in parts {
                match part {
                    MessageContentPart::Text { text: t } => text.push(t),
                    MessageContentPart::ImageUrl { .. } => attached.push("an image".to_string()),
                    MessageContentPart::File { file } => attached.push(file.name),
                    MessageContentPart::Sticker { name, .. } => {
                        attached.push(name.map_or_else(|| "a sticker".to_string(), |n| format!("sticker {n}")))
                    }
                }
            }
            let mut out = text.join("\n");
            if !attached.is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!("(attached: {})", attached.join(", ")));
            }
            out
        }
        // Damaged parts are not a message anyone can quote; the warning
        // still stands, without words to attach to it.
        Err(_) => "(this message could not be read back)".to_string(),
    }
}

/// The next item, asked for the way the runner's state allows.
///
/// `steerable` narrows to interjections; idle takes the front of the queue
/// whatever mode it is in, because with no turn to interrupt the distinction
/// has nothing to refer to.
async fn read(services: &Services, conversation_id: &str, steerable: bool) -> Option<queued_prompt::Model> {
    let found = if steerable {
        queue_ops::next_deliverable(&services.sea, conversation_id, Delivery::Interject).await
    } else {
        queue_ops::next_pending(&services.sea, conversation_id).await
    };

    match found {
        Ok(item) => item,
        Err(e) => {
            tracing::warn!(error = %e, conversation_id, "could not read the queue");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Counts ordinary starts, and separately the turns started for a
    /// background task.
    #[derive(Default)]
    struct CountingStarter(std::sync::atomic::AtomicUsize, std::sync::atomic::AtomicUsize);

    #[async_trait::async_trait]
    impl crate::services::StartTurn for CountingStarter {
        async fn start(
            &self,
            _conversation_id: &str,
            _queued: &crate::db::entity::queued_prompt::Model,
        ) -> Result<(), String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        async fn start_unprompted(&self, _conversation_id: &str) -> Result<(), String> {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    /// A background task that ended goes ahead of the queue, and goes even
    /// when the queue is held: it is something that happened, not an
    /// instruction resting on the step that failed. One a person stopped, or
    /// one lost to a restart, starts nothing.
    #[cfg(not(target_os = "android"))]
    #[tokio::test]
    async fn an_ended_background_task_wakes_ahead_of_a_held_queue() {
        use crate::db::entity::background_task::{BackgroundKind, BackgroundRunner, BackgroundState};
        use crate::db::sea::ops::background_task as tasks;
        let dir = tempfile::tempdir().unwrap();
        let services = crate::services::bare_services(dir.path()).await;
        let starter = std::sync::Arc::new(CountingStarter::default());
        services.turn_starter.set(starter.clone()).ok().unwrap();
        let task = async |id: &str, state: BackgroundState| {
            services
                .sea
                .write(async |tx| {
                    tasks::insert(
                        tx,
                        &tasks::BackgroundTaskInsert {
                            id,
                            conversation_id: "c1",
                            runner: BackgroundRunner::Native,
                            kind: BackgroundKind::Command,
                            spawned_turn_id: None,
                            command: Some("x"),
                            description: None,
                            cwd: None,
                            sandbox: None,
                            output_path: None,
                            started_at: 1,
                        },
                    )
                    .await?;
                    tasks::finish(
                        tx,
                        id,
                        &tasks::BackgroundTaskChangeset {
                            state,
                            exit_code: None,
                            ended_reason: None,
                            output_bytes: 0,
                            output_truncated: false,
                            ended_at: 2,
                        },
                    )
                    .await
                })
                .await
                .unwrap();
        };
        // A conversation with one follow-up queued and the queue held, as a
        // failed turn leaves it.
        crate::db::sea::execute_for_tests(
            &services.sea,
            "INSERT INTO conversations (id, title, is_pinned, is_archived, message_count, created_at, updated_at, fast_mode)
             VALUES ('c1', 't', 0, 0, 0, 1, 1, 0);
             INSERT INTO queued_prompts (id, conversation_id, content, delivery, position, created_at, held_at)
             VALUES ('q1', 'c1', 'next', 'follow_up', 0, 3, 4);",
        )
        .await
        .unwrap();
        task("stopped", BackgroundState::Stopped).await;
        task("lost", BackgroundState::Lost).await;
        pump(&services, "c1").await;
        assert_eq!(
            starter.1.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a stop and a loss wake nothing"
        );

        task("done", BackgroundState::Completed).await;
        pump(&services, "c1").await;
        assert_eq!(
            starter.1.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "held queue or not"
        );
        assert_eq!(
            starter.0.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "and ahead of the queue"
        );
    }

    #[tokio::test]
    async fn a_plan_review_blocks_then_acknowledgement_starts_the_queued_follow_up() {
        let dir = tempfile::tempdir().unwrap();
        let services = crate::services::bare_services(dir.path()).await;
        let starter = std::sync::Arc::new(CountingStarter::default());
        services.turn_starter.set(starter.clone()).ok().unwrap();
        {
            use crate::db::sea::ops::plan_review as review_ops;
            services
                .sea
                .write(async |tx| {
                    crate::db::sea::ops::conversation::create_conversation(tx, "c1", None, None, None, 1).await?;
                    let document = review_ops::create_or_resume_document(tx, "c1", 2).await?;
                    let appended = review_ops::append_assistant_revision(
                        tx,
                        &review_ops::PlanRevisionAppend {
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
                    .await?;
                    review_ops::mark_materialization_applied(tx, &appended.materialization.id, 4).await?;
                    crate::db::sea::ops::turn::begin(tx, "t1", "c1", crate::turn::TurnOrigin::Desktop, None, 5).await?;
                    review_ops::submit_native_head_for_review(
                        tx,
                        &review_ops::PlanReviewSubmit {
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
                    .await
                })
                .await
                .unwrap();
        }
        services
            .sea
            .write(async |tx| queue_ops::enqueue(tx, "q1", "c1", "bypass the review", Delivery::FollowUp, 7).await)
            .await
            .unwrap();

        assert!(
            has_plan_review_barrier(&services, "c1").await.unwrap(),
            "desktop and OneBot compaction share this durable guard after taking their mutation lease"
        );
        pump(&services, "c1").await;

        assert_eq!(starter.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        let queued = queue_ops::list(&services.sea, "c1").await.unwrap();
        assert_eq!(queued[0].settled_at, None, "the prompt remains durable and undelivered");
        assert_eq!(queued[0].state(), crate::db::models::queue::QueueState::Queued);

        after_recorded_turn(&services, "c1", "t1").await.unwrap();
        let queued = queue_ops::list(&services.sea, "c1").await.unwrap();
        assert_eq!(
            queued[0].state(),
            crate::db::models::queue::QueueState::Queued,
            "WaitingReview is a pause and must not hold the follow-up queue"
        );

        {
            use crate::db::sea::ops::plan_review as review_ops;
            services
                .sea
                .write(async |tx| {
                    let review = review_ops::get_pending_review_for_conversation(tx, "c1")
                        .await?
                        .expect("the review is pending");
                    let bundle = review_ops::get_review_bundle(tx, &review.id).await?;
                    let decided = review_ops::decide_review(
                        tx,
                        &review_ops::PlanReviewDecision {
                            review_id: &review.id,
                            decision_id: "approve-1",
                            expected_lock_version: review.lock_version,
                            expected_draft_generation: bundle.draft.generation,
                            expected_draft_sha256: &bundle.draft.draft_sha256,
                            action: review_ops::PlanReviewDecisionAction::Approve,
                            decision_summary: None,
                            delivery_target: Some(crate::db::models::plan_review::PlanDeliveryTarget::Native),
                            target_session_id: None,
                            target_turn_id: Some("continuation-1"),
                            now: 8,
                        },
                    )
                    .await?;
                    let delivery = decided.delivery.expect("an approval delivers");
                    review_ops::mark_delivery_dispatched(tx, &delivery.id, "attempt-1", 9).await?;
                    review_ops::mark_delivery_acknowledged(tx, &delivery.id, "attempt-1", 10).await
                })
                .await
                .unwrap();
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
        let services = crate::services::bare_services(dir.path()).await;
        services
            .sea
            .write(async |tx| {
                crate::db::sea::ops::conversation::create_conversation(tx, "c1", None, None, None, 1).await
            })
            .await
            .unwrap();
        services
            .sea
            .write(async |tx| queue_ops::enqueue(tx, "q1", "c1", "after review", Delivery::FollowUp, 2).await)
            .await
            .unwrap();

        after_turn(&services, "c1", Some(TurnStatus::WaitingReview)).await;
        let queued = queue_ops::list(&services.sea, "c1").await.unwrap();
        assert_eq!(queued[0].state(), crate::db::models::queue::QueueState::Queued);
    }

    fn doubtful(content: &str) -> queued_prompt::Model {
        queued_prompt::Model {
            id: format!("q-{content}"),
            conversation_id: "c1".into(),
            content: content.into(),
            delivery: Delivery::Interject,
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

    /// A message with attachments is quoted as words and names, never as the
    /// stored envelope with its local paths.
    #[test]
    fn attachments_are_named_not_quoted_as_json() {
        let parts = r#"[{"type":"text","text":"compare these"},{"type":"image_url","image_url":{"url":"file:///C:/data/files/c1/a.png"}},{"type":"file","file":{"url":"file:///C:/data/files/c1/b.pdf","mime_type":"application/pdf","name":"report.pdf"}}]"#;
        let text = describe(&[doubtful(parts)]);
        assert!(
            text.contains("<message>\ncompare these\n(attached: an image, report.pdf)\n</message>"),
            "{text}"
        );
        assert!(!text.contains("file:///"), "no local path reaches the agent: {text}");
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

    use super::{announce, queue_ops, read};
    use crate::acp::AcpSession;
    use crate::acp::protocol::SteerOutcome;
    use crate::db::entity::queued_prompt;
    use crate::db::models::queue::Delivery;
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
        // The agent is answering a background task by itself. There is nothing
        // to steer into and no prompt to send beside it; that turn's close
        // pumps again, which is when the front of the queue goes.
        if session.working_unprompted() {
            return;
        }

        // Which of the two modes may go depends on whether there is a turn to
        // interject into — and the answer can change under us, which is why
        // nothing below trusts it. A steer that arrives after the turn has
        // ended comes back `promptRequired` and takes the other path in the
        // same call.
        let steerable = session.supports_steering().then(|| session.current_turn_id()).flatten();
        let Some(next) = read(services, conversation_id, steerable.is_some()).await else {
            return;
        };

        // Held to the known modes at the read.
        let next_delivery = next.delivery;

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

    async fn steer(
        services: &Services,
        session: &Arc<AcpSession>,
        item: &queued_prompt::Model,
        turn_id: &str,
    ) -> Steered {
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
        let claimed = services
            .sea
            .write(async |tx| {
                queue_ops::mark_dispatched(
                    tx,
                    &session.conversation_id,
                    &item.id,
                    Some(Delivery::Interject),
                    turn_id,
                    now_ms(),
                )
                .await
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
                let _ = services
                    .sea
                    .write(async |tx| queue_ops::undispatch(tx, &item.id).await)
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
                let _ = services
                    .sea
                    .write(async |tx| queue_ops::mark_settled(tx, &item.id, None, now_ms()).await)
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

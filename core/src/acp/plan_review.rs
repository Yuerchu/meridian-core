//! Durable human review for ACP's `ExitPlanMode` permission boundary.
//!
//! Claude Code exposes plan submission as a `session/request_permission`
//! request.  The request contains the complete plan snapshot, not a patch, and
//! the adapter remains parked until the client chooses one of its option ids.
//! This module turns that transient request into the same revision/review rows
//! the native runtime uses, then keeps only the live protocol continuation in
//! memory.  Losing the process therefore loses a waiter, never the review.

use std::sync::Mutex;

use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::db::models::plan_review::{PlanReviewProviderKind, PlanReviewState};
use crate::db::ops::plan_review as ops;
use crate::events::PlanReviewEvent;
use crate::services::Services;
use crate::util::{get_conn, now_ms};

use super::mapping;
use super::protocol::{PermissionOption, RequestPermissionParams};

const EXIT_PLAN_MODE: &str = "ExitPlanMode";

/// The protocol identity and full snapshot carried by one ACP submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ExitPlanSubmission {
    pub session_id: String,
    pub call_id: String,
    pub markdown: String,
}

/// Recognise the real Claude Code tool name, not ACP's display title/kind.
///
/// A malformed `ExitPlanMode` is an error rather than an ordinary permission:
/// routing it to the generic approval card would approve a plan that was never
/// durably captured.
pub(super) fn exit_plan_submission(params: &RequestPermissionParams) -> Result<Option<ExitPlanSubmission>, String> {
    if mapping::tool_name_of(&params.tool_call) != EXIT_PLAN_MODE {
        return Ok(None);
    }
    let raw = params
        .tool_call
        .raw_input
        .as_ref()
        .ok_or("ACP ExitPlanMode did not include rawInput")?;
    let markdown = raw
        .as_object()
        .and_then(|object| object.get("plan"))
        .and_then(serde_json::Value::as_str)
        .ok_or("ACP ExitPlanMode rawInput.plan must be a string")?
        .to_string();
    if markdown.trim().is_empty() {
        return Err("ACP ExitPlanMode submitted an empty plan".into());
    }
    Ok(Some(ExitPlanSubmission {
        session_id: params.session_id.clone(),
        call_id: params.tool_call.tool_call_id.clone(),
        markdown,
    }))
}

/// The two reversible choices Meridian's review page can make.
///
/// A lasting option is deliberately never substituted.  The review page says
/// approve/reject this submission, so returning `allow_always` would grant a
/// permission the user was never shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReviewChoices {
    pub allow: String,
    pub reject: String,
}

impl ReviewChoices {
    pub fn pick(options: &[PermissionOption]) -> Option<Self> {
        let once = |allow: bool| {
            options
                .iter()
                .find(|option| option.is_once() && if allow { option.is_allow() } else { option.is_reject() })
        };
        Some(Self {
            allow: once(true)?.option_id.clone(),
            reject: once(false)?.option_id.clone(),
        })
    }
}

/// The durable review row plus the protocol ids needed to resume its live
/// permission request.
#[derive(Debug, Clone)]
pub(super) struct SubmittedReview {
    pub event: PlanReviewEvent,
    pub session_id: String,
    pub call_id: String,
}

/// Import one full ACP snapshot as a revision and submit it for review.
pub(super) async fn submit(
    services: &Services,
    conversation_id: &str,
    turn_id: &str,
    assistant_message_id: &str,
    submission: ExitPlanSubmission,
) -> Result<SubmittedReview, String> {
    let pool = services.db.clone();
    let files = services.plan_files.clone();
    let conversation_id = conversation_id.to_string();
    let turn_id = turn_id.to_string();
    let assistant_message_id = assistant_message_id.to_string();
    let stored_submission = submission.clone();

    let event = tokio::task::spawn_blocking(move || {
        let mut conn = get_conn(&pool)?;
        if ops::get_pending_review_for_conversation(&mut conn, &conversation_id)
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Err("this conversation already has a plan awaiting review".into());
        }

        let now = now_ms();
        let document =
            ops::create_or_resume_document(&mut conn, &conversation_id, now).map_err(|error| error.to_string())?;
        let head = ops::get_head_revision(&mut conn, &document.id).map_err(|error| error.to_string())?;
        let desired_sha = ops::markdown_sha256(&stored_submission.markdown);

        let revision = if head
            .as_ref()
            .is_some_and(|revision| revision.content_sha256 == desired_sha)
        {
            head.expect("checked above")
        } else {
            let before = head
                .as_ref()
                .map(|revision| revision.content_markdown.as_str())
                .unwrap_or("");
            let patch = similar::TextDiff::from_lines(before, &stored_submission.markdown)
                .unified_diff()
                .context_radius(3)
                .header("a/plan.md", "b/plan.md")
                .to_string();
            let responding_to = ops::list_reviews(&mut conn, &document.id)
                .map_err(|error| error.to_string())?
                .into_iter()
                .rev()
                .find_map(|review| {
                    (review.state == PlanReviewState::ChangesRequested.as_str())
                        .then_some(review.suggestion_revision_id)
                        .flatten()
                });
            ops::append_assistant_revision(
                &mut conn,
                &ops::PlanRevisionAppend {
                    document_id: &document.id,
                    expected_generation: document.working_generation,
                    expected_head_sha256: head.as_ref().map(|revision| revision.content_sha256.as_str()),
                    content_markdown: &stored_submission.markdown,
                    patch: &patch,
                    source_message_id: Some(&assistant_message_id),
                    source_call_id: Some(&stored_submission.call_id),
                    responding_to_suggestion_revision_id: responding_to.as_deref(),
                    now,
                },
            )
            .map_err(|error| error.to_string())?
            .revision
        };

        let document = ops::get_document(&mut conn, &document.id).map_err(|error| error.to_string())?;
        let materialized = files
            .reconcile_document(&mut conn, &document.id, now_ms())
            .map_err(|error| error.to_string())?;
        if materialized.conflict.is_some() {
            return Err("the durable plan.md projection is in conflict".into());
        }

        let bundle = ops::submit_head_for_review(
            &mut conn,
            &ops::PlanReviewSubmit {
                document_id: &document.id,
                expected_generation: document.working_generation,
                expected_head_sha256: &revision.content_sha256,
                turn_id: Some(&turn_id),
                assistant_message_id: Some(&assistant_message_id),
                provider_call_id: Some(&stored_submission.call_id),
                provider_kind: PlanReviewProviderKind::Acp,
                now: now_ms(),
            },
        )
        .map_err(|error| error.to_string())?;
        Ok::<_, String>(PlanReviewEvent {
            review_id: bundle.review.id,
            conversation_id,
            document_id: bundle.document.id,
            revision_id: bundle.submitted_revision.id,
            turn_id,
            status: bundle.review.state,
            lock_version: bundle.review.lock_version,
            delivery_state: None,
        })
    })
    .await
    .map_err(|error| format!("ACP plan submission task failed: {error}"))??;

    Ok(SubmittedReview {
        event,
        session_id: submission.session_id,
        call_id: submission.call_id,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecisionAction {
    Approve,
    RequestChanges,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryBoundary {
    Acknowledged,
    InDoubt(String),
}

/// Everything the command layer has already committed for one ACP delivery.
/// Owned strings keep the async boundary independent of Tauri request DTOs.
#[derive(Debug, Clone)]
pub struct AcpPlanReviewDelivery {
    pub delivery_id: String,
    pub review_id: String,
    pub provider_call_id: String,
    pub target_session_id: Option<String>,
    pub submitting_turn_id: String,
    pub payload_json: String,
}

/// Certainty at the ACP process boundary. The command layer maps these to the
/// durable delivery states with the attempt token it owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpPlanReviewDeliveryOutcome {
    Acknowledged,
    /// Nothing crossed an ambiguous process boundary; an explicit continuation
    /// can safely try again.
    Held(String),
    /// The adapter may have consumed the response/message. Never retry this
    /// automatically.
    InDoubt(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeliveryAction {
    Approve,
    RequestChanges,
}

pub(super) fn delivery_action(payload_json: &str) -> Result<DeliveryAction, String> {
    let payload: serde_json::Value = serde_json::from_str(payload_json)
        .map_err(|error| format!("stored ACP plan delivery payload is invalid JSON: {error}"))?;
    let object = payload
        .as_object()
        .ok_or("stored ACP plan delivery payload must be an object")?;
    match object.get("action").and_then(serde_json::Value::as_str) {
        Some("approve") => Ok(DeliveryAction::Approve),
        Some(other) => Err(format!("unknown ACP plan delivery action '{other}'")),
        None if object.contains_key("suggestionRevisionId") && object.contains_key("baseRevisionId") => {
            Ok(DeliveryAction::RequestChanges)
        }
        None => Err("stored ACP plan delivery payload has no decision shape".into()),
    }
}

pub(super) fn delivery_prompt(payload_json: &str, action: DeliveryAction) -> String {
    let heading = match action {
        DeliveryAction::Approve => "The user approved the submitted plan.",
        DeliveryAction::RequestChanges => {
            "The user requested changes to the submitted plan. Apply the suggested diff and address every annotation before submitting it again."
        }
    };
    let rendered = serde_json::from_str::<serde_json::Value>(payload_json)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| payload_json.to_string());
    format!("{heading}\n\n<plan_review_delivery>\n{rendered}\n</plan_review_delivery>")
}

struct Decision {
    option_id: String,
    completion: oneshot::Sender<DeliveryBoundary>,
}

struct PendingReview {
    review_id: String,
    turn_id: String,
    session_id: String,
    call_id: String,
    choices: ReviewChoices,
    decision: oneshot::Sender<Decision>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingIdentity {
    pub review_id: String,
    pub turn_id: String,
    pub session_id: String,
    pub call_id: String,
}

struct ArmedBoundary {
    turn_id: String,
    completion: oneshot::Sender<DeliveryBoundary>,
}

/// In-memory continuation for a durable review.  Only ACP option senders live
/// here; the plan, comments, edits and delivery state stay in SQLite.
pub(super) struct ReviewControl {
    pending: Mutex<Option<PendingReview>>,
    boundary: Mutex<Option<ArmedBoundary>>,
    pause: watch::Sender<Option<String>>,
}

impl Default for ReviewControl {
    fn default() -> Self {
        let (pause, _) = watch::channel(None);
        Self {
            pending: Mutex::new(None),
            boundary: Mutex::new(None),
            pause,
        }
    }
}

impl ReviewControl {
    pub fn subscribe(&self) -> watch::Receiver<Option<String>> {
        self.pause.subscribe()
    }

    fn install(
        &self,
        review: &SubmittedReview,
        turn_id: &str,
        choices: ReviewChoices,
    ) -> Result<oneshot::Receiver<Decision>, String> {
        let (decision, receive) = oneshot::channel();
        let mut pending = self.pending.lock().map_err(|_| "ACP plan review lock was poisoned")?;
        if pending.is_some() {
            return Err("this ACP session already has a live plan review request".into());
        }
        *pending = Some(PendingReview {
            review_id: review.event.review_id.clone(),
            turn_id: turn_id.to_string(),
            session_id: review.session_id.clone(),
            call_id: review.call_id.clone(),
            choices,
            decision,
        });
        let _ = self.pause.send(Some(turn_id.to_string()));
        Ok(receive)
    }

    pub fn identity(&self, review_id: &str) -> Option<PendingIdentity> {
        let pending = self.pending.lock().ok()?;
        let pending = pending.as_ref().filter(|pending| pending.review_id == review_id)?;
        Some(PendingIdentity {
            review_id: pending.review_id.clone(),
            turn_id: pending.turn_id.clone(),
            session_id: pending.session_id.clone(),
            call_id: pending.call_id.clone(),
        })
    }

    pub fn is_waiting_turn(&self, turn_id: &str) -> bool {
        let pending = self
            .pending
            .lock()
            .ok()
            .and_then(|pending| pending.as_ref().map(|pending| pending.turn_id == turn_id))
            .unwrap_or(false);
        pending
            || self
                .boundary
                .lock()
                .ok()
                .and_then(|boundary| boundary.as_ref().map(|boundary| boundary.turn_id == turn_id))
                .unwrap_or(false)
    }

    pub fn resolve(
        &self,
        review_id: &str,
        action: ReviewDecisionAction,
    ) -> Result<oneshot::Receiver<DeliveryBoundary>, String> {
        let mut pending = self.pending.lock().map_err(|_| "ACP plan review lock was poisoned")?;
        let current = pending
            .as_ref()
            .ok_or("the ACP plan permission request is no longer live")?;
        if current.review_id != review_id {
            return Err("the live ACP plan permission belongs to a different review".into());
        }
        let pending = pending.take().expect("checked above");
        let option_id = match action {
            ReviewDecisionAction::Approve => pending.choices.allow.clone(),
            ReviewDecisionAction::RequestChanges => pending.choices.reject.clone(),
        };
        let (completion, receive) = oneshot::channel();
        pending
            .decision
            .send(Decision { option_id, completion })
            .map_err(|_| "the ACP plan permission request ended before it could be answered")?;
        Ok(receive)
    }

    fn arm(&self, turn_id: &str, completion: oneshot::Sender<DeliveryBoundary>) {
        if let Ok(mut boundary) = self.boundary.lock() {
            *boundary = Some(ArmedBoundary {
                turn_id: turn_id.to_string(),
                completion,
            });
        }
    }

    pub fn take_boundary(&self, turn_id: &str) -> Option<BoundaryCompletion> {
        let mut boundary = self.boundary.lock().ok()?;
        if boundary.as_ref().is_some_and(|boundary| boundary.turn_id == turn_id) {
            let boundary = boundary.take()?;
            let _ = self.pause.send(None);
            Some(BoundaryCompletion(boundary.completion))
        } else {
            None
        }
    }

    pub fn cancel(&self, review_id: &str) {
        if let Ok(mut pending) = self.pending.lock()
            && pending.as_ref().is_some_and(|pending| pending.review_id == review_id)
        {
            pending.take();
            let _ = self.pause.send(None);
        }
    }
}

pub(super) struct BoundaryCompletion(oneshot::Sender<DeliveryBoundary>);

impl BoundaryCompletion {
    pub fn complete(self, boundary: DeliveryBoundary) {
        let _ = self.0.send(boundary);
    }
}

pub(super) struct PermissionWait(oneshot::Receiver<Decision>);

/// Install the live option sender before announcing the durable review. This
/// closes the small but real race where a fast click could otherwise dispatch
/// a decision while the review row existed but its ACP continuation did not.
pub(super) fn install_permission_wait(
    control: &ReviewControl,
    review: &SubmittedReview,
    turn_id: &str,
    choices: ReviewChoices,
) -> Result<PermissionWait, String> {
    control.install(review, turn_id, choices).map(PermissionWait)
}

/// Wait for the durable decision and turn it back into ACP's exact option id.
pub(super) async fn await_permission_decision(
    control: &ReviewControl,
    review: &SubmittedReview,
    turn_id: &str,
    cancel: &CancellationToken,
    wait: PermissionWait,
) -> serde_json::Value {
    let decision = tokio::select! {
        decision = wait.0 => decision.ok(),
        _ = cancel.cancelled() => None,
    };
    let Some(decision) = decision else {
        control.cancel(&review.event.review_id);
        return super::protocol::permission_cancelled();
    };
    control.arm(turn_id, decision.completion);
    super::protocol::permission_selected(&decision.option_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::protocol::{ClaudeCodeMeta, ToolCall, ToolCallMeta};
    use crate::db::models::turn::{TurnRow, TurnStatus};
    use crate::services::bare_services;
    use crate::turn::TurnOrigin;
    use diesel::prelude::*;

    fn request(tool_name: &str, raw_input: serde_json::Value) -> RequestPermissionParams {
        RequestPermissionParams {
            session_id: "session-1".into(),
            tool_call: ToolCall {
                tool_call_id: "call-1".into(),
                title: Some("Finish planning".into()),
                kind: Some("think".into()),
                status: None,
                raw_input: Some(raw_input),
                content: Vec::new(),
                locations: Vec::new(),
                meta: Some(ToolCallMeta {
                    claude_code: Some(ClaudeCodeMeta {
                        tool_name: Some(tool_name.into()),
                    }),
                }),
            },
            options: Vec::new(),
        }
    }

    #[test]
    fn exit_plan_is_detected_from_meta_and_preserves_the_full_snapshot() {
        let parsed = exit_plan_submission(&request("ExitPlanMode", serde_json::json!({"plan": "# Plan\nA"})))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.session_id, "session-1");
        assert_eq!(parsed.call_id, "call-1");
        assert_eq!(parsed.markdown, "# Plan\nA");

        assert!(
            exit_plan_submission(&request("Bash", serde_json::json!({"plan": "not a plan call"})))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn malformed_exit_plan_never_falls_back_to_a_generic_approval() {
        assert!(exit_plan_submission(&request("ExitPlanMode", serde_json::json!({}))).is_err());
        assert!(exit_plan_submission(&request("ExitPlanMode", serde_json::json!({"plan": "  "}))).is_err());
    }

    #[test]
    fn review_choices_are_always_one_shot() {
        let option = |id: &str, kind: &str| PermissionOption {
            option_id: id.into(),
            name: id.into(),
            kind: kind.into(),
        };
        let choices = ReviewChoices::pick(&[
            option("forever", "allow_always"),
            option("yes", "allow_once"),
            option("never", "reject_always"),
            option("no", "reject_once"),
        ])
        .unwrap();
        assert_eq!(choices.allow, "yes");
        assert_eq!(choices.reject, "no");
        assert!(ReviewChoices::pick(&[option("forever", "allow_always"), option("never", "reject_always"),]).is_none());
    }

    #[tokio::test]
    async fn an_acp_snapshot_becomes_a_materialized_waiting_review() {
        let dir = tempfile::tempdir().unwrap();
        let services = bare_services(dir.path());
        {
            let mut conn = services.db.get().unwrap();
            crate::db::ops::conversation::create_conversation(&mut conn, "conversation-1", Some("plan"), None, None, 1)
                .unwrap();
            crate::db::ops::turn::begin(&mut conn, "turn-1", "conversation-1", TurnOrigin::ClaudeCode, None, 2)
                .unwrap();
        }

        let submitted = submit(
            &services,
            "conversation-1",
            "turn-1",
            "message-1",
            ExitPlanSubmission {
                session_id: "session-1".into(),
                call_id: "call-1".into(),
                markdown: "# Plan\n\n- one\n".into(),
            },
        )
        .await
        .unwrap();

        let mut conn = services.db.get().unwrap();
        let bundle = ops::get_review_bundle(&mut conn, &submitted.event.review_id).unwrap();
        assert_eq!(bundle.review.provider_kind, PlanReviewProviderKind::Acp.as_str());
        assert_eq!(bundle.review.provider_call_id.as_deref(), Some("call-1"));
        assert_eq!(bundle.review.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(bundle.submitted_revision.content_markdown, "# Plan\n\n- one\n");
        let snapshot = services
            .plan_files
            .read_document(&mut conn, &bundle.document.id)
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.content, bundle.submitted_revision.content_markdown);
        let turn: TurnRow = crate::db::schema::turns::table.find("turn-1").first(&mut conn).unwrap();
        assert_eq!(turn.status, TurnStatus::WaitingReview.as_str());
    }

    #[tokio::test]
    async fn the_live_control_releases_the_lease_signal_and_keeps_exact_option_ids() {
        let control = ReviewControl::default();
        let mut pause = control.subscribe();
        let submitted = SubmittedReview {
            event: PlanReviewEvent {
                review_id: "review-1".into(),
                conversation_id: "conversation-1".into(),
                document_id: "document-1".into(),
                revision_id: "revision-1".into(),
                turn_id: "turn-1".into(),
                status: "pending".into(),
                lock_version: 0,
                delivery_state: None,
            },
            session_id: "session-1".into(),
            call_id: "call-1".into(),
        };
        let decision = control
            .install(
                &submitted,
                "turn-1",
                ReviewChoices {
                    allow: "allow-once-id".into(),
                    reject: "reject-once-id".into(),
                },
            )
            .unwrap();
        pause.changed().await.unwrap();
        assert_eq!(pause.borrow().as_deref(), Some("turn-1"));

        let completed = control
            .resolve("review-1", ReviewDecisionAction::RequestChanges)
            .unwrap();
        let decision = decision.await.unwrap();
        assert_eq!(decision.option_id, "reject-once-id");
        control.arm("turn-1", decision.completion);
        control
            .take_boundary("turn-1")
            .unwrap()
            .complete(DeliveryBoundary::Acknowledged);
        assert_eq!(completed.await.unwrap(), DeliveryBoundary::Acknowledged);
    }
}

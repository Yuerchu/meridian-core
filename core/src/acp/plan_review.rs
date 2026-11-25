//! Synchronous human review for ACP's `ExitPlanMode` permission boundary.
//!
//! Claude Code exposes plan submission as a `session/request_permission`
//! request whose `rawInput.plan` carries the full plan snapshot. This module
//! imports that snapshot into the durable `plan_documents` / `plan_revisions` /
//! `plan_review_sessions` tables (so Meridian's `PlanReviewPage` editor,
//! inline comments, and revision diff work identically for Native and ACP) and
//! parks synchronously on the live permission request until the user decides or
//! cancels the turn — matching Claude Code's native `ExitPlanMode` lifecycle.

use std::sync::Mutex;

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::db::entity::plan_review_session::{PlanReviewProviderKind, PlanReviewState};
use crate::db::sea::ops::plan_review as ops;
use crate::events::PlanReviewEvent;
use crate::services::Services;
use crate::util::now_ms;

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
/// captured.
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

/// The permission option ids Meridian can answer `ExitPlanMode` with.
///
/// - `allow_default`: approves the plan in manual-confirmation (`default`) mode.
/// - `allow_accept_edits`: optional `acceptEdits` approval option when offered
///   by the adapter (never `bypassPermissions`).
/// - `reject`: stays in plan mode so the user's requested changes can be applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReviewChoices {
    pub allow_default: String,
    pub allow_accept_edits: Option<String>,
    pub reject: String,
}

impl ReviewChoices {
    pub fn pick(options: &[PermissionOption]) -> Option<Self> {
        let allow_default = options
            .iter()
            .find(|o| o.is_allow() && o.option_id == "default")
            .or_else(|| options.iter().find(|o| o.is_once() && o.is_allow()))?;
        let allow_accept_edits = options
            .iter()
            .find(|o| o.is_allow() && matches!(o.option_id.as_str(), "acceptEdits" | "accept_edits"))
            .map(|o| o.option_id.clone());
        let reject = options
            .iter()
            .find(|o| o.is_reject() && o.option_id == "plan")
            .or_else(|| options.iter().find(|o| o.is_once() && o.is_reject()))?;
        Some(Self {
            allow_default: allow_default.option_id.clone(),
            allow_accept_edits,
            reject: reject.option_id.clone(),
        })
    }
}

/// The review event produced when an ACP `ExitPlanMode` snapshot is submitted.
#[derive(Debug, Clone)]
pub(super) struct SubmittedReview {
    pub event: PlanReviewEvent,
}

/// Import one full ACP snapshot as a revision and open a pending review row.
pub(super) async fn submit(
    services: &Services,
    conversation_id: &str,
    turn_id: &str,
    assistant_message_id: &str,
    submission: ExitPlanSubmission,
) -> Result<SubmittedReview, String> {
    let db = &services.db;
    let not_twice = async |tx: &crate::db::sea::cap::WriteTx| -> Result<(), ops::PlanReviewStoreError> {
        if ops::get_pending_review_for_conversation(tx, conversation_id)
            .await?
            .is_some()
        {
            return Err("this conversation already has a plan awaiting review".into());
        }
        Ok(())
    };

    let (document_id, revision) = db
        .write(async |tx| {
            not_twice(tx).await?;
            let now = now_ms();
            let document = ops::create_or_resume_document(tx, conversation_id, now).await?;
            let head = ops::get_head_revision(tx, &document.id).await?;
            let desired_sha = ops::markdown_sha256(&submission.markdown);
            if let Some(head) = head.clone().filter(|revision| revision.content_sha256 == desired_sha) {
                return Ok::<_, ops::PlanReviewStoreError>((document.id, head));
            }
            let before = head
                .as_ref()
                .map(|revision| revision.content_markdown.as_str())
                .unwrap_or("");
            let patch = similar::TextDiff::from_lines(before, &submission.markdown)
                .unified_diff()
                .context_radius(3)
                .header("a/plan.md", "b/plan.md")
                .to_string();
            let responding_to = ops::list_reviews(tx, &document.id)
                .await?
                .into_iter()
                .rev()
                .find_map(|review| {
                    (review.state == PlanReviewState::ChangesRequested)
                        .then_some(review.suggestion_revision_id)
                        .flatten()
                });
            let appended = ops::append_assistant_revision(
                tx,
                &ops::PlanRevisionAppend {
                    document_id: &document.id,
                    expected_generation: document.working_generation,
                    expected_head_sha256: head.as_ref().map(|revision| revision.content_sha256.as_str()),
                    content_markdown: &submission.markdown,
                    patch: &patch,
                    source_message_id: Some(assistant_message_id),
                    source_call_id: Some(&submission.call_id),
                    responding_to_suggestion_revision_id: responding_to.as_deref(),
                    now,
                },
            )
            .await?;
            Ok((document.id, appended.revision))
        })
        .await
        .map_err(|error| error.to_string())?;

    let materialized = services
        .plan_files
        .reconcile_document(db, &document_id, now_ms())
        .await
        .map_err(|error| error.to_string())?;
    if materialized.conflict.is_some() {
        return Err("the durable plan.md projection is in conflict".into());
    }

    let event = db
        .write(async |tx| {
            not_twice(tx).await?;
            let document = ops::get_document(tx, &document_id).await?;
            let bundle = ops::submit_head_for_review(
                tx,
                &ops::PlanReviewSubmit {
                    document_id: &document.id,
                    expected_generation: document.working_generation,
                    expected_head_sha256: &revision.content_sha256,
                    turn_id: Some(turn_id),
                    assistant_message_id: Some(assistant_message_id),
                    provider_call_id: Some(&submission.call_id),
                    provider_kind: PlanReviewProviderKind::Acp,
                    now: now_ms(),
                },
            )
            .await?;
            Ok::<_, ops::PlanReviewStoreError>(PlanReviewEvent {
                review_id: bundle.review.id,
                conversation_id: conversation_id.to_string(),
                document_id: bundle.document.id,
                revision_id: bundle.submitted_revision.id,
                turn_id: turn_id.to_string(),
                status: bundle.review.state.as_str().to_string(),
                lock_version: bundle.review.lock_version,
            })
        })
        .await
        .map_err(|error| error.to_string())?;

    Ok(SubmittedReview { event })
}

/// Decision sent from `decide_plan_review` into a live ACP session.
#[derive(Debug, Clone)]
pub enum AcpPlanReviewResolution {
    Approve {
        review_id: String,
        accept_edits: bool,
        clear_context: bool,
        plan_markdown: String,
        edited_by_user: bool,
        feedback: Option<String>,
    },
    RequestChanges {
        review_id: String,
        feedback: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AcpPlanReply {
    Approve { accept_edits: bool },
    RequestChanges,
    CancelForClearContext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PermissionReply {
    Selected(String),
    Cancelled,
}

struct PendingReview {
    review_id: String,
    turn_id: String,
    choices: ReviewChoices,
    decision: oneshot::Sender<PermissionReply>,
}

/// In-memory waiter for one live ACP `ExitPlanMode` permission request.
#[derive(Default)]
pub(super) struct ReviewControl {
    pending: Mutex<Option<PendingReview>>,
}

impl ReviewControl {
    fn install(
        &self,
        review: &SubmittedReview,
        turn_id: &str,
        choices: ReviewChoices,
    ) -> Result<oneshot::Receiver<PermissionReply>, String> {
        let (decision, receive) = oneshot::channel();
        let mut pending = self.pending.lock().map_err(|_| "ACP plan review lock was poisoned")?;
        if pending.is_some() {
            return Err("this ACP session already has a live plan review request".into());
        }
        *pending = Some(PendingReview {
            review_id: review.event.review_id.clone(),
            turn_id: turn_id.to_string(),
            choices,
            decision,
        });
        Ok(receive)
    }

    pub fn has_pending(&self, review_id: &str) -> bool {
        self.pending
            .lock()
            .ok()
            .and_then(|pending| pending.as_ref().map(|p| p.review_id == review_id))
            .unwrap_or(false)
    }

    pub fn is_waiting_turn(&self, turn_id: &str) -> bool {
        self.pending
            .lock()
            .ok()
            .and_then(|pending| pending.as_ref().map(|p| p.turn_id == turn_id))
            .unwrap_or(false)
    }

    /// Resolve the live `ExitPlanMode` waiter. Returns `true` when the chosen
    /// `option_id` already handled `acceptEdits` natively.
    pub fn resolve(&self, review_id: &str, reply: AcpPlanReply) -> Result<bool, String> {
        let mut pending = self.pending.lock().map_err(|_| "ACP plan review lock was poisoned")?;
        let current = pending
            .as_ref()
            .ok_or("the ACP plan review request is no longer active")?;
        if current.review_id != review_id {
            return Err("the active ACP plan review belongs to a different review".into());
        }
        let pending = pending.take().expect("checked above");
        let (wire, used_accept_edits_option) = match reply {
            AcpPlanReply::Approve { accept_edits: true } => {
                if let Some(id) = pending.choices.allow_accept_edits {
                    (PermissionReply::Selected(id), true)
                } else {
                    (PermissionReply::Selected(pending.choices.allow_default), false)
                }
            }
            AcpPlanReply::Approve { accept_edits: false } => {
                (PermissionReply::Selected(pending.choices.allow_default), false)
            }
            AcpPlanReply::RequestChanges => (PermissionReply::Selected(pending.choices.reject), false),
            AcpPlanReply::CancelForClearContext => (PermissionReply::Cancelled, false),
        };
        pending
            .decision
            .send(wire)
            .map_err(|_| "the ACP plan permission request ended before it could be answered")?;
        Ok(used_accept_edits_option)
    }

    pub fn cancel(&self, review_id: &str) -> bool {
        if let Ok(mut pending) = self.pending.lock()
            && pending.as_ref().is_some_and(|pending| pending.review_id == review_id)
        {
            pending.take();
            return true;
        }
        false
    }
}

pub(super) struct PermissionWait(oneshot::Receiver<PermissionReply>);

/// Install the live permission sender before announcing the review so a fast
/// user click cannot race ahead of the waiter.
pub(super) fn install_permission_wait(
    control: &ReviewControl,
    review: &SubmittedReview,
    turn_id: &str,
    choices: ReviewChoices,
) -> Result<PermissionWait, String> {
    control.install(review, turn_id, choices).map(PermissionWait)
}

/// Wait for the decision and turn it into the ACP JSON-RPC permission reply.
/// If the turn is cancelled before the user decides, marks the review orphaned
/// and announces the update.
pub(super) async fn await_permission_decision(
    services: &Services,
    control: &ReviewControl,
    review: &SubmittedReview,
    cancel: &CancellationToken,
    wait: PermissionWait,
) -> serde_json::Value {
    let reply = tokio::select! {
        reply = wait.0 => reply.ok(),
        _ = cancel.cancelled() => None,
    };
    match reply {
        Some(PermissionReply::Selected(option_id)) => super::protocol::permission_selected(&option_id),
        Some(PermissionReply::Cancelled) => super::protocol::permission_cancelled(),
        None => {
            control.cancel(&review.event.review_id);
            let orphaned = services
                .db
                .write(async |tx| {
                    ops::orphan_review(
                        tx,
                        &review.event.review_id,
                        "The turn was stopped before this plan was decided",
                        now_ms(),
                    )
                    .await
                })
                .await;
            if let Ok(Some((doc, rev))) = orphaned {
                let event = PlanReviewEvent {
                    review_id: rev.id,
                    conversation_id: doc.conversation_id,
                    document_id: rev.document_id,
                    revision_id: rev.submitted_revision_id,
                    turn_id: rev.turn_id.unwrap_or_default(),
                    status: rev.state.as_str().to_string(),
                    lock_version: rev.lock_version,
                };
                let _ = services.events.emit_plan_review_updated(&event);
            }
            super::protocol::permission_cancelled()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::protocol::{ClaudeCodeMeta, ToolCall, ToolCallMeta};
    use crate::db::models::turn::TurnStatus;
    use crate::services::bare_services;
    use crate::turn::TurnOrigin;

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
                raw_output: None,
                locations: Vec::new(),
                name: None,
                meta: Some(ToolCallMeta {
                    claude_code: Some(ClaudeCodeMeta {
                        tool_name: Some(tool_name.into()),
                        non_execution_kind: None,
                    }),
                }),
            },
            options: Vec::new(),
        }
    }

    #[test]
    fn exit_plan_is_detected_as_the_adapter_actually_sends_it() {
        let params: RequestPermissionParams = serde_json::from_value(serde_json::json!({
            "sessionId": "session-1",
            "toolCall": {
                "toolCallId": "call-1",
                "name": "ExitPlanMode",
                "status": "pending",
                "title": "Ready to code?",
                "kind": "switch_mode",
                "rawInput": { "plan": "# Plan\nA" },
            },
            "options": [],
        }))
        .unwrap();
        let parsed = exit_plan_submission(&params).unwrap().expect("recognised as a plan");
        assert_eq!(parsed.markdown, "# Plan\nA");
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
    fn review_choices_pick_default_accept_edits_and_reject_without_bypass() {
        let option = |id: &str, kind: &str| PermissionOption {
            option_id: id.into(),
            name: id.into(),
            kind: kind.into(),
        };
        let choices = ReviewChoices::pick(&[
            option("bypassPermissions", "allow_always"),
            option("acceptEdits", "allow_always"),
            option("default", "allow_once"),
            option("plan", "reject_once"),
        ])
        .unwrap();
        assert_eq!(choices.allow_default, "default");
        assert_eq!(choices.allow_accept_edits.as_deref(), Some("acceptEdits"));
        assert_eq!(choices.reject, "plan");

        // Never falls back to bypassPermissions.
        let basic = ReviewChoices::pick(&[
            option("bypassPermissions", "allow_always"),
            option("yes", "allow_once"),
            option("no", "reject_once"),
        ])
        .unwrap();
        assert_eq!(basic.allow_default, "yes");
        assert_eq!(basic.allow_accept_edits, None);
        assert_eq!(basic.reject, "no");

        assert!(ReviewChoices::pick(&[option("forever", "allow_always"), option("never", "reject_always"),]).is_none());
    }

    #[tokio::test]
    async fn an_acp_snapshot_becomes_a_materialized_pending_review_on_a_running_turn() {
        let dir = tempfile::tempdir().unwrap();
        let services = bare_services(dir.path()).await;
        services
            .db
            .write(async |tx| {
                crate::db::sea::ops::conversation::create(tx, "conversation-1", Some("plan"), None, None, 1).await?;
                crate::db::sea::ops::turn::begin(tx, "turn-1", "conversation-1", TurnOrigin::ClaudeCode, None, 2).await
            })
            .await
            .unwrap();

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

        let bundle = services
            .db
            .read(async |tx| ops::get_review_bundle(tx, &submitted.event.review_id).await)
            .await
            .unwrap();
        assert_eq!(bundle.review.provider_kind, PlanReviewProviderKind::Acp);
        assert_eq!(bundle.review.provider_call_id.as_deref(), Some("call-1"));
        assert_eq!(bundle.review.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(bundle.submitted_revision.content_markdown, "# Plan\n\n- one\n");
        let turn = crate::db::sea::ops::turn::get(&services.db, "turn-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(turn.status, TurnStatus::Running);
    }

    #[tokio::test]
    async fn live_control_resolves_approve_accept_edits_and_clear_context() {
        let control = ReviewControl::default();
        let submitted = SubmittedReview {
            event: PlanReviewEvent {
                review_id: "review-1".into(),
                conversation_id: "conversation-1".into(),
                document_id: "document-1".into(),
                revision_id: "revision-1".into(),
                turn_id: "turn-1".into(),
                status: "pending".into(),
                lock_version: 0,
            },
        };
        let rx = control
            .install(
                &submitted,
                "turn-1",
                ReviewChoices {
                    allow_default: "default".into(),
                    allow_accept_edits: Some("acceptEdits".into()),
                    reject: "plan".into(),
                },
            )
            .unwrap();
        assert!(control.has_pending("review-1"));
        let used_accept_edits = control
            .resolve("review-1", AcpPlanReply::Approve { accept_edits: true })
            .unwrap();
        assert!(used_accept_edits);
        assert_eq!(rx.await.unwrap(), PermissionReply::Selected("acceptEdits".into()));
    }
}

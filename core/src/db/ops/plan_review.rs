//! Durable plan document, review draft, comment and delivery operations.
//!
//! Every public mutation is safe to call inside a wider Diesel transaction.
//! Diesel implements nested transactions with savepoints, which lets the turn
//! runtime commit transcript rows beside these domain changes without opening
//! a crash window between them.

use std::collections::{HashMap, HashSet};

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::db::models::plan::ModeArtifactRow;
use crate::db::models::plan_review::*;
use crate::db::schema::{
    mode_artifacts, plan_comments, plan_documents, plan_materializations, plan_review_deliveries, plan_review_drafts,
    plan_review_sessions, plan_revisions,
};

#[derive(Debug, thiserror::Error)]
pub enum PlanReviewStoreError {
    #[error(transparent)]
    Database(#[from] diesel::result::Error),
    #[error("{0} was not found")]
    NotFound(&'static str),
    #[error("plan state conflict: {0}")]
    Conflict(String),
    #[error("invalid plan state: {0}")]
    InvalidState(String),
    #[error("{field} is not valid JSON: {message}")]
    InvalidJson { field: &'static str, message: String },
    #[error("{0}")]
    Contract(String),
}

impl From<String> for PlanReviewStoreError {
    fn from(value: String) -> Self {
        Self::Contract(value)
    }
}

pub type PlanReviewStoreResult<T> = Result<T, PlanReviewStoreError>;

pub fn markdown_sha256(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn json_object(value: &str, field: &'static str) -> PlanReviewStoreResult<()> {
    let parsed: serde_json::Value = serde_json::from_str(value).map_err(|error| PlanReviewStoreError::InvalidJson {
        field,
        message: error.to_string(),
    })?;
    if !parsed.is_object() {
        return Err(PlanReviewStoreError::InvalidJson {
            field,
            message: "expected an object".into(),
        });
    }
    Ok(())
}

fn optional_json_object(value: Option<&str>, field: &'static str) -> PlanReviewStoreResult<()> {
    if let Some(value) = value {
        json_object(value, field)?;
    }
    Ok(())
}

#[derive(Debug)]
pub struct PlanRevisionAppend<'a> {
    pub document_id: &'a str,
    pub expected_generation: i64,
    pub expected_head_sha256: Option<&'a str>,
    pub content_markdown: &'a str,
    pub patch: &'a str,
    pub source_message_id: Option<&'a str>,
    pub source_call_id: Option<&'a str>,
    pub responding_to_suggestion_revision_id: Option<&'a str>,
    pub now: i64,
}

#[derive(Debug)]
pub struct PlanRevisionAppendResult {
    pub document: PlanDocumentRow,
    pub revision: PlanRevisionRow,
    pub materialization: PlanMaterializationRow,
}

#[derive(Debug)]
pub struct PlanReviewSubmit<'a> {
    pub document_id: &'a str,
    pub expected_generation: i64,
    pub expected_head_sha256: &'a str,
    pub turn_id: Option<&'a str>,
    pub assistant_message_id: Option<&'a str>,
    pub provider_call_id: Option<&'a str>,
    pub provider_kind: PlanReviewProviderKind,
    pub now: i64,
}

#[derive(Debug, Clone)]
pub struct PlanCommentSave<'a> {
    pub id: &'a str,
    pub position: i32,
    pub state: PlanCommentState,
    pub anchor_kind: PlanCommentAnchorKind,
    pub anchor_json: &'a str,
    pub body: &'a str,
}

#[derive(Debug)]
pub struct PlanReviewDraftSave<'a> {
    pub review_id: &'a str,
    pub expected_generation: i64,
    pub mode: PlanReviewDraftMode,
    pub base_editor_json: Option<&'a str>,
    pub draft_editor_json: Option<&'a str>,
    pub base_normalized_markdown: &'a str,
    pub draft_normalized_markdown: &'a str,
    pub source_text: Option<&'a str>,
    pub editor_schema_version: Option<i32>,
    pub editor_schema_hash: Option<&'a str>,
    /// Present only for the one-time rich -> source fallback caused by an
    /// incompatible persisted editor schema. The old identity is matched
    /// under the same generation CAS before the immutable baseline may change.
    pub schema_fallback_from_version: Option<i32>,
    pub schema_fallback_from_hash: Option<&'a str>,
    pub global_note: Option<&'a str>,
    pub selection_json: Option<&'a str>,
    pub comments: &'a [PlanCommentSave<'a>],
    pub now: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanReviewDecisionAction {
    Approve,
    RequestChanges,
}

#[derive(Debug)]
pub struct PlanReviewDecision<'a> {
    pub review_id: &'a str,
    pub decision_id: &'a str,
    pub expected_lock_version: i64,
    pub expected_draft_generation: i64,
    pub expected_draft_sha256: &'a str,
    pub action: PlanReviewDecisionAction,
    pub decision_summary: Option<&'a str>,
    pub delivery_target: Option<PlanDeliveryTarget>,
    pub target_session_id: Option<&'a str>,
    pub target_turn_id: Option<&'a str>,
    pub now: i64,
}

#[derive(Debug)]
pub struct PlanReviewDecisionResult {
    pub document: PlanDocumentRow,
    pub review: PlanReviewSessionRow,
    pub suggestion: Option<PlanRevisionRow>,
    pub delivery: Option<PlanReviewDeliveryRow>,
}

#[derive(Debug)]
pub struct PlanReviewBundle {
    pub document: PlanDocumentRow,
    pub submitted_revision: PlanRevisionRow,
    pub review: PlanReviewSessionRow,
    pub draft: PlanReviewDraftRow,
    pub comments: Vec<PlanCommentRow>,
    pub deliveries: Vec<PlanReviewDeliveryRow>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlanFeedbackComment<'a> {
    id: &'a str,
    state: &'a str,
    anchor_kind: &'a str,
    anchor: serde_json::Value,
    body: &'a str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlanFeedbackEnvelope<'a> {
    schema_version: u32,
    delivery_id: &'a str,
    review_id: &'a str,
    document_id: &'a str,
    base_revision_id: &'a str,
    base_sha256: &'a str,
    suggestion_revision_id: &'a str,
    suggested_markdown: &'a str,
    suggested_patch: Option<&'a str>,
    comments: Vec<PlanFeedbackComment<'a>>,
    global_note: Option<&'a str>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlanApprovalEnvelope<'a> {
    schema_version: u32,
    delivery_id: &'a str,
    action: &'static str,
    review_id: &'a str,
    document_id: &'a str,
    approved_revision_id: &'a str,
    approved_sha256: &'a str,
}

pub fn get_document(conn: &mut SqliteConnection, id: &str) -> PlanReviewStoreResult<PlanDocumentRow> {
    plan_documents::table
        .find(id)
        .first(conn)
        .optional()?
        .ok_or(PlanReviewStoreError::NotFound("plan document"))
}

pub fn get_active_document(
    conn: &mut SqliteConnection,
    conversation_id: &str,
) -> PlanReviewStoreResult<Option<PlanDocumentRow>> {
    Ok(plan_documents::table
        .filter(plan_documents::conversation_id.eq(conversation_id))
        .filter(plan_documents::state.ne(PlanDocumentState::Done.as_str()))
        .order(plan_documents::created_at.desc())
        .first(conn)
        .optional()?)
}

pub fn list_active_documents(conn: &mut SqliteConnection) -> PlanReviewStoreResult<Vec<PlanDocumentRow>> {
    Ok(plan_documents::table
        .filter(plan_documents::state.ne(PlanDocumentState::Done.as_str()))
        .order(plan_documents::created_at.asc())
        .load(conn)?)
}

pub fn create_or_resume_document(
    conn: &mut SqliteConnection,
    conversation_id: &str,
    now: i64,
) -> PlanReviewStoreResult<PlanDocumentRow> {
    conn.transaction(|conn| {
        if let Some(document) = get_active_document(conn, conversation_id)? {
            return Ok(document);
        }

        let id = uuid::Uuid::new_v4().to_string();
        let file_rel_path = format!("plans/{id}/plan.md");
        diesel::insert_or_ignore_into(plan_documents::table)
            .values(&PlanDocumentInsert {
                id: &id,
                conversation_id,
                state: PlanDocumentState::Drafting.as_str(),
                head_revision_id: None,
                approved_revision_id: None,
                working_generation: 0,
                file_rel_path: &file_rel_path,
                lock_version: 0,
                created_at: now,
                updated_at: now,
            })
            .execute(conn)?;

        get_active_document(conn, conversation_id)?.ok_or(PlanReviewStoreError::NotFound("plan document"))
    })
}

pub fn get_revision(conn: &mut SqliteConnection, id: &str) -> PlanReviewStoreResult<PlanRevisionRow> {
    plan_revisions::table
        .find(id)
        .first(conn)
        .optional()?
        .ok_or(PlanReviewStoreError::NotFound("plan revision"))
}

pub fn get_head_revision(
    conn: &mut SqliteConnection,
    document_id: &str,
) -> PlanReviewStoreResult<Option<PlanRevisionRow>> {
    let document = get_document(conn, document_id)?;
    document
        .head_revision_id
        .as_deref()
        .map(|id| get_revision(conn, id))
        .transpose()
}

pub fn get_approved_revision_for_conversation(
    conn: &mut SqliteConnection,
    conversation_id: &str,
) -> PlanReviewStoreResult<Option<PlanRevisionRow>> {
    let document = plan_documents::table
        .filter(plan_documents::conversation_id.eq(conversation_id))
        .filter(plan_documents::state.ne(PlanDocumentState::Done.as_str()))
        .filter(plan_documents::approved_revision_id.is_not_null())
        .order(plan_documents::updated_at.desc())
        .first::<PlanDocumentRow>(conn)
        .optional()?;
    document
        .and_then(|row| row.approved_revision_id)
        .as_deref()
        .map(|id| get_revision(conn, id))
        .transpose()
}

pub fn format_approved_plan_block(revision: &PlanRevisionRow) -> Option<String> {
    let content = revision.content_markdown.trim();
    if content.is_empty() {
        return None;
    }
    Some(format!("\n\n<approved_plan>\n{content}\n</approved_plan>"))
}

pub fn list_revisions(conn: &mut SqliteConnection, document_id: &str) -> PlanReviewStoreResult<Vec<PlanRevisionRow>> {
    Ok(plan_revisions::table
        .filter(plan_revisions::document_id.eq(document_id))
        .order(plan_revisions::revision_no.asc())
        .load(conn)?)
}

fn next_revision_no(conn: &mut SqliteConnection, document_id: &str) -> QueryResult<i64> {
    plan_revisions::table
        .filter(plan_revisions::document_id.eq(document_id))
        .select(diesel::dsl::max(plan_revisions::revision_no))
        .first::<Option<i64>>(conn)
        .map(|value| value.unwrap_or(0) + 1)
}

pub fn append_assistant_revision(
    conn: &mut SqliteConnection,
    append: &PlanRevisionAppend<'_>,
) -> PlanReviewStoreResult<PlanRevisionAppendResult> {
    conn.transaction(|conn| {
        let document = get_document(conn, append.document_id)?;
        if document.state()? == PlanDocumentState::Done {
            return Err(PlanReviewStoreError::InvalidState("the plan document is done".into()));
        }
        if document.working_generation != append.expected_generation {
            return Err(PlanReviewStoreError::Conflict(format!(
                "expected generation {}, found {}",
                append.expected_generation, document.working_generation
            )));
        }

        let current = document
            .head_revision_id
            .as_deref()
            .map(|id| get_revision(conn, id))
            .transpose()?;
        let current_sha = current.as_ref().map(|revision| revision.content_sha256.as_str());
        if current_sha != append.expected_head_sha256 {
            return Err(PlanReviewStoreError::Conflict(format!(
                "expected head hash {:?}, found {:?}",
                append.expected_head_sha256, current_sha
            )));
        }
        if plan_review_sessions::table
            .filter(plan_review_sessions::document_id.eq(append.document_id))
            .filter(plan_review_sessions::state.eq(PlanReviewState::Pending.as_str()))
            .select(plan_review_sessions::id)
            .first::<String>(conn)
            .optional()?
            .is_some()
        {
            return Err(PlanReviewStoreError::InvalidState(
                "the current plan revision is still awaiting review".into(),
            ));
        }

        let content_sha256 = markdown_sha256(append.content_markdown);
        if current_sha == Some(content_sha256.as_str()) {
            return Err(PlanReviewStoreError::InvalidState(
                "the plan patch made no change".into(),
            ));
        }
        let revision_id = uuid::Uuid::new_v4().to_string();
        let revision_no = next_revision_no(conn, append.document_id)?;
        diesel::insert_into(plan_revisions::table)
            .values(&PlanRevisionInsert {
                id: &revision_id,
                document_id: append.document_id,
                revision_no,
                parent_revision_id: document.head_revision_id.as_deref(),
                author_kind: PlanRevisionAuthorKind::Assistant.as_str(),
                content_markdown: append.content_markdown,
                content_sha256: &content_sha256,
                patch: Some(append.patch),
                source_message_id: append.source_message_id,
                source_call_id: append.source_call_id,
                responding_to_suggestion_revision_id: append.responding_to_suggestion_revision_id,
                editor_json: None,
                editor_schema_version: None,
                editor_schema_hash: None,
                legacy_source_artifact_id: None,
                created_at: append.now,
            })
            .execute(conn)?;

        let generation = document.working_generation + 1;
        let changed = diesel::update(
            plan_documents::table
                .find(append.document_id)
                .filter(plan_documents::working_generation.eq(append.expected_generation))
                .filter(plan_documents::lock_version.eq(document.lock_version)),
        )
        .set((
            plan_documents::head_revision_id.eq(Some(revision_id.as_str())),
            plan_documents::working_generation.eq(generation),
            plan_documents::state.eq(PlanDocumentState::Drafting.as_str()),
            plan_documents::lock_version.eq(document.lock_version + 1),
            plan_documents::updated_at.eq(append.now),
        ))
        .execute(conn)?;
        if changed != 1 {
            return Err(PlanReviewStoreError::Conflict(
                "the plan document changed concurrently".into(),
            ));
        }

        let materialization_id = uuid::Uuid::new_v4().to_string();
        diesel::insert_into(plan_materializations::table)
            .values(&PlanMaterializationInsert {
                id: &materialization_id,
                document_id: append.document_id,
                revision_id: &revision_id,
                generation,
                expected_sha256: current_sha,
                desired_sha256: &content_sha256,
                state: PlanMaterializationState::Pending.as_str(),
                force_replace: 0,
                error: None,
                created_at: append.now,
                updated_at: append.now,
                applied_at: None,
            })
            .execute(conn)?;

        Ok(PlanRevisionAppendResult {
            document: get_document(conn, append.document_id)?,
            revision: get_revision(conn, &revision_id)?,
            materialization: plan_materializations::table.find(&materialization_id).first(conn)?,
        })
    })
}

fn revision_chain_answers_suggestion(
    conn: &mut SqliteConnection,
    head: &PlanRevisionRow,
    submitted_revision_id: &str,
    suggestion_revision_id: &str,
) -> PlanReviewStoreResult<bool> {
    let mut cursor = head.clone();
    let mut visited = HashSet::new();
    let mut linked = false;
    while cursor.id != submitted_revision_id {
        if !visited.insert(cursor.id.clone()) {
            return Err(PlanReviewStoreError::Contract(
                "the plan revision ancestry contains a cycle".into(),
            ));
        }
        if cursor.author_kind()? != PlanRevisionAuthorKind::Assistant {
            return Ok(false);
        }
        linked |= cursor.responding_to_suggestion_revision_id.as_deref() == Some(suggestion_revision_id);
        let Some(parent_id) = cursor.parent_revision_id.as_deref() else {
            return Ok(false);
        };
        cursor = get_revision(conn, parent_id)?;
    }
    Ok(linked)
}

pub fn submit_head_for_review(
    conn: &mut SqliteConnection,
    submit: &PlanReviewSubmit<'_>,
) -> PlanReviewStoreResult<PlanReviewBundle> {
    submit_head_for_review_inner(conn, submit, None)
}

pub fn submit_native_head_for_review(
    conn: &mut SqliteConnection,
    submit: &PlanReviewSubmit<'_>,
    runtime: &NativePlanReviewRuntimeConfig,
) -> PlanReviewStoreResult<PlanReviewBundle> {
    submit_head_for_review_inner(conn, submit, Some(runtime))
}

fn submit_head_for_review_inner(
    conn: &mut SqliteConnection,
    submit: &PlanReviewSubmit<'_>,
    native_runtime: Option<&NativePlanReviewRuntimeConfig>,
) -> PlanReviewStoreResult<PlanReviewBundle> {
    match (submit.provider_kind, native_runtime) {
        (PlanReviewProviderKind::Native, Some(runtime)) => {
            if runtime.provider_id.trim().is_empty() || runtime.model.trim().is_empty() {
                return Err(PlanReviewStoreError::Contract(
                    "native plan-review runtime provider/model must not be empty".into(),
                ));
            }
        }
        (PlanReviewProviderKind::Native, None) => {
            return Err(PlanReviewStoreError::Contract(
                "native plan review submission requires its effective runtime config".into(),
            ));
        }
        (_, Some(_)) => {
            return Err(PlanReviewStoreError::Contract(
                "only native plan reviews may carry native runtime config".into(),
            ));
        }
        (_, None) => {}
    }
    let native_runtime_config_json = native_runtime
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| PlanReviewStoreError::Contract(format!("could not encode native runtime config: {error}")))?;
    conn.transaction(|conn| {
        let document = get_document(conn, submit.document_id)?;
        if document.working_generation != submit.expected_generation {
            return Err(PlanReviewStoreError::Conflict(format!(
                "expected generation {}, found {}",
                submit.expected_generation, document.working_generation
            )));
        }
        if document.state()? == PlanDocumentState::Done {
            return Err(PlanReviewStoreError::InvalidState("the plan document is done".into()));
        }
        let revision_id = document
            .head_revision_id
            .as_deref()
            .ok_or_else(|| PlanReviewStoreError::InvalidState("the plan has no revision to submit".into()))?;
        let revision = get_revision(conn, revision_id)?;
        if revision.content_sha256 != submit.expected_head_sha256 {
            return Err(PlanReviewStoreError::Conflict(format!(
                "expected head hash {}, found {}",
                submit.expected_head_sha256, revision.content_sha256
            )));
        }
        let previous_review = list_reviews(conn, submit.document_id)?
            .into_iter()
            .map(|review| {
                let submitted = get_revision(conn, &review.submitted_revision_id)?;
                Ok((review, submitted))
            })
            .collect::<PlanReviewStoreResult<Vec<_>>>()?
            .into_iter()
            .max_by_key(|(_, submitted)| submitted.revision_no);
        if let Some((previous_review, previous_submitted)) =
            previous_review.filter(|(review, _)| review.state == PlanReviewState::ChangesRequested.as_str())
        {
            let suggestion_revision_id = previous_review.suggestion_revision_id.as_deref().ok_or_else(|| {
                PlanReviewStoreError::InvalidState("the previous change request has no suggestion revision".into())
            })?;
            if revision.content_sha256 == previous_submitted.content_sha256
                || !revision_chain_answers_suggestion(
                    conn,
                    &revision,
                    &previous_review.submitted_revision_id,
                    suggestion_revision_id,
                )?
            {
                return Err(PlanReviewStoreError::InvalidState(
                    "a change request must be answered by a new update_plan ancestry linked to its suggestion".into(),
                ));
            }
        }
        let materialized = plan_materializations::table
            .filter(plan_materializations::document_id.eq(submit.document_id))
            .filter(plan_materializations::generation.eq(submit.expected_generation))
            .filter(plan_materializations::revision_id.eq(revision_id))
            .order(plan_materializations::created_at.desc())
            .first::<PlanMaterializationRow>(conn)
            .optional()?;
        if materialized.is_none_or(|row| row.state != PlanMaterializationState::Applied.as_str()) {
            return Err(PlanReviewStoreError::InvalidState(
                "plan.md has not been materialized at the submitted revision".into(),
            ));
        }

        let review_id = uuid::Uuid::new_v4().to_string();
        diesel::insert_into(plan_review_sessions::table)
            .values(&PlanReviewSessionInsert {
                id: &review_id,
                document_id: submit.document_id,
                submitted_revision_id: revision_id,
                turn_id: submit.turn_id,
                assistant_message_id: submit.assistant_message_id,
                provider_call_id: submit.provider_call_id,
                provider_kind: submit.provider_kind.as_str(),
                native_runtime_config_json: native_runtime_config_json.as_deref(),
                state: PlanReviewState::Pending.as_str(),
                decision_id: None,
                decision_summary: None,
                suggestion_revision_id: None,
                lock_version: 0,
                created_at: submit.now,
                updated_at: submit.now,
                decided_at: None,
            })
            .execute(conn)?;
        diesel::insert_into(plan_review_drafts::table)
            .values(&PlanReviewDraftInsert {
                review_id: &review_id,
                base_revision_id: revision_id,
                generation: 0,
                mode: PlanReviewDraftMode::Source.as_str(),
                base_editor_json: None,
                draft_editor_json: None,
                base_normalized_markdown: &revision.content_markdown,
                draft_normalized_markdown: &revision.content_markdown,
                source_text: Some(&revision.content_markdown),
                editor_schema_version: None,
                editor_schema_hash: None,
                global_note: None,
                selection_json: None,
                draft_sha256: &revision.content_sha256,
                created_at: submit.now,
                updated_at: submit.now,
            })
            .execute(conn)?;

        let changed = diesel::update(
            plan_documents::table
                .find(submit.document_id)
                .filter(plan_documents::working_generation.eq(submit.expected_generation))
                .filter(plan_documents::lock_version.eq(document.lock_version)),
        )
        .set((
            plan_documents::state.eq(PlanDocumentState::Reviewing.as_str()),
            plan_documents::lock_version.eq(document.lock_version + 1),
            plan_documents::updated_at.eq(submit.now),
        ))
        .execute(conn)?;
        if changed != 1 {
            return Err(PlanReviewStoreError::Conflict(
                "the plan document changed concurrently".into(),
            ));
        }

        if let Some(turn_id) = submit.turn_id {
            let changed = crate::db::ops::turn::wait_for_review(conn, turn_id, submit.now)?;
            if changed != 1 {
                return Err(PlanReviewStoreError::InvalidState(
                    "the submitting turn is not running".into(),
                ));
            }
        }

        get_review_bundle(conn, &review_id)
    })
}

pub fn get_review(conn: &mut SqliteConnection, review_id: &str) -> PlanReviewStoreResult<PlanReviewSessionRow> {
    plan_review_sessions::table
        .find(review_id)
        .first(conn)
        .optional()?
        .ok_or(PlanReviewStoreError::NotFound("plan review"))
}

pub fn get_pending_review_for_conversation(
    conn: &mut SqliteConnection,
    conversation_id: &str,
) -> PlanReviewStoreResult<Option<PlanReviewSessionRow>> {
    Ok(plan_review_sessions::table
        .inner_join(plan_documents::table)
        .filter(plan_documents::conversation_id.eq(conversation_id))
        .filter(plan_review_sessions::state.eq(PlanReviewState::Pending.as_str()))
        .select(PlanReviewSessionRow::as_select())
        .order(plan_review_sessions::created_at.desc())
        .first(conn)
        .optional()?)
}

/// Whether ordinary conversation traffic must stop at a durable plan-review
/// boundary.  A settled decision keeps the barrier while its continuation is
/// queued, being dispatched, held, or explicitly in doubt; otherwise a normal
/// prompt can race the decision command and take the conversation before the
/// approved/feedback continuation does.
pub fn has_conversation_barrier(conn: &mut SqliteConnection, conversation_id: &str) -> PlanReviewStoreResult<bool> {
    if get_pending_review_for_conversation(conn, conversation_id)?.is_some() {
        return Ok(true);
    }
    let document_ids = plan_documents::table
        .filter(plan_documents::conversation_id.eq(conversation_id))
        .select(plan_documents::id)
        .load::<String>(conn)?;
    if document_ids.is_empty() {
        return Ok(false);
    }
    let review_ids = plan_review_sessions::table
        .filter(plan_review_sessions::document_id.eq_any(document_ids))
        .select(plan_review_sessions::id)
        .load::<String>(conn)?;
    if review_ids.is_empty() {
        return Ok(false);
    }
    Ok(plan_review_deliveries::table
        .filter(plan_review_deliveries::review_id.eq_any(review_ids))
        .filter(plan_review_deliveries::state.eq_any([
            PlanDeliveryState::Queued.as_str(),
            PlanDeliveryState::Dispatched.as_str(),
            PlanDeliveryState::Held.as_str(),
            PlanDeliveryState::InDoubt.as_str(),
        ]))
        .select(plan_review_deliveries::id)
        .first::<String>(conn)
        .optional()?
        .is_some())
}

fn review_has_delivery_barrier(conn: &mut SqliteConnection, review_id: &str) -> PlanReviewStoreResult<bool> {
    Ok(plan_review_deliveries::table
        .filter(plan_review_deliveries::review_id.eq(review_id))
        .filter(plan_review_deliveries::state.eq_any([
            PlanDeliveryState::Queued.as_str(),
            PlanDeliveryState::Dispatched.as_str(),
            PlanDeliveryState::Held.as_str(),
            PlanDeliveryState::InDoubt.as_str(),
        ]))
        .select(plan_review_deliveries::id)
        .first::<String>(conn)
        .optional()?
        .is_some())
}

fn active_barrier_reviews_for_conversation(
    conn: &mut SqliteConnection,
    conversation_id: &str,
) -> PlanReviewStoreResult<Vec<PlanReviewSessionRow>> {
    let mut active = Vec::new();
    for review in list_reviews_for_conversation(conn, conversation_id)? {
        if review.state()? == PlanReviewState::Pending || review_has_delivery_barrier(conn, &review.id)? {
            active.push(review);
        }
    }
    Ok(active)
}

/// Conversations whose currently blocked native continuation depends on this
/// assistant. Historical settled reviews are deliberately ignored: an old
/// review using assistant A must not freeze A while an unrelated active review
/// uses assistant B.
pub fn barrier_conversations_for_assistant(
    conn: &mut SqliteConnection,
    assistant_id: &str,
) -> PlanReviewStoreResult<Vec<String>> {
    let mut blocked = Vec::new();
    for conversation_id in crate::db::ops::conversation::all_ids(conn)? {
        let active_reviews = active_barrier_reviews_for_conversation(conn, &conversation_id)?;
        if active_reviews.is_empty() {
            continue;
        }
        let conversation = crate::db::ops::conversation::get_conversation(conn, &conversation_id)?;
        let mut missing_runtime = false;
        let mut frozen = false;
        for review in active_reviews {
            match review.native_runtime_config()? {
                Some(runtime) => {
                    frozen |= runtime.assistant_id.as_deref() == Some(assistant_id);
                }
                None => missing_runtime = true,
            }
        }
        let standing_fallback = missing_runtime && conversation.assistant_id.as_deref() == Some(assistant_id);
        if frozen || standing_fallback {
            blocked.push(conversation_id);
        }
    }
    blocked.sort();
    blocked.dedup();
    Ok(blocked)
}

/// Conversations whose currently blocked native continuation depends on this
/// provider. Frozen runtime identity is authoritative; the conversation value
/// is consulted only for an active legacy row with no frozen snapshot.
pub fn barrier_conversations_for_provider(
    conn: &mut SqliteConnection,
    provider_id: &str,
) -> PlanReviewStoreResult<Vec<String>> {
    let mut blocked = Vec::new();
    for conversation_id in crate::db::ops::conversation::all_ids(conn)? {
        let active_reviews = active_barrier_reviews_for_conversation(conn, &conversation_id)?;
        if active_reviews.is_empty() {
            continue;
        }
        let conversation = crate::db::ops::conversation::get_conversation(conn, &conversation_id)?;
        let mut missing_runtime = false;
        let mut frozen = false;
        for review in active_reviews {
            match review.native_runtime_config()? {
                Some(runtime) => frozen |= runtime.provider_id == provider_id,
                None => missing_runtime = true,
            }
        }
        let standing_fallback = missing_runtime && conversation.agent_provider_id.as_deref() == Some(provider_id);
        if frozen || standing_fallback {
            blocked.push(conversation_id);
        }
    }
    blocked.sort();
    blocked.dedup();
    Ok(blocked)
}

/// Conversations whose blocked native continuation was resolved against this
/// exact provider/model capability record.
pub fn barrier_conversations_for_model(
    conn: &mut SqliteConnection,
    provider_id: &str,
    model: &str,
) -> PlanReviewStoreResult<Vec<String>> {
    let mut blocked = Vec::new();
    for conversation_id in crate::db::ops::conversation::all_ids(conn)? {
        for review in active_barrier_reviews_for_conversation(conn, &conversation_id)? {
            if review
                .native_runtime_config()?
                .is_some_and(|runtime| runtime.provider_id == provider_id && runtime.model == model)
            {
                blocked.push(conversation_id.clone());
                break;
            }
        }
    }
    blocked.sort();
    blocked.dedup();
    Ok(blocked)
}

pub fn list_reviews(
    conn: &mut SqliteConnection,
    document_id: &str,
) -> PlanReviewStoreResult<Vec<PlanReviewSessionRow>> {
    Ok(plan_review_sessions::table
        .filter(plan_review_sessions::document_id.eq(document_id))
        .order(plan_review_sessions::created_at.asc())
        .load(conn)?)
}

/// Review cards for every planning episode in one conversation.  The message
/// snapshot applies branch visibility; this query deliberately crosses active
/// and done documents so settled cards remain reconstructable after restart.
pub fn list_reviews_for_conversation(
    conn: &mut SqliteConnection,
    conversation_id: &str,
) -> PlanReviewStoreResult<Vec<PlanReviewSessionRow>> {
    Ok(plan_review_sessions::table
        .inner_join(plan_documents::table)
        .filter(plan_documents::conversation_id.eq(conversation_id))
        .select(PlanReviewSessionRow::as_select())
        .order(plan_review_sessions::created_at.asc())
        .load(conn)?)
}

pub fn get_review_bundle(conn: &mut SqliteConnection, review_id: &str) -> PlanReviewStoreResult<PlanReviewBundle> {
    let review = get_review(conn, review_id)?;
    let document = get_document(conn, &review.document_id)?;
    let submitted_revision = get_revision(conn, &review.submitted_revision_id)?;
    let draft = plan_review_drafts::table
        .find(review_id)
        .first(conn)
        .optional()?
        .ok_or(PlanReviewStoreError::NotFound("plan review draft"))?;
    let comments = plan_comments::table
        .filter(plan_comments::review_id.eq(review_id))
        .order(plan_comments::position.asc())
        .load(conn)?;
    let deliveries = plan_review_deliveries::table
        .filter(plan_review_deliveries::review_id.eq(review_id))
        .order(plan_review_deliveries::created_at.asc())
        .load(conn)?;
    Ok(PlanReviewBundle {
        document,
        submitted_revision,
        review,
        draft,
        comments,
        deliveries,
    })
}

fn review_is_pending(review: &PlanReviewSessionRow) -> PlanReviewStoreResult<()> {
    if review.state()? != PlanReviewState::Pending {
        return Err(PlanReviewStoreError::InvalidState(
            "the plan review is no longer pending".into(),
        ));
    }
    Ok(())
}

fn validate_draft_save(save: &PlanReviewDraftSave<'_>) -> PlanReviewStoreResult<()> {
    optional_json_object(save.base_editor_json, "baseEditorJson")?;
    optional_json_object(save.draft_editor_json, "draftEditorJson")?;
    optional_json_object(save.selection_json, "selectionJson")?;
    match save.mode {
        PlanReviewDraftMode::Rich => {
            if save.base_editor_json.is_none()
                || save.draft_editor_json.is_none()
                || save.source_text.is_some()
                || save.editor_schema_version.is_none()
                || save.editor_schema_hash.is_none()
            {
                return Err(PlanReviewStoreError::InvalidState(
                    "rich drafts require base and draft editor JSON, schema identity, and no source text".into(),
                ));
            }
        }
        PlanReviewDraftMode::Source => {
            if save.source_text.is_none()
                || save.base_editor_json.is_some()
                || save.draft_editor_json.is_some()
                || save.editor_schema_version.is_some()
                || save.editor_schema_hash.is_some()
            {
                return Err(PlanReviewStoreError::InvalidState(
                    "source drafts require exact source text and no editor projection".into(),
                ));
            }
            if save.source_text != Some(save.draft_normalized_markdown) {
                return Err(PlanReviewStoreError::InvalidState(
                    "source text and normalized markdown must be identical".into(),
                ));
            }
        }
    }
    if save.schema_fallback_from_version.is_none() && save.schema_fallback_from_hash.is_some() {
        return Err(PlanReviewStoreError::InvalidState(
            "schema fallback hash requires a source schema version".into(),
        ));
    }
    if save.schema_fallback_from_version.is_some() && save.mode != PlanReviewDraftMode::Source {
        return Err(PlanReviewStoreError::InvalidState(
            "editor schema fallback is only valid for source drafts".into(),
        ));
    }
    let mut ids = HashSet::new();
    let mut positions = HashSet::new();
    for comment in save.comments {
        if !ids.insert(comment.id) || !positions.insert(comment.position) {
            return Err(PlanReviewStoreError::InvalidState(
                "comment ids and positions must be unique within a review".into(),
            ));
        }
        if comment.position < 0 {
            return Err(PlanReviewStoreError::InvalidState(
                "comment positions cannot be negative".into(),
            ));
        }
        if comment.state == PlanCommentState::Submitted {
            return Err(PlanReviewStoreError::InvalidState(
                "a draft save cannot create submitted comments".into(),
            ));
        }
        if comment.state != PlanCommentState::Deleted && comment.body.trim().is_empty() {
            return Err(PlanReviewStoreError::InvalidState(
                "plan comments cannot be blank".into(),
            ));
        }
        json_object(comment.anchor_json, "comment.anchorJson")?;
    }
    Ok(())
}

pub fn save_review_draft(
    conn: &mut SqliteConnection,
    save: &PlanReviewDraftSave<'_>,
) -> PlanReviewStoreResult<PlanReviewBundle> {
    validate_draft_save(save)?;
    conn.transaction(|conn| {
        let review = get_review(conn, save.review_id)?;
        review_is_pending(&review)?;
        let current = plan_review_drafts::table
            .find(save.review_id)
            .first::<PlanReviewDraftRow>(conn)?;
        if current.generation != save.expected_generation {
            return Err(PlanReviewStoreError::Conflict(format!(
                "expected draft generation {}, found {}",
                save.expected_generation, current.generation
            )));
        }

        // The first save may establish the rich projection produced from the
        // immutable raw revision.  Once a generation has been acknowledged the
        // baseline is frozen, otherwise a later save could redefine “clean” and
        // make an edited plan approvable as the original.
        let establishing_projection = current.generation == 0
            && current.mode == PlanReviewDraftMode::Source.as_str()
            && save.mode == PlanReviewDraftMode::Rich;
        // A persisted rich projection from an incompatible editor schema is
        // allowed one lossless downgrade to source mode.  The raw submitted
        // revision becomes the immutable source baseline; all editor-shaped
        // fields are cleared.  Once stored as source this exception cannot be
        // taken again.
        let submitted_revision = get_revision(conn, &review.submitted_revision_id)?;
        let schema_mismatch_fallback = current.mode == PlanReviewDraftMode::Rich.as_str()
            && save.mode == PlanReviewDraftMode::Source
            && current.editor_schema_version.is_some()
            && save.schema_fallback_from_version == current.editor_schema_version
            && save.schema_fallback_from_hash == current.editor_schema_hash.as_deref()
            && save.base_editor_json.is_none()
            && save.draft_editor_json.is_none()
            && save.editor_schema_version.is_none()
            && save.editor_schema_hash.is_none()
            && save.base_normalized_markdown == submitted_revision.content_markdown;
        if !establishing_projection
            && !schema_mismatch_fallback
            && (current.base_editor_json.as_deref() != save.base_editor_json
                || current.base_normalized_markdown != save.base_normalized_markdown
                || current.editor_schema_version != save.editor_schema_version
                || current.editor_schema_hash.as_deref() != save.editor_schema_hash)
        {
            return Err(PlanReviewStoreError::Conflict(
                "the immutable review baseline cannot be changed".into(),
            ));
        }

        let draft_content = match save.mode {
            PlanReviewDraftMode::Rich => save.draft_normalized_markdown,
            PlanReviewDraftMode::Source => save.source_text.expect("validated source text"),
        };
        let draft_sha256 = markdown_sha256(draft_content);
        let generation = current.generation + 1;
        let changed = diesel::update(
            plan_review_drafts::table
                .find(save.review_id)
                .filter(plan_review_drafts::generation.eq(save.expected_generation)),
        )
        .set((
            plan_review_drafts::generation.eq(generation),
            plan_review_drafts::mode.eq(save.mode.as_str()),
            plan_review_drafts::base_editor_json.eq(save.base_editor_json),
            plan_review_drafts::draft_editor_json.eq(save.draft_editor_json),
            plan_review_drafts::base_normalized_markdown.eq(save.base_normalized_markdown),
            plan_review_drafts::draft_normalized_markdown.eq(save.draft_normalized_markdown),
            plan_review_drafts::source_text.eq(save.source_text),
            plan_review_drafts::editor_schema_version.eq(save.editor_schema_version),
            plan_review_drafts::editor_schema_hash.eq(save.editor_schema_hash),
            plan_review_drafts::global_note.eq(save.global_note),
            plan_review_drafts::selection_json.eq(save.selection_json),
            plan_review_drafts::draft_sha256.eq(&draft_sha256),
            plan_review_drafts::updated_at.eq(save.now),
        ))
        .execute(conn)?;
        if changed != 1 {
            return Err(PlanReviewStoreError::Conflict(
                "the review draft changed concurrently".into(),
            ));
        }

        let existing_created_at: HashMap<String, i64> = plan_comments::table
            .filter(plan_comments::review_id.eq(save.review_id))
            .select((plan_comments::id, plan_comments::created_at))
            .load(conn)?
            .into_iter()
            .collect();
        diesel::delete(
            plan_comments::table
                .filter(plan_comments::review_id.eq(save.review_id))
                .filter(plan_comments::state.ne(PlanCommentState::Submitted.as_str())),
        )
        .execute(conn)?;
        for comment in save.comments {
            diesel::insert_into(plan_comments::table)
                .values(&PlanCommentInsert {
                    id: comment.id,
                    review_id: save.review_id,
                    position: comment.position,
                    state: comment.state.as_str(),
                    anchor_kind: comment.anchor_kind.as_str(),
                    anchor_json: comment.anchor_json,
                    body: comment.body,
                    created_at: existing_created_at.get(comment.id).copied().unwrap_or(save.now),
                    updated_at: save.now,
                })
                .execute(conn)?;
        }
        get_review_bundle(conn, save.review_id)
    })
}

pub fn discard_review_draft(
    conn: &mut SqliteConnection,
    review_id: &str,
    expected_generation: i64,
    now: i64,
) -> PlanReviewStoreResult<PlanReviewBundle> {
    conn.transaction(|conn| {
        let review = get_review(conn, review_id)?;
        review_is_pending(&review)?;
        let current = plan_review_drafts::table
            .find(review_id)
            .first::<PlanReviewDraftRow>(conn)?;
        if current.generation != expected_generation {
            return Err(PlanReviewStoreError::Conflict(format!(
                "expected draft generation {expected_generation}, found {}",
                current.generation
            )));
        }
        let base_revision = get_revision(conn, &current.base_revision_id)?;
        let (source_text, draft_markdown, draft_sha256) = match current.mode()? {
            PlanReviewDraftMode::Rich => (
                None,
                current.base_normalized_markdown.clone(),
                markdown_sha256(&current.base_normalized_markdown),
            ),
            PlanReviewDraftMode::Source => (
                Some(base_revision.content_markdown.clone()),
                base_revision.content_markdown.clone(),
                base_revision.content_sha256,
            ),
        };
        let changed = diesel::update(
            plan_review_drafts::table
                .find(review_id)
                .filter(plan_review_drafts::generation.eq(expected_generation)),
        )
        .set((
            plan_review_drafts::generation.eq(expected_generation + 1),
            plan_review_drafts::draft_editor_json.eq(current.base_editor_json.as_deref()),
            plan_review_drafts::draft_normalized_markdown.eq(draft_markdown),
            plan_review_drafts::source_text.eq(source_text),
            plan_review_drafts::global_note.eq::<Option<&str>>(None),
            plan_review_drafts::selection_json.eq::<Option<&str>>(None),
            plan_review_drafts::draft_sha256.eq(draft_sha256),
            plan_review_drafts::updated_at.eq(now),
        ))
        .execute(conn)?;
        if changed != 1 {
            return Err(PlanReviewStoreError::Conflict(
                "the review draft changed concurrently".into(),
            ));
        }
        diesel::delete(
            plan_comments::table
                .filter(plan_comments::review_id.eq(review_id))
                .filter(plan_comments::state.ne(PlanCommentState::Submitted.as_str())),
        )
        .execute(conn)?;
        get_review_bundle(conn, review_id)
    })
}

fn draft_is_dirty(draft: &PlanReviewDraftRow) -> PlanReviewStoreResult<bool> {
    Ok(match draft.mode()? {
        PlanReviewDraftMode::Rich => draft.draft_normalized_markdown != draft.base_normalized_markdown,
        PlanReviewDraftMode::Source => draft
            .source_text
            .as_deref()
            .is_none_or(|source| markdown_sha256(source) != markdown_sha256(&draft.base_normalized_markdown)),
    })
}

fn meaningful_comments(comments: &[PlanCommentRow]) -> Vec<&PlanCommentRow> {
    comments
        .iter()
        .filter(|comment| comment.state != PlanCommentState::Deleted.as_str())
        .collect()
}

pub fn markdown_diff(base: &str, draft: &str) -> Option<String> {
    if base == draft {
        return None;
    }
    Some(
        similar::TextDiff::from_lines(base, draft)
            .unified_diff()
            .context_radius(3)
            .header("a/plan.md", "b/plan.md")
            .to_string(),
    )
}

fn decision_matches(review: &PlanReviewSessionRow, action: PlanReviewDecisionAction) -> bool {
    matches!(
        (review.state.as_str(), action),
        ("approved", PlanReviewDecisionAction::Approve)
            | ("changes_requested", PlanReviewDecisionAction::RequestChanges)
    )
}

pub fn decide_review(
    conn: &mut SqliteConnection,
    decision: &PlanReviewDecision<'_>,
) -> PlanReviewStoreResult<PlanReviewDecisionResult> {
    conn.transaction(|conn| {
        let review = get_review(conn, decision.review_id)?;
        if review.state()? != PlanReviewState::Pending {
            if review.decision_id.as_deref() == Some(decision.decision_id) && decision_matches(&review, decision.action)
            {
                let suggestion = review
                    .suggestion_revision_id
                    .as_deref()
                    .map(|id| get_revision(conn, id))
                    .transpose()?;
                let delivery = plan_review_deliveries::table
                    .filter(plan_review_deliveries::review_id.eq(decision.review_id))
                    .first(conn)
                    .optional()?;
                return Ok(PlanReviewDecisionResult {
                    document: get_document(conn, &review.document_id)?,
                    review,
                    suggestion,
                    delivery,
                });
            }
            return Err(PlanReviewStoreError::Conflict(
                "the review was already decided by a different decision".into(),
            ));
        }
        if review.lock_version != decision.expected_lock_version {
            return Err(PlanReviewStoreError::Conflict(format!(
                "expected review lock version {}, found {}",
                decision.expected_lock_version, review.lock_version
            )));
        }
        let draft = plan_review_drafts::table
            .find(decision.review_id)
            .first::<PlanReviewDraftRow>(conn)?;
        if draft.generation != decision.expected_draft_generation {
            return Err(PlanReviewStoreError::Conflict(format!(
                "expected draft generation {}, found {}",
                decision.expected_draft_generation, draft.generation
            )));
        }
        if draft.draft_sha256 != decision.expected_draft_sha256 {
            return Err(PlanReviewStoreError::Conflict(format!(
                "expected draft hash {}, found {}",
                decision.expected_draft_sha256, draft.draft_sha256
            )));
        }
        let comments = plan_comments::table
            .filter(plan_comments::review_id.eq(decision.review_id))
            .order(plan_comments::position.asc())
            .load::<PlanCommentRow>(conn)?;
        let active_comments = meaningful_comments(&comments);
        let has_note = draft.global_note.as_deref().is_some_and(|note| !note.trim().is_empty());
        let dirty = draft_is_dirty(&draft)?;
        let document = get_document(conn, &review.document_id)?;

        let mut suggestion = None;
        let mut delivery = None;
        let (review_state, document_state, approved_revision_id) = match decision.action {
            PlanReviewDecisionAction::Approve => {
                if dirty || has_note || !active_comments.is_empty() {
                    return Err(PlanReviewStoreError::InvalidState(
                        "discard or submit all edits, comments and the global note before approving".into(),
                    ));
                }
                if let Some(target) = decision.delivery_target {
                    let approved = get_revision(conn, &review.submitted_revision_id)?;
                    let delivery_id = uuid::Uuid::new_v4().to_string();
                    let payload_json = serde_json::to_string(&PlanApprovalEnvelope {
                        schema_version: 1,
                        delivery_id: &delivery_id,
                        action: "approve",
                        review_id: decision.review_id,
                        document_id: &review.document_id,
                        approved_revision_id: &review.submitted_revision_id,
                        approved_sha256: &approved.content_sha256,
                    })
                    .expect("the approval envelope contains only serializable values");
                    diesel::insert_into(plan_review_deliveries::table)
                        .values(&PlanReviewDeliveryInsert {
                            id: &delivery_id,
                            review_id: decision.review_id,
                            target: target.as_str(),
                            state: PlanDeliveryState::Queued.as_str(),
                            payload_json: &payload_json,
                            attempt_token: None,
                            target_session_id: decision.target_session_id,
                            target_turn_id: decision.target_turn_id,
                            error: None,
                            created_at: decision.now,
                            updated_at: decision.now,
                            dispatched_at: None,
                            acknowledged_at: None,
                            held_at: None,
                        })
                        .execute(conn)?;
                    delivery = Some(plan_review_deliveries::table.find(&delivery_id).first(conn)?);
                }
                (
                    PlanReviewState::Approved,
                    PlanDocumentState::Approved,
                    Some(review.submitted_revision_id.as_str()),
                )
            }
            PlanReviewDecisionAction::RequestChanges => {
                if !dirty && !has_note && active_comments.is_empty() {
                    return Err(PlanReviewStoreError::InvalidState(
                        "request changes requires an edit, comment or global note".into(),
                    ));
                }
                let target = decision.delivery_target.ok_or_else(|| {
                    PlanReviewStoreError::InvalidState("request changes requires a delivery target".into())
                })?;
                let submitted = get_revision(conn, &review.submitted_revision_id)?;
                let suggested_markdown = match draft.mode()? {
                    // A comment-only rich review must not silently rewrite the
                    // submitted source just because the editor normalised it.
                    PlanReviewDraftMode::Rich if !dirty => submitted.content_markdown.as_str(),
                    PlanReviewDraftMode::Rich => draft.draft_normalized_markdown.as_str(),
                    PlanReviewDraftMode::Source => draft.source_text.as_deref().ok_or_else(|| {
                        PlanReviewStoreError::InvalidState("source draft has no exact source text".into())
                    })?,
                };
                // The suggestion revision declares the submitted revision as
                // its parent and the envelope declares its SHA as baseSha256.
                // Its patch therefore has to apply to those exact raw bytes,
                // including any formatting the rich editor normalises.
                let patch = markdown_diff(&submitted.content_markdown, suggested_markdown);
                let suggestion_id = uuid::Uuid::new_v4().to_string();
                let content_sha256 = markdown_sha256(suggested_markdown);
                let revision_no = next_revision_no(conn, &review.document_id)?;
                diesel::insert_into(plan_revisions::table)
                    .values(&PlanRevisionInsert {
                        id: &suggestion_id,
                        document_id: &review.document_id,
                        revision_no,
                        parent_revision_id: Some(&review.submitted_revision_id),
                        author_kind: PlanRevisionAuthorKind::UserSuggestion.as_str(),
                        content_markdown: suggested_markdown,
                        content_sha256: &content_sha256,
                        patch: patch.as_deref(),
                        source_message_id: None,
                        source_call_id: None,
                        responding_to_suggestion_revision_id: None,
                        editor_json: draft.draft_editor_json.as_deref(),
                        editor_schema_version: draft.editor_schema_version,
                        editor_schema_hash: draft.editor_schema_hash.as_deref(),
                        legacy_source_artifact_id: None,
                        created_at: decision.now,
                    })
                    .execute(conn)?;
                diesel::update(
                    plan_comments::table
                        .filter(plan_comments::review_id.eq(decision.review_id))
                        .filter(
                            plan_comments::state
                                .eq_any([PlanCommentState::Draft.as_str(), PlanCommentState::Active.as_str()]),
                        ),
                )
                .set((
                    plan_comments::state.eq(PlanCommentState::Submitted.as_str()),
                    plan_comments::updated_at.eq(decision.now),
                ))
                .execute(conn)?;

                let delivery_id = uuid::Uuid::new_v4().to_string();
                let feedback_comments = active_comments
                    .iter()
                    .map(|comment| {
                        let anchor = serde_json::from_str(&comment.anchor_json).map_err(|error| {
                            PlanReviewStoreError::InvalidJson {
                                field: "comment.anchorJson",
                                message: error.to_string(),
                            }
                        })?;
                        Ok(PlanFeedbackComment {
                            id: &comment.id,
                            state: &comment.state,
                            anchor_kind: &comment.anchor_kind,
                            anchor,
                            body: &comment.body,
                        })
                    })
                    .collect::<PlanReviewStoreResult<Vec<_>>>()?;
                let payload_json = serde_json::to_string(&PlanFeedbackEnvelope {
                    schema_version: 1,
                    delivery_id: &delivery_id,
                    review_id: decision.review_id,
                    document_id: &review.document_id,
                    base_revision_id: &review.submitted_revision_id,
                    base_sha256: &submitted.content_sha256,
                    suggestion_revision_id: &suggestion_id,
                    suggested_markdown,
                    suggested_patch: patch.as_deref(),
                    comments: feedback_comments,
                    global_note: draft.global_note.as_deref(),
                })
                .expect("the feedback envelope contains only serializable values");
                diesel::insert_into(plan_review_deliveries::table)
                    .values(&PlanReviewDeliveryInsert {
                        id: &delivery_id,
                        review_id: decision.review_id,
                        target: target.as_str(),
                        state: PlanDeliveryState::Queued.as_str(),
                        payload_json: &payload_json,
                        attempt_token: None,
                        target_session_id: decision.target_session_id,
                        target_turn_id: decision.target_turn_id,
                        error: None,
                        created_at: decision.now,
                        updated_at: decision.now,
                        dispatched_at: None,
                        acknowledged_at: None,
                        held_at: None,
                    })
                    .execute(conn)?;
                suggestion = Some(get_revision(conn, &suggestion_id)?);
                delivery = Some(plan_review_deliveries::table.find(&delivery_id).first(conn)?);
                (
                    PlanReviewState::ChangesRequested,
                    PlanDocumentState::Drafting,
                    document.approved_revision_id.as_deref(),
                )
            }
        };

        let suggestion_id = suggestion.as_ref().map(|revision| revision.id.as_str());
        let changed = diesel::update(
            plan_review_sessions::table
                .find(decision.review_id)
                .filter(plan_review_sessions::state.eq(PlanReviewState::Pending.as_str()))
                .filter(plan_review_sessions::lock_version.eq(decision.expected_lock_version)),
        )
        .set((
            plan_review_sessions::state.eq(review_state.as_str()),
            plan_review_sessions::decision_id.eq(Some(decision.decision_id)),
            plan_review_sessions::decision_summary.eq(decision.decision_summary),
            plan_review_sessions::suggestion_revision_id.eq(suggestion_id),
            plan_review_sessions::lock_version.eq(decision.expected_lock_version + 1),
            plan_review_sessions::updated_at.eq(decision.now),
            plan_review_sessions::decided_at.eq(Some(decision.now)),
        ))
        .execute(conn)?;
        if changed != 1 {
            return Err(PlanReviewStoreError::Conflict(
                "the plan review changed concurrently".into(),
            ));
        }
        let changed = diesel::update(
            plan_documents::table
                .find(&review.document_id)
                .filter(plan_documents::lock_version.eq(document.lock_version)),
        )
        .set((
            plan_documents::state.eq(document_state.as_str()),
            plan_documents::approved_revision_id.eq(approved_revision_id),
            plan_documents::lock_version.eq(document.lock_version + 1),
            plan_documents::updated_at.eq(decision.now),
        ))
        .execute(conn)?;
        if changed != 1 {
            return Err(PlanReviewStoreError::Conflict(
                "the plan document changed concurrently".into(),
            ));
        }
        if decision.action == PlanReviewDecisionAction::Approve {
            diesel::update(crate::db::schema::conversations::table.find(&document.conversation_id))
                .set((
                    crate::db::schema::conversations::mode.eq::<Option<&str>>(None),
                    crate::db::schema::conversations::updated_at.eq(decision.now),
                ))
                .execute(conn)?;
        }

        Ok(PlanReviewDecisionResult {
            document: get_document(conn, &review.document_id)?,
            review: get_review(conn, decision.review_id)?,
            suggestion,
            delivery,
        })
    })
}

pub fn get_delivery(conn: &mut SqliteConnection, delivery_id: &str) -> PlanReviewStoreResult<PlanReviewDeliveryRow> {
    plan_review_deliveries::table
        .find(delivery_id)
        .first(conn)
        .optional()?
        .ok_or(PlanReviewStoreError::NotFound("plan review delivery"))
}

pub fn list_recoverable_deliveries(conn: &mut SqliteConnection) -> PlanReviewStoreResult<Vec<PlanReviewDeliveryRow>> {
    Ok(plan_review_deliveries::table
        .filter(plan_review_deliveries::state.eq_any([
            PlanDeliveryState::Queued.as_str(),
            PlanDeliveryState::Held.as_str(),
            PlanDeliveryState::InDoubt.as_str(),
        ]))
        .order(plan_review_deliveries::created_at.asc())
        .load(conn)?)
}

const STARTUP_QUEUE_RESUME_PENDING: &str = "startup_queue_resume_pending";

/// Native continuations that completed before a crash but whose prompt queue
/// has not yet been resumed by the newly constructed runtime. The marker is
/// durable because database startup runs before a `StartTurn` exists.
pub fn list_startup_queue_resumes(conn: &mut SqliteConnection) -> PlanReviewStoreResult<Vec<(String, String)>> {
    let deliveries = plan_review_deliveries::table
        .filter(plan_review_deliveries::state.eq(PlanDeliveryState::Acknowledged.as_str()))
        .filter(plan_review_deliveries::error.eq(STARTUP_QUEUE_RESUME_PENDING))
        .order(plan_review_deliveries::updated_at.asc())
        .load::<PlanReviewDeliveryRow>(conn)?;
    let mut resumes = Vec::with_capacity(deliveries.len());
    for delivery in deliveries {
        let review = get_review(conn, &delivery.review_id)?;
        let document = get_document(conn, &review.document_id)?;
        resumes.push((delivery.id, document.conversation_id));
    }
    Ok(resumes)
}

pub fn finish_startup_queue_resume(
    conn: &mut SqliteConnection,
    delivery_id: &str,
    now: i64,
) -> PlanReviewStoreResult<bool> {
    Ok(diesel::update(
        plan_review_deliveries::table
            .find(delivery_id)
            .filter(plan_review_deliveries::state.eq(PlanDeliveryState::Acknowledged.as_str()))
            .filter(plan_review_deliveries::error.eq(STARTUP_QUEUE_RESUME_PENDING)),
    )
    .set((
        plan_review_deliveries::error.eq::<Option<&str>>(None),
        plan_review_deliveries::updated_at.eq(now),
    ))
    .execute(conn)?
        == 1)
}

/// A process cannot know whether a provider consumed a delivery for which no
/// acknowledgement was committed. Startup records that uncertainty but never
/// retries it; only an explicit user continuation may resolve an in-doubt row.
pub fn reconcile_dispatched_deliveries(conn: &mut SqliteConnection, now: i64) -> PlanReviewStoreResult<usize> {
    conn.transaction(|conn| {
        let dispatched = plan_review_deliveries::table
            .filter(plan_review_deliveries::state.eq(PlanDeliveryState::Dispatched.as_str()))
            .load::<PlanReviewDeliveryRow>(conn)?;
        for delivery in &dispatched {
            let completed_native_turn = delivery.target()? == PlanDeliveryTarget::Native
                && delivery
                    .target_turn_id
                    .as_deref()
                    .map(|turn_id| crate::db::ops::turn::get(conn, turn_id))
                    .transpose()?
                    .flatten()
                    .map(|turn| turn.status())
                    .transpose()?
                    == Some(crate::db::models::turn::TurnStatus::Done);
            let (state, error) = if completed_native_turn {
                (PlanDeliveryState::Acknowledged, Some(STARTUP_QUEUE_RESUME_PENDING))
            } else {
                (
                    PlanDeliveryState::InDoubt,
                    Some("process exited before delivery acknowledgement"),
                )
            };
            transition_delivery(
                conn,
                &delivery.id,
                &[PlanDeliveryState::Dispatched],
                state,
                delivery.attempt_token.as_deref(),
                error,
                now,
            )?;
        }
        Ok(dispatched.len())
    })
}

fn transition_delivery(
    conn: &mut SqliteConnection,
    delivery_id: &str,
    from: &[PlanDeliveryState],
    to: PlanDeliveryState,
    attempt_token: Option<&str>,
    error: Option<&str>,
    now: i64,
) -> PlanReviewStoreResult<PlanReviewDeliveryRow> {
    conn.transaction(|conn| {
        let delivery = get_delivery(conn, delivery_id)?;
        let from = from.iter().copied().map(PlanDeliveryState::as_str).collect::<Vec<_>>();
        let changed = diesel::update(
            plan_review_deliveries::table
                .find(delivery_id)
                .filter(plan_review_deliveries::state.eq_any(from)),
        )
        .set((
            plan_review_deliveries::state.eq(to.as_str()),
            plan_review_deliveries::attempt_token.eq(attempt_token),
            plan_review_deliveries::error.eq(error),
            plan_review_deliveries::updated_at.eq(now),
        ))
        .execute(conn)?;
        if changed != 1 {
            return Err(PlanReviewStoreError::Conflict(format!(
                "delivery {delivery_id} is not in an allowed source state"
            )));
        }
        match to {
            PlanDeliveryState::Dispatched => {
                diesel::update(plan_review_deliveries::table.find(delivery_id))
                    .set(plan_review_deliveries::dispatched_at.eq(Some(now)))
                    .execute(conn)?;
            }
            PlanDeliveryState::Acknowledged => {
                diesel::update(plan_review_deliveries::table.find(delivery_id))
                    .set(plan_review_deliveries::acknowledged_at.eq(Some(now)))
                    .execute(conn)?;
            }
            PlanDeliveryState::Held => {
                diesel::update(plan_review_deliveries::table.find(delivery_id))
                    .set(plan_review_deliveries::held_at.eq(Some(now)))
                    .execute(conn)?;
            }
            PlanDeliveryState::Queued | PlanDeliveryState::InDoubt => {}
        }
        let bumped = diesel::update(plan_review_sessions::table.find(&delivery.review_id))
            .set((
                plan_review_sessions::lock_version.eq(plan_review_sessions::lock_version + 1),
                plan_review_sessions::updated_at.eq(now),
            ))
            .execute(conn)?;
        if bumped != 1 {
            return Err(PlanReviewStoreError::Conflict(
                "the delivery's plan review no longer exists".into(),
            ));
        }
        get_delivery(conn, delivery_id)
    })
}

pub fn mark_delivery_dispatched(
    conn: &mut SqliteConnection,
    delivery_id: &str,
    attempt_token: &str,
    now: i64,
) -> PlanReviewStoreResult<PlanReviewDeliveryRow> {
    transition_delivery(
        conn,
        delivery_id,
        &[PlanDeliveryState::Queued, PlanDeliveryState::Held],
        PlanDeliveryState::Dispatched,
        Some(attempt_token),
        None,
        now,
    )
}

/// Dispatch a native continuation and persist the exact turn identity in the
/// same transaction as the delivery/review version transition. Retries mint a
/// new turn id; leaving the old id on the durable row makes recovery and event
/// consumers point at a turn that will never run.
pub fn mark_delivery_dispatched_for_turn(
    conn: &mut SqliteConnection,
    delivery_id: &str,
    attempt_token: &str,
    target_turn_id: &str,
    now: i64,
) -> PlanReviewStoreResult<PlanReviewDeliveryRow> {
    conn.transaction(|conn| {
        transition_delivery(
            conn,
            delivery_id,
            &[PlanDeliveryState::Queued, PlanDeliveryState::Held],
            PlanDeliveryState::Dispatched,
            Some(attempt_token),
            None,
            now,
        )?;
        diesel::update(plan_review_deliveries::table.find(delivery_id))
            .set(plan_review_deliveries::target_turn_id.eq(Some(target_turn_id)))
            .execute(conn)?;
        get_delivery(conn, delivery_id)
    })
}

/// Explicit user retry for a delivery whose previous consumption is unknown.
/// Kept separate from the ordinary queued path so startup code cannot
/// accidentally turn inspection of recoverable rows into a blind redelivery.
pub fn retry_delivery_dispatched(
    conn: &mut SqliteConnection,
    delivery_id: &str,
    attempt_token: &str,
    now: i64,
) -> PlanReviewStoreResult<PlanReviewDeliveryRow> {
    transition_delivery(
        conn,
        delivery_id,
        &[PlanDeliveryState::InDoubt],
        PlanDeliveryState::Dispatched,
        Some(attempt_token),
        None,
        now,
    )
}

pub fn retry_delivery_dispatched_for_turn(
    conn: &mut SqliteConnection,
    delivery_id: &str,
    attempt_token: &str,
    target_turn_id: &str,
    now: i64,
) -> PlanReviewStoreResult<PlanReviewDeliveryRow> {
    conn.transaction(|conn| {
        transition_delivery(
            conn,
            delivery_id,
            &[PlanDeliveryState::InDoubt],
            PlanDeliveryState::Dispatched,
            Some(attempt_token),
            None,
            now,
        )?;
        diesel::update(plan_review_deliveries::table.find(delivery_id))
            .set(plan_review_deliveries::target_turn_id.eq(Some(target_turn_id)))
            .execute(conn)?;
        get_delivery(conn, delivery_id)
    })
}

pub fn mark_delivery_acknowledged(
    conn: &mut SqliteConnection,
    delivery_id: &str,
    attempt_token: &str,
    now: i64,
) -> PlanReviewStoreResult<PlanReviewDeliveryRow> {
    conn.transaction(|conn| {
        let delivery = get_delivery(conn, delivery_id)?;
        if delivery.attempt_token.as_deref() != Some(attempt_token) {
            return Err(PlanReviewStoreError::Conflict(
                "delivery attempt token does not match".into(),
            ));
        }
        transition_delivery(
            conn,
            delivery_id,
            &[PlanDeliveryState::Dispatched],
            PlanDeliveryState::Acknowledged,
            Some(attempt_token),
            None,
            now,
        )
    })
}

pub fn mark_delivery_held(
    conn: &mut SqliteConnection,
    delivery_id: &str,
    error: Option<&str>,
    now: i64,
) -> PlanReviewStoreResult<PlanReviewDeliveryRow> {
    transition_delivery(
        conn,
        delivery_id,
        &[PlanDeliveryState::Queued, PlanDeliveryState::Dispatched],
        PlanDeliveryState::Held,
        None,
        error,
        now,
    )
}

pub fn mark_delivery_in_doubt(
    conn: &mut SqliteConnection,
    delivery_id: &str,
    attempt_token: &str,
    error: &str,
    now: i64,
) -> PlanReviewStoreResult<PlanReviewDeliveryRow> {
    conn.transaction(|conn| {
        let delivery = get_delivery(conn, delivery_id)?;
        if delivery.attempt_token.as_deref() != Some(attempt_token) {
            return Err(PlanReviewStoreError::Conflict(
                "delivery attempt token does not match".into(),
            ));
        }
        transition_delivery(
            conn,
            delivery_id,
            &[PlanDeliveryState::Dispatched],
            PlanDeliveryState::InDoubt,
            Some(attempt_token),
            Some(error),
            now,
        )
    })
}

pub fn pending_materializations(
    conn: &mut SqliteConnection,
    document_id: Option<&str>,
) -> PlanReviewStoreResult<Vec<PlanMaterializationRow>> {
    let mut query = plan_materializations::table
        .filter(plan_materializations::state.eq(PlanMaterializationState::Pending.as_str()))
        .into_boxed();
    if let Some(document_id) = document_id {
        query = query.filter(plan_materializations::document_id.eq(document_id));
    }
    Ok(query
        .order((
            plan_materializations::created_at.asc(),
            plan_materializations::generation.asc(),
        ))
        .load(conn)?)
}

pub fn latest_materialization(
    conn: &mut SqliteConnection,
    document_id: &str,
) -> PlanReviewStoreResult<Option<PlanMaterializationRow>> {
    Ok(plan_materializations::table
        .filter(plan_materializations::document_id.eq(document_id))
        .order(plan_materializations::generation.desc())
        .first(conn)
        .optional()?)
}

pub fn mark_materialization_applied(
    conn: &mut SqliteConnection,
    id: &str,
    now: i64,
) -> PlanReviewStoreResult<PlanMaterializationRow> {
    let changed = diesel::update(
        plan_materializations::table
            .find(id)
            .filter(plan_materializations::state.eq(PlanMaterializationState::Pending.as_str())),
    )
    .set((
        plan_materializations::state.eq(PlanMaterializationState::Applied.as_str()),
        plan_materializations::force_replace.eq(0),
        plan_materializations::error.eq::<Option<&str>>(None),
        plan_materializations::updated_at.eq(now),
        plan_materializations::applied_at.eq(Some(now)),
    ))
    .execute(conn)?;
    if changed != 1 {
        return Err(PlanReviewStoreError::Conflict("materialization is not pending".into()));
    }
    Ok(plan_materializations::table.find(id).first(conn)?)
}

pub fn mark_materialization_conflict(
    conn: &mut SqliteConnection,
    id: &str,
    error: &str,
    now: i64,
) -> PlanReviewStoreResult<PlanMaterializationRow> {
    let changed = diesel::update(
        plan_materializations::table
            .find(id)
            .filter(plan_materializations::state.eq(PlanMaterializationState::Pending.as_str())),
    )
    .set((
        plan_materializations::state.eq(PlanMaterializationState::Conflict.as_str()),
        plan_materializations::force_replace.eq(0),
        plan_materializations::error.eq(Some(error)),
        plan_materializations::updated_at.eq(now),
        plan_materializations::applied_at.eq::<Option<i64>>(None),
    ))
    .execute(conn)?;
    if changed != 1 {
        return Err(PlanReviewStoreError::Conflict("materialization is not pending".into()));
    }
    Ok(plan_materializations::table.find(id).first(conn)?)
}

pub fn mark_materialization_drift(
    conn: &mut SqliteConnection,
    id: &str,
    error: &str,
    now: i64,
) -> PlanReviewStoreResult<PlanMaterializationRow> {
    let changed = diesel::update(
        plan_materializations::table
            .find(id)
            .filter(plan_materializations::state.eq(PlanMaterializationState::Applied.as_str())),
    )
    .set((
        plan_materializations::state.eq(PlanMaterializationState::Conflict.as_str()),
        plan_materializations::force_replace.eq(0),
        plan_materializations::error.eq(Some(error)),
        plan_materializations::updated_at.eq(now),
        plan_materializations::applied_at.eq::<Option<i64>>(None),
    ))
    .execute(conn)?;
    if changed != 1 {
        return Err(PlanReviewStoreError::Conflict("materialization is not applied".into()));
    }
    Ok(plan_materializations::table.find(id).first(conn)?)
}

/// Explicit user recovery: make the latest conflicted generation eligible to
/// restore from the database.  Nothing calls this during startup.
pub fn retry_materialization_from_database(
    conn: &mut SqliteConnection,
    document_id: &str,
    now: i64,
) -> PlanReviewStoreResult<PlanMaterializationRow> {
    let row = plan_materializations::table
        .filter(plan_materializations::document_id.eq(document_id))
        .filter(plan_materializations::state.eq(PlanMaterializationState::Conflict.as_str()))
        .order(plan_materializations::generation.desc())
        .first::<PlanMaterializationRow>(conn)
        .optional()?
        .ok_or(PlanReviewStoreError::NotFound("conflicted plan materialization"))?;
    diesel::update(plan_materializations::table.find(&row.id))
        .set((
            // NULL authorises replacing an absent file.  PlanFileStore treats a
            // conflict retry specially and atomically replaces the unexpected
            // bytes because this function is only reached from the explicit
            // restore action.
            plan_materializations::state.eq(PlanMaterializationState::Pending.as_str()),
            plan_materializations::expected_sha256.eq::<Option<&str>>(None),
            plan_materializations::force_replace.eq(1),
            plan_materializations::error.eq::<Option<&str>>(None),
            plan_materializations::updated_at.eq(now),
        ))
        .execute(conn)?;
    Ok(plan_materializations::table.find(&row.id).first(conn)?)
}

pub fn complete_active_document(
    conn: &mut SqliteConnection,
    conversation_id: &str,
    now: i64,
) -> PlanReviewStoreResult<usize> {
    Ok(diesel::update(
        plan_documents::table
            .filter(plan_documents::conversation_id.eq(conversation_id))
            .filter(plan_documents::state.eq(PlanDocumentState::Approved.as_str())),
    )
    .set((
        plan_documents::state.eq(PlanDocumentState::Done.as_str()),
        plan_documents::lock_version.eq(plan_documents::lock_version + 1),
        plan_documents::updated_at.eq(now),
    ))
    .execute(conn)?)
}

/// Convert pre-document plan artifacts into one immutable legacy episode per
/// conversation.  SQL cannot compute SHA-256, so startup runs this idempotent
/// Rust backfill after migration 51.  A conversation already owned by the new
/// flow is never mixed with legacy rows.
pub fn backfill_legacy_artifacts(conn: &mut SqliteConnection, now: i64) -> PlanReviewStoreResult<usize> {
    // Early builds of migration 51 imported legacy pending artifacts as live
    // reviews even though they have no turn/message/call identity and cannot
    // be opened by the review UI. Repair those rows before the idempotence
    // check so an already-upgraded database cannot remain permanently barred.
    let stranded_legacy_documents = plan_review_sessions::table
        .filter(plan_review_sessions::provider_kind.eq(PlanReviewProviderKind::Legacy.as_str()))
        .filter(plan_review_sessions::state.eq(PlanReviewState::Pending.as_str()))
        .select(plan_review_sessions::document_id)
        .load::<String>(conn)?;
    if !stranded_legacy_documents.is_empty() {
        conn.transaction(|conn| {
            diesel::update(
                plan_review_sessions::table
                    .filter(plan_review_sessions::provider_kind.eq(PlanReviewProviderKind::Legacy.as_str()))
                    .filter(plan_review_sessions::state.eq(PlanReviewState::Pending.as_str())),
            )
            .set((
                plan_review_sessions::state.eq(PlanReviewState::Orphaned.as_str()),
                plan_review_sessions::decision_summary
                    .eq(Some("Imported legacy review has no resumable transcript identity")),
                plan_review_sessions::lock_version.eq(plan_review_sessions::lock_version + 1),
                plan_review_sessions::updated_at.eq(now),
                plan_review_sessions::decided_at.eq(Some(now)),
            ))
            .execute(conn)?;
            diesel::update(
                plan_documents::table
                    .filter(plan_documents::id.eq_any(&stranded_legacy_documents))
                    .filter(plan_documents::state.eq(PlanDocumentState::Reviewing.as_str())),
            )
            .set((
                plan_documents::state.eq(PlanDocumentState::Drafting.as_str()),
                plan_documents::lock_version.eq(plan_documents::lock_version + 1),
                plan_documents::updated_at.eq(now),
            ))
            .execute(conn)?;
            Ok::<(), PlanReviewStoreError>(())
        })?;
    }

    let artifacts = mode_artifacts::table
        .filter(mode_artifacts::kind.eq("plan"))
        .order((
            mode_artifacts::conversation_id.asc(),
            mode_artifacts::created_at.asc(),
            mode_artifacts::id.asc(),
        ))
        .load::<ModeArtifactRow>(conn)?;
    let mut groups: Vec<(String, Vec<ModeArtifactRow>)> = Vec::new();
    for artifact in artifacts {
        if let Some((_, rows)) = groups
            .last_mut()
            .filter(|(conversation_id, _)| conversation_id == &artifact.conversation_id)
        {
            rows.push(artifact);
        } else {
            groups.push((artifact.conversation_id.clone(), vec![artifact]));
        }
    }

    let mut inserted = 0;
    for (conversation_id, rows) in groups {
        let exists = plan_documents::table
            .filter(plan_documents::conversation_id.eq(&conversation_id))
            .select(plan_documents::id)
            .first::<String>(conn)
            .optional()?
            .is_some();
        if exists || rows.is_empty() {
            continue;
        }
        conn.transaction(|conn| {
            let document_id = format!("legacy-{}", rows[0].id);
            let file_rel_path = format!("plans/{document_id}/plan.md");
            let pending = rows.last().filter(|row| row.status == "pending");
            let approved = rows.iter().rev().find(|row| row.status == "approved");
            let state = if pending.is_some() {
                // Legacy pending artifacts have no transcript identity, so
                // preserve their bytes as an editable working document rather
                // than creating a hidden review barrier.
                PlanDocumentState::Drafting
            } else if approved.is_some() {
                PlanDocumentState::Approved
            } else {
                PlanDocumentState::Done
            };
            let head_revision_id = format!("legacy-revision-{}", rows.last().expect("nonempty").id);
            let approved_revision_id = approved.map(|row| format!("legacy-revision-{}", row.id));
            diesel::insert_into(plan_documents::table)
                .values(&PlanDocumentInsert {
                    id: &document_id,
                    conversation_id: &conversation_id,
                    state: state.as_str(),
                    head_revision_id: Some(&head_revision_id),
                    approved_revision_id: approved_revision_id.as_deref(),
                    working_generation: rows.len() as i64,
                    file_rel_path: &file_rel_path,
                    lock_version: 0,
                    created_at: rows[0].created_at,
                    updated_at: rows.last().expect("nonempty").updated_at,
                })
                .execute(conn)?;

            let mut parent: Option<String> = None;
            for (index, artifact) in rows.iter().enumerate() {
                let revision_id = format!("legacy-revision-{}", artifact.id);
                let sha = markdown_sha256(&artifact.content);
                diesel::insert_into(plan_revisions::table)
                    .values(&PlanRevisionInsert {
                        id: &revision_id,
                        document_id: &document_id,
                        revision_no: index as i64 + 1,
                        parent_revision_id: parent.as_deref(),
                        author_kind: PlanRevisionAuthorKind::Legacy.as_str(),
                        content_markdown: &artifact.content,
                        content_sha256: &sha,
                        patch: None,
                        source_message_id: None,
                        source_call_id: None,
                        responding_to_suggestion_revision_id: None,
                        editor_json: None,
                        editor_schema_version: None,
                        editor_schema_hash: None,
                        legacy_source_artifact_id: Some(&artifact.id),
                        created_at: artifact.created_at,
                    })
                    .execute(conn)?;
                parent = Some(revision_id);
            }

            let head = rows.last().expect("nonempty");
            let desired_sha = markdown_sha256(&head.content);
            let materialization_id = format!("legacy-materialization-{}", head.id);
            diesel::insert_into(plan_materializations::table)
                .values(&PlanMaterializationInsert {
                    id: &materialization_id,
                    document_id: &document_id,
                    revision_id: &head_revision_id,
                    generation: rows.len() as i64,
                    expected_sha256: None,
                    desired_sha256: &desired_sha,
                    state: PlanMaterializationState::Pending.as_str(),
                    force_replace: 0,
                    error: None,
                    created_at: now,
                    updated_at: now,
                    applied_at: None,
                })
                .execute(conn)?;

            if let Some(artifact) = pending {
                let review_id = format!("legacy-review-{}", artifact.id);
                let revision_id = format!("legacy-revision-{}", artifact.id);
                let sha = markdown_sha256(&artifact.content);
                diesel::insert_into(plan_review_sessions::table)
                    .values(&PlanReviewSessionInsert {
                        id: &review_id,
                        document_id: &document_id,
                        submitted_revision_id: &revision_id,
                        turn_id: None,
                        assistant_message_id: None,
                        provider_call_id: None,
                        provider_kind: PlanReviewProviderKind::Legacy.as_str(),
                        native_runtime_config_json: None,
                        state: PlanReviewState::Orphaned.as_str(),
                        decision_id: None,
                        decision_summary: Some("Imported legacy review has no resumable transcript identity"),
                        suggestion_revision_id: None,
                        lock_version: 0,
                        created_at: artifact.created_at,
                        updated_at: artifact.updated_at,
                        decided_at: Some(artifact.updated_at),
                    })
                    .execute(conn)?;
                diesel::insert_into(plan_review_drafts::table)
                    .values(&PlanReviewDraftInsert {
                        review_id: &review_id,
                        base_revision_id: &revision_id,
                        generation: 0,
                        mode: PlanReviewDraftMode::Source.as_str(),
                        base_editor_json: None,
                        draft_editor_json: None,
                        base_normalized_markdown: &artifact.content,
                        draft_normalized_markdown: &artifact.content,
                        source_text: Some(&artifact.content),
                        editor_schema_version: None,
                        editor_schema_hash: None,
                        global_note: None,
                        selection_json: None,
                        draft_sha256: &sha,
                        created_at: artifact.created_at,
                        updated_at: artifact.updated_at,
                    })
                    .execute(conn)?;
            }
            Ok::<(), PlanReviewStoreError>(())
        })?;
        inserted += 1;
    }
    Ok(inserted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::turn::TurnStatus;
    use crate::turn::TurnOrigin;

    fn seed_conversation(conn: &mut SqliteConnection, id: &str) {
        crate::db::ops::conversation::create_conversation(conn, id, None, None, None, 1).unwrap();
    }

    fn append_first(conn: &mut SqliteConnection, conversation_id: &str, markdown: &str) -> PlanRevisionAppendResult {
        seed_conversation(conn, conversation_id);
        let document = create_or_resume_document(conn, conversation_id, 2).unwrap();
        append_assistant_revision(
            conn,
            &PlanRevisionAppend {
                document_id: &document.id,
                expected_generation: 0,
                expected_head_sha256: None,
                content_markdown: markdown,
                patch: "*** Add File: plan.md",
                source_message_id: Some("m1"),
                source_call_id: Some("call-1"),
                responding_to_suggestion_revision_id: None,
                now: 3,
            },
        )
        .unwrap()
    }

    fn mark_applied(conn: &mut SqliteConnection, appended: &PlanRevisionAppendResult) {
        mark_materialization_applied(conn, &appended.materialization.id, 4).unwrap();
    }

    fn submit(
        conn: &mut SqliteConnection,
        appended: &PlanRevisionAppendResult,
        turn_id: Option<&str>,
    ) -> PlanReviewBundle {
        submit_native_head_for_review(
            conn,
            &PlanReviewSubmit {
                document_id: &appended.document.id,
                expected_generation: appended.document.working_generation,
                expected_head_sha256: &appended.revision.content_sha256,
                turn_id,
                assistant_message_id: Some("m1"),
                provider_call_id: Some("call-1"),
                provider_kind: PlanReviewProviderKind::Native,
                now: 5,
            },
            &NativePlanReviewRuntimeConfig::fixture(),
        )
        .unwrap()
    }

    #[test]
    fn append_uses_generation_and_hash_cas() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        let first = append_first(&mut conn, "c1", "# First\n");

        let stale = append_assistant_revision(
            &mut conn,
            &PlanRevisionAppend {
                document_id: &first.document.id,
                expected_generation: 0,
                expected_head_sha256: None,
                content_markdown: "# Stale\n",
                patch: "stale",
                source_message_id: None,
                source_call_id: None,
                responding_to_suggestion_revision_id: None,
                now: 4,
            },
        );
        assert!(matches!(stale, Err(PlanReviewStoreError::Conflict(_))));
        assert_eq!(
            get_head_revision(&mut conn, &first.document.id).unwrap().unwrap().id,
            first.revision.id
        );
        assert_eq!(list_revisions(&mut conn, &first.document.id).unwrap().len(), 1);
    }

    #[test]
    fn submit_and_waiting_review_commit_together_and_survive_reconcile() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        let first = append_first(&mut conn, "c1", "# Plan\n");
        mark_applied(&mut conn, &first);
        crate::db::ops::turn::begin(&mut conn, "t1", "c1", TurnOrigin::Desktop, None, 4).unwrap();

        let review = submit(&mut conn, &first, Some("t1"));
        assert_eq!(review.review.state, PlanReviewState::Pending.as_str());
        let turn = crate::db::ops::turn::list_for_conversation(&mut conn, "c1")
            .unwrap()
            .remove(0);
        assert_eq!(turn.status().unwrap(), TurnStatus::WaitingReview);
        assert_eq!(crate::db::ops::turn::reconcile_interrupted(&mut conn, 10).unwrap(), 0);
        assert_eq!(
            crate::db::ops::turn::list_for_conversation(&mut conn, "c1").unwrap()[0]
                .status()
                .unwrap(),
            TurnStatus::WaitingReview
        );
    }

    #[test]
    fn approve_accepts_only_a_pristine_draft() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        let first = append_first(&mut conn, "c1", "# Plan\n");
        mark_applied(&mut conn, &first);
        crate::db::ops::conversation::update_mode(&mut conn, "c1", Some("plan"), 4).unwrap();
        let review = submit(&mut conn, &first, None);

        let approved = decide_review(
            &mut conn,
            &PlanReviewDecision {
                review_id: &review.review.id,
                decision_id: "decision-approve",
                expected_lock_version: 0,
                expected_draft_generation: 0,
                expected_draft_sha256: &review.draft.draft_sha256,
                action: PlanReviewDecisionAction::Approve,
                decision_summary: None,
                delivery_target: None,
                target_session_id: None,
                target_turn_id: None,
                now: 6,
            },
        )
        .unwrap();
        assert_eq!(
            approved.document.approved_revision_id.as_deref(),
            Some(first.revision.id.as_str())
        );
        assert_eq!(
            crate::db::ops::conversation::get_conversation(&mut conn, "c1")
                .unwrap()
                .mode,
            None,
            "approval and leaving plan mode are one transaction"
        );

        let second = append_first(&mut conn, "c2", "# Plan\n");
        mark_applied(&mut conn, &second);
        let review = submit(&mut conn, &second, None);
        let dirty = save_review_draft(
            &mut conn,
            &PlanReviewDraftSave {
                review_id: &review.review.id,
                expected_generation: 0,
                mode: PlanReviewDraftMode::Source,
                base_editor_json: None,
                draft_editor_json: None,
                base_normalized_markdown: "# Plan\n",
                draft_normalized_markdown: "# Changed\n",
                source_text: Some("# Changed\n"),
                editor_schema_version: None,
                editor_schema_hash: None,
                schema_fallback_from_version: None,
                schema_fallback_from_hash: None,
                global_note: None,
                selection_json: None,
                comments: &[],
                now: 6,
            },
        )
        .unwrap();
        let result = decide_review(
            &mut conn,
            &PlanReviewDecision {
                review_id: &review.review.id,
                decision_id: "decision-dirty",
                expected_lock_version: 0,
                expected_draft_generation: dirty.draft.generation,
                expected_draft_sha256: &dirty.draft.draft_sha256,
                action: PlanReviewDecisionAction::Approve,
                decision_summary: None,
                delivery_target: None,
                target_session_id: None,
                target_turn_id: None,
                now: 7,
            },
        );
        assert!(matches!(result, Err(PlanReviewStoreError::InvalidState(_))));

        // The first rich projection carries two values.  Treating the current
        // edited document as both baseline and draft would make this pristine
        // and allow approval; preserving the immutable parsed baseline must
        // keep it dirty.
        let third = append_first(&mut conn, "c3", "# Plan\n");
        mark_applied(&mut conn, &third);
        let review = submit(&mut conn, &third, None);
        let base = r#"{"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Plan"}]}]}"#;
        let edited = r#"{"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Changed"}]}]}"#;
        let dirty = save_review_draft(
            &mut conn,
            &PlanReviewDraftSave {
                review_id: &review.review.id,
                expected_generation: 0,
                mode: PlanReviewDraftMode::Rich,
                base_editor_json: Some(base),
                draft_editor_json: Some(edited),
                base_normalized_markdown: "# Plan\n",
                draft_normalized_markdown: "# Changed\n",
                source_text: None,
                editor_schema_version: Some(1),
                editor_schema_hash: Some("schema-1"),
                schema_fallback_from_version: None,
                schema_fallback_from_hash: None,
                global_note: None,
                selection_json: None,
                comments: &[],
                now: 6,
            },
        )
        .unwrap();
        let result = decide_review(
            &mut conn,
            &PlanReviewDecision {
                review_id: &review.review.id,
                decision_id: "decision-dirty-rich",
                expected_lock_version: 0,
                expected_draft_generation: dirty.draft.generation,
                expected_draft_sha256: &dirty.draft.draft_sha256,
                action: PlanReviewDecisionAction::Approve,
                decision_summary: None,
                delivery_target: None,
                target_session_id: None,
                target_turn_id: None,
                now: 7,
            },
        );
        assert!(matches!(result, Err(PlanReviewStoreError::InvalidState(_))));
    }

    #[test]
    fn incompatible_rich_schema_can_fall_back_to_source_once_without_rebasing() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        let first = append_first(&mut conn, "c1", "# Plan\n");
        mark_applied(&mut conn, &first);
        let review = submit(&mut conn, &first, None);
        let base = r#"{"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Plan"}]}]}"#;
        let edited = r#"{"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Changed"}]}]}"#;
        let rich = save_review_draft(
            &mut conn,
            &PlanReviewDraftSave {
                review_id: &review.review.id,
                expected_generation: 0,
                mode: PlanReviewDraftMode::Rich,
                base_editor_json: Some(base),
                draft_editor_json: Some(edited),
                base_normalized_markdown: "# Plan\n",
                draft_normalized_markdown: "# Changed\n",
                source_text: None,
                editor_schema_version: Some(7),
                editor_schema_hash: Some("old-schema"),
                schema_fallback_from_version: None,
                schema_fallback_from_hash: None,
                global_note: None,
                selection_json: None,
                comments: &[],
                now: 6,
            },
        )
        .unwrap();

        let stale = save_review_draft(
            &mut conn,
            &PlanReviewDraftSave {
                review_id: &review.review.id,
                expected_generation: 0,
                mode: PlanReviewDraftMode::Source,
                base_editor_json: None,
                draft_editor_json: None,
                base_normalized_markdown: "# Plan\n",
                draft_normalized_markdown: "# Changed\n",
                source_text: Some("# Changed\n"),
                editor_schema_version: None,
                editor_schema_hash: None,
                schema_fallback_from_version: Some(7),
                schema_fallback_from_hash: Some("old-schema"),
                global_note: None,
                selection_json: None,
                comments: &[],
                now: 7,
            },
        );
        assert!(matches!(stale, Err(PlanReviewStoreError::Conflict(_))));

        let source = save_review_draft(
            &mut conn,
            &PlanReviewDraftSave {
                review_id: &review.review.id,
                expected_generation: rich.draft.generation,
                mode: PlanReviewDraftMode::Source,
                base_editor_json: None,
                draft_editor_json: None,
                base_normalized_markdown: "# Plan\n",
                draft_normalized_markdown: "# Changed\n",
                source_text: Some("# Changed\n"),
                editor_schema_version: None,
                editor_schema_hash: None,
                schema_fallback_from_version: Some(7),
                schema_fallback_from_hash: Some("old-schema"),
                global_note: None,
                selection_json: None,
                comments: &[],
                now: 8,
            },
        )
        .unwrap();
        assert_eq!(source.draft.mode().unwrap(), PlanReviewDraftMode::Source);
        assert_eq!(source.draft.base_normalized_markdown, "# Plan\n");
        assert_eq!(source.draft.draft_normalized_markdown, "# Changed\n");
        assert!(source.draft.base_editor_json.is_none());
        assert!(source.draft.draft_editor_json.is_none());

        let rebase = save_review_draft(
            &mut conn,
            &PlanReviewDraftSave {
                review_id: &review.review.id,
                expected_generation: source.draft.generation,
                mode: PlanReviewDraftMode::Source,
                base_editor_json: None,
                draft_editor_json: None,
                base_normalized_markdown: "# Changed\n",
                draft_normalized_markdown: "# Changed\n",
                source_text: Some("# Changed\n"),
                editor_schema_version: None,
                editor_schema_hash: None,
                schema_fallback_from_version: None,
                schema_fallback_from_hash: None,
                global_note: None,
                selection_json: None,
                comments: &[],
                now: 9,
            },
        );
        assert!(matches!(rebase, Err(PlanReviewStoreError::Conflict(_))));

        let approval = decide_review(
            &mut conn,
            &PlanReviewDecision {
                review_id: &review.review.id,
                decision_id: "decision-after-fallback",
                expected_lock_version: 0,
                expected_draft_generation: source.draft.generation,
                expected_draft_sha256: &source.draft.draft_sha256,
                action: PlanReviewDecisionAction::Approve,
                decision_summary: None,
                delivery_target: None,
                target_session_id: None,
                target_turn_id: None,
                now: 10,
            },
        );
        assert!(matches!(approval, Err(PlanReviewStoreError::InvalidState(_))));
    }

    #[test]
    fn continuation_delivery_holds_the_conversation_barrier_until_acknowledged() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        let first = append_first(&mut conn, "c1", "# Plan\n");
        mark_applied(&mut conn, &first);
        let review = submit(&mut conn, &first, None);
        assert!(has_conversation_barrier(&mut conn, "c1").unwrap());

        let decided = decide_review(
            &mut conn,
            &PlanReviewDecision {
                review_id: &review.review.id,
                decision_id: "decision-with-continuation",
                expected_lock_version: 0,
                expected_draft_generation: 0,
                expected_draft_sha256: &review.draft.draft_sha256,
                action: PlanReviewDecisionAction::Approve,
                decision_summary: None,
                delivery_target: Some(PlanDeliveryTarget::Native),
                target_session_id: None,
                target_turn_id: Some("continuation-1"),
                now: 6,
            },
        )
        .unwrap();
        let delivery = decided.delivery.unwrap();
        assert_eq!(
            decided.review.lock_version, 1,
            "decision + queued delivery is version 1"
        );
        assert!(
            has_conversation_barrier(&mut conn, "c1").unwrap(),
            "queued continuation still blocks"
        );
        mark_delivery_dispatched(&mut conn, &delivery.id, "attempt-1", 7).unwrap();
        assert_eq!(get_review(&mut conn, &review.review.id).unwrap().lock_version, 2);
        assert!(
            has_conversation_barrier(&mut conn, "c1").unwrap(),
            "dispatched continuation still blocks"
        );
        mark_delivery_acknowledged(&mut conn, &delivery.id, "attempt-1", 8).unwrap();
        assert_eq!(get_review(&mut conn, &review.review.id).unwrap().lock_version, 3);
        assert!(
            !has_conversation_barrier(&mut conn, "c1").unwrap(),
            "acknowledgement releases the barrier"
        );
    }

    #[test]
    fn startup_reconciliation_versions_each_dispatched_delivery_as_in_doubt() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        let first = append_first(&mut conn, "c1", "# Plan\n");
        mark_applied(&mut conn, &first);
        let review = submit(&mut conn, &first, None);
        let decided = decide_review(
            &mut conn,
            &PlanReviewDecision {
                review_id: &review.review.id,
                decision_id: "approve-before-restart",
                expected_lock_version: 0,
                expected_draft_generation: 0,
                expected_draft_sha256: &review.draft.draft_sha256,
                action: PlanReviewDecisionAction::Approve,
                decision_summary: None,
                delivery_target: Some(PlanDeliveryTarget::Native),
                target_session_id: None,
                target_turn_id: Some("continuation-1"),
                now: 6,
            },
        )
        .unwrap();
        let delivery = decided.delivery.unwrap();
        mark_delivery_dispatched(&mut conn, &delivery.id, "attempt-1", 7).unwrap();
        assert_eq!(get_review(&mut conn, &review.review.id).unwrap().lock_version, 2);

        assert_eq!(reconcile_dispatched_deliveries(&mut conn, 8).unwrap(), 1);
        assert_eq!(
            get_delivery(&mut conn, &delivery.id).unwrap().state().unwrap(),
            PlanDeliveryState::InDoubt
        );
        assert_eq!(get_review(&mut conn, &review.review.id).unwrap().lock_version, 3);
        assert_eq!(reconcile_dispatched_deliveries(&mut conn, 9).unwrap(), 0);
        assert_eq!(get_review(&mut conn, &review.review.id).unwrap().lock_version, 3);
    }

    #[test]
    fn startup_acknowledges_a_native_delivery_whose_persisted_continuation_turn_finished() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        let first = append_first(&mut conn, "c1", "# Plan\n");
        mark_applied(&mut conn, &first);
        let review = submit(&mut conn, &first, None);
        let decided = decide_review(
            &mut conn,
            &PlanReviewDecision {
                review_id: &review.review.id,
                decision_id: "approve-before-crash",
                expected_lock_version: 0,
                expected_draft_generation: 0,
                expected_draft_sha256: &review.draft.draft_sha256,
                action: PlanReviewDecisionAction::Approve,
                decision_summary: None,
                delivery_target: Some(PlanDeliveryTarget::Native),
                target_session_id: None,
                target_turn_id: Some("continuation-done"),
                now: 6,
            },
        )
        .unwrap();
        let delivery = decided.delivery.unwrap();
        mark_delivery_dispatched_for_turn(&mut conn, &delivery.id, "attempt-1", "continuation-done", 7).unwrap();
        crate::db::ops::turn::begin(
            &mut conn,
            "continuation-done",
            "c1",
            crate::turn::TurnOrigin::PlanReview,
            None,
            8,
        )
        .unwrap();
        crate::db::ops::turn::finish(
            &mut conn,
            "continuation-done",
            crate::db::models::turn::TurnStatus::Done,
            None,
            9,
        )
        .unwrap();

        assert_eq!(reconcile_dispatched_deliveries(&mut conn, 10).unwrap(), 1);
        assert_eq!(
            get_delivery(&mut conn, &delivery.id).unwrap().state().unwrap(),
            PlanDeliveryState::Acknowledged
        );
        assert!(!has_conversation_barrier(&mut conn, "c1").unwrap());
        assert_eq!(get_review(&mut conn, &review.review.id).unwrap().lock_version, 3);
        assert_eq!(
            list_startup_queue_resumes(&mut conn).unwrap(),
            [(delivery.id.clone(), "c1".to_string())]
        );
        assert!(finish_startup_queue_resume(&mut conn, &delivery.id, 11).unwrap());
        assert!(list_startup_queue_resumes(&mut conn).unwrap().is_empty());
    }

    #[test]
    fn an_explicit_native_retry_persists_its_new_continuation_turn_identity() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        let first = append_first(&mut conn, "c1", "# Plan\n");
        mark_applied(&mut conn, &first);
        let review = submit(&mut conn, &first, None);
        let decided = decide_review(
            &mut conn,
            &PlanReviewDecision {
                review_id: &review.review.id,
                decision_id: "approve-retry",
                expected_lock_version: 0,
                expected_draft_generation: 0,
                expected_draft_sha256: &review.draft.draft_sha256,
                action: PlanReviewDecisionAction::Approve,
                decision_summary: None,
                delivery_target: Some(PlanDeliveryTarget::Native),
                target_session_id: None,
                target_turn_id: Some("continuation-old"),
                now: 6,
            },
        )
        .unwrap();
        let delivery = decided.delivery.unwrap();
        mark_delivery_dispatched(&mut conn, &delivery.id, "attempt-old", 7).unwrap();
        mark_delivery_in_doubt(&mut conn, &delivery.id, "attempt-old", "unknown", 8).unwrap();
        let retried =
            retry_delivery_dispatched_for_turn(&mut conn, &delivery.id, "attempt-new", "continuation-new", 9).unwrap();
        assert_eq!(retried.target_turn_id.as_deref(), Some("continuation-new"));
        assert_eq!(retried.attempt_token.as_deref(), Some("attempt-new"));
    }

    #[test]
    fn native_runtime_selection_is_strict_and_survives_a_fresh_database_read() {
        let pool = crate::db::test_db();
        let review_id = {
            let mut conn = pool.get().unwrap();
            let first = append_first(&mut conn, "c1", "# Plan\n");
            mark_applied(&mut conn, &first);
            let runtime = NativePlanReviewRuntimeConfig {
                provider_id: "provider-b".into(),
                model: "model-b".into(),
                assistant_id: Some("assistant-b".into()),
                thinking_level: Some(crate::provider::capabilities::StoredThinkingLevel::Xhigh),
                fast: true,
                project_id: Some("project-b".into()),
                project_path: Some("C:/work/b".into()),
                accept_edits: true,
            };
            submit_native_head_for_review(
                &mut conn,
                &PlanReviewSubmit {
                    document_id: &first.document.id,
                    expected_generation: first.document.working_generation,
                    expected_head_sha256: &first.revision.content_sha256,
                    turn_id: None,
                    assistant_message_id: Some("m1"),
                    provider_call_id: Some("exit-1"),
                    provider_kind: PlanReviewProviderKind::Native,
                    now: 5,
                },
                &runtime,
            )
            .unwrap()
            .review
            .id
        };

        let mut fresh = pool.get().unwrap();
        let stored = get_review(&mut fresh, &review_id)
            .unwrap()
            .native_runtime_config()
            .unwrap()
            .unwrap();
        assert_eq!(stored.provider_id, "provider-b");
        assert_eq!(stored.model, "model-b");
        assert_eq!(stored.assistant_id.as_deref(), Some("assistant-b"));
        assert_eq!(
            stored.thinking_level,
            Some(crate::provider::capabilities::StoredThinkingLevel::Xhigh)
        );
        assert!(stored.fast);
        assert_eq!(stored.project_id.as_deref(), Some("project-b"));
        assert_eq!(stored.project_path.as_deref(), Some("C:/work/b"));
        assert!(stored.accept_edits);
        assert!(
            serde_json::from_str::<NativePlanReviewRuntimeConfig>(
                r#"{"provider_id":"p","model":"m","assistant_id":null,"thinking_level":null,"fast":false,"project_id":null,"project_path":null,"accept_edits":false,"future":true}"#
            )
            .is_err(),
            "unknown persisted runtime fields cannot be silently ignored"
        );
    }

    #[test]
    fn runtime_mutation_guards_ignore_settled_historical_reviews() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        let first = append_first(&mut conn, "c1", "# First plan\n");
        mark_applied(&mut conn, &first);
        let runtime_a = NativePlanReviewRuntimeConfig {
            provider_id: "provider-a".into(),
            model: "model-a".into(),
            assistant_id: Some("assistant-a".into()),
            ..NativePlanReviewRuntimeConfig::fixture()
        };
        let first_review = submit_native_head_for_review(
            &mut conn,
            &PlanReviewSubmit {
                document_id: &first.document.id,
                expected_generation: first.document.working_generation,
                expected_head_sha256: &first.revision.content_sha256,
                turn_id: None,
                assistant_message_id: Some("m1"),
                provider_call_id: Some("exit-a"),
                provider_kind: PlanReviewProviderKind::Native,
                now: 5,
            },
            &runtime_a,
        )
        .unwrap();
        decide_review(
            &mut conn,
            &PlanReviewDecision {
                review_id: &first_review.review.id,
                decision_id: "approve-a",
                expected_lock_version: 0,
                expected_draft_generation: 0,
                expected_draft_sha256: &first_review.draft.draft_sha256,
                action: PlanReviewDecisionAction::Approve,
                decision_summary: None,
                delivery_target: None,
                target_session_id: None,
                target_turn_id: None,
                now: 6,
            },
        )
        .unwrap();
        assert_eq!(complete_active_document(&mut conn, "c1", 7).unwrap(), 1);

        let document = create_or_resume_document(&mut conn, "c1", 8).unwrap();
        let second = append_assistant_revision(
            &mut conn,
            &PlanRevisionAppend {
                document_id: &document.id,
                expected_generation: 0,
                expected_head_sha256: None,
                content_markdown: "# Second plan\n",
                patch: "second plan",
                source_message_id: Some("m2"),
                source_call_id: Some("update-b"),
                responding_to_suggestion_revision_id: None,
                now: 9,
            },
        )
        .unwrap();
        mark_applied(&mut conn, &second);
        let runtime_b = NativePlanReviewRuntimeConfig {
            provider_id: "provider-b".into(),
            model: "model-b".into(),
            assistant_id: Some("assistant-b".into()),
            ..NativePlanReviewRuntimeConfig::fixture()
        };
        submit_native_head_for_review(
            &mut conn,
            &PlanReviewSubmit {
                document_id: &document.id,
                expected_generation: second.document.working_generation,
                expected_head_sha256: &second.revision.content_sha256,
                turn_id: None,
                assistant_message_id: Some("m2"),
                provider_call_id: Some("exit-b"),
                provider_kind: PlanReviewProviderKind::Native,
                now: 10,
            },
            &runtime_b,
        )
        .unwrap();

        assert!(
            barrier_conversations_for_assistant(&mut conn, "assistant-a")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            barrier_conversations_for_assistant(&mut conn, "assistant-b").unwrap(),
            ["c1"]
        );
        assert!(
            barrier_conversations_for_provider(&mut conn, "provider-a")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            barrier_conversations_for_provider(&mut conn, "provider-b").unwrap(),
            ["c1"]
        );
        assert!(
            barrier_conversations_for_model(&mut conn, "provider-a", "model-a")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            barrier_conversations_for_model(&mut conn, "provider-b", "model-b").unwrap(),
            ["c1"]
        );
    }

    #[test]
    fn a_change_request_requires_a_new_linked_update_before_resubmission() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        let first = append_first(&mut conn, "c1", "# Plan\n");
        mark_applied(&mut conn, &first);
        let review = submit(&mut conn, &first, None);
        let feedback = save_review_draft(
            &mut conn,
            &PlanReviewDraftSave {
                review_id: &review.review.id,
                expected_generation: 0,
                mode: PlanReviewDraftMode::Source,
                base_editor_json: None,
                draft_editor_json: None,
                base_normalized_markdown: "# Plan\n",
                draft_normalized_markdown: "# Plan\n",
                source_text: Some("# Plan\n"),
                editor_schema_version: None,
                editor_schema_hash: None,
                schema_fallback_from_version: None,
                schema_fallback_from_hash: None,
                global_note: Some("Please add verification."),
                selection_json: None,
                comments: &[],
                now: 6,
            },
        )
        .unwrap();
        let changed = decide_review(
            &mut conn,
            &PlanReviewDecision {
                review_id: &review.review.id,
                decision_id: "request-verification",
                expected_lock_version: 0,
                expected_draft_generation: feedback.draft.generation,
                expected_draft_sha256: &feedback.draft.draft_sha256,
                action: PlanReviewDecisionAction::RequestChanges,
                decision_summary: None,
                delivery_target: Some(PlanDeliveryTarget::Native),
                target_session_id: None,
                target_turn_id: None,
                now: 7,
            },
        )
        .unwrap();
        let delivery = changed.delivery.unwrap();
        mark_delivery_dispatched(&mut conn, &delivery.id, "attempt-1", 8).unwrap();
        mark_delivery_acknowledged(&mut conn, &delivery.id, "attempt-1", 9).unwrap();

        let unchanged = submit_native_head_for_review(
            &mut conn,
            &PlanReviewSubmit {
                document_id: &first.document.id,
                expected_generation: first.document.working_generation,
                expected_head_sha256: &first.revision.content_sha256,
                turn_id: None,
                assistant_message_id: Some("m2"),
                provider_call_id: Some("call-2"),
                provider_kind: PlanReviewProviderKind::Native,
                now: 10,
            },
            &NativePlanReviewRuntimeConfig::fixture(),
        );
        assert!(matches!(unchanged, Err(PlanReviewStoreError::InvalidState(_))));

        let unlinked = append_assistant_revision(
            &mut conn,
            &PlanRevisionAppend {
                document_id: &first.document.id,
                expected_generation: first.document.working_generation,
                expected_head_sha256: Some(&first.revision.content_sha256),
                content_markdown: "# Plan\n\nUnlinked change.\n",
                patch: "unlinked",
                source_message_id: Some("m2"),
                source_call_id: Some("call-update-1"),
                responding_to_suggestion_revision_id: None,
                now: 11,
            },
        )
        .unwrap();
        mark_applied(&mut conn, &unlinked);
        let unlinked_submit = submit_native_head_for_review(
            &mut conn,
            &PlanReviewSubmit {
                document_id: &unlinked.document.id,
                expected_generation: unlinked.document.working_generation,
                expected_head_sha256: &unlinked.revision.content_sha256,
                turn_id: None,
                assistant_message_id: Some("m2"),
                provider_call_id: Some("call-2"),
                provider_kind: PlanReviewProviderKind::Native,
                now: 12,
            },
            &NativePlanReviewRuntimeConfig::fixture(),
        );
        assert!(matches!(unlinked_submit, Err(PlanReviewStoreError::InvalidState(_))));

        let suggestion_id = changed.review.suggestion_revision_id.unwrap();
        let linked = append_assistant_revision(
            &mut conn,
            &PlanRevisionAppend {
                document_id: &unlinked.document.id,
                expected_generation: unlinked.document.working_generation,
                expected_head_sha256: Some(&unlinked.revision.content_sha256),
                content_markdown: "# Plan\n\nVerified change.\n",
                patch: "linked",
                source_message_id: Some("m2"),
                source_call_id: Some("call-update-2"),
                responding_to_suggestion_revision_id: Some(&suggestion_id),
                now: 13,
            },
        )
        .unwrap();
        mark_applied(&mut conn, &linked);
        let reverted = append_assistant_revision(
            &mut conn,
            &PlanRevisionAppend {
                document_id: &linked.document.id,
                expected_generation: linked.document.working_generation,
                expected_head_sha256: Some(&linked.revision.content_sha256),
                content_markdown: "# Plan\n",
                patch: "reverse the linked change",
                source_message_id: Some("m2"),
                source_call_id: Some("call-update-revert"),
                responding_to_suggestion_revision_id: None,
                now: 14,
            },
        )
        .unwrap();
        mark_applied(&mut conn, &reverted);
        let reverted_submit = submit_native_head_for_review(
            &mut conn,
            &PlanReviewSubmit {
                document_id: &reverted.document.id,
                expected_generation: reverted.document.working_generation,
                expected_head_sha256: &reverted.revision.content_sha256,
                turn_id: None,
                assistant_message_id: Some("m2"),
                provider_call_id: Some("call-reverted"),
                provider_kind: PlanReviewProviderKind::Native,
                now: 15,
            },
            &NativePlanReviewRuntimeConfig::fixture(),
        );
        assert!(matches!(reverted_submit, Err(PlanReviewStoreError::InvalidState(_))));

        let final_patch = append_assistant_revision(
            &mut conn,
            &PlanRevisionAppend {
                document_id: &reverted.document.id,
                expected_generation: reverted.document.working_generation,
                expected_head_sha256: Some(&reverted.revision.content_sha256),
                content_markdown: "# Plan\n\nVerified change.\n\nTests included.\n",
                patch: "final unlinked patch",
                source_message_id: Some("m2"),
                source_call_id: Some("call-update-3"),
                responding_to_suggestion_revision_id: None,
                now: 16,
            },
        )
        .unwrap();
        mark_applied(&mut conn, &final_patch);
        let resubmitted = submit(&mut conn, &final_patch, None);
        assert_eq!(resubmitted.submitted_revision.id, final_patch.revision.id);
    }

    #[test]
    fn request_changes_is_idempotent_and_suggestion_does_not_advance_head() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        let first = append_first(&mut conn, "c1", "# Plan\n");
        mark_applied(&mut conn, &first);
        let review = submit(&mut conn, &first, None);
        let comments = [PlanCommentSave {
            id: "comment-1",
            position: 0,
            state: PlanCommentState::Active,
            anchor_kind: PlanCommentAnchorKind::Source,
            anchor_json: r##"{"fromUtf16":0,"toUtf16":6,"quote":"# Plan"}"##,
            body: "Rename this heading",
        }];
        let dirty = save_review_draft(
            &mut conn,
            &PlanReviewDraftSave {
                review_id: &review.review.id,
                expected_generation: 0,
                mode: PlanReviewDraftMode::Source,
                base_editor_json: None,
                draft_editor_json: None,
                base_normalized_markdown: "# Plan\n",
                draft_normalized_markdown: "# Better plan\n",
                source_text: Some("# Better plan\n"),
                editor_schema_version: None,
                editor_schema_hash: None,
                schema_fallback_from_version: None,
                schema_fallback_from_hash: None,
                global_note: Some("Keep the tests focused."),
                selection_json: None,
                comments: &comments,
                now: 6,
            },
        )
        .unwrap();
        let decision = PlanReviewDecision {
            review_id: &review.review.id,
            decision_id: "decision-1",
            expected_lock_version: 0,
            expected_draft_generation: dirty.draft.generation,
            expected_draft_sha256: &dirty.draft.draft_sha256,
            action: PlanReviewDecisionAction::RequestChanges,
            decision_summary: Some("one edit"),
            delivery_target: Some(PlanDeliveryTarget::Native),
            target_session_id: None,
            target_turn_id: None,
            now: 7,
        };
        let first_result = decide_review(&mut conn, &decision).unwrap();
        let replay = decide_review(&mut conn, &decision).unwrap();

        assert_eq!(
            first_result.review.suggestion_revision_id,
            replay.review.suggestion_revision_id
        );
        assert_eq!(
            first_result.delivery.as_ref().unwrap().id,
            replay.delivery.as_ref().unwrap().id
        );
        assert_eq!(
            get_document(&mut conn, &first.document.id).unwrap().head_revision_id,
            Some(first.revision.id)
        );
        let suggestion = first_result.suggestion.unwrap();
        assert_eq!(
            suggestion.parent_revision_id.as_deref(),
            Some(review.submitted_revision.id.as_str())
        );
        assert_eq!(list_revisions(&mut conn, &first.document.id).unwrap().len(), 2);
        let payload: serde_json::Value = serde_json::from_str(&first_result.delivery.unwrap().payload_json).unwrap();
        assert_eq!(payload["suggestionRevisionId"], suggestion.id);
        assert_eq!(payload["comments"][0]["body"], "Rename this heading");
    }

    #[test]
    fn rich_suggestion_patch_uses_the_exact_submitted_revision_as_its_base() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        let first = append_first(&mut conn, "c1", "# Plan  \n");
        mark_applied(&mut conn, &first);
        let review = submit(&mut conn, &first, None);
        let dirty = save_review_draft(
            &mut conn,
            &PlanReviewDraftSave {
                review_id: &review.review.id,
                expected_generation: 0,
                mode: PlanReviewDraftMode::Rich,
                base_editor_json: Some(r#"{"type":"doc","content":[{"type":"heading","attrs":{"level":1},"content":[{"type":"text","text":"Plan"}]}]}"#),
                draft_editor_json: Some(r#"{"type":"doc","content":[{"type":"heading","attrs":{"level":1},"content":[{"type":"text","text":"Better plan"}]}]}"#),
                base_normalized_markdown: "# Plan\n",
                draft_normalized_markdown: "# Better plan\n",
                source_text: None,
                editor_schema_version: Some(1),
                editor_schema_hash: Some("schema-v1"),
                schema_fallback_from_version: None,
                schema_fallback_from_hash: None,
                global_note: None,
                selection_json: None,
                comments: &[],
                now: 6,
            },
        )
        .unwrap();
        let changed = decide_review(
            &mut conn,
            &PlanReviewDecision {
                review_id: &review.review.id,
                decision_id: "rich-change",
                expected_lock_version: 0,
                expected_draft_generation: dirty.draft.generation,
                expected_draft_sha256: &dirty.draft.draft_sha256,
                action: PlanReviewDecisionAction::RequestChanges,
                decision_summary: None,
                delivery_target: Some(PlanDeliveryTarget::Native),
                target_session_id: None,
                target_turn_id: None,
                now: 7,
            },
        )
        .unwrap();

        let suggestion = changed.suggestion.unwrap();
        assert_eq!(suggestion.patch, markdown_diff("# Plan  \n", "# Better plan\n"));
        let payload: serde_json::Value = serde_json::from_str(&changed.delivery.unwrap().payload_json).unwrap();
        assert_eq!(payload["baseSha256"], first.revision.content_sha256);
        assert_eq!(payload["suggestedPatch"], suggestion.patch.unwrap());
    }

    #[test]
    fn draft_save_rejects_blank_non_deleted_comments() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        let first = append_first(&mut conn, "c1", "# Plan\n");
        mark_applied(&mut conn, &first);
        let review = submit(&mut conn, &first, None);
        let comments = [PlanCommentSave {
            id: "blank-comment",
            position: 0,
            state: PlanCommentState::Active,
            anchor_kind: PlanCommentAnchorKind::Source,
            anchor_json: r##"{"fromUtf16":0,"toUtf16":6,"quote":"# Plan"}"##,
            body: "  \n",
        }];

        let result = save_review_draft(
            &mut conn,
            &PlanReviewDraftSave {
                review_id: &review.review.id,
                expected_generation: 0,
                mode: PlanReviewDraftMode::Source,
                base_editor_json: None,
                draft_editor_json: None,
                base_normalized_markdown: "# Plan\n",
                draft_normalized_markdown: "# Plan\n",
                source_text: Some("# Plan\n"),
                editor_schema_version: None,
                editor_schema_hash: None,
                schema_fallback_from_version: None,
                schema_fallback_from_hash: None,
                global_note: None,
                selection_json: None,
                comments: &comments,
                now: 6,
            },
        );

        assert!(matches!(result, Err(PlanReviewStoreError::InvalidState(_))));
    }

    #[test]
    fn legacy_backfill_is_idempotent_hashes_content_and_never_creates_a_hidden_barrier() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        seed_conversation(&mut conn, "c1");
        let artifact = crate::db::ops::plan::record_plan(&mut conn, "c1", "# Legacy\n", 2).unwrap();

        assert_eq!(backfill_legacy_artifacts(&mut conn, 3).unwrap(), 1);
        assert_eq!(backfill_legacy_artifacts(&mut conn, 4).unwrap(), 0);
        let document = get_active_document(&mut conn, "c1").unwrap().unwrap();
        let revision = get_head_revision(&mut conn, &document.id).unwrap().unwrap();
        assert_eq!(
            revision.legacy_source_artifact_id.as_deref(),
            Some(artifact.id.as_str())
        );
        assert_eq!(revision.content_sha256, markdown_sha256("# Legacy\n"));
        assert_eq!(document.state().unwrap(), PlanDocumentState::Drafting);
        let reviews = list_reviews(&mut conn, &document.id).unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].state().unwrap(), PlanReviewState::Orphaned);
        assert!(get_pending_review_for_conversation(&mut conn, "c1").unwrap().is_none());
        assert!(!has_conversation_barrier(&mut conn, "c1").unwrap());

        // Also repair databases touched by the earlier migration-51 backfill,
        // which created an unreachable pending review and reviewing document.
        diesel::update(plan_review_sessions::table.find(&reviews[0].id))
            .set((
                plan_review_sessions::state.eq(PlanReviewState::Pending.as_str()),
                plan_review_sessions::decided_at.eq::<Option<i64>>(None),
            ))
            .execute(&mut conn)
            .unwrap();
        diesel::update(plan_documents::table.find(&document.id))
            .set(plan_documents::state.eq(PlanDocumentState::Reviewing.as_str()))
            .execute(&mut conn)
            .unwrap();
        assert!(has_conversation_barrier(&mut conn, "c1").unwrap());
        assert_eq!(backfill_legacy_artifacts(&mut conn, 5).unwrap(), 0);
        assert!(!has_conversation_barrier(&mut conn, "c1").unwrap());
        assert_eq!(
            get_document(&mut conn, &document.id).unwrap().state().unwrap(),
            PlanDocumentState::Drafting
        );
    }

    #[test]
    fn completing_a_conversation_marks_the_new_document_done() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        let first = append_first(&mut conn, "c1", "# Plan\n");
        mark_applied(&mut conn, &first);
        let review = submit(&mut conn, &first, None);
        decide_review(
            &mut conn,
            &PlanReviewDecision {
                review_id: &review.review.id,
                decision_id: "approve-before-complete",
                expected_lock_version: 0,
                expected_draft_generation: 0,
                expected_draft_sha256: &review.draft.draft_sha256,
                action: PlanReviewDecisionAction::Approve,
                decision_summary: None,
                delivery_target: None,
                target_session_id: None,
                target_turn_id: None,
                now: 6,
            },
        )
        .unwrap();
        assert!(
            get_approved_revision_for_conversation(&mut conn, "c1")
                .unwrap()
                .is_some()
        );

        crate::db::ops::plan::complete_active(&mut conn, "c1", 10).unwrap();
        assert_eq!(
            get_document(&mut conn, &first.document.id).unwrap().state().unwrap(),
            PlanDocumentState::Done
        );
        assert!(get_active_document(&mut conn, "c1").unwrap().is_none());
        assert!(
            get_approved_revision_for_conversation(&mut conn, "c1")
                .unwrap()
                .is_none()
        );
    }
}

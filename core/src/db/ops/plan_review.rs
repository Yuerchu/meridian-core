//! Durable plan document, review draft, comment and delivery operations.
//!
//! Every public mutation is safe to call inside a wider Diesel transaction.
//! Diesel implements nested transactions with savepoints, which lets the turn
//! runtime commit transcript rows beside these domain changes without opening
//! a crash window between them.

use std::collections::HashSet;

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::db::models::plan_review::*;
use crate::db::schema::{
    plan_comments, plan_documents, plan_materializations, plan_review_deliveries, plan_review_drafts,
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

fn markdown_sha256(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
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

pub(super) fn get_document(conn: &mut SqliteConnection, id: &str) -> PlanReviewStoreResult<PlanDocumentRow> {
    plan_documents::table
        .find(id)
        .first(conn)
        .optional()?
        .ok_or(PlanReviewStoreError::NotFound("plan document"))
}

fn get_active_document(
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

fn get_revision(conn: &mut SqliteConnection, id: &str) -> PlanReviewStoreResult<PlanRevisionRow> {
    plan_revisions::table
        .find(id)
        .first(conn)
        .optional()?
        .ok_or(PlanReviewStoreError::NotFound("plan revision"))
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

fn get_review(conn: &mut SqliteConnection, review_id: &str) -> PlanReviewStoreResult<PlanReviewSessionRow> {
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

fn list_reviews(conn: &mut SqliteConnection, document_id: &str) -> PlanReviewStoreResult<Vec<PlanReviewSessionRow>> {
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

fn markdown_diff(base: &str, draft: &str) -> Option<String> {
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

fn get_delivery(conn: &mut SqliteConnection, delivery_id: &str) -> PlanReviewStoreResult<PlanReviewDeliveryRow> {
    plan_review_deliveries::table
        .find(delivery_id)
        .first(conn)
        .optional()?
        .ok_or(PlanReviewStoreError::NotFound("plan review delivery"))
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

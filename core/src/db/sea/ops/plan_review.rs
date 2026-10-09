//! The plan-review store on SeaORM: durable plan documents, their revisions,
//! review sessions with their drafts and comments, continuation deliveries and
//! plan-file materializations.
//!
//! No function opens a top-level transaction. A mutation that the Diesel
//! store ran as a (possibly nested) transaction runs in a savepoint of the
//! caller's write (`WriteTx::nested`), so a failure undoes exactly what it
//! did, and the caller's `Db::write` — `BEGIN IMMEDIATE` — is what holds the
//! lock across its reads. The turn runtime commits transcript rows beside
//! these changes in that same write, with no crash window between them.
//!
//! Every read that issues several statements and joins the answers takes
//! `&impl Snapshot`: on the pool, a review settling between two of them could
//! make a conversation look blocked by nothing, or free. The Diesel
//! `db::ops::plan_review` stays while Diesel roots still use it
//! (`docs/dual-impl.md`).

use std::collections::{HashMap, HashSet};

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{
    ColumnTrait, DbErr, EntityTrait, IntoActiveModel, JoinType, QueryFilter, QueryOrder, QuerySelect, RelationTrait,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::db::entity::plan_comment::{PlanCommentAnchorKind, PlanCommentState};
use crate::db::entity::plan_document::PlanDocumentState;
use crate::db::entity::plan_materialization::PlanMaterializationState;
use crate::db::entity::plan_review_delivery::{PlanDeliveryState, PlanDeliveryTarget};
use crate::db::entity::plan_review_draft::PlanReviewDraftMode;
use crate::db::entity::plan_review_session::{NativePlanReviewRuntimeConfig, PlanReviewProviderKind, PlanReviewState};
use crate::db::entity::plan_revision::PlanRevisionAuthorKind;
use crate::db::entity::{
    conversation as conversation_entity, mode_artifact, plan_comment, plan_document, plan_materialization,
    plan_review_delivery, plan_review_draft, plan_review_session, plan_revision,
};
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, Snapshot, WriteTx};
use crate::db::sea::ops::conversation;
use crate::db::types::{EpochMs, Json, SqlBool};

#[derive(Debug, thiserror::Error)]
pub enum PlanReviewStoreError {
    #[error(transparent)]
    Database(#[from] DbErr),
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

impl From<&str> for PlanReviewStoreError {
    fn from(value: &str) -> Self {
        Self::Contract(value.to_owned())
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
    pub document: plan_document::Model,
    pub revision: plan_revision::Model,
    pub materialization: plan_materialization::Model,
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
    pub document: plan_document::Model,
    pub review: plan_review_session::Model,
    pub suggestion: Option<plan_revision::Model>,
    pub delivery: Option<plan_review_delivery::Model>,
}

#[derive(Debug)]
pub struct PlanReviewBundle {
    pub document: plan_document::Model,
    pub submitted_revision: plan_revision::Model,
    pub review: plan_review_session::Model,
    pub draft: plan_review_draft::Model,
    pub comments: Vec<plan_comment::Model>,
    pub deliveries: Vec<plan_review_delivery::Model>,
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

/// The pending review, if any, for which a document's head may not move.
async fn pending_review_id(db: &impl Read, document_id: &str) -> Result<Option<String>, DbErr> {
    plan_review_session::Entity::find()
        .filter(plan_review_session::Column::DocumentId.eq(document_id))
        .filter(plan_review_session::Column::State.eq(PlanReviewState::Pending))
        .select_only()
        .column(plan_review_session::Column::Id)
        .into_tuple::<String>()
        .one(db.conn()?)
        .await
}

pub async fn get_document(db: &impl Read, id: &str) -> PlanReviewStoreResult<plan_document::Model> {
    plan_document::Entity::find_by_id(id)
        .one(db.conn()?)
        .await?
        .ok_or(PlanReviewStoreError::NotFound("plan document"))
}

pub async fn get_active_document(
    db: &impl Read,
    conversation_id: &str,
) -> PlanReviewStoreResult<Option<plan_document::Model>> {
    Ok(plan_document::Entity::find()
        .filter(plan_document::Column::ConversationId.eq(conversation_id))
        .filter(plan_document::Column::State.ne(PlanDocumentState::Done))
        .order_by_desc(plan_document::Column::CreatedAt)
        .one(db.conn()?)
        .await?)
}

pub async fn list_active_documents(db: &impl Read) -> PlanReviewStoreResult<Vec<plan_document::Model>> {
    Ok(plan_document::Entity::find()
        .filter(plan_document::Column::State.ne(PlanDocumentState::Done))
        .order_by_asc(plan_document::Column::CreatedAt)
        .all(db.conn()?)
        .await?)
}

/// The conversation's live document, or a new drafting one. The insert
/// ignores a conflict so that a concurrent creator's row is the one both
/// read back.
pub async fn create_or_resume_document(
    tx: &WriteTx,
    conversation_id: &str,
    now: EpochMs,
) -> PlanReviewStoreResult<plan_document::Model> {
    tx.nested(async |tx| {
        if let Some(document) = get_active_document(tx, conversation_id).await? {
            return Ok(document);
        }
        let id = uuid::Uuid::new_v4().to_string();
        plan_document::Entity::insert(plan_document::ActiveModel {
            id: Set(id.clone()),
            conversation_id: Set(conversation_id.to_owned()),
            state: Set(PlanDocumentState::Drafting),
            head_revision_id: Set(None),
            approved_revision_id: Set(None),
            working_generation: Set(0),
            file_rel_path: Set(format!("plans/{id}/plan.md")),
            lock_version: Set(0),
            created_at: Set(now),
            updated_at: Set(now),
        })
        .on_conflict_do_nothing()
        .exec_without_returning(tx.conn()?)
        .await?;
        get_active_document(tx, conversation_id)
            .await?
            .ok_or(PlanReviewStoreError::NotFound("plan document"))
    })
    .await
}

pub async fn get_revision(db: &impl Read, id: &str) -> PlanReviewStoreResult<plan_revision::Model> {
    plan_revision::Entity::find_by_id(id)
        .one(db.conn()?)
        .await?
        .ok_or(PlanReviewStoreError::NotFound("plan revision"))
}

pub async fn get_head_revision(
    db: &impl Snapshot,
    document_id: &str,
) -> PlanReviewStoreResult<Option<plan_revision::Model>> {
    let document = get_document(db, document_id).await?;
    match document.head_revision_id.as_deref() {
        Some(id) => Ok(Some(get_revision(db, id).await?)),
        None => Ok(None),
    }
}

pub async fn get_approved_revision_for_conversation(
    db: &impl Snapshot,
    conversation_id: &str,
) -> PlanReviewStoreResult<Option<plan_revision::Model>> {
    let document = plan_document::Entity::find()
        .filter(plan_document::Column::ConversationId.eq(conversation_id))
        .filter(plan_document::Column::State.ne(PlanDocumentState::Done))
        .filter(plan_document::Column::ApprovedRevisionId.is_not_null())
        .order_by_desc(plan_document::Column::UpdatedAt)
        .one(db.conn()?)
        .await?;
    match document.and_then(|row| row.approved_revision_id) {
        Some(id) => Ok(Some(get_revision(db, &id).await?)),
        None => Ok(None),
    }
}

pub fn format_approved_plan_block(revision: &plan_revision::Model) -> Option<String> {
    let content = revision.content_markdown.trim();
    if content.is_empty() {
        return None;
    }
    Some(format!("\n\n<approved_plan>\n{content}\n</approved_plan>"))
}

pub async fn list_revisions(db: &impl Read, document_id: &str) -> PlanReviewStoreResult<Vec<plan_revision::Model>> {
    Ok(plan_revision::Entity::find()
        .filter(plan_revision::Column::DocumentId.eq(document_id))
        .order_by_asc(plan_revision::Column::RevisionNo)
        .all(db.conn()?)
        .await?)
}

async fn next_revision_no(db: &impl Read, document_id: &str) -> Result<i64, DbErr> {
    let max: Option<Option<i64>> = plan_revision::Entity::find()
        .filter(plan_revision::Column::DocumentId.eq(document_id))
        .select_only()
        .column_as(plan_revision::Column::RevisionNo.max(), "max")
        .into_tuple()
        .one(db.conn()?)
        .await?;
    // domain-default: a document with no revision yet numbers its first one 1.
    Ok(max.flatten().unwrap_or(0) + 1)
}

/// A revision row with nothing an assistant revision does not carry.
fn revision_row(
    id: &str,
    document_id: &str,
    revision_no: i64,
    author_kind: PlanRevisionAuthorKind,
    content_markdown: &str,
    created_at: EpochMs,
) -> plan_revision::Model {
    plan_revision::Model {
        id: id.to_owned(),
        document_id: document_id.to_owned(),
        revision_no,
        parent_revision_id: None,
        author_kind,
        content_markdown: content_markdown.to_owned(),
        content_sha256: markdown_sha256(content_markdown),
        patch: None,
        source_message_id: None,
        source_call_id: None,
        responding_to_suggestion_revision_id: None,
        editor_json: None,
        editor_schema_version: None,
        editor_schema_hash: None,
        legacy_source_artifact_id: None,
        created_at,
    }
}

/// A pending materialization of `revision_id` at `generation`.
#[allow(clippy::too_many_arguments)]
fn materialization_row(
    id: &str,
    document_id: &str,
    revision_id: &str,
    generation: i64,
    expected_sha256: Option<&str>,
    desired_sha256: &str,
    now: EpochMs,
) -> plan_materialization::Model {
    plan_materialization::Model {
        id: id.to_owned(),
        document_id: document_id.to_owned(),
        revision_id: revision_id.to_owned(),
        generation,
        expected_sha256: expected_sha256.map(str::to_owned),
        desired_sha256: desired_sha256.to_owned(),
        state: PlanMaterializationState::Pending,
        force_replace: SqlBool::FALSE,
        error: None,
        created_at: now,
        updated_at: now,
        applied_at: None,
    }
}

async fn get_materialization(db: &impl Read, id: &str) -> PlanReviewStoreResult<plan_materialization::Model> {
    plan_materialization::Entity::find_by_id(id)
        .one(db.conn()?)
        .await?
        .ok_or(PlanReviewStoreError::NotFound("plan materialization"))
}

pub async fn append_assistant_revision(
    tx: &WriteTx,
    append: &PlanRevisionAppend<'_>,
) -> PlanReviewStoreResult<PlanRevisionAppendResult> {
    tx.nested(async |tx| {
        let document = get_document(tx, append.document_id).await?;
        if document.state == PlanDocumentState::Done {
            return Err(PlanReviewStoreError::InvalidState("the plan document is done".into()));
        }
        if document.working_generation != append.expected_generation {
            return Err(PlanReviewStoreError::Conflict(format!(
                "expected generation {}, found {}",
                append.expected_generation, document.working_generation
            )));
        }

        let current = match document.head_revision_id.as_deref() {
            Some(id) => Some(get_revision(tx, id).await?),
            None => None,
        };
        let current_sha = current.as_ref().map(|revision| revision.content_sha256.as_str());
        if current_sha != append.expected_head_sha256 {
            return Err(PlanReviewStoreError::Conflict(format!(
                "expected head hash {:?}, found {:?}",
                append.expected_head_sha256, current_sha
            )));
        }
        if pending_review_id(tx, append.document_id).await?.is_some() {
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
        let revision_no = next_revision_no(tx, append.document_id).await?;
        let revision = plan_revision::Model {
            parent_revision_id: document.head_revision_id.clone(),
            patch: Some(append.patch.to_owned()),
            source_message_id: append.source_message_id.map(str::to_owned),
            source_call_id: append.source_call_id.map(str::to_owned),
            responding_to_suggestion_revision_id: append.responding_to_suggestion_revision_id.map(str::to_owned),
            ..revision_row(
                &revision_id,
                append.document_id,
                revision_no,
                PlanRevisionAuthorKind::Assistant,
                append.content_markdown,
                append.now,
            )
        };
        plan_revision::Entity::insert(revision.into_active_model())
            .exec_without_returning(tx.conn()?)
            .await?;

        let generation = document.working_generation + 1;
        let changed = plan_document::Entity::update_many()
            .col_expr(plan_document::Column::HeadRevisionId, Expr::value(revision_id.as_str()))
            .col_expr(plan_document::Column::WorkingGeneration, Expr::value(generation))
            .col_expr(plan_document::Column::State, Expr::value(PlanDocumentState::Drafting))
            .col_expr(
                plan_document::Column::LockVersion,
                Expr::value(document.lock_version + 1),
            )
            .col_expr(plan_document::Column::UpdatedAt, Expr::value(append.now))
            .filter(plan_document::Column::Id.eq(append.document_id))
            .filter(plan_document::Column::WorkingGeneration.eq(append.expected_generation))
            .filter(plan_document::Column::LockVersion.eq(document.lock_version))
            .exec(tx.conn()?)
            .await?
            .rows_affected;
        if changed != 1 {
            return Err(PlanReviewStoreError::Conflict(
                "the plan document changed concurrently".into(),
            ));
        }

        let materialization_id = uuid::Uuid::new_v4().to_string();
        plan_materialization::Entity::insert(
            materialization_row(
                &materialization_id,
                append.document_id,
                &revision_id,
                generation,
                current_sha,
                &content_sha256,
                append.now,
            )
            .into_active_model(),
        )
        .exec_without_returning(tx.conn()?)
        .await?;

        Ok(PlanRevisionAppendResult {
            document: get_document(tx, append.document_id).await?,
            revision: get_revision(tx, &revision_id).await?,
            materialization: get_materialization(tx, &materialization_id).await?,
        })
    })
    .await
}

/// Whether every revision from `head` back to the submitted one is the
/// assistant's, and one of them answers the suggestion.
async fn revision_chain_answers_suggestion(
    db: &impl Snapshot,
    head: &plan_revision::Model,
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
        if cursor.author_kind != PlanRevisionAuthorKind::Assistant {
            return Ok(false);
        }
        linked |= cursor.responding_to_suggestion_revision_id.as_deref() == Some(suggestion_revision_id);
        let Some(parent_id) = cursor.parent_revision_id.clone() else {
            return Ok(false);
        };
        cursor = get_revision(db, &parent_id).await?;
    }
    Ok(linked)
}

pub async fn submit_head_for_review(
    tx: &WriteTx,
    submit: &PlanReviewSubmit<'_>,
) -> PlanReviewStoreResult<PlanReviewBundle> {
    submit_head_for_review_inner(tx, submit, None).await
}

pub async fn submit_native_head_for_review(
    tx: &WriteTx,
    submit: &PlanReviewSubmit<'_>,
    runtime: &NativePlanReviewRuntimeConfig,
) -> PlanReviewStoreResult<PlanReviewBundle> {
    submit_head_for_review_inner(tx, submit, Some(runtime)).await
}

async fn submit_head_for_review_inner(
    tx: &WriteTx,
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
    tx.nested(async |tx| {
        let document = get_document(tx, submit.document_id).await?;
        if document.working_generation != submit.expected_generation {
            return Err(PlanReviewStoreError::Conflict(format!(
                "expected generation {}, found {}",
                submit.expected_generation, document.working_generation
            )));
        }
        if document.state == PlanDocumentState::Done {
            return Err(PlanReviewStoreError::InvalidState("the plan document is done".into()));
        }
        let revision_id = document
            .head_revision_id
            .clone()
            .ok_or_else(|| PlanReviewStoreError::InvalidState("the plan has no revision to submit".into()))?;
        let revision = get_revision(tx, &revision_id).await?;
        if revision.content_sha256 != submit.expected_head_sha256 {
            return Err(PlanReviewStoreError::Conflict(format!(
                "expected head hash {}, found {}",
                submit.expected_head_sha256, revision.content_sha256
            )));
        }
        let mut previous: Option<(plan_review_session::Model, plan_revision::Model)> = None;
        for review in list_reviews(tx, submit.document_id).await? {
            let submitted = get_revision(tx, &review.submitted_revision_id).await?;
            // `max_by_key` keeps the last of equal keys; so does `>=`.
            if previous
                .as_ref()
                .is_none_or(|(_, best)| submitted.revision_no >= best.revision_no)
            {
                previous = Some((review, submitted));
            }
        }
        if let Some((previous_review, previous_submitted)) =
            previous.filter(|(review, _)| review.state == PlanReviewState::ChangesRequested)
        {
            let suggestion_revision_id = previous_review.suggestion_revision_id.as_deref().ok_or_else(|| {
                PlanReviewStoreError::InvalidState("the previous change request has no suggestion revision".into())
            })?;
            if revision.content_sha256 == previous_submitted.content_sha256
                || !revision_chain_answers_suggestion(
                    tx,
                    &revision,
                    &previous_review.submitted_revision_id,
                    suggestion_revision_id,
                )
                .await?
            {
                return Err(PlanReviewStoreError::InvalidState(
                    "a change request must be answered by a new update_plan ancestry linked to its suggestion".into(),
                ));
            }
        }
        let materialized = plan_materialization::Entity::find()
            .filter(plan_materialization::Column::DocumentId.eq(submit.document_id))
            .filter(plan_materialization::Column::Generation.eq(submit.expected_generation))
            .filter(plan_materialization::Column::RevisionId.eq(revision_id.as_str()))
            .order_by_desc(plan_materialization::Column::CreatedAt)
            .one(tx.conn()?)
            .await?;
        if materialized.is_none_or(|row| row.state != PlanMaterializationState::Applied) {
            return Err(PlanReviewStoreError::InvalidState(
                "plan.md has not been materialized at the submitted revision".into(),
            ));
        }

        let review_id = uuid::Uuid::new_v4().to_string();
        plan_review_session::Entity::insert(plan_review_session::ActiveModel {
            id: Set(review_id.clone()),
            document_id: Set(submit.document_id.to_owned()),
            submitted_revision_id: Set(revision_id.clone()),
            turn_id: Set(submit.turn_id.map(str::to_owned)),
            assistant_message_id: Set(submit.assistant_message_id.map(str::to_owned)),
            provider_call_id: Set(submit.provider_call_id.map(str::to_owned)),
            provider_kind: Set(submit.provider_kind),
            native_runtime_config_json: Set(native_runtime.cloned().map(Json)),
            state: Set(PlanReviewState::Pending),
            decision_id: Set(None),
            decision_summary: Set(None),
            suggestion_revision_id: Set(None),
            lock_version: Set(0),
            created_at: Set(submit.now),
            updated_at: Set(submit.now),
            decided_at: Set(None),
        })
        .exec_without_returning(tx.conn()?)
        .await?;
        plan_review_draft::Entity::insert(
            source_draft(
                &review_id,
                &revision_id,
                &revision.content_markdown,
                submit.now,
                submit.now,
            )
            .into_active_model(),
        )
        .exec_without_returning(tx.conn()?)
        .await?;

        let changed = plan_document::Entity::update_many()
            .col_expr(plan_document::Column::State, Expr::value(PlanDocumentState::Reviewing))
            .col_expr(
                plan_document::Column::LockVersion,
                Expr::value(document.lock_version + 1),
            )
            .col_expr(plan_document::Column::UpdatedAt, Expr::value(submit.now))
            .filter(plan_document::Column::Id.eq(submit.document_id))
            .filter(plan_document::Column::WorkingGeneration.eq(submit.expected_generation))
            .filter(plan_document::Column::LockVersion.eq(document.lock_version))
            .exec(tx.conn()?)
            .await?
            .rows_affected;
        if changed != 1 {
            return Err(PlanReviewStoreError::Conflict(
                "the plan document changed concurrently".into(),
            ));
        }

        if let Some(turn_id) = submit.turn_id
            && crate::db::sea::ops::turn::wait_for_review(tx, turn_id, submit.now).await? != 1
        {
            return Err(PlanReviewStoreError::InvalidState(
                "the submitting turn is not running".into(),
            ));
        }

        get_review_bundle(tx, &review_id).await
    })
    .await
}

/// A fresh source-mode draft whose baseline and draft are both `markdown`.
fn source_draft(
    review_id: &str,
    base_revision_id: &str,
    markdown: &str,
    created_at: EpochMs,
    updated_at: EpochMs,
) -> plan_review_draft::Model {
    plan_review_draft::Model {
        review_id: review_id.to_owned(),
        base_revision_id: base_revision_id.to_owned(),
        generation: 0,
        mode: PlanReviewDraftMode::Source,
        base_editor_json: None,
        draft_editor_json: None,
        base_normalized_markdown: markdown.to_owned(),
        draft_normalized_markdown: markdown.to_owned(),
        source_text: Some(markdown.to_owned()),
        editor_schema_version: None,
        editor_schema_hash: None,
        global_note: None,
        selection_json: None,
        draft_sha256: markdown_sha256(markdown),
        created_at,
        updated_at,
    }
}

pub async fn get_review(db: &impl Read, review_id: &str) -> PlanReviewStoreResult<plan_review_session::Model> {
    plan_review_session::Entity::find_by_id(review_id)
        .one(db.conn()?)
        .await?
        .ok_or(PlanReviewStoreError::NotFound("plan review"))
}

pub async fn get_pending_review_for_conversation(
    db: &impl Read,
    conversation_id: &str,
) -> PlanReviewStoreResult<Option<plan_review_session::Model>> {
    Ok(plan_review_session::Entity::find()
        .join(JoinType::InnerJoin, plan_review_session::Relation::PlanDocument.def())
        .filter(plan_document::Column::ConversationId.eq(conversation_id))
        .filter(plan_review_session::Column::State.eq(PlanReviewState::Pending))
        .order_by_desc(plan_review_session::Column::CreatedAt)
        .one(db.conn()?)
        .await?)
}

pub async fn list_reviews(db: &impl Read, document_id: &str) -> PlanReviewStoreResult<Vec<plan_review_session::Model>> {
    Ok(plan_review_session::Entity::find()
        .filter(plan_review_session::Column::DocumentId.eq(document_id))
        .order_by_asc(plan_review_session::Column::CreatedAt)
        .all(db.conn()?)
        .await?)
}

async fn get_draft(db: &impl Read, review_id: &str) -> PlanReviewStoreResult<plan_review_draft::Model> {
    plan_review_draft::Entity::find_by_id(review_id)
        .one(db.conn()?)
        .await?
        .ok_or(PlanReviewStoreError::NotFound("plan review draft"))
}

async fn list_comments(db: &impl Read, review_id: &str) -> Result<Vec<plan_comment::Model>, DbErr> {
    plan_comment::Entity::find()
        .filter(plan_comment::Column::ReviewId.eq(review_id))
        .order_by_asc(plan_comment::Column::Position)
        .all(db.conn()?)
        .await
}

pub async fn get_review_bundle(db: &impl Snapshot, review_id: &str) -> PlanReviewStoreResult<PlanReviewBundle> {
    let review = get_review(db, review_id).await?;
    let document = get_document(db, &review.document_id).await?;
    let submitted_revision = get_revision(db, &review.submitted_revision_id).await?;
    let draft = get_draft(db, review_id).await?;
    let comments = list_comments(db, review_id).await?;
    let deliveries = plan_review_delivery::Entity::find()
        .filter(plan_review_delivery::Column::ReviewId.eq(review_id))
        .order_by_asc(plan_review_delivery::Column::CreatedAt)
        .all(db.conn()?)
        .await?;
    Ok(PlanReviewBundle {
        document,
        submitted_revision,
        review,
        draft,
        comments,
        deliveries,
    })
}

fn review_is_pending(review: &plan_review_session::Model) -> PlanReviewStoreResult<()> {
    if review.state != PlanReviewState::Pending {
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

/// Drop every comment the review has not submitted yet.
async fn delete_unsubmitted_comments(tx: &WriteTx, review_id: &str) -> Result<(), DbErr> {
    plan_comment::Entity::delete_many()
        .filter(plan_comment::Column::ReviewId.eq(review_id))
        .filter(plan_comment::Column::State.ne(PlanCommentState::Submitted))
        .exec(tx.conn()?)
        .await?;
    Ok(())
}

pub async fn save_review_draft(
    tx: &WriteTx,
    save: &PlanReviewDraftSave<'_>,
) -> PlanReviewStoreResult<PlanReviewBundle> {
    validate_draft_save(save)?;
    tx.nested(async |tx| {
        let review = get_review(tx, save.review_id).await?;
        review_is_pending(&review)?;
        let current = get_draft(tx, save.review_id).await?;
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
            && current.mode == PlanReviewDraftMode::Source
            && save.mode == PlanReviewDraftMode::Rich;
        // A persisted rich projection from an incompatible editor schema is
        // allowed one lossless downgrade to source mode.  The raw submitted
        // revision becomes the immutable source baseline; all editor-shaped
        // fields are cleared.  Once stored as source this exception cannot be
        // taken again.
        let submitted_revision = get_revision(tx, &review.submitted_revision_id).await?;
        let schema_mismatch_fallback = current.mode == PlanReviewDraftMode::Rich
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
        let owned = |value: Option<&str>| value.map(str::to_owned);
        let changed = plan_review_draft::Entity::update_many()
            .set(plan_review_draft::ActiveModel {
                generation: Set(current.generation + 1),
                mode: Set(save.mode),
                base_editor_json: Set(owned(save.base_editor_json)),
                draft_editor_json: Set(owned(save.draft_editor_json)),
                base_normalized_markdown: Set(save.base_normalized_markdown.to_owned()),
                draft_normalized_markdown: Set(save.draft_normalized_markdown.to_owned()),
                source_text: Set(owned(save.source_text)),
                editor_schema_version: Set(save.editor_schema_version),
                editor_schema_hash: Set(owned(save.editor_schema_hash)),
                global_note: Set(owned(save.global_note)),
                selection_json: Set(owned(save.selection_json)),
                draft_sha256: Set(markdown_sha256(draft_content)),
                updated_at: Set(save.now),
                ..Default::default()
            })
            .filter(plan_review_draft::Column::ReviewId.eq(save.review_id))
            .filter(plan_review_draft::Column::Generation.eq(save.expected_generation))
            .exec(tx.conn()?)
            .await?
            .rows_affected;
        if changed != 1 {
            return Err(PlanReviewStoreError::Conflict(
                "the review draft changed concurrently".into(),
            ));
        }

        let existing_created_at: HashMap<String, EpochMs> = list_comments(tx, save.review_id)
            .await?
            .into_iter()
            .map(|comment| (comment.id, comment.created_at))
            .collect();
        delete_unsubmitted_comments(tx, save.review_id).await?;
        for comment in save.comments {
            plan_comment::Entity::insert(plan_comment::ActiveModel {
                id: Set(comment.id.to_owned()),
                review_id: Set(save.review_id.to_owned()),
                position: Set(comment.position),
                state: Set(comment.state),
                anchor_kind: Set(comment.anchor_kind),
                anchor_json: Set(comment.anchor_json.to_owned()),
                body: Set(comment.body.to_owned()),
                created_at: Set(existing_created_at.get(comment.id).copied().unwrap_or(save.now)),
                updated_at: Set(save.now),
            })
            .exec_without_returning(tx.conn()?)
            .await?;
        }
        get_review_bundle(tx, save.review_id).await
    })
    .await
}

pub async fn discard_review_draft(
    tx: &WriteTx,
    review_id: &str,
    expected_generation: i64,
    now: EpochMs,
) -> PlanReviewStoreResult<PlanReviewBundle> {
    tx.nested(async |tx| {
        let review = get_review(tx, review_id).await?;
        review_is_pending(&review)?;
        let current = get_draft(tx, review_id).await?;
        if current.generation != expected_generation {
            return Err(PlanReviewStoreError::Conflict(format!(
                "expected draft generation {expected_generation}, found {}",
                current.generation
            )));
        }
        let base_revision = get_revision(tx, &current.base_revision_id).await?;
        let (source_text, draft_markdown, draft_sha256) = match current.mode {
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
        let changed = plan_review_draft::Entity::update_many()
            .set(plan_review_draft::ActiveModel {
                generation: Set(expected_generation + 1),
                draft_editor_json: Set(current.base_editor_json.clone()),
                draft_normalized_markdown: Set(draft_markdown),
                source_text: Set(source_text),
                global_note: Set(None),
                selection_json: Set(None),
                draft_sha256: Set(draft_sha256),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(plan_review_draft::Column::ReviewId.eq(review_id))
            .filter(plan_review_draft::Column::Generation.eq(expected_generation))
            .exec(tx.conn()?)
            .await?
            .rows_affected;
        if changed != 1 {
            return Err(PlanReviewStoreError::Conflict(
                "the review draft changed concurrently".into(),
            ));
        }
        delete_unsubmitted_comments(tx, review_id).await?;
        get_review_bundle(tx, review_id).await
    })
    .await
}

fn draft_is_dirty(draft: &plan_review_draft::Model) -> bool {
    match draft.mode {
        PlanReviewDraftMode::Rich => draft.draft_normalized_markdown != draft.base_normalized_markdown,
        PlanReviewDraftMode::Source => draft
            .source_text
            .as_deref()
            .is_none_or(|source| markdown_sha256(source) != markdown_sha256(&draft.base_normalized_markdown)),
    }
}

fn meaningful_comments(comments: &[plan_comment::Model]) -> Vec<&plan_comment::Model> {
    comments
        .iter()
        .filter(|comment| comment.state != PlanCommentState::Deleted)
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

fn decision_matches(review: &plan_review_session::Model, action: PlanReviewDecisionAction) -> bool {
    matches!(
        (review.state, action),
        (PlanReviewState::Approved, PlanReviewDecisionAction::Approve)
            | (
                PlanReviewState::ChangesRequested,
                PlanReviewDecisionAction::RequestChanges
            )
    )
}

/// A queued delivery of `payload_json` for the decided review.
fn queued_delivery(
    id: &str,
    decision: &PlanReviewDecision<'_>,
    target: PlanDeliveryTarget,
    payload_json: String,
) -> plan_review_delivery::ActiveModel {
    plan_review_delivery::ActiveModel {
        id: Set(id.to_owned()),
        review_id: Set(decision.review_id.to_owned()),
        target: Set(target),
        state: Set(PlanDeliveryState::Queued),
        payload_json: Set(payload_json),
        attempt_token: Set(None),
        target_session_id: Set(decision.target_session_id.map(str::to_owned)),
        target_turn_id: Set(decision.target_turn_id.map(str::to_owned)),
        error: Set(None),
        created_at: Set(decision.now),
        updated_at: Set(decision.now),
        dispatched_at: Set(None),
        acknowledged_at: Set(None),
        held_at: Set(None),
    }
}

pub async fn decide_review(
    tx: &WriteTx,
    decision: &PlanReviewDecision<'_>,
) -> PlanReviewStoreResult<PlanReviewDecisionResult> {
    tx.nested(async |tx| {
        let review = get_review(tx, decision.review_id).await?;
        if review.state != PlanReviewState::Pending {
            if review.decision_id.as_deref() == Some(decision.decision_id) && decision_matches(&review, decision.action)
            {
                let suggestion = match review.suggestion_revision_id.as_deref() {
                    Some(id) => Some(get_revision(tx, id).await?),
                    None => None,
                };
                let delivery = plan_review_delivery::Entity::find()
                    .filter(plan_review_delivery::Column::ReviewId.eq(decision.review_id))
                    .one(tx.conn()?)
                    .await?;
                return Ok(PlanReviewDecisionResult {
                    document: get_document(tx, &review.document_id).await?,
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
        let draft = get_draft(tx, decision.review_id).await?;
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
        let comments = list_comments(tx, decision.review_id).await?;
        let active_comments = meaningful_comments(&comments);
        let has_note = draft.global_note.as_deref().is_some_and(|note| !note.trim().is_empty());
        let dirty = draft_is_dirty(&draft);
        let document = get_document(tx, &review.document_id).await?;

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
                    let approved = get_revision(tx, &review.submitted_revision_id).await?;
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
                    plan_review_delivery::Entity::insert(queued_delivery(&delivery_id, decision, target, payload_json))
                        .exec_without_returning(tx.conn()?)
                        .await?;
                    delivery = Some(get_delivery(tx, &delivery_id).await?);
                }
                (
                    PlanReviewState::Approved,
                    PlanDocumentState::Approved,
                    Some(review.submitted_revision_id.clone()),
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
                let submitted = get_revision(tx, &review.submitted_revision_id).await?;
                let suggested_markdown = match draft.mode {
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
                let revision_no = next_revision_no(tx, &review.document_id).await?;
                let revision = plan_revision::Model {
                    parent_revision_id: Some(review.submitted_revision_id.clone()),
                    patch: patch.clone(),
                    editor_json: draft.draft_editor_json.clone(),
                    editor_schema_version: draft.editor_schema_version,
                    editor_schema_hash: draft.editor_schema_hash.clone(),
                    ..revision_row(
                        &suggestion_id,
                        &review.document_id,
                        revision_no,
                        PlanRevisionAuthorKind::UserSuggestion,
                        suggested_markdown,
                        decision.now,
                    )
                };
                plan_revision::Entity::insert(revision.into_active_model())
                    .exec_without_returning(tx.conn()?)
                    .await?;
                plan_comment::Entity::update_many()
                    .col_expr(plan_comment::Column::State, Expr::value(PlanCommentState::Submitted))
                    .col_expr(plan_comment::Column::UpdatedAt, Expr::value(decision.now))
                    .filter(plan_comment::Column::ReviewId.eq(decision.review_id))
                    .filter(plan_comment::Column::State.is_in([PlanCommentState::Draft, PlanCommentState::Active]))
                    .exec(tx.conn()?)
                    .await?;

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
                            state: comment.state.as_str(),
                            anchor_kind: comment.anchor_kind.as_str(),
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
                plan_review_delivery::Entity::insert(queued_delivery(&delivery_id, decision, target, payload_json))
                    .exec_without_returning(tx.conn()?)
                    .await?;
                suggestion = Some(get_revision(tx, &suggestion_id).await?);
                delivery = Some(get_delivery(tx, &delivery_id).await?);
                (
                    PlanReviewState::ChangesRequested,
                    PlanDocumentState::Drafting,
                    document.approved_revision_id.clone(),
                )
            }
        };

        let changed = plan_review_session::Entity::update_many()
            .set(plan_review_session::ActiveModel {
                state: Set(review_state),
                decision_id: Set(Some(decision.decision_id.to_owned())),
                decision_summary: Set(decision.decision_summary.map(str::to_owned)),
                suggestion_revision_id: Set(suggestion.as_ref().map(|revision| revision.id.clone())),
                lock_version: Set(decision.expected_lock_version + 1),
                updated_at: Set(decision.now),
                decided_at: Set(Some(decision.now)),
                ..Default::default()
            })
            .filter(plan_review_session::Column::Id.eq(decision.review_id))
            .filter(plan_review_session::Column::State.eq(PlanReviewState::Pending))
            .filter(plan_review_session::Column::LockVersion.eq(decision.expected_lock_version))
            .exec(tx.conn()?)
            .await?
            .rows_affected;
        if changed != 1 {
            return Err(PlanReviewStoreError::Conflict(
                "the plan review changed concurrently".into(),
            ));
        }
        let changed = plan_document::Entity::update_many()
            .set(plan_document::ActiveModel {
                state: Set(document_state),
                approved_revision_id: Set(approved_revision_id),
                lock_version: Set(document.lock_version + 1),
                updated_at: Set(decision.now),
                ..Default::default()
            })
            .filter(plan_document::Column::Id.eq(review.document_id.as_str()))
            .filter(plan_document::Column::LockVersion.eq(document.lock_version))
            .exec(tx.conn()?)
            .await?
            .rows_affected;
        if changed != 1 {
            return Err(PlanReviewStoreError::Conflict(
                "the plan document changed concurrently".into(),
            ));
        }
        if decision.action == PlanReviewDecisionAction::Approve {
            conversation::update_mode(tx, &document.conversation_id, None, decision.now).await?;
        }

        Ok(PlanReviewDecisionResult {
            document: get_document(tx, &review.document_id).await?,
            review: get_review(tx, decision.review_id).await?,
            suggestion,
            delivery,
        })
    })
    .await
}

pub async fn get_delivery(db: &impl Read, delivery_id: &str) -> PlanReviewStoreResult<plan_review_delivery::Model> {
    plan_review_delivery::Entity::find_by_id(delivery_id)
        .one(db.conn()?)
        .await?
        .ok_or(PlanReviewStoreError::NotFound("plan review delivery"))
}

pub async fn list_recoverable_deliveries(db: &impl Read) -> PlanReviewStoreResult<Vec<plan_review_delivery::Model>> {
    Ok(plan_review_delivery::Entity::find()
        .filter(plan_review_delivery::Column::State.is_in([
            PlanDeliveryState::Queued,
            PlanDeliveryState::Held,
            PlanDeliveryState::InDoubt,
        ]))
        .order_by_asc(plan_review_delivery::Column::CreatedAt)
        .all(db.conn()?)
        .await?)
}

const STARTUP_QUEUE_RESUME_PENDING: &str = "startup_queue_resume_pending";

/// Native continuations that completed before a crash but whose prompt queue
/// has not yet been resumed by the newly constructed runtime. The marker is
/// durable because database startup runs before a `StartTurn` exists.
pub async fn list_startup_queue_resumes(db: &impl Snapshot) -> PlanReviewStoreResult<Vec<(String, String)>> {
    let deliveries = plan_review_delivery::Entity::find()
        .filter(plan_review_delivery::Column::State.eq(PlanDeliveryState::Acknowledged))
        .filter(plan_review_delivery::Column::Error.eq(STARTUP_QUEUE_RESUME_PENDING))
        .order_by_asc(plan_review_delivery::Column::UpdatedAt)
        .all(db.conn()?)
        .await?;
    let mut resumes = Vec::with_capacity(deliveries.len());
    for delivery in deliveries {
        let review = get_review(db, &delivery.review_id).await?;
        let document = get_document(db, &review.document_id).await?;
        resumes.push((delivery.id, document.conversation_id));
    }
    Ok(resumes)
}

pub async fn finish_startup_queue_resume(tx: &WriteTx, delivery_id: &str, now: EpochMs) -> PlanReviewStoreResult<bool> {
    Ok(plan_review_delivery::Entity::update_many()
        .col_expr(plan_review_delivery::Column::Error, Expr::value(Option::<String>::None))
        .col_expr(plan_review_delivery::Column::UpdatedAt, Expr::value(now))
        .filter(plan_review_delivery::Column::Id.eq(delivery_id))
        .filter(plan_review_delivery::Column::State.eq(PlanDeliveryState::Acknowledged))
        .filter(plan_review_delivery::Column::Error.eq(STARTUP_QUEUE_RESUME_PENDING))
        .exec(tx.conn()?)
        .await?
        .rows_affected
        == 1)
}

/// A process cannot know whether a provider consumed a delivery for which no
/// acknowledgement was committed. Startup records that uncertainty but never
/// retries it; only an explicit user continuation may resolve an in-doubt row.
pub async fn reconcile_dispatched_deliveries(tx: &WriteTx, now: EpochMs) -> PlanReviewStoreResult<usize> {
    tx.nested(async |tx| {
        let dispatched = plan_review_delivery::Entity::find()
            .filter(plan_review_delivery::Column::State.eq(PlanDeliveryState::Dispatched))
            .all(tx.conn()?)
            .await?;
        for delivery in &dispatched {
            let continuation_done = match (delivery.target, delivery.target_turn_id.as_deref()) {
                (PlanDeliveryTarget::Native, Some(turn_id)) => crate::db::sea::ops::turn::get(tx, turn_id)
                    .await?
                    .is_some_and(|turn| turn.status == crate::db::entity::turn::TurnStatus::Done),
                _ => false,
            };
            let (state, error) = if continuation_done {
                (PlanDeliveryState::Acknowledged, STARTUP_QUEUE_RESUME_PENDING)
            } else {
                (
                    PlanDeliveryState::InDoubt,
                    "process exited before delivery acknowledgement",
                )
            };
            transition_delivery(
                tx,
                &delivery.id,
                &[PlanDeliveryState::Dispatched],
                state,
                delivery.attempt_token.as_deref(),
                Some(error),
                now,
            )
            .await?;
        }
        Ok(dispatched.len())
    })
    .await
}

async fn transition_delivery(
    tx: &WriteTx,
    delivery_id: &str,
    from: &[PlanDeliveryState],
    to: PlanDeliveryState,
    attempt_token: Option<&str>,
    error: Option<&str>,
    now: EpochMs,
) -> PlanReviewStoreResult<plan_review_delivery::Model> {
    tx.nested(async |tx| {
        let delivery = get_delivery(tx, delivery_id).await?;
        let mut row = plan_review_delivery::ActiveModel {
            state: Set(to),
            attempt_token: Set(attempt_token.map(str::to_owned)),
            error: Set(error.map(str::to_owned)),
            updated_at: Set(now),
            ..Default::default()
        };
        match to {
            PlanDeliveryState::Dispatched => row.dispatched_at = Set(Some(now)),
            PlanDeliveryState::Acknowledged => row.acknowledged_at = Set(Some(now)),
            PlanDeliveryState::Held => row.held_at = Set(Some(now)),
            PlanDeliveryState::Queued | PlanDeliveryState::InDoubt => {}
        }
        let changed = plan_review_delivery::Entity::update_many()
            .set(row)
            .filter(plan_review_delivery::Column::Id.eq(delivery_id))
            .filter(plan_review_delivery::Column::State.is_in(from.iter().copied()))
            .exec(tx.conn()?)
            .await?
            .rows_affected;
        if changed != 1 {
            return Err(PlanReviewStoreError::Conflict(format!(
                "delivery {delivery_id} is not in an allowed source state"
            )));
        }
        let bumped = plan_review_session::Entity::update_many()
            .col_expr(
                plan_review_session::Column::LockVersion,
                Expr::col(plan_review_session::Column::LockVersion).add(1),
            )
            .col_expr(plan_review_session::Column::UpdatedAt, Expr::value(now))
            .filter(plan_review_session::Column::Id.eq(delivery.review_id.as_str()))
            .exec(tx.conn()?)
            .await?
            .rows_affected;
        if bumped != 1 {
            return Err(PlanReviewStoreError::Conflict(
                "the delivery's plan review no longer exists".into(),
            ));
        }
        get_delivery(tx, delivery_id).await
    })
    .await
}

pub async fn mark_delivery_dispatched(
    tx: &WriteTx,
    delivery_id: &str,
    attempt_token: &str,
    now: EpochMs,
) -> PlanReviewStoreResult<plan_review_delivery::Model> {
    transition_delivery(
        tx,
        delivery_id,
        &[PlanDeliveryState::Queued, PlanDeliveryState::Held],
        PlanDeliveryState::Dispatched,
        Some(attempt_token),
        None,
        now,
    )
    .await
}

/// Point a just-dispatched delivery at the turn that will carry it.
async fn set_target_turn(
    tx: &WriteTx,
    delivery_id: &str,
    target_turn_id: &str,
) -> PlanReviewStoreResult<plan_review_delivery::Model> {
    plan_review_delivery::Entity::update_many()
        .col_expr(plan_review_delivery::Column::TargetTurnId, Expr::value(target_turn_id))
        .filter(plan_review_delivery::Column::Id.eq(delivery_id))
        .exec(tx.conn()?)
        .await?;
    get_delivery(tx, delivery_id).await
}

/// Dispatch a native continuation and persist the exact turn identity in the
/// same transaction as the delivery/review version transition. Retries mint a
/// new turn id; leaving the old id on the durable row makes recovery and event
/// consumers point at a turn that will never run.
pub async fn mark_delivery_dispatched_for_turn(
    tx: &WriteTx,
    delivery_id: &str,
    attempt_token: &str,
    target_turn_id: &str,
    now: EpochMs,
) -> PlanReviewStoreResult<plan_review_delivery::Model> {
    tx.nested(async |tx| {
        mark_delivery_dispatched(tx, delivery_id, attempt_token, now).await?;
        set_target_turn(tx, delivery_id, target_turn_id).await
    })
    .await
}

/// Explicit user retry for a delivery whose previous consumption is unknown.
/// Kept separate from the ordinary queued path so startup code cannot
/// accidentally turn inspection of recoverable rows into a blind redelivery.
pub async fn retry_delivery_dispatched(
    tx: &WriteTx,
    delivery_id: &str,
    attempt_token: &str,
    now: EpochMs,
) -> PlanReviewStoreResult<plan_review_delivery::Model> {
    transition_delivery(
        tx,
        delivery_id,
        &[PlanDeliveryState::InDoubt],
        PlanDeliveryState::Dispatched,
        Some(attempt_token),
        None,
        now,
    )
    .await
}

pub async fn retry_delivery_dispatched_for_turn(
    tx: &WriteTx,
    delivery_id: &str,
    attempt_token: &str,
    target_turn_id: &str,
    now: EpochMs,
) -> PlanReviewStoreResult<plan_review_delivery::Model> {
    tx.nested(async |tx| {
        retry_delivery_dispatched(tx, delivery_id, attempt_token, now).await?;
        set_target_turn(tx, delivery_id, target_turn_id).await
    })
    .await
}

/// The dispatched delivery, if `attempt_token` is the attempt it carries.
async fn owned_attempt(
    tx: &WriteTx,
    delivery_id: &str,
    attempt_token: &str,
) -> PlanReviewStoreResult<plan_review_delivery::Model> {
    let delivery = get_delivery(tx, delivery_id).await?;
    if delivery.attempt_token.as_deref() != Some(attempt_token) {
        return Err(PlanReviewStoreError::Conflict(
            "delivery attempt token does not match".into(),
        ));
    }
    Ok(delivery)
}

pub async fn mark_delivery_acknowledged(
    tx: &WriteTx,
    delivery_id: &str,
    attempt_token: &str,
    now: EpochMs,
) -> PlanReviewStoreResult<plan_review_delivery::Model> {
    tx.nested(async |tx| {
        owned_attempt(tx, delivery_id, attempt_token).await?;
        transition_delivery(
            tx,
            delivery_id,
            &[PlanDeliveryState::Dispatched],
            PlanDeliveryState::Acknowledged,
            Some(attempt_token),
            None,
            now,
        )
        .await
    })
    .await
}

pub async fn mark_delivery_held(
    tx: &WriteTx,
    delivery_id: &str,
    error: Option<&str>,
    now: EpochMs,
) -> PlanReviewStoreResult<plan_review_delivery::Model> {
    transition_delivery(
        tx,
        delivery_id,
        &[PlanDeliveryState::Queued, PlanDeliveryState::Dispatched],
        PlanDeliveryState::Held,
        None,
        error,
        now,
    )
    .await
}

pub async fn mark_delivery_in_doubt(
    tx: &WriteTx,
    delivery_id: &str,
    attempt_token: &str,
    error: &str,
    now: EpochMs,
) -> PlanReviewStoreResult<plan_review_delivery::Model> {
    tx.nested(async |tx| {
        owned_attempt(tx, delivery_id, attempt_token).await?;
        transition_delivery(
            tx,
            delivery_id,
            &[PlanDeliveryState::Dispatched],
            PlanDeliveryState::InDoubt,
            Some(attempt_token),
            Some(error),
            now,
        )
        .await
    })
    .await
}

pub async fn pending_materializations(
    db: &impl Read,
    document_id: Option<&str>,
) -> PlanReviewStoreResult<Vec<plan_materialization::Model>> {
    let mut query = plan_materialization::Entity::find()
        .filter(plan_materialization::Column::State.eq(PlanMaterializationState::Pending));
    if let Some(document_id) = document_id {
        query = query.filter(plan_materialization::Column::DocumentId.eq(document_id));
    }
    Ok(query
        .order_by_asc(plan_materialization::Column::CreatedAt)
        .order_by_asc(plan_materialization::Column::Generation)
        .all(db.conn()?)
        .await?)
}

pub async fn latest_materialization(
    db: &impl Read,
    document_id: &str,
) -> PlanReviewStoreResult<Option<plan_materialization::Model>> {
    Ok(plan_materialization::Entity::find()
        .filter(plan_materialization::Column::DocumentId.eq(document_id))
        .order_by_desc(plan_materialization::Column::Generation)
        .one(db.conn()?)
        .await?)
}

/// Move one materialization out of `from`, or say which state it was not in.
async fn settle_materialization(
    tx: &WriteTx,
    id: &str,
    from: PlanMaterializationState,
    to: PlanMaterializationState,
    error: Option<&str>,
    now: EpochMs,
) -> PlanReviewStoreResult<plan_materialization::Model> {
    let changed = plan_materialization::Entity::update_many()
        .set(plan_materialization::ActiveModel {
            state: Set(to),
            force_replace: Set(SqlBool::FALSE),
            error: Set(error.map(str::to_owned)),
            updated_at: Set(now),
            applied_at: Set((to == PlanMaterializationState::Applied).then_some(now)),
            ..Default::default()
        })
        .filter(plan_materialization::Column::Id.eq(id))
        .filter(plan_materialization::Column::State.eq(from))
        .exec(tx.conn()?)
        .await?
        .rows_affected;
    if changed != 1 {
        return Err(PlanReviewStoreError::Conflict(format!(
            "materialization is not {}",
            from.as_str()
        )));
    }
    get_materialization(tx, id).await
}

pub async fn mark_materialization_applied(
    tx: &WriteTx,
    id: &str,
    now: EpochMs,
) -> PlanReviewStoreResult<plan_materialization::Model> {
    settle_materialization(
        tx,
        id,
        PlanMaterializationState::Pending,
        PlanMaterializationState::Applied,
        None,
        now,
    )
    .await
}

pub async fn mark_materialization_conflict(
    tx: &WriteTx,
    id: &str,
    error: &str,
    now: EpochMs,
) -> PlanReviewStoreResult<plan_materialization::Model> {
    settle_materialization(
        tx,
        id,
        PlanMaterializationState::Pending,
        PlanMaterializationState::Conflict,
        Some(error),
        now,
    )
    .await
}

pub async fn mark_materialization_drift(
    tx: &WriteTx,
    id: &str,
    error: &str,
    now: EpochMs,
) -> PlanReviewStoreResult<plan_materialization::Model> {
    settle_materialization(
        tx,
        id,
        PlanMaterializationState::Applied,
        PlanMaterializationState::Conflict,
        Some(error),
        now,
    )
    .await
}

/// Explicit user recovery: make the latest conflicted generation eligible to
/// restore from the database.  Nothing calls this during startup.
pub async fn retry_materialization_from_database(
    tx: &WriteTx,
    document_id: &str,
    now: EpochMs,
) -> PlanReviewStoreResult<plan_materialization::Model> {
    let row = plan_materialization::Entity::find()
        .filter(plan_materialization::Column::DocumentId.eq(document_id))
        .filter(plan_materialization::Column::State.eq(PlanMaterializationState::Conflict))
        .order_by_desc(plan_materialization::Column::Generation)
        .one(tx.conn()?)
        .await?
        .ok_or(PlanReviewStoreError::NotFound("conflicted plan materialization"))?;
    plan_materialization::Entity::update_many()
        .set(plan_materialization::ActiveModel {
            // NULL authorises replacing an absent file.  PlanFileStore treats a
            // conflict retry specially and atomically replaces the unexpected
            // bytes because this function is only reached from the explicit
            // restore action.
            state: Set(PlanMaterializationState::Pending),
            expected_sha256: Set(None),
            force_replace: Set(SqlBool::TRUE),
            error: Set(None),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(plan_materialization::Column::Id.eq(row.id.as_str()))
        .exec(tx.conn()?)
        .await?;
    get_materialization(tx, &row.id).await
}

pub async fn complete_active_document(tx: &WriteTx, conversation_id: &str, now: EpochMs) -> PlanReviewStoreResult<u64> {
    Ok(plan_document::Entity::update_many()
        .col_expr(plan_document::Column::State, Expr::value(PlanDocumentState::Done))
        .col_expr(
            plan_document::Column::LockVersion,
            Expr::col(plan_document::Column::LockVersion).add(1),
        )
        .col_expr(plan_document::Column::UpdatedAt, Expr::value(now))
        .filter(plan_document::Column::ConversationId.eq(conversation_id))
        .filter(plan_document::Column::State.eq(PlanDocumentState::Approved))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

const LEGACY_ORPHAN_SUMMARY: &str = "Imported legacy review has no resumable transcript identity";

/// Convert pre-document plan artifacts into one immutable legacy episode per
/// conversation.  SQL cannot compute SHA-256, so startup runs this idempotent
/// Rust backfill after migration 51.  A conversation already owned by the new
/// flow is never mixed with legacy rows.
pub async fn backfill_legacy_artifacts(tx: &WriteTx, now: EpochMs) -> PlanReviewStoreResult<usize> {
    // Early builds of migration 51 imported legacy pending artifacts as live
    // reviews even though they have no turn/message/call identity and cannot
    // be opened by the review UI. Repair those rows before the idempotence
    // check so an already-upgraded database cannot remain permanently barred.
    let stranded_legacy_documents: Vec<String> = plan_review_session::Entity::find()
        .filter(plan_review_session::Column::ProviderKind.eq(PlanReviewProviderKind::Legacy))
        .filter(plan_review_session::Column::State.eq(PlanReviewState::Pending))
        .select_only()
        .column(plan_review_session::Column::DocumentId)
        .into_tuple()
        .all(tx.conn()?)
        .await?;
    if !stranded_legacy_documents.is_empty() {
        tx.nested(async |tx| {
            plan_review_session::Entity::update_many()
                .col_expr(
                    plan_review_session::Column::State,
                    Expr::value(PlanReviewState::Orphaned),
                )
                .col_expr(
                    plan_review_session::Column::DecisionSummary,
                    Expr::value(LEGACY_ORPHAN_SUMMARY),
                )
                .col_expr(
                    plan_review_session::Column::LockVersion,
                    Expr::col(plan_review_session::Column::LockVersion).add(1),
                )
                .col_expr(plan_review_session::Column::UpdatedAt, Expr::value(now))
                .col_expr(plan_review_session::Column::DecidedAt, Expr::value(now))
                .filter(plan_review_session::Column::ProviderKind.eq(PlanReviewProviderKind::Legacy))
                .filter(plan_review_session::Column::State.eq(PlanReviewState::Pending))
                .exec(tx.conn()?)
                .await?;
            plan_document::Entity::update_many()
                .col_expr(plan_document::Column::State, Expr::value(PlanDocumentState::Drafting))
                .col_expr(
                    plan_document::Column::LockVersion,
                    Expr::col(plan_document::Column::LockVersion).add(1),
                )
                .col_expr(plan_document::Column::UpdatedAt, Expr::value(now))
                .filter(plan_document::Column::Id.is_in(stranded_legacy_documents.iter().map(String::as_str)))
                .filter(plan_document::Column::State.eq(PlanDocumentState::Reviewing))
                .exec(tx.conn()?)
                .await?;
            Ok::<(), PlanReviewStoreError>(())
        })
        .await?;
    }

    let artifacts = mode_artifact::Entity::find()
        .filter(mode_artifact::Column::Kind.eq("plan"))
        .order_by_asc(mode_artifact::Column::ConversationId)
        .order_by_asc(mode_artifact::Column::CreatedAt)
        .order_by_asc(mode_artifact::Column::Id)
        .all(tx.conn()?)
        .await?;
    let mut groups: Vec<(String, Vec<mode_artifact::Model>)> = Vec::new();
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
        let exists = plan_document::Entity::find()
            .filter(plan_document::Column::ConversationId.eq(conversation_id.as_str()))
            .select_only()
            .column(plan_document::Column::Id)
            .into_tuple::<String>()
            .one(tx.conn()?)
            .await?
            .is_some();
        if exists || rows.is_empty() {
            continue;
        }
        tx.nested(async |tx| backfill_one(tx, &conversation_id, &rows, now).await)
            .await?;
        inserted += 1;
    }
    Ok(inserted)
}

/// One conversation's legacy artifacts as a document, its revisions, a
/// pending materialization of the head and, for a pending artifact, an
/// orphaned review.
async fn backfill_one(
    tx: &WriteTx,
    conversation_id: &str,
    rows: &[mode_artifact::Model],
    now: EpochMs,
) -> PlanReviewStoreResult<()> {
    use crate::db::entity::mode_artifact::PlanStatus;

    let first = rows.first().expect("nonempty");
    let head = rows.last().expect("nonempty");
    let document_id = format!("legacy-{}", first.id);
    let pending = Some(head).filter(|row| row.status == PlanStatus::Pending);
    let approved = rows.iter().rev().find(|row| row.status == PlanStatus::Approved);
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
    let head_revision_id = format!("legacy-revision-{}", head.id);
    plan_document::Entity::insert(plan_document::ActiveModel {
        id: Set(document_id.clone()),
        conversation_id: Set(conversation_id.to_owned()),
        state: Set(state),
        head_revision_id: Set(Some(head_revision_id.clone())),
        approved_revision_id: Set(approved.map(|row| format!("legacy-revision-{}", row.id))),
        working_generation: Set(rows.len() as i64),
        file_rel_path: Set(format!("plans/{document_id}/plan.md")),
        lock_version: Set(0),
        created_at: Set(first.created_at),
        updated_at: Set(head.updated_at),
    })
    .exec_without_returning(tx.conn()?)
    .await?;

    let mut parent: Option<String> = None;
    for (index, artifact) in rows.iter().enumerate() {
        let revision_id = format!("legacy-revision-{}", artifact.id);
        let revision = plan_revision::Model {
            parent_revision_id: parent.clone(),
            legacy_source_artifact_id: Some(artifact.id.clone()),
            ..revision_row(
                &revision_id,
                &document_id,
                index as i64 + 1,
                PlanRevisionAuthorKind::Legacy,
                &artifact.content,
                artifact.created_at,
            )
        };
        plan_revision::Entity::insert(revision.into_active_model())
            .exec_without_returning(tx.conn()?)
            .await?;
        parent = Some(revision_id);
    }

    plan_materialization::Entity::insert(
        materialization_row(
            &format!("legacy-materialization-{}", head.id),
            &document_id,
            &head_revision_id,
            rows.len() as i64,
            None,
            &markdown_sha256(&head.content),
            now,
        )
        .into_active_model(),
    )
    .exec_without_returning(tx.conn()?)
    .await?;

    if let Some(artifact) = pending {
        let review_id = format!("legacy-review-{}", artifact.id);
        let revision_id = format!("legacy-revision-{}", artifact.id);
        plan_review_session::Entity::insert(plan_review_session::ActiveModel {
            id: Set(review_id.clone()),
            document_id: Set(document_id.clone()),
            submitted_revision_id: Set(revision_id.clone()),
            turn_id: Set(None),
            assistant_message_id: Set(None),
            provider_call_id: Set(None),
            provider_kind: Set(PlanReviewProviderKind::Legacy),
            native_runtime_config_json: Set(None),
            state: Set(PlanReviewState::Orphaned),
            decision_id: Set(None),
            decision_summary: Set(Some(LEGACY_ORPHAN_SUMMARY.to_owned())),
            suggestion_revision_id: Set(None),
            lock_version: Set(0),
            created_at: Set(artifact.created_at),
            updated_at: Set(artifact.updated_at),
            decided_at: Set(Some(artifact.updated_at)),
        })
        .exec_without_returning(tx.conn()?)
        .await?;
        plan_review_draft::Entity::insert(
            source_draft(
                &review_id,
                &revision_id,
                &artifact.content,
                artifact.created_at,
                artifact.updated_at,
            )
            .into_active_model(),
        )
        .exec_without_returning(tx.conn()?)
        .await?;
    }
    Ok(())
}

/// The delivery states that keep a settled review's barrier up: its
/// continuation is queued, on its way, held, or explicitly in doubt.
const BARRIER_DELIVERY_STATES: [PlanDeliveryState; 4] = [
    PlanDeliveryState::Queued,
    PlanDeliveryState::Dispatched,
    PlanDeliveryState::Held,
    PlanDeliveryState::InDoubt,
];

/// Review cards for every planning episode in one conversation, oldest first,
/// across active and done documents.
pub async fn list_reviews_for_conversation(
    db: &impl Snapshot,
    conversation_id: &str,
) -> Result<Vec<plan_review_session::Model>, DbErr> {
    plan_review_session::Entity::find()
        .join(JoinType::InnerJoin, plan_review_session::Relation::PlanDocument.def())
        .filter(plan_document::Column::ConversationId.eq(conversation_id))
        .order_by_asc(plan_review_session::Column::CreatedAt)
        .all(db.conn()?)
        .await
}

async fn review_has_delivery_barrier(db: &impl Snapshot, review_id: &str) -> Result<bool, DbErr> {
    Ok(plan_review_delivery::Entity::find()
        .filter(plan_review_delivery::Column::ReviewId.eq(review_id))
        .filter(plan_review_delivery::Column::State.is_in(BARRIER_DELIVERY_STATES))
        .select_only()
        .column(plan_review_delivery::Column::Id)
        .into_tuple::<String>()
        .one(db.conn()?)
        .await?
        .is_some())
}

/// The conversation's reviews that hold its barrier: pending, or settled with
/// a continuation not yet acknowledged.
async fn active_barrier_reviews_for_conversation(
    db: &impl Snapshot,
    conversation_id: &str,
) -> Result<Vec<plan_review_session::Model>, DbErr> {
    let mut active = Vec::new();
    for review in list_reviews_for_conversation(db, conversation_id).await? {
        if review.state == PlanReviewState::Pending || review_has_delivery_barrier(db, &review.id).await? {
            active.push(review);
        }
    }
    Ok(active)
}

/// Whether ordinary traffic in this conversation must stop at a plan-review
/// boundary: a review is pending, or a settled one's continuation is still
/// queued, on its way, held or in doubt.
pub async fn has_conversation_barrier(db: &impl Snapshot, conversation_id: &str) -> Result<bool, DbErr> {
    Ok(!active_barrier_reviews_for_conversation(db, conversation_id)
        .await?
        .is_empty())
}

/// Conversations whose currently blocked native continuation depends on
/// something: `frozen_on` asks a review's frozen runtime, and `standing` asks
/// the conversation itself, which is consulted only for an active legacy
/// review with no frozen snapshot. Historical settled reviews are ignored: an
/// old review using A must not freeze A while an unrelated active review uses
/// B. Sorted, without duplicates.
async fn barrier_conversations(
    db: &impl Snapshot,
    frozen_on: impl Fn(&NativePlanReviewRuntimeConfig) -> bool,
    standing: impl Fn(&conversation_entity::Model) -> bool,
) -> Result<Vec<String>, DbErr> {
    let mut blocked = Vec::new();
    for conversation_id in conversation::all_ids(db).await? {
        let active_reviews = active_barrier_reviews_for_conversation(db, &conversation_id).await?;
        if active_reviews.is_empty() {
            continue;
        }
        let conversation = conversation::get_conversation(db, &conversation_id)
            .await?
            .ok_or_else(|| DbErr::RecordNotFound(format!("conversation `{conversation_id}`")))?;
        let mut missing_runtime = false;
        let mut frozen = false;
        for review in active_reviews {
            match review.native_runtime_config_json {
                Some(runtime) => frozen |= frozen_on(&runtime),
                None => missing_runtime = true,
            }
        }
        if frozen || (missing_runtime && standing(&conversation)) {
            blocked.push(conversation_id);
        }
    }
    blocked.sort();
    blocked.dedup();
    Ok(blocked)
}

/// Conversations whose currently blocked native continuation depends on this
/// provider.
pub async fn barrier_conversations_for_provider(db: &impl Snapshot, provider_id: &str) -> Result<Vec<String>, DbErr> {
    barrier_conversations(
        db,
        |runtime| runtime.provider_id == provider_id,
        |conversation| conversation.agent_provider_id.as_deref() == Some(provider_id),
    )
    .await
}

/// Conversations whose currently blocked native continuation depends on this
/// assistant.
pub async fn barrier_conversations_for_assistant(db: &impl Snapshot, assistant_id: &str) -> Result<Vec<String>, DbErr> {
    barrier_conversations(
        db,
        |runtime| runtime.assistant_id.as_deref() == Some(assistant_id),
        |conversation| conversation.assistant_id.as_deref() == Some(assistant_id),
    )
    .await
}

/// Conversations whose currently blocked native continuation was resolved
/// against this exact provider and model. A legacy review with no frozen
/// runtime blocks no model: the conversation never recorded one.
pub async fn barrier_conversations_for_model(
    db: &impl Snapshot,
    provider_id: &str,
    model: &str,
) -> Result<Vec<String>, DbErr> {
    barrier_conversations(
        db,
        |runtime| runtime.provider_id == provider_id && runtime.model == model,
        |_| false,
    )
    .await
}

/// A native review waiting on a person: a document, one applied revision and
/// its head submitted, the way a plan-mode turn leaves it. For tests that
/// need a conversation behind the barrier; `runtime` is what the approved
/// plan would run with, which is what the settings barriers compare against.
#[cfg(any(test, feature = "test-support"))]
pub async fn seed_pending_native_review(
    tx: &WriteTx,
    conversation_id: &str,
    runtime: &NativePlanReviewRuntimeConfig,
) -> PlanReviewStoreResult<PlanReviewBundle> {
    let document = create_or_resume_document(tx, conversation_id, 2).await?;
    let appended = append_assistant_revision(
        tx,
        &PlanRevisionAppend {
            document_id: &document.id,
            expected_generation: 0,
            expected_head_sha256: None,
            content_markdown: "# Plan\n",
            patch: "first patch",
            source_message_id: Some("m1"),
            source_call_id: Some("update-1"),
            responding_to_suggestion_revision_id: None,
            now: 3,
        },
    )
    .await?;
    mark_materialization_applied(tx, &appended.materialization.id, 4).await?;
    submit_native_head_for_review(
        tx,
        &PlanReviewSubmit {
            document_id: &document.id,
            expected_generation: appended.document.working_generation,
            expected_head_sha256: &appended.revision.content_sha256,
            turn_id: None,
            assistant_message_id: Some("m1"),
            provider_call_id: Some("exit-1"),
            provider_kind: PlanReviewProviderKind::Native,
            now: 5,
        },
        runtime,
    )
    .await
}

/// A runtime for [`seed_pending_native_review`]: provider, model and
/// project, everything else off.
#[cfg(any(test, feature = "test-support"))]
pub fn test_runtime(provider_id: &str, model: &str, project: Option<(&str, &str)>) -> NativePlanReviewRuntimeConfig {
    NativePlanReviewRuntimeConfig {
        provider_id: provider_id.into(),
        model: model.into(),
        assistant_id: None,
        thinking_level: None,
        fast: false,
        project_id: project.map(|(id, _)| id.to_owned()),
        project_path: project.map(|(_, path)| path.to_owned()),
        accept_edits: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::cap::Db;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    const SHA: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    /// A conversation with one document, one revision and one review in
    /// `state`, frozen on `provider` (or on nothing, as a legacy row).
    async fn review(db: &Db, conversation: &str, state: &str, provider: Option<&str>, agent_provider: Option<&str>) {
        frozen(
            db,
            conversation,
            state,
            provider.map(|p| (p, None)),
            agent_provider,
            None,
        )
        .await;
    }

    fn sql_text(value: Option<&str>) -> String {
        value.map_or("NULL".to_owned(), |v| format!("'{v}'"))
    }

    /// The same, with the frozen runtime's assistant and the conversation's
    /// own assistant given too.
    async fn frozen(
        db: &Db,
        conversation: &str,
        state: &str,
        runtime: Option<(&str, Option<&str>)>,
        agent_provider: Option<&str>,
        assistant: Option<&str>,
    ) {
        let agent = sql_text(agent_provider);
        let standing_assistant = sql_text(assistant);
        let config = runtime.map_or("NULL".to_owned(), |(p, a)| {
            let a = a.map_or("null".to_owned(), |a| format!("\"{a}\""));
            format!(
                r#"'{{"provider_id":"{p}","model":"m","assistant_id":{a},"thinking_level":null,"fast":false,"project_id":null,"project_path":null,"accept_edits":false}}'"#
            )
        });
        let document_state = if state == "pending" { "reviewing" } else { "approved" };
        execute_for_tests(
            db,
            &format!(
                "INSERT INTO conversations (id, agent_provider_id, assistant_id, created_at, updated_at)
                     VALUES ('{conversation}', {agent}, {standing_assistant}, 1, 1);
                 INSERT INTO plan_documents (id, conversation_id, state, file_rel_path, created_at, updated_at)
                     VALUES ('d-{conversation}', '{conversation}', '{document_state}', 'plan.md', 1, 1);
                 INSERT INTO plan_revisions (id, document_id, revision_no, author_kind, content_markdown, content_sha256, created_at)
                     VALUES ('r-{conversation}', 'd-{conversation}', 1, 'assistant', '# plan', '{SHA}', 1);
                 INSERT INTO plan_review_sessions (id, document_id, submitted_revision_id, provider_kind,
                     native_runtime_config_json, state, created_at, updated_at)
                     VALUES ('s-{conversation}', 'd-{conversation}', 'r-{conversation}', 'native', {config}, '{state}', 1, 1);"
            ),
        )
        .await
        .unwrap();
    }

    async fn delivery(db: &Db, conversation: &str, state: &str) {
        execute_for_tests(
            db,
            &format!(
                "INSERT INTO plan_review_deliveries (id, review_id, target, state, payload_json, created_at, updated_at)
                     VALUES ('dl-{conversation}', 's-{conversation}', 'native', '{state}', '{{}}', 1, 1)"
            ),
        )
        .await
        .unwrap();
    }

    async fn blocked(db: &Db, provider: &str) -> Vec<String> {
        db.read(async |tx| barrier_conversations_for_provider(tx, provider).await)
            .await
            .unwrap()
    }

    /// A pending review blocks the provider it froze; a settled one blocks it
    /// only while its continuation is undelivered; a legacy review with no
    /// frozen runtime falls back to the conversation's own provider; and an
    /// unrelated provider is free.
    #[tokio::test]
    async fn the_barrier_follows_the_frozen_runtime_and_undelivered_continuations() {
        let db = sea_test_db().await;
        review(&db, "pending", "pending", Some("p1"), None).await;
        review(&db, "queued", "approved", Some("p1"), None).await;
        delivery(&db, "queued", "queued").await;
        review(&db, "acked", "approved", Some("p1"), None).await;
        delivery(&db, "acked", "acknowledged").await;
        review(&db, "legacy", "pending", None, Some("p1")).await;
        review(&db, "elsewhere", "pending", Some("p2"), Some("p1")).await;

        assert_eq!(blocked(&db, "p1").await, ["legacy", "pending", "queued"]);
        assert_eq!(blocked(&db, "p2").await, ["elsewhere"]);
        assert!(blocked(&db, "p3").await.is_empty());
    }

    /// The same rules on the assistant: the frozen runtime's assistant, and
    /// the conversation's own assistant for a legacy review.
    #[tokio::test]
    async fn the_assistant_barrier_follows_the_frozen_runtime_and_the_legacy_fallback() {
        let db = sea_test_db().await;
        execute_for_tests(
            &db,
            "INSERT INTO assistants (id, name, created_at, updated_at) VALUES ('a1', 'A', 1, 1), ('a2', 'B', 1, 1)",
        )
        .await
        .unwrap();
        frozen(&db, "frozen", "pending", Some(("p", Some("a1"))), None, Some("a2")).await;
        frozen(&db, "legacy", "pending", None, None, Some("a1")).await;
        frozen(&db, "settled", "approved", Some(("p", Some("a1"))), None, None).await;

        let assistant = |id: &'static str| {
            let db = db.clone();
            async move {
                db.read(async |tx| barrier_conversations_for_assistant(tx, id).await)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(assistant("a1").await, ["frozen", "legacy"]);
        assert!(
            assistant("a2").await.is_empty(),
            "a frozen runtime outranks the conversation's own"
        );
    }

    /// The model barrier matches the frozen provider and model together, and
    /// has no legacy fallback.
    #[tokio::test]
    async fn the_model_barrier_needs_the_frozen_provider_and_model() {
        let db = sea_test_db().await;
        review(&db, "frozen", "pending", Some("p1"), None).await;
        review(&db, "legacy", "pending", None, Some("p1")).await;
        review(&db, "settled", "approved", Some("p2"), None).await;

        let model = |provider: &'static str, model: &'static str| {
            let db = db.clone();
            async move {
                db.read(async |tx| barrier_conversations_for_model(tx, provider, model).await)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(model("p1", "m").await, ["frozen"]);
        assert!(model("p1", "other").await.is_empty(), "the model has to match too");
        assert!(model("p2", "m").await.is_empty(), "a settled review holds nothing");
    }

    /// A conversation's own barrier: a pending review, or a settled one whose
    /// continuation is not yet acknowledged.
    #[tokio::test]
    async fn a_conversation_is_held_by_a_pending_review_or_an_undelivered_continuation() {
        let db = sea_test_db().await;
        review(&db, "pending", "pending", None, None).await;
        review(&db, "queued", "approved", None, None).await;
        delivery(&db, "queued", "held").await;
        review(&db, "acked", "approved", None, None).await;
        delivery(&db, "acked", "acknowledged").await;
        execute_for_tests(
            &db,
            "INSERT INTO conversations (id, created_at, updated_at) VALUES ('quiet', 1, 1)",
        )
        .await
        .unwrap();

        for (conversation, held) in [("pending", true), ("queued", true), ("acked", false), ("quiet", false)] {
            let answer = db
                .read(async |tx| has_conversation_barrier(tx, conversation).await)
                .await
                .unwrap();
            assert_eq!(answer, held, "{conversation}");
        }
    }
}

#[cfg(test)]
mod store_tests {
    use super::*;
    use crate::db::entity::turn::TurnStatus;
    use crate::db::sea::cap::Db;
    use crate::db::sea::{execute_for_tests, sea_test_db};
    use crate::turn::TurnOrigin;

    /// One op in a write of its own.
    macro_rules! w {
        ($db:expr, $($f:ident)::+ ( $($arg:expr),* $(,)? )) => {
            $db.write(async |tx| $($f)::+(tx $(, $arg)*).await).await
        };
    }

    /// One read in a snapshot of its own.
    macro_rules! r {
        ($db:expr, $($f:ident)::+ ( $($arg:expr),* $(,)? )) => {
            $db.read(async |tx| $($f)::+(tx $(, $arg)*).await).await
        };
    }

    async fn seed_conversation(db: &Db, id: &str) {
        db.write(async |tx| conversation::create_conversation(tx, id, None, None, None, 1).await)
            .await
            .unwrap();
    }

    async fn append_first(db: &Db, conversation_id: &str, markdown: &str) -> PlanRevisionAppendResult {
        seed_conversation(db, conversation_id).await;
        let document = w!(db, create_or_resume_document(conversation_id, 2)).unwrap();
        w!(
            db,
            append_assistant_revision(&PlanRevisionAppend {
                document_id: &document.id,
                expected_generation: 0,
                expected_head_sha256: None,
                content_markdown: markdown,
                patch: "*** Add File: plan.md",
                source_message_id: Some("m1"),
                source_call_id: Some("call-1"),
                responding_to_suggestion_revision_id: None,
                now: 3,
            })
        )
        .unwrap()
    }

    async fn mark_applied(db: &Db, appended: &PlanRevisionAppendResult) {
        w!(db, mark_materialization_applied(&appended.materialization.id, 4)).unwrap();
    }

    async fn submit(db: &Db, appended: &PlanRevisionAppendResult, turn_id: Option<&str>) -> PlanReviewBundle {
        w!(
            db,
            submit_native_head_for_review(
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
        )
        .unwrap()
    }

    #[tokio::test]
    async fn append_uses_generation_and_hash_cas() {
        let db = sea_test_db().await;
        let first = append_first(&db, "c1", "# First\n").await;

        let stale = w!(
            db,
            append_assistant_revision(&PlanRevisionAppend {
                document_id: &first.document.id,
                expected_generation: 0,
                expected_head_sha256: None,
                content_markdown: "# Stale\n",
                patch: "stale",
                source_message_id: None,
                source_call_id: None,
                responding_to_suggestion_revision_id: None,
                now: 4,
            },)
        );
        assert!(matches!(stale, Err(PlanReviewStoreError::Conflict(_))));
        assert_eq!(
            r!(db, get_head_revision(&first.document.id)).unwrap().unwrap().id,
            first.revision.id
        );
        assert_eq!(r!(db, list_revisions(&first.document.id)).unwrap().len(), 1);
    }

    /// A review is of what plan.md says on disk: until the head revision has
    /// been written there, submitting it is refused and nothing is recorded.
    #[tokio::test]
    async fn a_revision_not_yet_on_disk_cannot_be_submitted() {
        let db = sea_test_db().await;
        let first = append_first(
            &db, "c1", "# Plan
",
        )
        .await;
        let refused = w!(
            db,
            submit_native_head_for_review(
                &PlanReviewSubmit {
                    document_id: &first.document.id,
                    expected_generation: first.document.working_generation,
                    expected_head_sha256: &first.revision.content_sha256,
                    turn_id: None,
                    assistant_message_id: Some("m1"),
                    provider_call_id: Some("call-1"),
                    provider_kind: PlanReviewProviderKind::Native,
                    now: 5,
                },
                &NativePlanReviewRuntimeConfig::fixture(),
            )
        );
        assert!(matches!(refused, Err(PlanReviewStoreError::InvalidState(_))));
        assert!(r!(db, list_reviews(&first.document.id)).unwrap().is_empty());
        assert!(!r!(db, has_conversation_barrier("c1")).unwrap());

        mark_applied(&db, &first).await;
        assert_eq!(submit(&db, &first, None).await.review.state, PlanReviewState::Pending);
    }

    #[tokio::test]
    async fn submit_and_waiting_review_commit_together_and_survive_reconcile() {
        let db = sea_test_db().await;
        let first = append_first(&db, "c1", "# Plan\n").await;
        mark_applied(&db, &first).await;
        w!(
            db,
            crate::db::sea::ops::turn::begin("t1", "c1", TurnOrigin::Desktop, None, 4)
        )
        .unwrap();

        let review = submit(&db, &first, Some("t1")).await;
        assert_eq!(review.review.state, PlanReviewState::Pending);
        let turn = crate::db::sea::ops::turn::list_for_conversation(&db, "c1")
            .await
            .unwrap()
            .remove(0);
        assert_eq!(turn.status, TurnStatus::WaitingReview);
    }

    #[tokio::test]
    async fn approve_accepts_only_a_pristine_draft() {
        let db = sea_test_db().await;
        let first = append_first(&db, "c1", "# Plan\n").await;
        mark_applied(&db, &first).await;
        w!(db, conversation::update_mode("c1", Some("plan"), 4)).unwrap();
        let review = submit(&db, &first, None).await;

        let approved = w!(
            db,
            decide_review(&PlanReviewDecision {
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
            },)
        )
        .unwrap();
        assert_eq!(
            approved.document.approved_revision_id.as_deref(),
            Some(first.revision.id.as_str())
        );
        assert_eq!(
            conversation::get_conversation(&db, "c1").await.unwrap().unwrap().mode,
            None,
            "approval and leaving plan mode are one transaction"
        );

        let second = append_first(&db, "c2", "# Plan\n").await;
        mark_applied(&db, &second).await;
        let review = submit(&db, &second, None).await;
        let dirty = w!(
            db,
            save_review_draft(&PlanReviewDraftSave {
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
            },)
        )
        .unwrap();
        let result = w!(
            db,
            decide_review(&PlanReviewDecision {
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
            },)
        );
        assert!(matches!(result, Err(PlanReviewStoreError::InvalidState(_))));

        // The first rich projection carries two values.  Treating the current
        // edited document as both baseline and draft would make this pristine
        // and allow approval; preserving the immutable parsed baseline must
        // keep it dirty.
        let third = append_first(&db, "c3", "# Plan\n").await;
        mark_applied(&db, &third).await;
        let review = submit(&db, &third, None).await;
        let base = r#"{"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Plan"}]}]}"#;
        let edited = r#"{"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Changed"}]}]}"#;
        let dirty = w!(
            db,
            save_review_draft(&PlanReviewDraftSave {
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
            },)
        )
        .unwrap();
        let result = w!(
            db,
            decide_review(&PlanReviewDecision {
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
            },)
        );
        assert!(matches!(result, Err(PlanReviewStoreError::InvalidState(_))));
    }

    #[tokio::test]
    async fn incompatible_rich_schema_can_fall_back_to_source_once_without_rebasing() {
        let db = sea_test_db().await;
        let first = append_first(&db, "c1", "# Plan\n").await;
        mark_applied(&db, &first).await;
        let review = submit(&db, &first, None).await;
        let base = r#"{"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Plan"}]}]}"#;
        let edited = r#"{"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Changed"}]}]}"#;
        let rich = w!(
            db,
            save_review_draft(&PlanReviewDraftSave {
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
            },)
        )
        .unwrap();

        let stale = w!(
            db,
            save_review_draft(&PlanReviewDraftSave {
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
            },)
        );
        assert!(matches!(stale, Err(PlanReviewStoreError::Conflict(_))));

        let source = w!(
            db,
            save_review_draft(&PlanReviewDraftSave {
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
            },)
        )
        .unwrap();
        assert_eq!(source.draft.mode, PlanReviewDraftMode::Source);
        assert_eq!(source.draft.base_normalized_markdown, "# Plan\n");
        assert_eq!(source.draft.draft_normalized_markdown, "# Changed\n");
        assert!(source.draft.base_editor_json.is_none());
        assert!(source.draft.draft_editor_json.is_none());

        let rebase = w!(
            db,
            save_review_draft(&PlanReviewDraftSave {
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
            },)
        );
        assert!(matches!(rebase, Err(PlanReviewStoreError::Conflict(_))));

        let approval = w!(
            db,
            decide_review(&PlanReviewDecision {
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
            },)
        );
        assert!(matches!(approval, Err(PlanReviewStoreError::InvalidState(_))));
    }

    #[tokio::test]
    async fn continuation_delivery_holds_the_conversation_barrier_until_acknowledged() {
        let db = sea_test_db().await;
        let first = append_first(&db, "c1", "# Plan\n").await;
        mark_applied(&db, &first).await;
        let review = submit(&db, &first, None).await;
        assert!(r!(db, has_conversation_barrier("c1")).unwrap());

        let decided = w!(
            db,
            decide_review(&PlanReviewDecision {
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
            },)
        )
        .unwrap();
        let delivery = decided.delivery.unwrap();
        assert_eq!(
            decided.review.lock_version, 1,
            "decision + queued delivery is version 1"
        );
        assert!(
            r!(db, has_conversation_barrier("c1")).unwrap(),
            "queued continuation still blocks"
        );
        w!(db, mark_delivery_dispatched(&delivery.id, "attempt-1", 7)).unwrap();
        assert_eq!(r!(db, get_review(&review.review.id)).unwrap().lock_version, 2);
        assert!(
            r!(db, has_conversation_barrier("c1")).unwrap(),
            "dispatched continuation still blocks"
        );
        w!(db, mark_delivery_acknowledged(&delivery.id, "attempt-1", 8)).unwrap();
        assert_eq!(r!(db, get_review(&review.review.id)).unwrap().lock_version, 3);
        assert!(
            !r!(db, has_conversation_barrier("c1")).unwrap(),
            "acknowledgement releases the barrier"
        );
    }

    #[tokio::test]
    async fn startup_reconciliation_versions_each_dispatched_delivery_as_in_doubt() {
        let db = sea_test_db().await;
        let first = append_first(&db, "c1", "# Plan\n").await;
        mark_applied(&db, &first).await;
        let review = submit(&db, &first, None).await;
        let decided = w!(
            db,
            decide_review(&PlanReviewDecision {
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
            },)
        )
        .unwrap();
        let delivery = decided.delivery.unwrap();
        w!(db, mark_delivery_dispatched(&delivery.id, "attempt-1", 7)).unwrap();
        assert_eq!(r!(db, get_review(&review.review.id)).unwrap().lock_version, 2);

        assert_eq!(w!(db, reconcile_dispatched_deliveries(8)).unwrap(), 1);
        assert_eq!(
            r!(db, get_delivery(&delivery.id)).unwrap().state,
            PlanDeliveryState::InDoubt
        );
        assert_eq!(r!(db, get_review(&review.review.id)).unwrap().lock_version, 3);
        assert_eq!(w!(db, reconcile_dispatched_deliveries(9)).unwrap(), 0);
        assert_eq!(r!(db, get_review(&review.review.id)).unwrap().lock_version, 3);
    }

    #[tokio::test]
    async fn startup_acknowledges_a_native_delivery_whose_persisted_continuation_turn_finished() {
        let db = sea_test_db().await;
        let first = append_first(&db, "c1", "# Plan\n").await;
        mark_applied(&db, &first).await;
        let review = submit(&db, &first, None).await;
        let decided = w!(
            db,
            decide_review(&PlanReviewDecision {
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
            },)
        )
        .unwrap();
        let delivery = decided.delivery.unwrap();
        w!(
            db,
            mark_delivery_dispatched_for_turn(&delivery.id, "attempt-1", "continuation-done", 7)
        )
        .unwrap();
        w!(
            db,
            crate::db::sea::ops::turn::begin("continuation-done", "c1", TurnOrigin::PlanReview, None, 8)
        )
        .unwrap();
        w!(
            db,
            crate::db::sea::ops::turn::finish("continuation-done", TurnStatus::Done, None, 9)
        )
        .unwrap();

        assert_eq!(w!(db, reconcile_dispatched_deliveries(10)).unwrap(), 1);
        assert_eq!(
            r!(db, get_delivery(&delivery.id)).unwrap().state,
            PlanDeliveryState::Acknowledged
        );
        assert!(!r!(db, has_conversation_barrier("c1")).unwrap());
        assert_eq!(r!(db, get_review(&review.review.id)).unwrap().lock_version, 3);
        assert_eq!(
            r!(db, list_startup_queue_resumes()).unwrap(),
            [(delivery.id.clone(), "c1".to_string())]
        );
        assert!(w!(db, finish_startup_queue_resume(&delivery.id, 11)).unwrap());
        assert!(r!(db, list_startup_queue_resumes()).unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_explicit_native_retry_persists_its_new_continuation_turn_identity() {
        let db = sea_test_db().await;
        let first = append_first(&db, "c1", "# Plan\n").await;
        mark_applied(&db, &first).await;
        let review = submit(&db, &first, None).await;
        let decided = w!(
            db,
            decide_review(&PlanReviewDecision {
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
            },)
        )
        .unwrap();
        let delivery = decided.delivery.unwrap();
        w!(db, mark_delivery_dispatched(&delivery.id, "attempt-old", 7)).unwrap();
        w!(db, mark_delivery_in_doubt(&delivery.id, "attempt-old", "unknown", 8)).unwrap();
        let retried = w!(
            db,
            retry_delivery_dispatched_for_turn(&delivery.id, "attempt-new", "continuation-new", 9)
        )
        .unwrap();
        assert_eq!(retried.target_turn_id.as_deref(), Some("continuation-new"));
        assert_eq!(retried.attempt_token.as_deref(), Some("attempt-new"));
    }

    #[tokio::test]
    async fn native_runtime_selection_is_strict_and_survives_a_fresh_database_read() {
        let db = sea_test_db().await;
        let review_id = {
            let first = append_first(&db, "c1", "# Plan\n").await;
            mark_applied(&db, &first).await;
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
            w!(
                db,
                submit_native_head_for_review(
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
            )
            .unwrap()
            .review
            .id
        };

        let stored = r!(db, get_review(&review_id))
            .unwrap()
            .native_runtime_config_json
            .unwrap()
            .0;
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

    /// Built through the real review flow; the barrier is read in a snapshot.
    #[tokio::test]
    async fn runtime_mutation_guards_ignore_settled_historical_reviews() {
        let db = sea_test_db().await;
        let first = append_first(&db, "c1", "# First plan\n").await;
        mark_applied(&db, &first).await;
        let runtime_a = NativePlanReviewRuntimeConfig {
            provider_id: "provider-a".into(),
            model: "model-a".into(),
            assistant_id: Some("assistant-a".into()),
            ..NativePlanReviewRuntimeConfig::fixture()
        };
        let first_review = w!(
            db,
            submit_native_head_for_review(
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
        )
        .unwrap();
        w!(
            db,
            decide_review(&PlanReviewDecision {
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
            },)
        )
        .unwrap();
        assert_eq!(w!(db, complete_active_document("c1", 7)).unwrap(), 1);

        let document = w!(db, create_or_resume_document("c1", 8)).unwrap();
        let second = w!(
            db,
            append_assistant_revision(&PlanRevisionAppend {
                document_id: &document.id,
                expected_generation: 0,
                expected_head_sha256: None,
                content_markdown: "# Second plan\n",
                patch: "second plan",
                source_message_id: Some("m2"),
                source_call_id: Some("update-b"),
                responding_to_suggestion_revision_id: None,
                now: 9,
            },)
        )
        .unwrap();
        mark_applied(&db, &second).await;
        let runtime_b = NativePlanReviewRuntimeConfig {
            provider_id: "provider-b".into(),
            model: "model-b".into(),
            assistant_id: Some("assistant-b".into()),
            ..NativePlanReviewRuntimeConfig::fixture()
        };
        w!(
            db,
            submit_native_head_for_review(
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
        )
        .unwrap();

        let blocked = |provider: &'static str, model: &'static str| {
            let db = db.clone();
            async move { r!(db, barrier_conversations_for_model(provider, model)).unwrap() }
        };
        assert!(blocked("provider-a", "model-a").await.is_empty());
        assert_eq!(blocked("provider-b", "model-b").await, ["c1"]);
    }

    #[tokio::test]
    async fn a_change_request_requires_a_new_linked_update_before_resubmission() {
        let db = sea_test_db().await;
        let first = append_first(&db, "c1", "# Plan\n").await;
        mark_applied(&db, &first).await;
        let review = submit(&db, &first, None).await;
        let feedback = w!(
            db,
            save_review_draft(&PlanReviewDraftSave {
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
            },)
        )
        .unwrap();
        let changed = w!(
            db,
            decide_review(&PlanReviewDecision {
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
            },)
        )
        .unwrap();
        let delivery = changed.delivery.unwrap();
        w!(db, mark_delivery_dispatched(&delivery.id, "attempt-1", 8)).unwrap();
        w!(db, mark_delivery_acknowledged(&delivery.id, "attempt-1", 9)).unwrap();

        let unchanged = w!(
            db,
            submit_native_head_for_review(
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
            )
        );
        assert!(matches!(unchanged, Err(PlanReviewStoreError::InvalidState(_))));

        let unlinked = w!(
            db,
            append_assistant_revision(&PlanRevisionAppend {
                document_id: &first.document.id,
                expected_generation: first.document.working_generation,
                expected_head_sha256: Some(&first.revision.content_sha256),
                content_markdown: "# Plan\n\nUnlinked change.\n",
                patch: "unlinked",
                source_message_id: Some("m2"),
                source_call_id: Some("call-update-1"),
                responding_to_suggestion_revision_id: None,
                now: 11,
            },)
        )
        .unwrap();
        mark_applied(&db, &unlinked).await;
        let unlinked_submit = w!(
            db,
            submit_native_head_for_review(
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
            )
        );
        assert!(matches!(unlinked_submit, Err(PlanReviewStoreError::InvalidState(_))));

        let suggestion_id = changed.review.suggestion_revision_id.unwrap();
        let linked = w!(
            db,
            append_assistant_revision(&PlanRevisionAppend {
                document_id: &unlinked.document.id,
                expected_generation: unlinked.document.working_generation,
                expected_head_sha256: Some(&unlinked.revision.content_sha256),
                content_markdown: "# Plan\n\nVerified change.\n",
                patch: "linked",
                source_message_id: Some("m2"),
                source_call_id: Some("call-update-2"),
                responding_to_suggestion_revision_id: Some(&suggestion_id),
                now: 13,
            },)
        )
        .unwrap();
        mark_applied(&db, &linked).await;
        let reverted = w!(
            db,
            append_assistant_revision(&PlanRevisionAppend {
                document_id: &linked.document.id,
                expected_generation: linked.document.working_generation,
                expected_head_sha256: Some(&linked.revision.content_sha256),
                content_markdown: "# Plan\n",
                patch: "reverse the linked change",
                source_message_id: Some("m2"),
                source_call_id: Some("call-update-revert"),
                responding_to_suggestion_revision_id: None,
                now: 14,
            },)
        )
        .unwrap();
        mark_applied(&db, &reverted).await;
        let reverted_submit = w!(
            db,
            submit_native_head_for_review(
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
            )
        );
        assert!(matches!(reverted_submit, Err(PlanReviewStoreError::InvalidState(_))));

        let final_patch = w!(
            db,
            append_assistant_revision(&PlanRevisionAppend {
                document_id: &reverted.document.id,
                expected_generation: reverted.document.working_generation,
                expected_head_sha256: Some(&reverted.revision.content_sha256),
                content_markdown: "# Plan\n\nVerified change.\n\nTests included.\n",
                patch: "final unlinked patch",
                source_message_id: Some("m2"),
                source_call_id: Some("call-update-3"),
                responding_to_suggestion_revision_id: None,
                now: 16,
            },)
        )
        .unwrap();
        mark_applied(&db, &final_patch).await;
        let resubmitted = submit(&db, &final_patch, None).await;
        assert_eq!(resubmitted.submitted_revision.id, final_patch.revision.id);
    }

    #[tokio::test]
    async fn request_changes_is_idempotent_and_suggestion_does_not_advance_head() {
        let db = sea_test_db().await;
        let first = append_first(&db, "c1", "# Plan\n").await;
        mark_applied(&db, &first).await;
        let review = submit(&db, &first, None).await;
        let comments = [PlanCommentSave {
            id: "comment-1",
            position: 0,
            state: PlanCommentState::Active,
            anchor_kind: PlanCommentAnchorKind::Source,
            anchor_json: r##"{"fromUtf16":0,"toUtf16":6,"quote":"# Plan"}"##,
            body: "Rename this heading",
        }];
        let dirty = w!(
            db,
            save_review_draft(&PlanReviewDraftSave {
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
            },)
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
        let first_result = w!(db, decide_review(&decision)).unwrap();
        let replay = w!(db, decide_review(&decision)).unwrap();

        assert_eq!(
            first_result.review.suggestion_revision_id,
            replay.review.suggestion_revision_id
        );
        assert_eq!(
            first_result.delivery.as_ref().unwrap().id,
            replay.delivery.as_ref().unwrap().id
        );
        assert_eq!(
            r!(db, get_document(&first.document.id)).unwrap().head_revision_id,
            Some(first.revision.id)
        );
        let suggestion = first_result.suggestion.unwrap();
        assert_eq!(
            suggestion.parent_revision_id.as_deref(),
            Some(review.submitted_revision.id.as_str())
        );
        assert_eq!(r!(db, list_revisions(&first.document.id)).unwrap().len(), 2);
        let payload: serde_json::Value = serde_json::from_str(&first_result.delivery.unwrap().payload_json).unwrap();
        assert_eq!(payload["suggestionRevisionId"], suggestion.id);
        assert_eq!(payload["comments"][0]["body"], "Rename this heading");
    }

    #[tokio::test]
    async fn rich_suggestion_patch_uses_the_exact_submitted_revision_as_its_base() {
        let db = sea_test_db().await;
        let first = append_first(&db, "c1", "# Plan  \n").await;
        mark_applied(&db, &first).await;
        let review = submit(&db, &first, None).await;
        let dirty = w!(db, save_review_draft(&PlanReviewDraftSave {
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
        ))
        .unwrap();
        let changed = w!(
            db,
            decide_review(&PlanReviewDecision {
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
            },)
        )
        .unwrap();

        let suggestion = changed.suggestion.unwrap();
        assert_eq!(suggestion.patch, markdown_diff("# Plan  \n", "# Better plan\n"));
        let payload: serde_json::Value = serde_json::from_str(&changed.delivery.unwrap().payload_json).unwrap();
        assert_eq!(payload["baseSha256"], first.revision.content_sha256);
        assert_eq!(payload["suggestedPatch"], suggestion.patch.unwrap());
    }

    #[tokio::test]
    async fn draft_save_rejects_blank_non_deleted_comments() {
        let db = sea_test_db().await;
        let first = append_first(&db, "c1", "# Plan\n").await;
        mark_applied(&db, &first).await;
        let review = submit(&db, &first, None).await;
        let comments = [PlanCommentSave {
            id: "blank-comment",
            position: 0,
            state: PlanCommentState::Active,
            anchor_kind: PlanCommentAnchorKind::Source,
            anchor_json: r##"{"fromUtf16":0,"toUtf16":6,"quote":"# Plan"}"##,
            body: "  \n",
        }];

        let result = w!(
            db,
            save_review_draft(&PlanReviewDraftSave {
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
            },)
        );

        assert!(matches!(result, Err(PlanReviewStoreError::InvalidState(_))));
    }

    #[tokio::test]
    async fn legacy_backfill_is_idempotent_hashes_content_and_never_creates_a_hidden_barrier() {
        let db = sea_test_db().await;
        seed_conversation(&db, "c1").await;
        let artifact = mode_artifact::Model {
            id: "a1".into(),
            conversation_id: "c1".into(),
            kind: "plan".into(),
            content: "# Legacy\n".into(),
            status: crate::db::entity::mode_artifact::PlanStatus::Pending,
            created_at: 2,
            updated_at: 2,
        };
        mode_artifact::Entity::insert(artifact.clone().into_active_model())
            .exec_without_returning(db.conn().unwrap())
            .await
            .unwrap();

        assert_eq!(w!(db, backfill_legacy_artifacts(3)).unwrap(), 1);
        assert_eq!(w!(db, backfill_legacy_artifacts(4)).unwrap(), 0);
        let document = r!(db, get_active_document("c1")).unwrap().unwrap();
        let revision = r!(db, get_head_revision(&document.id)).unwrap().unwrap();
        assert_eq!(
            revision.legacy_source_artifact_id.as_deref(),
            Some(artifact.id.as_str())
        );
        assert_eq!(revision.content_sha256, markdown_sha256("# Legacy\n"));
        assert_eq!(document.state, PlanDocumentState::Drafting);
        let reviews = r!(db, list_reviews(&document.id)).unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].state, PlanReviewState::Orphaned);
        assert!(r!(db, get_pending_review_for_conversation("c1")).unwrap().is_none());
        assert!(!r!(db, has_conversation_barrier("c1")).unwrap());

        // Also repair databases touched by the earlier migration-51 backfill,
        // which created an unreachable pending review and reviewing document.
        execute_for_tests(
            &db,
            &format!(
                "UPDATE plan_review_sessions SET state = 'pending', decided_at = NULL WHERE id = '{}';
                 UPDATE plan_documents SET state = 'reviewing' WHERE id = '{}'",
                reviews[0].id, document.id
            ),
        )
        .await
        .unwrap();
        assert!(r!(db, has_conversation_barrier("c1")).unwrap());
        assert_eq!(w!(db, backfill_legacy_artifacts(5)).unwrap(), 0);
        assert!(!r!(db, has_conversation_barrier("c1")).unwrap());
        assert_eq!(
            r!(db, get_document(&document.id)).unwrap().state,
            PlanDocumentState::Drafting
        );
    }

    #[tokio::test]
    async fn completing_a_conversation_marks_the_new_document_done() {
        let db = sea_test_db().await;
        let first = append_first(&db, "c1", "# Plan\n").await;
        mark_applied(&db, &first).await;
        let review = submit(&db, &first, None).await;
        w!(
            db,
            decide_review(&PlanReviewDecision {
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
            },)
        )
        .unwrap();
        assert!(r!(db, get_approved_revision_for_conversation("c1")).unwrap().is_some());

        w!(db, crate::db::sea::ops::plan::complete_active("c1", 10)).unwrap();
        assert_eq!(
            r!(db, get_document(&first.document.id)).unwrap().state,
            PlanDocumentState::Done
        );
        assert!(r!(db, get_active_document("c1")).unwrap().is_none());
        assert!(r!(db, get_approved_revision_for_conversation("c1")).unwrap().is_none());
    }
}

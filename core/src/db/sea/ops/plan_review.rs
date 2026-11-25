//! The plan-review store on SeaORM: durable plan documents, their revisions,
//! review sessions with their drafts and comments, and plan-file
//! materializations.
//!
//! No function opens a top-level transaction. A mutation runs in a savepoint of
//! the caller's write (`WriteTx::nested`), so a failure undoes exactly what it
//! did, and the caller's `Db::write` — `BEGIN IMMEDIATE` — is what holds the
//! lock across its reads.
//!
//! Every read that issues several statements and joins the answers takes
//! `&impl Snapshot`.

use std::collections::{HashMap, HashSet};

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{
    ColumnTrait, DbErr, EntityTrait, IntoActiveModel, JoinType, QueryFilter, QueryOrder, QuerySelect, RelationTrait,
};
use sha2::{Digest, Sha256};

use crate::db::entity::plan_comment::{PlanCommentAnchorKind, PlanCommentState};
use crate::db::entity::plan_document::PlanDocumentState;
use crate::db::entity::plan_materialization::PlanMaterializationState;
use crate::db::entity::plan_review_draft::PlanReviewDraftMode;
use crate::db::entity::plan_review_session::{PlanReviewProviderKind, PlanReviewState};
use crate::db::entity::plan_revision::PlanRevisionAuthorKind;
use crate::db::entity::{
    mode_artifact, plan_comment, plan_document, plan_materialization, plan_review_draft, plan_review_session,
    plan_revision,
};
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, Snapshot, WriteTx};
use crate::db::sea::ops::conversation;
use crate::db::types::{EpochMs, SqlBool};

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
    /// Only used for `Approve`: updates `conversations.accept_edits` when `Some`.
    pub accept_edits: Option<bool>,
    pub decision_summary: Option<&'a str>,
    pub now: i64,
}

#[derive(Debug)]
pub struct PlanReviewDecisionResult {
    pub document: plan_document::Model,
    pub review: plan_review_session::Model,
    pub approved_revision: Option<plan_revision::Model>,
    pub suggestion: Option<plan_revision::Model>,
    pub edited_by_user: bool,
    /// Human/model-readable feedback (global note, inline comments, user edit diff).
    pub feedback: Option<String>,
    /// True when this call matched an already-committed `decision_id`.
    pub replayed: bool,
}

#[derive(Debug)]
pub struct PlanReviewBundle {
    pub document: plan_document::Model,
    pub submitted_revision: plan_revision::Model,
    pub review: plan_review_session::Model,
    pub draft: plan_review_draft::Model,
    pub comments: Vec<plan_comment::Model>,
}

/// Format optional global note, inline comments, and user edit diff into clean
/// markdown feedback that travels directly in the tool result (or ACP steer).
pub fn format_review_feedback(
    patch: Option<&str>,
    comments: &[&plan_comment::Model],
    global_note: Option<&str>,
) -> Option<String> {
    let mut sections: Vec<String> = Vec::new();
    if let Some(note) = global_note.map(str::trim).filter(|s| !s.is_empty()) {
        sections.push(format!("### Overall note\n{note}"));
    }
    if !comments.is_empty() {
        let mut lines = Vec::with_capacity(comments.len() + 1);
        lines.push("### Inline comments".to_string());
        for comment in comments {
            let body = comment.body.trim();
            if body.is_empty() {
                continue;
            }
            let quote = serde_json::from_str::<serde_json::Value>(&comment.anchor_json)
                .ok()
                .and_then(|v| v.get("quote").and_then(|q| q.as_str()).map(str::to_owned))
                .filter(|q| !q.trim().is_empty());
            let orphan_tag = if comment.state == PlanCommentState::Orphaned {
                " [original anchor moved]"
            } else {
                ""
            };
            if let Some(quote) = quote {
                lines.push(format!(
                    "- On `{}`{orphan_tag}: {body}",
                    quote.trim().replace('\n', " ")
                ));
            } else {
                lines.push(format!("- {body}{orphan_tag}"));
            }
        }
        if lines.len() > 1 {
            sections.push(lines.join("\n"));
        }
    }
    if let Some(patch) = patch.map(str::trim).filter(|s| !s.is_empty()) {
        sections.push(format!("### User edits to plan.md\n```diff\n{patch}\n```"));
    }
    if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n\n"))
    }
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

pub async fn submit_head_for_review(
    tx: &WriteTx,
    submit: &PlanReviewSubmit<'_>,
) -> PlanReviewStoreResult<PlanReviewBundle> {
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
            native_runtime_config_json: Set(None),
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
    Ok(PlanReviewBundle {
        document,
        submitted_revision,
        review,
        draft,
        comments,
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

pub async fn decide_review(
    tx: &WriteTx,
    decision: &PlanReviewDecision<'_>,
) -> PlanReviewStoreResult<PlanReviewDecisionResult> {
    tx.nested(async |tx| {
        let review = get_review(tx, decision.review_id).await?;
        if review.state != PlanReviewState::Pending {
            if review.decision_id.as_deref() == Some(decision.decision_id) && decision_matches(&review, decision.action)
            {
                let document = get_document(tx, &review.document_id).await?;
                let suggestion = match review.suggestion_revision_id.as_deref() {
                    Some(id) => Some(get_revision(tx, id).await?),
                    None => None,
                };
                let approved_revision = match document.approved_revision_id.as_deref() {
                    Some(id) if review.state == PlanReviewState::Approved => Some(get_revision(tx, id).await?),
                    _ => None,
                };
                let edited_by_user = approved_revision
                    .as_ref()
                    .is_some_and(|rev| rev.id != review.submitted_revision_id);
                return Ok(PlanReviewDecisionResult {
                    document,
                    review,
                    approved_revision,
                    suggestion,
                    edited_by_user,
                    feedback: None,
                    replayed: true,
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
        let submitted = get_revision(tx, &review.submitted_revision_id).await?;

        let draft_markdown = match draft.mode {
            // A comment-only rich review must not silently rewrite the
            // submitted source just because the editor normalised it.
            PlanReviewDraftMode::Rich if !dirty => submitted.content_markdown.as_str(),
            PlanReviewDraftMode::Rich => draft.draft_normalized_markdown.as_str(),
            PlanReviewDraftMode::Source => draft
                .source_text
                .as_deref()
                .ok_or_else(|| PlanReviewStoreError::InvalidState("source draft has no exact source text".into()))?,
        };
        let patch = markdown_diff(&submitted.content_markdown, draft_markdown);
        let edited_by_user = patch.is_some();

        let mut suggestion = None;
        let mut next_head_revision_id = document.head_revision_id.clone();
        let mut next_working_generation = document.working_generation;

        let (review_state, document_state, approved_revision_id, feedback) = match decision.action {
            PlanReviewDecisionAction::Approve => {
                // Approve allows edits, inline comments, and a global note:
                // - User edits become a new revision that advances head & plan.md.
                // - Inline comments and the global note travel as optional approval feedback.
                let final_approved_id = if edited_by_user {
                    let edited_id = uuid::Uuid::new_v4().to_string();
                    let revision_no = next_revision_no(tx, &review.document_id).await?;
                    let revision = plan_revision::Model {
                        parent_revision_id: Some(review.submitted_revision_id.clone()),
                        patch: patch.clone(),
                        editor_json: draft.draft_editor_json.clone(),
                        editor_schema_version: draft.editor_schema_version,
                        editor_schema_hash: draft.editor_schema_hash.clone(),
                        ..revision_row(
                            &edited_id,
                            &review.document_id,
                            revision_no,
                            PlanRevisionAuthorKind::UserSuggestion,
                            draft_markdown,
                            decision.now,
                        )
                    };
                    let desired_sha = revision.content_sha256.clone();
                    plan_revision::Entity::insert(revision.into_active_model())
                        .exec_without_returning(tx.conn()?)
                        .await?;
                    next_head_revision_id = Some(edited_id.clone());
                    next_working_generation = document.working_generation + 1;
                    let materialization_id = uuid::Uuid::new_v4().to_string();
                    plan_materialization::Entity::insert(
                        materialization_row(
                            &materialization_id,
                            &review.document_id,
                            &edited_id,
                            next_working_generation,
                            Some(&submitted.content_sha256),
                            &desired_sha,
                            decision.now,
                        )
                        .into_active_model(),
                    )
                    .exec_without_returning(tx.conn()?)
                    .await?;
                    suggestion = Some(get_revision(tx, &edited_id).await?);
                    edited_id
                } else {
                    review.submitted_revision_id.clone()
                };

                if !active_comments.is_empty() {
                    plan_comment::Entity::update_many()
                        .col_expr(plan_comment::Column::State, Expr::value(PlanCommentState::Submitted))
                        .col_expr(plan_comment::Column::UpdatedAt, Expr::value(decision.now))
                        .filter(plan_comment::Column::ReviewId.eq(decision.review_id))
                        .filter(plan_comment::Column::State.is_in([PlanCommentState::Draft, PlanCommentState::Active]))
                        .exec(tx.conn()?)
                        .await?;
                }

                // On approve, user edits are already reflected in the approved
                // plan markdown itself; only inline comments and global note
                // go into the extra feedback block (plus diff if edited).
                let feedback = format_review_feedback(patch.as_deref(), &active_comments, draft.global_note.as_deref());
                (
                    PlanReviewState::Approved,
                    PlanDocumentState::Approved,
                    Some(final_approved_id),
                    feedback,
                )
            }
            PlanReviewDecisionAction::RequestChanges => {
                if !dirty && !has_note && active_comments.is_empty() {
                    return Err(PlanReviewStoreError::InvalidState(
                        "request changes requires an edit, comment or global note".into(),
                    ));
                }
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
                        draft_markdown,
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
                suggestion = Some(get_revision(tx, &suggestion_id).await?);
                let feedback = format_review_feedback(patch.as_deref(), &active_comments, draft.global_note.as_deref());
                (
                    PlanReviewState::ChangesRequested,
                    PlanDocumentState::Drafting,
                    document.approved_revision_id.clone(),
                    feedback,
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
                head_revision_id: Set(next_head_revision_id),
                working_generation: Set(next_working_generation),
                approved_revision_id: Set(approved_revision_id.clone()),
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
            if let Some(accept_edits) = decision.accept_edits {
                conversation::update_accept_edits(tx, &document.conversation_id, accept_edits, decision.now).await?;
            }
        }

        let approved_revision = match approved_revision_id.as_deref() {
            Some(id) if decision.action == PlanReviewDecisionAction::Approve => Some(get_revision(tx, id).await?),
            _ => None,
        };
        Ok(PlanReviewDecisionResult {
            document: get_document(tx, &review.document_id).await?,
            review: get_review(tx, decision.review_id).await?,
            approved_revision,
            suggestion,
            edited_by_user,
            feedback,
            replayed: false,
        })
    })
    .await
}

/// Mark a single pending review as `orphaned` (e.g., when its live turn is
/// cancelled before the user decides). Returns the updated review & document
/// if the transition landed, or `None` if it was already settled.
pub async fn orphan_review(
    tx: &WriteTx,
    review_id: &str,
    summary: &str,
    now: EpochMs,
) -> PlanReviewStoreResult<Option<(plan_document::Model, plan_review_session::Model)>> {
    tx.nested(async |tx| {
        let Some(review) = plan_review_session::Entity::find_by_id(review_id)
            .one(tx.conn()?)
            .await?
        else {
            return Ok(None);
        };
        if review.state != PlanReviewState::Pending {
            return Ok(None);
        }
        let changed = plan_review_session::Entity::update_many()
            .col_expr(
                plan_review_session::Column::State,
                Expr::value(PlanReviewState::Orphaned),
            )
            .col_expr(plan_review_session::Column::DecisionSummary, Expr::value(summary))
            .col_expr(
                plan_review_session::Column::LockVersion,
                Expr::col(plan_review_session::Column::LockVersion).add(1),
            )
            .col_expr(plan_review_session::Column::UpdatedAt, Expr::value(now))
            .col_expr(plan_review_session::Column::DecidedAt, Expr::value(now))
            .filter(plan_review_session::Column::Id.eq(review_id))
            .filter(plan_review_session::Column::State.eq(PlanReviewState::Pending))
            .exec(tx.conn()?)
            .await?
            .rows_affected;
        if changed != 1 {
            return Ok(None);
        }
        plan_document::Entity::update_many()
            .col_expr(plan_document::Column::State, Expr::value(PlanDocumentState::Drafting))
            .col_expr(
                plan_document::Column::LockVersion,
                Expr::col(plan_document::Column::LockVersion).add(1),
            )
            .col_expr(plan_document::Column::UpdatedAt, Expr::value(now))
            .filter(plan_document::Column::Id.eq(review.document_id.as_str()))
            .filter(plan_document::Column::State.eq(PlanDocumentState::Reviewing))
            .exec(tx.conn()?)
            .await?;
        Ok(Some((
            get_document(tx, &review.document_id).await?,
            get_review(tx, review_id).await?,
        )))
    })
    .await
}

/// Orphan every `pending` review left behind by a previous process at startup.
/// A plan review lives inside its turn just like a tool approval; after a
/// restart the plan document remains readable in history without blocking the
/// conversation.
pub async fn orphan_pending_reviews(tx: &WriteTx, now: EpochMs) -> PlanReviewStoreResult<usize> {
    tx.nested(async |tx| {
        let pending_docs: Vec<String> = plan_review_session::Entity::find()
            .filter(plan_review_session::Column::State.eq(PlanReviewState::Pending))
            .select_only()
            .column(plan_review_session::Column::DocumentId)
            .into_tuple()
            .all(tx.conn()?)
            .await?;
        if pending_docs.is_empty() {
            return Ok(0);
        }
        let count = plan_review_session::Entity::update_many()
            .col_expr(
                plan_review_session::Column::State,
                Expr::value(PlanReviewState::Orphaned),
            )
            .col_expr(
                plan_review_session::Column::DecisionSummary,
                Expr::value("The turn ended before this plan was decided"),
            )
            .col_expr(
                plan_review_session::Column::LockVersion,
                Expr::col(plan_review_session::Column::LockVersion).add(1),
            )
            .col_expr(plan_review_session::Column::UpdatedAt, Expr::value(now))
            .col_expr(plan_review_session::Column::DecidedAt, Expr::value(now))
            .filter(plan_review_session::Column::State.eq(PlanReviewState::Pending))
            .exec(tx.conn()?)
            .await?
            .rows_affected as usize;
        plan_document::Entity::update_many()
            .col_expr(plan_document::Column::State, Expr::value(PlanDocumentState::Drafting))
            .col_expr(
                plan_document::Column::LockVersion,
                Expr::col(plan_document::Column::LockVersion).add(1),
            )
            .col_expr(plan_document::Column::UpdatedAt, Expr::value(now))
            .filter(plan_document::Column::Id.is_in(pending_docs.iter().map(String::as_str)))
            .filter(plan_document::Column::State.eq(PlanDocumentState::Reviewing))
            .exec(tx.conn()?)
            .await?;
        Ok(count)
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

#[cfg(test)]
mod store_tests {
    use super::*;
    use crate::db::entity::turn::TurnStatus;
    use crate::db::sea::cap::Db;
    use crate::db::sea::{execute_for_tests, sea_test_db};
    use crate::turn::TurnOrigin;

    macro_rules! w {
        ($db:expr, $($f:ident)::+ ( $($arg:expr),* $(,)? )) => {
            $db.write(async |tx| $($f)::+(tx $(, $arg)*).await).await
        };
    }

    macro_rules! r {
        ($db:expr, $($f:ident)::+ ( $($arg:expr),* $(,)? )) => {
            $db.read(async |tx| $($f)::+(tx $(, $arg)*).await).await
        };
    }

    async fn seed_conversation(db: &Db, id: &str) {
        db.write(async |tx| conversation::create(tx, id, None, None, None, 1).await)
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
            submit_head_for_review(&PlanReviewSubmit {
                document_id: &appended.document.id,
                expected_generation: appended.document.working_generation,
                expected_head_sha256: &appended.revision.content_sha256,
                turn_id,
                assistant_message_id: Some("m1"),
                provider_call_id: Some("call-1"),
                provider_kind: PlanReviewProviderKind::Native,
                now: 5,
            })
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
            })
        );
        assert!(matches!(stale, Err(PlanReviewStoreError::Conflict(_))));
        assert_eq!(
            r!(db, get_head_revision(&first.document.id)).unwrap().unwrap().id,
            first.revision.id
        );
        assert_eq!(r!(db, list_revisions(&first.document.id)).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_revision_not_yet_on_disk_cannot_be_submitted() {
        let db = sea_test_db().await;
        let first = append_first(&db, "c1", "# Plan\n").await;
        let refused = w!(
            db,
            submit_head_for_review(&PlanReviewSubmit {
                document_id: &first.document.id,
                expected_generation: first.document.working_generation,
                expected_head_sha256: &first.revision.content_sha256,
                turn_id: None,
                assistant_message_id: Some("m1"),
                provider_call_id: Some("call-1"),
                provider_kind: PlanReviewProviderKind::Native,
                now: 5,
            })
        );
        assert!(matches!(refused, Err(PlanReviewStoreError::InvalidState(_))));
        assert!(r!(db, list_reviews(&first.document.id)).unwrap().is_empty());

        mark_applied(&db, &first).await;
        assert_eq!(submit(&db, &first, None).await.review.state, PlanReviewState::Pending);
    }

    #[tokio::test]
    async fn submit_leaves_the_turn_running_for_synchronous_approval() {
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
        assert_eq!(turn.status, TurnStatus::Running);
    }

    #[tokio::test]
    async fn approve_supports_edits_comments_note_and_accept_edits_flag() {
        let db = sea_test_db().await;
        let first = append_first(&db, "c1", "# Plan\n- Step 1\n").await;
        mark_applied(&db, &first).await;
        w!(db, conversation::update_mode("c1", Some("plan"), 4)).unwrap();
        let review = submit(&db, &first, None).await;

        let comments = [PlanCommentSave {
            id: "c-1",
            position: 0,
            state: PlanCommentState::Active,
            anchor_kind: PlanCommentAnchorKind::Source,
            anchor_json: r##"{"kind":"source_range","from":7,"to":15,"quote":"- Step 1","prefix":"","suffix":""}"##,
            body: "Add error handling here",
        }];
        let edited = w!(
            db,
            save_review_draft(&PlanReviewDraftSave {
                review_id: &review.review.id,
                expected_generation: 0,
                mode: PlanReviewDraftMode::Source,
                base_editor_json: None,
                draft_editor_json: None,
                base_normalized_markdown: "# Plan\n- Step 1\n",
                draft_normalized_markdown: "# Plan\n- Step 1 (with tests)\n",
                source_text: Some("# Plan\n- Step 1 (with tests)\n"),
                editor_schema_version: None,
                editor_schema_hash: None,
                schema_fallback_from_version: None,
                schema_fallback_from_hash: None,
                global_note: Some("Ship fast"),
                selection_json: None,
                comments: &comments,
                now: 6,
            })
        )
        .unwrap();

        let approved = w!(
            db,
            decide_review(&PlanReviewDecision {
                review_id: &review.review.id,
                decision_id: "approve-with-edits",
                expected_lock_version: 0,
                expected_draft_generation: edited.draft.generation,
                expected_draft_sha256: &edited.draft.draft_sha256,
                action: PlanReviewDecisionAction::Approve,
                accept_edits: Some(true),
                decision_summary: None,
                now: 7,
            })
        )
        .unwrap();

        assert!(approved.edited_by_user);
        assert!(!approved.replayed);
        let approved_rev = approved.approved_revision.expect("approved revision");
        assert_ne!(approved_rev.id, first.revision.id);
        assert_eq!(approved_rev.content_markdown, "# Plan\n- Step 1 (with tests)\n");
        assert_eq!(
            approved.document.head_revision_id.as_deref(),
            Some(approved_rev.id.as_str())
        );
        assert_eq!(
            approved.document.approved_revision_id.as_deref(),
            Some(approved_rev.id.as_str())
        );
        let fb = approved.feedback.expect("feedback");
        assert!(fb.contains("Ship fast"));
        assert!(fb.contains("Add error handling here"));
        assert!(fb.contains("- Step 1 (with tests)"));

        let conv = conversation::get(&db, "c1").await.unwrap().unwrap();
        assert_eq!(conv.mode, None);
        assert!(conv.accept_edits.get());

        // Idempotent replay with the same decision_id succeeds.
        let replay = w!(
            db,
            decide_review(&PlanReviewDecision {
                review_id: &review.review.id,
                decision_id: "approve-with-edits",
                expected_lock_version: 0,
                expected_draft_generation: edited.draft.generation,
                expected_draft_sha256: &edited.draft.draft_sha256,
                action: PlanReviewDecisionAction::Approve,
                accept_edits: Some(true),
                decision_summary: None,
                now: 8,
            })
        )
        .unwrap();
        assert!(replay.replayed);
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
            })
        )
        .unwrap();

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
            })
        )
        .unwrap();
        assert_eq!(source.draft.mode, PlanReviewDraftMode::Source);

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
            })
        );
        assert!(matches!(rebase, Err(PlanReviewStoreError::Conflict(_))));
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
            })
        )
        .unwrap();
        let decision = PlanReviewDecision {
            review_id: &review.review.id,
            decision_id: "decision-1",
            expected_lock_version: 0,
            expected_draft_generation: dirty.draft.generation,
            expected_draft_sha256: &dirty.draft.draft_sha256,
            action: PlanReviewDecisionAction::RequestChanges,
            accept_edits: None,
            decision_summary: Some("one edit"),
            now: 7,
        };
        let first_result = w!(db, decide_review(&decision)).unwrap();
        let replay = w!(db, decide_review(&decision)).unwrap();

        assert!(!first_result.replayed);
        assert!(replay.replayed);
        assert_eq!(
            first_result.review.suggestion_revision_id,
            replay.review.suggestion_revision_id
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
        let fb = first_result.feedback.unwrap();
        assert!(fb.contains("Keep the tests focused."));
        assert!(fb.contains("Rename this heading"));
        assert!(fb.contains("# Better plan"));
    }

    #[tokio::test]
    async fn rich_suggestion_patch_uses_the_exact_submitted_revision_as_its_base() {
        let db = sea_test_db().await;
        let first = append_first(&db, "c1", "# Plan  \n").await;
        mark_applied(&db, &first).await;
        let review = submit(&db, &first, None).await;
        let dirty = w!(
            db,
            save_review_draft(&PlanReviewDraftSave {
                review_id: &review.review.id,
                expected_generation: 0,
                mode: PlanReviewDraftMode::Rich,
                base_editor_json: Some(
                    r#"{"type":"doc","content":[{"type":"heading","attrs":{"level":1},"content":[{"type":"text","text":"Plan"}]}]}"#
                ),
                draft_editor_json: Some(
                    r#"{"type":"doc","content":[{"type":"heading","attrs":{"level":1},"content":[{"type":"text","text":"Better plan"}]}]}"#
                ),
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
            })
        )
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
                accept_edits: None,
                decision_summary: None,
                now: 7,
            })
        )
        .unwrap();

        let suggestion = changed.suggestion.unwrap();
        assert_eq!(suggestion.patch, markdown_diff("# Plan  \n", "# Better plan\n"));
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
            })
        );

        assert!(matches!(result, Err(PlanReviewStoreError::InvalidState(_))));
    }

    #[tokio::test]
    async fn orphan_review_and_startup_orphan_pending_reviews_work_cleanly() {
        let db = sea_test_db().await;
        let first = append_first(&db, "c1", "# Plan 1\n").await;
        mark_applied(&db, &first).await;
        let rev1 = submit(&db, &first, None).await;

        let second = append_first(&db, "c2", "# Plan 2\n").await;
        mark_applied(&db, &second).await;
        let rev2 = submit(&db, &second, None).await;

        let orphaned = w!(db, orphan_review(&rev1.review.id, "cancelled", 10))
            .unwrap()
            .expect("orphaned");
        assert_eq!(orphaned.1.state, PlanReviewState::Orphaned);
        assert_eq!(orphaned.0.state, PlanDocumentState::Drafting);

        let count = w!(db, orphan_pending_reviews(11)).unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            r!(db, get_review(&rev2.review.id)).unwrap().state,
            PlanReviewState::Orphaned
        );
    }

    #[tokio::test]
    async fn legacy_backfill_is_idempotent_and_hashes_content() {
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
        assert_eq!(w!(db, backfill_legacy_artifacts(5)).unwrap(), 0);
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
                accept_edits: Some(false),
                decision_summary: None,
                now: 6,
            })
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

//! Persistence rows for the durable plan document and its reviews.
//!
//! These rows deliberately stay below IPC.  JSON columns are storage details;
//! command handlers must decode them into their exact request/response types
//! instead of exposing the strings.

use diesel::prelude::*;
use serde::{Deserialize, Serialize};

use crate::db::schema::{
    plan_comments, plan_documents, plan_materializations, plan_review_deliveries, plan_review_drafts,
    plan_review_sessions, plan_revisions,
};

macro_rules! stored_enum {
    ($name:ident { $($variant:ident),+ $(,)? }) => {
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            Serialize,
            strum::IntoStaticStr,
            strum::EnumString,
        )]
        #[serde(rename_all = "snake_case")]
        #[strum(serialize_all = "snake_case")]
        pub enum $name { $($variant),+ }

        impl $name {
            pub fn as_str(self) -> &'static str { self.into() }

            pub fn parse(value: &str) -> Result<Self, String> {
                value
                    .parse()
                    .map_err(|_| format!("unknown {} '{}'", stringify!($name), value))
            }
        }
    };
}

stored_enum!(PlanDocumentState {
    Drafting,
    Reviewing,
    Approved,
    Done,
});
stored_enum!(PlanRevisionAuthorKind {
    Assistant,
    UserSuggestion,
    Legacy,
});
stored_enum!(PlanReviewProviderKind { Native, Acp, Legacy });
stored_enum!(PlanReviewState {
    Pending,
    Approved,
    ChangesRequested,
    Orphaned,
});
stored_enum!(PlanReviewDraftMode { Rich, Source });
stored_enum!(PlanCommentState {
    Draft,
    Active,
    Orphaned,
    Submitted,
    Deleted,
});
stored_enum!(PlanCommentAnchorKind { Rich, Source });
stored_enum!(PlanDeliveryTarget { Native, Acp });
stored_enum!(PlanDeliveryState {
    Queued,
    Dispatched,
    Acknowledged,
    Held,
    InDoubt,
});
stored_enum!(PlanMaterializationState {
    Pending,
    Applied,
    Conflict,
});

/// The effective runtime selection of the native turn that submitted a plan.
///
/// This is deliberately strict JSON rather than a snapshot of the whole chat
/// request. The continuation needs exactly these five values; consulting the
/// conversation again would let a later preference/model change move the
/// second half of one provider transcript to another upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePlanReviewRuntimeConfig {
    pub provider_id: String,
    pub model: String,
    pub assistant_id: Option<String>,
    pub thinking_level: Option<crate::provider::capabilities::StoredThinkingLevel>,
    pub fast: bool,
    pub project_id: Option<String>,
    pub project_path: Option<String>,
    pub accept_edits: bool,
}

#[cfg(test)]
impl NativePlanReviewRuntimeConfig {
    pub fn fixture() -> Self {
        Self {
            provider_id: "provider-fixture".into(),
            model: "model-fixture".into(),
            assistant_id: None,
            thinking_level: None,
            fast: false,
            project_id: None,
            project_path: None,
            accept_edits: false,
        }
    }
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = plan_documents)]
pub struct PlanDocumentRow {
    pub id: String,
    pub conversation_id: String,
    pub state: String,
    pub head_revision_id: Option<String>,
    pub approved_revision_id: Option<String>,
    pub working_generation: i64,
    pub file_rel_path: String,
    pub lock_version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl PlanDocumentRow {
    pub fn state(&self) -> Result<PlanDocumentState, String> {
        PlanDocumentState::parse(&self.state)
    }
}

#[derive(Debug, Insertable)]
#[diesel(table_name = plan_documents)]
pub struct PlanDocumentInsert<'a> {
    pub id: &'a str,
    pub conversation_id: &'a str,
    pub state: &'a str,
    pub head_revision_id: Option<&'a str>,
    pub approved_revision_id: Option<&'a str>,
    pub working_generation: i64,
    pub file_rel_path: &'a str,
    pub lock_version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = plan_revisions)]
pub struct PlanRevisionRow {
    pub id: String,
    pub document_id: String,
    pub revision_no: i64,
    pub parent_revision_id: Option<String>,
    pub author_kind: String,
    pub content_markdown: String,
    pub content_sha256: String,
    pub patch: Option<String>,
    pub source_message_id: Option<String>,
    pub source_call_id: Option<String>,
    pub responding_to_suggestion_revision_id: Option<String>,
    pub editor_json: Option<String>,
    pub editor_schema_version: Option<i32>,
    pub editor_schema_hash: Option<String>,
    pub legacy_source_artifact_id: Option<String>,
    pub created_at: i64,
}

impl PlanRevisionRow {
    pub fn author_kind(&self) -> Result<PlanRevisionAuthorKind, String> {
        PlanRevisionAuthorKind::parse(&self.author_kind)
    }
}

#[derive(Debug, Insertable)]
#[diesel(table_name = plan_revisions)]
pub struct PlanRevisionInsert<'a> {
    pub id: &'a str,
    pub document_id: &'a str,
    pub revision_no: i64,
    pub parent_revision_id: Option<&'a str>,
    pub author_kind: &'a str,
    pub content_markdown: &'a str,
    pub content_sha256: &'a str,
    pub patch: Option<&'a str>,
    pub source_message_id: Option<&'a str>,
    pub source_call_id: Option<&'a str>,
    pub responding_to_suggestion_revision_id: Option<&'a str>,
    pub editor_json: Option<&'a str>,
    pub editor_schema_version: Option<i32>,
    pub editor_schema_hash: Option<&'a str>,
    pub legacy_source_artifact_id: Option<&'a str>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = plan_review_sessions)]
pub struct PlanReviewSessionRow {
    pub id: String,
    pub document_id: String,
    pub submitted_revision_id: String,
    pub turn_id: Option<String>,
    pub assistant_message_id: Option<String>,
    pub provider_call_id: Option<String>,
    pub provider_kind: String,
    pub native_runtime_config_json: Option<String>,
    pub state: String,
    pub decision_id: Option<String>,
    pub decision_summary: Option<String>,
    pub suggestion_revision_id: Option<String>,
    pub lock_version: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub decided_at: Option<i64>,
}

impl PlanReviewSessionRow {
    pub fn provider_kind(&self) -> Result<PlanReviewProviderKind, String> {
        PlanReviewProviderKind::parse(&self.provider_kind)
    }

    pub fn state(&self) -> Result<PlanReviewState, String> {
        PlanReviewState::parse(&self.state)
    }

    pub fn native_runtime_config(&self) -> Result<Option<NativePlanReviewRuntimeConfig>, String> {
        self.native_runtime_config_json
            .as_deref()
            .map(|json| {
                serde_json::from_str(json)
                    .map_err(|error| format!("stored native plan-review runtime config is invalid: {error}"))
            })
            .transpose()
    }
}

#[derive(Debug, Insertable)]
#[diesel(table_name = plan_review_sessions)]
pub struct PlanReviewSessionInsert<'a> {
    pub id: &'a str,
    pub document_id: &'a str,
    pub submitted_revision_id: &'a str,
    pub turn_id: Option<&'a str>,
    pub assistant_message_id: Option<&'a str>,
    pub provider_call_id: Option<&'a str>,
    pub provider_kind: &'a str,
    pub native_runtime_config_json: Option<&'a str>,
    pub state: &'a str,
    pub decision_id: Option<&'a str>,
    pub decision_summary: Option<&'a str>,
    pub suggestion_revision_id: Option<&'a str>,
    pub lock_version: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub decided_at: Option<i64>,
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = plan_review_drafts)]
pub struct PlanReviewDraftRow {
    pub review_id: String,
    pub base_revision_id: String,
    pub generation: i64,
    pub mode: String,
    pub base_editor_json: Option<String>,
    pub draft_editor_json: Option<String>,
    pub base_normalized_markdown: String,
    pub draft_normalized_markdown: String,
    pub source_text: Option<String>,
    pub editor_schema_version: Option<i32>,
    pub editor_schema_hash: Option<String>,
    pub global_note: Option<String>,
    pub selection_json: Option<String>,
    pub draft_sha256: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl PlanReviewDraftRow {
    pub fn mode(&self) -> Result<PlanReviewDraftMode, String> {
        PlanReviewDraftMode::parse(&self.mode)
    }
}

#[derive(Debug, Insertable)]
#[diesel(table_name = plan_review_drafts)]
pub struct PlanReviewDraftInsert<'a> {
    pub review_id: &'a str,
    pub base_revision_id: &'a str,
    pub generation: i64,
    pub mode: &'a str,
    pub base_editor_json: Option<&'a str>,
    pub draft_editor_json: Option<&'a str>,
    pub base_normalized_markdown: &'a str,
    pub draft_normalized_markdown: &'a str,
    pub source_text: Option<&'a str>,
    pub editor_schema_version: Option<i32>,
    pub editor_schema_hash: Option<&'a str>,
    pub global_note: Option<&'a str>,
    pub selection_json: Option<&'a str>,
    pub draft_sha256: &'a str,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = plan_comments)]
pub struct PlanCommentRow {
    pub id: String,
    pub review_id: String,
    pub position: i32,
    pub state: String,
    pub anchor_kind: String,
    pub anchor_json: String,
    pub body: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl PlanCommentRow {
    pub fn state(&self) -> Result<PlanCommentState, String> {
        PlanCommentState::parse(&self.state)
    }

    pub fn anchor_kind(&self) -> Result<PlanCommentAnchorKind, String> {
        PlanCommentAnchorKind::parse(&self.anchor_kind)
    }
}

#[derive(Debug, Insertable)]
#[diesel(table_name = plan_comments)]
pub struct PlanCommentInsert<'a> {
    pub id: &'a str,
    pub review_id: &'a str,
    pub position: i32,
    pub state: &'a str,
    pub anchor_kind: &'a str,
    pub anchor_json: &'a str,
    pub body: &'a str,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = plan_review_deliveries)]
pub struct PlanReviewDeliveryRow {
    pub id: String,
    pub review_id: String,
    pub target: String,
    pub state: String,
    pub payload_json: String,
    pub attempt_token: Option<String>,
    pub target_session_id: Option<String>,
    pub target_turn_id: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub dispatched_at: Option<i64>,
    pub acknowledged_at: Option<i64>,
    pub held_at: Option<i64>,
}

impl PlanReviewDeliveryRow {
    pub fn target(&self) -> Result<PlanDeliveryTarget, String> {
        PlanDeliveryTarget::parse(&self.target)
    }

    pub fn state(&self) -> Result<PlanDeliveryState, String> {
        PlanDeliveryState::parse(&self.state)
    }
}

#[derive(Debug, Insertable)]
#[diesel(table_name = plan_review_deliveries)]
pub struct PlanReviewDeliveryInsert<'a> {
    pub id: &'a str,
    pub review_id: &'a str,
    pub target: &'a str,
    pub state: &'a str,
    pub payload_json: &'a str,
    pub attempt_token: Option<&'a str>,
    pub target_session_id: Option<&'a str>,
    pub target_turn_id: Option<&'a str>,
    pub error: Option<&'a str>,
    pub created_at: i64,
    pub updated_at: i64,
    pub dispatched_at: Option<i64>,
    pub acknowledged_at: Option<i64>,
    pub held_at: Option<i64>,
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = plan_materializations)]
pub struct PlanMaterializationRow {
    pub id: String,
    pub document_id: String,
    pub revision_id: String,
    pub generation: i64,
    pub expected_sha256: Option<String>,
    pub desired_sha256: String,
    pub state: String,
    pub force_replace: i32,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub applied_at: Option<i64>,
}

impl PlanMaterializationRow {
    pub fn state(&self) -> Result<PlanMaterializationState, String> {
        PlanMaterializationState::parse(&self.state)
    }
}

#[derive(Debug, Insertable)]
#[diesel(table_name = plan_materializations)]
pub struct PlanMaterializationInsert<'a> {
    pub id: &'a str,
    pub document_id: &'a str,
    pub revision_id: &'a str,
    pub generation: i64,
    pub expected_sha256: Option<&'a str>,
    pub desired_sha256: &'a str,
    pub state: &'a str,
    pub force_replace: i32,
    pub error: Option<&'a str>,
    pub created_at: i64,
    pub updated_at: i64,
    pub applied_at: Option<i64>,
}

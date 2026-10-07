//! `plan_review_sessions`: one submission of a plan revision for review, and
//! its decision.
//!
//! `native_runtime_config_json` decodes at the read into
//! [`NativePlanReviewRuntimeConfig`], strictly: the barrier check reads it to
//! decide which provider, model and assistant a blocked continuation depends
//! on, and a config it cannot read must fail that check rather than freeze or
//! release the wrong one.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

use crate::db::types::{EpochMs, Json, checked_text_enum};

checked_text_enum!(PlanReviewProviderKind {
    Native = "native",
    Acp = "acp",
    Legacy = "legacy",
});

checked_text_enum!(PlanReviewState {
    Pending = "pending",
    Approved = "approved",
    ChangesRequested = "changes_requested",
    Orphaned = "orphaned",
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

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "plan_review_sessions")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub document_id: String,
    pub submitted_revision_id: String,
    pub turn_id: Option<String>,
    pub assistant_message_id: Option<String>,
    pub provider_call_id: Option<String>,
    pub provider_kind: PlanReviewProviderKind,
    pub native_runtime_config_json: Option<Json<NativePlanReviewRuntimeConfig>>,
    pub state: PlanReviewState,
    pub decision_id: Option<String>,
    pub decision_summary: Option<String>,
    pub suggestion_revision_id: Option<String>,
    pub lock_version: i64,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
    pub decided_at: Option<EpochMs>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::plan_revision::Entity",
        from = "Column::SubmittedRevisionId",
        to = "super::plan_revision::Column::Id",
        on_delete = "Cascade"
    )]
    SubmittedRevision,
    #[sea_orm(
        belongs_to = "super::plan_document::Entity",
        from = "Column::DocumentId",
        to = "super::plan_document::Column::Id",
        on_delete = "Cascade"
    )]
    PlanDocument,
}

impl Related<super::plan_document::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::PlanDocument.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::cap::sealed::Access;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    const PARENTS: &str = "\
        INSERT INTO conversations (id, created_at, updated_at) VALUES ('c', 1, 1);
        INSERT INTO plan_documents (id, conversation_id, state, file_rel_path, created_at, updated_at)
            VALUES ('d', 'c', 'reviewing', 'plan.md', 1, 1);
        INSERT INTO plan_revisions (id, document_id, revision_no, author_kind, content_markdown, content_sha256, created_at)
            VALUES ('r', 'd', 1, 'assistant', '# plan',
                    '0000000000000000000000000000000000000000000000000000000000000000', 1);";

    fn session(config: &str) -> String {
        format!(
            "INSERT INTO plan_review_sessions (id, document_id, submitted_revision_id, provider_kind, \
             native_runtime_config_json, state, created_at, updated_at) \
             VALUES ('s', 'd', 'r', 'native', '{config}', 'pending', 1, 1)"
        )
    }

    /// The frozen runtime reads back typed, and one it cannot read exactly
    /// fails the query: the barrier must not decide on a guess.
    #[tokio::test]
    async fn the_frozen_runtime_decodes_strictly() {
        let db = sea_test_db().await;
        execute_for_tests(&db, PARENTS).await.unwrap();
        let config = r#"{"provider_id":"p","model":"m","assistant_id":"a","thinking_level":null,"fast":true,"project_id":null,"project_path":null,"accept_edits":false}"#;
        execute_for_tests(&db, &session(config)).await.unwrap();
        let row = Entity::find_by_id("s").one(db.conn().unwrap()).await.unwrap().unwrap();
        let runtime = row.native_runtime_config_json.unwrap().into_inner();
        assert_eq!(
            (
                runtime.provider_id.as_str(),
                runtime.assistant_id.as_deref(),
                runtime.fast
            ),
            ("p", Some("a"), true)
        );
        assert_eq!(
            (row.provider_kind, row.state),
            (PlanReviewProviderKind::Native, PlanReviewState::Pending)
        );

        execute_for_tests(&db, "DELETE FROM plan_review_sessions")
            .await
            .unwrap();
        let extra = config.replace("}", r#","later":1}"#);
        execute_for_tests(&db, &session(&extra)).await.unwrap();
        assert!(Entity::find_by_id("s").one(db.conn().unwrap()).await.is_err());
    }
}

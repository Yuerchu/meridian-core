//! `plan_revisions`: one immutable version of a plan document.
//!
//! `editor_json` stays text: the editor's document shape is versioned by
//! `editor_schema_version` and decoded where it is used.

use sea_orm::entity::prelude::*;

use crate::db::types::{EpochMs, checked_text_enum};

checked_text_enum!(PlanRevisionAuthorKind {
    Assistant = "assistant",
    UserSuggestion = "user_suggestion",
    Legacy = "legacy",
});

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "plan_revisions")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub document_id: String,
    pub revision_no: i64,
    pub parent_revision_id: Option<String>,
    pub author_kind: PlanRevisionAuthorKind,
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
    pub created_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
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

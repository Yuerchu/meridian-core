//! `plan_review_drafts`: the reviewer's unsent edits to a pending review.
//!
//! The editor and selection JSON stay text, decoded by the review page's
//! own types where they are used.

use sea_orm::entity::prelude::*;

use crate::db::types::{EpochMs, checked_text_enum};

checked_text_enum!(PlanReviewDraftMode {
    Rich = "rich",
    Source = "source",
});

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "plan_review_drafts")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub review_id: String,
    pub base_revision_id: String,
    pub generation: i64,
    pub mode: PlanReviewDraftMode,
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
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::plan_revision::Entity",
        from = "Column::BaseRevisionId",
        to = "super::plan_revision::Column::Id",
        on_delete = "Cascade"
    )]
    BaseRevision,
    #[sea_orm(
        belongs_to = "super::plan_review_session::Entity",
        from = "Column::ReviewId",
        to = "super::plan_review_session::Column::Id",
        on_delete = "Cascade"
    )]
    Review,
}

impl Related<super::plan_review_session::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Review.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

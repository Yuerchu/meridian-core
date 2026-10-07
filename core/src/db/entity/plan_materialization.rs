//! `plan_materializations`: writing a revision out to the plan's file on
//! disk, one row per working generation.

use sea_orm::entity::prelude::*;

use crate::db::types::{EpochMs, SqlBool, checked_text_enum};

checked_text_enum!(PlanMaterializationState {
    Pending = "pending",
    Applied = "applied",
    Conflict = "conflict",
});

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "plan_materializations")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub document_id: String,
    pub revision_id: String,
    pub generation: i64,
    pub expected_sha256: Option<String>,
    pub desired_sha256: String,
    pub state: PlanMaterializationState,
    pub force_replace: SqlBool,
    pub error: Option<String>,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
    pub applied_at: Option<EpochMs>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::plan_revision::Entity",
        from = "Column::RevisionId",
        to = "super::plan_revision::Column::Id",
        on_delete = "Cascade"
    )]
    Revision,
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

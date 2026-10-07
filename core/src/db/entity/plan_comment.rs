//! `plan_comments`: the reviewer's comments on a review, anchored in the
//! rich or source view. `anchor_json` stays text, decoded per anchor kind
//! where it is used.

use sea_orm::entity::prelude::*;

use crate::db::types::{EpochMs, checked_text_enum};

checked_text_enum!(PlanCommentState {
    Draft = "draft",
    Active = "active",
    Orphaned = "orphaned",
    Submitted = "submitted",
    Deleted = "deleted",
});

checked_text_enum!(PlanCommentAnchorKind {
    Rich = "rich",
    Source = "source",
});

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "plan_comments")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub review_id: String,
    pub position: i32,
    pub state: PlanCommentState,
    pub anchor_kind: PlanCommentAnchorKind,
    pub anchor_json: String,
    pub body: String,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
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

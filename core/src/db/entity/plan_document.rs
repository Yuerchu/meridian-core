//! `plan_documents`: one durable plan per planning episode of a conversation.
//!
//! The plan-review entities exist ahead of the plan-review ops, which still
//! run on Diesel inside the barrier, queue and turn transactions: the barrier
//! check reads these tables, and its SeaORM version needs them registered.
//! The state enums live here and are re-exported from `db::models::plan_review`
//! while the Diesel rows still use them.

use sea_orm::entity::prelude::*;

use crate::db::types::{EpochMs, checked_text_enum};

checked_text_enum!(PlanDocumentState {
    Drafting = "drafting",
    Reviewing = "reviewing",
    Approved = "approved",
    Done = "done",
});

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "plan_documents")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub conversation_id: String,
    pub state: PlanDocumentState,
    pub head_revision_id: Option<String>,
    pub approved_revision_id: Option<String>,
    pub working_generation: i64,
    pub file_rel_path: String,
    pub lock_version: i64,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::conversation::Entity",
        from = "Column::ConversationId",
        to = "super::conversation::Column::Id",
        on_delete = "Cascade"
    )]
    Conversation,
}

impl Related<super::conversation::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Conversation.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use sea_orm::{ActiveEnum, Iterable};

    use super::super::{
        allowed_by_check, plan_comment, plan_materialization, plan_review_delivery, plan_review_draft,
        plan_review_session, plan_revision,
    };
    use super::*;

    /// Every stored value is strum's spelling, parses back, and the set is
    /// exactly what the column's `CHECK` allows.
    fn pinned<E>(table: &str, column: &str)
    where
        E: ActiveEnum<Value = String> + Iterable + Copy + std::fmt::Debug + PartialEq,
        &'static str: From<E>,
    {
        let mut stored = BTreeSet::new();
        for value in E::iter() {
            let db = value.to_value();
            assert_eq!(<&'static str>::from(value), db, "{table}.{column}: {value:?}");
            assert_eq!(E::try_from_value(&db).unwrap(), value);
            stored.insert(db);
        }
        assert_eq!(stored, allowed_by_check(table, column), "{table}.{column}");
    }

    #[test]
    fn every_plan_enum_is_its_check_list() {
        pinned::<PlanDocumentState>("plan_documents", "state");
        pinned::<plan_revision::PlanRevisionAuthorKind>("plan_revisions", "author_kind");
        pinned::<plan_review_session::PlanReviewProviderKind>("plan_review_sessions", "provider_kind");
        pinned::<plan_review_session::PlanReviewState>("plan_review_sessions", "state");
        pinned::<plan_review_draft::PlanReviewDraftMode>("plan_review_drafts", "mode");
        pinned::<plan_comment::PlanCommentState>("plan_comments", "state");
        pinned::<plan_comment::PlanCommentAnchorKind>("plan_comments", "anchor_kind");
        pinned::<plan_review_delivery::PlanDeliveryTarget>("plan_review_deliveries", "target");
        pinned::<plan_review_delivery::PlanDeliveryState>("plan_review_deliveries", "state");
        pinned::<plan_materialization::PlanMaterializationState>("plan_materializations", "state");
        assert_eq!(
            PlanDocumentState::parse("archived").unwrap_err(),
            "unknown PlanDocumentState 'archived'"
        );
    }
}

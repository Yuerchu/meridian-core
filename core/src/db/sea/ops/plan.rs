//! Reading and writing `mode_artifacts`, the legacy approved-plan rows.
//!
//! The versioned plan documents (`db::sea::ops::plan_review`) are the source of
//! truth; what is left here is the active-plan read the prompt builder makes
//! and the completion that retires both kinds at once. The Diesel
//! `db::ops::plan` keeps only the completion the Diesel checklist write
//! still makes.

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter};

use crate::db::entity::mode_artifact::PlanStatus;
use crate::db::entity::plan_document::PlanDocumentState;
use crate::db::entity::{mode_artifact, plan_document};
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::types::EpochMs;

/// The only artifact kind there is.
pub const KIND_PLAN: &str = "plan";

/// The plan currently being implemented, if any.
pub async fn get_active(db: &impl Read, conversation_id: &str) -> Result<Option<mode_artifact::Model>, DbErr> {
    mode_artifact::Entity::find()
        .filter(mode_artifact::Column::ConversationId.eq(conversation_id))
        .filter(mode_artifact::Column::Kind.eq(KIND_PLAN))
        .filter(mode_artifact::Column::Status.eq(PlanStatus::Approved))
        .one(db.conn()?)
        .await
}

/// Retire whatever plan is in force because the work it described is
/// finished: the legacy approved artifact and the approved versioned
/// document both, or the document would keep being injected after its
/// mirror says the work is done. How many rows of either kind it retired.
pub async fn complete_active(tx: &WriteTx, conversation_id: &str, now: EpochMs) -> Result<u64, DbErr> {
    let legacy = mode_artifact::Entity::update_many()
        .set(mode_artifact::ActiveModel {
            status: Set(PlanStatus::Done),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(mode_artifact::Column::ConversationId.eq(conversation_id))
        .filter(mode_artifact::Column::Kind.eq(KIND_PLAN))
        .filter(mode_artifact::Column::Status.eq(PlanStatus::Approved))
        .exec(tx.conn()?)
        .await?
        .rows_affected;
    let documents = plan_document::Entity::update_many()
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
        .rows_affected;
    Ok(legacy + documents)
}

/// The approved plan for the system prompt: `None` when there is nothing to
/// say, a leading blank line built in, wrapped in a tag.
pub fn format_plan_block(plan: &mode_artifact::Model) -> Option<String> {
    let content = plan.content.trim();
    if content.is_empty() {
        return None;
    }
    Some(format!("\n\n<approved_plan>\n{content}\n</approved_plan>"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    /// The approved artifact is the active one, per conversation, and the
    /// prompt block carries it and skips an empty one.
    #[tokio::test]
    async fn the_approved_artifact_is_active_and_its_block_skips_emptiness() {
        let db = sea_test_db().await;
        execute_for_tests(
            &db,
            "INSERT INTO conversations (id, created_at, updated_at) VALUES ('c1', 1, 1), ('c2', 1, 1);
             INSERT INTO mode_artifacts (id, conversation_id, kind, content, status, created_at, updated_at) VALUES
                 ('pending', 'c1', 'plan', 'not yet', 'pending', 1, 1),
                 ('approved', 'c1', 'plan', '  the plan  ', 'approved', 2, 2),
                 ('blank', 'c2', 'plan', '   ', 'approved', 3, 3)",
        )
        .await
        .unwrap();

        let active = get_active(&db, "c1").await.unwrap().unwrap();
        assert_eq!(active.id, "approved");
        assert_eq!(
            format_plan_block(&active).as_deref(),
            Some(
                "

<approved_plan>
the plan
</approved_plan>"
            )
        );
        let blank = get_active(&db, "c2").await.unwrap().unwrap();
        assert_eq!(format_plan_block(&blank), None, "an empty plan says nothing");

        db.write(async |tx| complete_active(tx, "c1", 9).await).await.unwrap();
        assert_eq!(
            get_active(&db, "c1").await.unwrap(),
            None,
            "finished work retires the plan"
        );
        assert!(
            get_active(&db, "c2").await.unwrap().is_some(),
            "and only that conversation's"
        );
    }
}

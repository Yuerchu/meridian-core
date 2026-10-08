//! Reading and writing `mode_artifacts`, the legacy approved-plan rows.
//!
//! The versioned plan documents (`db::ops::plan_review`) are the source of
//! truth; what is left here is the active-plan read the prompt builder makes
//! and the completion that retires both kinds at once. The turn
//! configuration still reads through `db::ops::plan`; the pairs are in
//! `docs/dual-impl.md`.

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

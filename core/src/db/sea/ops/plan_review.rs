//! Reading the plan-review tables.
//!
//! The plan-review ops still run on Diesel; what is here is the barrier check
//! the guarded mutations run inside their write, moved over with the first of
//! those roots (the provider commands). The Diesel versions stay until their
//! last caller has moved; `docs/dual-impl.md` lists the pairs.
//!
//! Every function here issues several statements and joins the answers, so
//! each takes `&impl Snapshot`: on the pool, a review settling between two of
//! them could make a conversation look blocked by nothing, or free.

use sea_orm::{ColumnTrait, DbErr, EntityTrait, JoinType, QueryFilter, QueryOrder, QuerySelect, RelationTrait};

use crate::db::entity::plan_review_delivery::PlanDeliveryState;
use crate::db::entity::plan_review_session::PlanReviewState;
use crate::db::entity::{plan_document, plan_review_delivery, plan_review_session};
use crate::db::sea::cap::Snapshot;
use crate::db::sea::ops::conversation;

/// The delivery states that keep a settled review's barrier up: its
/// continuation is queued, on its way, held, or explicitly in doubt.
const BARRIER_DELIVERY_STATES: [PlanDeliveryState; 4] = [
    PlanDeliveryState::Queued,
    PlanDeliveryState::Dispatched,
    PlanDeliveryState::Held,
    PlanDeliveryState::InDoubt,
];

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

async fn review_has_delivery_barrier(db: &impl Snapshot, review_id: &str) -> Result<bool, DbErr> {
    Ok(plan_review_delivery::Entity::find()
        .filter(plan_review_delivery::Column::ReviewId.eq(review_id))
        .filter(plan_review_delivery::Column::State.is_in(BARRIER_DELIVERY_STATES))
        .select_only()
        .column(plan_review_delivery::Column::Id)
        .into_tuple::<String>()
        .one(db.conn()?)
        .await?
        .is_some())
}

/// The conversation's reviews that hold its barrier: pending, or settled with
/// a continuation not yet acknowledged.
async fn active_barrier_reviews_for_conversation(
    db: &impl Snapshot,
    conversation_id: &str,
) -> Result<Vec<plan_review_session::Model>, DbErr> {
    let mut active = Vec::new();
    for review in list_reviews_for_conversation(db, conversation_id).await? {
        if review.state == PlanReviewState::Pending || review_has_delivery_barrier(db, &review.id).await? {
            active.push(review);
        }
    }
    Ok(active)
}

/// Conversations whose currently blocked native continuation depends on this
/// provider. Frozen runtime identity is authoritative; the conversation value
/// is consulted only for an active legacy row with no frozen snapshot.
/// Sorted, without duplicates.
pub async fn barrier_conversations_for_provider(db: &impl Snapshot, provider_id: &str) -> Result<Vec<String>, DbErr> {
    let mut blocked = Vec::new();
    for conversation_id in conversation::all_ids(db).await? {
        let active_reviews = active_barrier_reviews_for_conversation(db, &conversation_id).await?;
        if active_reviews.is_empty() {
            continue;
        }
        let conversation = conversation::get_conversation(db, &conversation_id)
            .await?
            .ok_or_else(|| DbErr::RecordNotFound(format!("conversation `{conversation_id}`")))?;
        let mut missing_runtime = false;
        let mut frozen = false;
        for review in active_reviews {
            match review.native_runtime_config_json {
                Some(runtime) => frozen |= runtime.provider_id == provider_id,
                None => missing_runtime = true,
            }
        }
        let standing_fallback = missing_runtime && conversation.agent_provider_id.as_deref() == Some(provider_id);
        if frozen || standing_fallback {
            blocked.push(conversation_id);
        }
    }
    blocked.sort();
    blocked.dedup();
    Ok(blocked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::cap::Db;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    const SHA: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    /// A conversation with one document, one revision and one review in
    /// `state`, frozen on `provider` (or on nothing, as a legacy row).
    async fn review(db: &Db, conversation: &str, state: &str, provider: Option<&str>, agent_provider: Option<&str>) {
        let agent = agent_provider.map_or("NULL".to_owned(), |p| format!("'{p}'"));
        let config = provider.map_or("NULL".to_owned(), |p| {
            format!(
                r#"'{{"provider_id":"{p}","model":"m","assistant_id":null,"thinking_level":null,"fast":false,"project_id":null,"project_path":null,"accept_edits":false}}'"#
            )
        });
        let document_state = if state == "pending" { "reviewing" } else { "approved" };
        execute_for_tests(
            db,
            &format!(
                "INSERT INTO conversations (id, agent_provider_id, created_at, updated_at) VALUES ('{conversation}', {agent}, 1, 1);
                 INSERT INTO plan_documents (id, conversation_id, state, file_rel_path, created_at, updated_at)
                     VALUES ('d-{conversation}', '{conversation}', '{document_state}', 'plan.md', 1, 1);
                 INSERT INTO plan_revisions (id, document_id, revision_no, author_kind, content_markdown, content_sha256, created_at)
                     VALUES ('r-{conversation}', 'd-{conversation}', 1, 'assistant', '# plan', '{SHA}', 1);
                 INSERT INTO plan_review_sessions (id, document_id, submitted_revision_id, provider_kind,
                     native_runtime_config_json, state, created_at, updated_at)
                     VALUES ('s-{conversation}', 'd-{conversation}', 'r-{conversation}', 'native', {config}, '{state}', 1, 1);"
            ),
        )
        .await
        .unwrap();
    }

    async fn delivery(db: &Db, conversation: &str, state: &str) {
        execute_for_tests(
            db,
            &format!(
                "INSERT INTO plan_review_deliveries (id, review_id, target, state, payload_json, created_at, updated_at)
                     VALUES ('dl-{conversation}', 's-{conversation}', 'native', '{state}', '{{}}', 1, 1)"
            ),
        )
        .await
        .unwrap();
    }

    async fn blocked(db: &Db, provider: &str) -> Vec<String> {
        db.read(async |tx| barrier_conversations_for_provider(tx, provider).await)
            .await
            .unwrap()
    }

    /// A pending review blocks the provider it froze; a settled one blocks it
    /// only while its continuation is undelivered; a legacy review with no
    /// frozen runtime falls back to the conversation's own provider; and an
    /// unrelated provider is free.
    #[tokio::test]
    async fn the_barrier_follows_the_frozen_runtime_and_undelivered_continuations() {
        let db = sea_test_db().await;
        review(&db, "pending", "pending", Some("p1"), None).await;
        review(&db, "queued", "approved", Some("p1"), None).await;
        delivery(&db, "queued", "queued").await;
        review(&db, "acked", "approved", Some("p1"), None).await;
        delivery(&db, "acked", "acknowledged").await;
        review(&db, "legacy", "pending", None, Some("p1")).await;
        review(&db, "elsewhere", "pending", Some("p2"), Some("p1")).await;

        assert_eq!(blocked(&db, "p1").await, ["legacy", "pending", "queued"]);
        assert_eq!(blocked(&db, "p2").await, ["elsewhere"]);
        assert!(blocked(&db, "p3").await.is_empty());
    }
}

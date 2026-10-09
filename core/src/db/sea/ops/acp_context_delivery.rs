//! Which context items a hosted agent has already been sent.

use std::collections::HashSet;

use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter, QuerySelect, TryInsertResult};

use crate::db::entity::acp_context_delivery;
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::types::EpochMs;

/// Which candidate context items an ACP prompt has already carried.
pub async fn delivered(db: &impl Read, context_item_ids: &[String]) -> Result<HashSet<String>, DbErr> {
    if context_item_ids.is_empty() {
        return Ok(HashSet::new());
    }
    let ids: Vec<String> = acp_context_delivery::Entity::find()
        .filter(acp_context_delivery::Column::ContextItemId.is_in(context_item_ids.iter().map(String::as_str)))
        .select_only()
        .column(acp_context_delivery::Column::ContextItemId)
        .into_tuple()
        .all(db.conn()?)
        .await?;
    Ok(ids.into_iter().collect())
}

/// Mark a prompt's context delivered. Idempotent: a reply can be observed
/// twice while an adapter is shutting down, and one receipt is the fact.
pub async fn mark_delivered(tx: &WriteTx, context_item_ids: &[String], now: EpochMs) -> Result<u64, DbErr> {
    if context_item_ids.is_empty() {
        return Ok(0);
    }
    let inserted = acp_context_delivery::Entity::insert_many(context_item_ids.iter().map(|id| {
        acp_context_delivery::ActiveModel {
            context_item_id: Set(id.clone()),
            delivered_at: Set(now),
        }
    }))
    .on_conflict_do_nothing_on([acp_context_delivery::Column::ContextItemId])
    .exec_without_returning(tx.conn()?)
    .await?;
    Ok(match inserted {
        TryInsertResult::Inserted(rows) => rows,
        TryInsertResult::Conflicted | TryInsertResult::Empty => 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    /// A receipt is written once, a second sighting changes nothing, and it
    /// goes with the context item it is about.
    #[tokio::test]
    async fn receipt_is_idempotent_and_cascades_with_the_item() {
        let db = sea_test_db().await;
        execute_for_tests(
            &db,
            "INSERT INTO conversations (id, created_at, updated_at) VALUES ('c1', 1, 1);
             INSERT INTO messages (id, conversation_id, role, content, source, created_at)
                 VALUES ('m1', 'c1', 'user', '!echo hi', 'shell', 1);
             INSERT INTO message_context_items (id, message_id, position, kind, content, content_hash,
                 byte_count, line_count, token_count, created_at)
                 VALUES ('i1', 'm1', 0, 'shell_output', 'hi', 'hash', 2, 1, 1, 1)",
        )
        .await
        .unwrap();
        let ids = vec!["i1".to_string()];
        let mark = |now| {
            let (db, ids) = (db.clone(), ids.clone());
            async move { db.write(async |tx| mark_delivered(tx, &ids, now).await).await.unwrap() }
        };
        assert_eq!(mark(2).await, 1);
        assert_eq!(mark(3).await, 0);
        assert_eq!(delivered(&db, &ids).await.unwrap(), HashSet::from(["i1".to_string()]));

        execute_for_tests(&db, "DELETE FROM message_context_items WHERE id = 'i1'")
            .await
            .unwrap();
        assert!(delivered(&db, &ids).await.unwrap().is_empty());
    }
}

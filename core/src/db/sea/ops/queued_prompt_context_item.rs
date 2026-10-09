//! The `@` context a queued prompt carries, frozen when it was queued.

use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter, QueryOrder};

use crate::db::entity::queued_prompt_context_item;
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::types::{EpochMs, SqlBool};
use crate::workspace::reference::PreparedContextItem;

pub async fn insert_prepared(
    tx: &WriteTx,
    queue_id: &str,
    items: &[PreparedContextItem],
    now: EpochMs,
) -> Result<u64, DbErr> {
    if items.is_empty() {
        return Ok(0);
    }
    let rows = items.iter().enumerate().map(|(position, item)| {
        Ok(queued_prompt_context_item::ActiveModel {
            id: Set(item.id.clone()),
            queue_id: Set(queue_id.to_owned()),
            position: Set(position as i32),
            kind: Set(item.kind),
            content: Set(item.content.clone()),
            display_path: Set(item.display_path.clone()),
            line_start: Set(item.line_start),
            line_end: Set(item.line_end),
            content_hash: Set(item.content_hash.clone()),
            byte_count: Set(item.byte_count),
            line_count: Set(item.line_count),
            token_count: Set(item.token_count),
            truncated: Set(SqlBool::try_from(item.truncated).map_err(DbErr::Type)?),
            metadata: Set(item.metadata.clone()),
            created_at: Set(now),
        })
    });
    let rows = rows.collect::<Result<Vec<_>, DbErr>>()?;
    let count = rows.len() as u64;
    queued_prompt_context_item::Entity::insert_many(rows)
        .exec_without_returning(tx.conn()?)
        .await?;
    Ok(count)
}

pub async fn list_prepared(db: &impl Read, queue_id: &str) -> Result<Vec<PreparedContextItem>, DbErr> {
    Ok(queued_prompt_context_item::Entity::find()
        .filter(queued_prompt_context_item::Column::QueueId.eq(queue_id))
        .order_by_asc(queued_prompt_context_item::Column::Position)
        .all(db.conn()?)
        .await?
        .into_iter()
        .map(|row| PreparedContextItem {
            id: row.id,
            kind: row.kind,
            content: row.content,
            display_path: row.display_path,
            line_start: row.line_start,
            line_end: row.line_end,
            content_hash: row.content_hash,
            byte_count: row.byte_count,
            line_count: row.line_count,
            token_count: row.token_count,
            truncated: i32::from(row.truncated.get()),
            metadata: row.metadata,
        })
        .collect())
}

pub async fn delete_for_queue(tx: &WriteTx, queue_id: &str) -> Result<u64, DbErr> {
    Ok(queued_prompt_context_item::Entity::delete_many()
        .filter(queued_prompt_context_item::Column::QueueId.eq(queue_id))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::entity::queued_prompt::Delivery;
    use crate::db::sea::ops::queue;
    use crate::db::sea::{execute_for_tests, sea_test_db};
    use crate::workspace::reference::MessageContextKind;

    fn prepared(id: &str, content: &str) -> PreparedContextItem {
        PreparedContextItem {
            id: id.into(),
            kind: MessageContextKind::ProjectFile,
            content: content.into(),
            display_path: Some("src/lib.rs".into()),
            line_start: None,
            line_end: None,
            content_hash: "hash".into(),
            byte_count: 9,
            line_count: 1,
            token_count: 2,
            truncated: 0,
            metadata: None,
        }
    }

    /// The prompt and its frozen snapshot are written together and read back
    /// as they were frozen; spending the item drops the snapshot, and so does
    /// deleting the prompt.
    #[tokio::test]
    async fn a_prompt_and_its_frozen_snapshot_live_and_die_together() {
        let db = sea_test_db().await;
        execute_for_tests(
            &db,
            "INSERT INTO conversations (id, created_at, updated_at) VALUES ('c1', 1, 1)",
        )
        .await
        .unwrap();
        let context = [prepared("x1", "old bytes"), prepared("x2", "more bytes")];
        db.write(async |tx| {
            queue::enqueue_with_context(tx, "q1", "c1", "read @src/lib.rs", Delivery::FollowUp, &context, 2).await?;
            queue::enqueue_with_context(tx, "q2", "c1", "and again", Delivery::FollowUp, &context[..1], 3)
                .await
                .map(|_| ())
        })
        .await
        .expect_err("a context id is frozen once");
        assert!(
            list_prepared(&db, "q1").await.unwrap().is_empty(),
            "the failed write took q1 with it"
        );

        db.write(async |tx| {
            queue::enqueue_with_context(tx, "q1", "c1", "read @src/lib.rs", Delivery::FollowUp, &context, 2).await
        })
        .await
        .unwrap();
        let got = list_prepared(&db, "q1").await.unwrap();
        assert_eq!(got, context);

        assert_eq!(db.write(async |tx| delete_for_queue(tx, "q1").await).await.unwrap(), 2);
        assert!(list_prepared(&db, "q1").await.unwrap().is_empty());

        db.write(async |tx| {
            queue::enqueue_with_context(tx, "q2", "c1", "again", Delivery::FollowUp, &[prepared("x3", "b")], 3).await
        })
        .await
        .unwrap();
        execute_for_tests(&db, "DELETE FROM queued_prompts WHERE id = 'q2'")
            .await
            .unwrap();
        assert!(
            list_prepared(&db, "q2").await.unwrap().is_empty(),
            "the snapshot goes with its prompt"
        );
    }
}

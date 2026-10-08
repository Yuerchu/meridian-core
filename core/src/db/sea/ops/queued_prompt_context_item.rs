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

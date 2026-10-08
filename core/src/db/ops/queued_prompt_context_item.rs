use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::queued_prompt_context_item::QueuedPromptContextItemInsert;
use crate::db::schema::queued_prompt_context_items;

pub(super) fn insert_prepared(
    conn: &mut SqliteConnection,
    queue_id: &str,
    items: &[crate::workspace::reference::PreparedContextItem],
    now: i64,
) -> QueryResult<usize> {
    if items.is_empty() {
        return Ok(0);
    }
    let rows = items
        .iter()
        .enumerate()
        .map(|(position, item)| QueuedPromptContextItemInsert {
            id: &item.id,
            queue_id,
            position: position as i32,
            kind: item.kind.as_str(),
            content: &item.content,
            display_path: item.display_path.as_deref(),
            line_start: item.line_start,
            line_end: item.line_end,
            content_hash: &item.content_hash,
            byte_count: item.byte_count,
            line_count: item.line_count,
            token_count: item.token_count,
            truncated: item.truncated,
            metadata: item.metadata.as_deref(),
            created_at: now,
        })
        .collect::<Vec<_>>();
    diesel::insert_into(queued_prompt_context_items::table)
        .values(&rows)
        .execute(conn)
}

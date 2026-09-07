use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::queued_prompt_context_item::{QueuedPromptContextItemInsert, QueuedPromptContextItemRow};
use crate::db::schema::queued_prompt_context_items;

pub fn insert_prepared(
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

pub fn list_prepared(
    conn: &mut SqliteConnection,
    queue_id: &str,
) -> QueryResult<Vec<crate::workspace::reference::PreparedContextItem>> {
    queued_prompt_context_items::table
        .filter(queued_prompt_context_items::queue_id.eq(queue_id))
        .order(queued_prompt_context_items::position.asc())
        .select(QueuedPromptContextItemRow::as_select())
        .load::<QueuedPromptContextItemRow>(conn)?
        .into_iter()
        .map(|row| {
            row.try_into().map_err(|error: String| {
                diesel::result::Error::DeserializationError(Box::new(std::io::Error::other(error)))
            })
        })
        .collect()
}

pub fn delete_for_queue(conn: &mut SqliteConnection, queue_id: &str) -> QueryResult<usize> {
    diesel::delete(queued_prompt_context_items::table.filter(queued_prompt_context_items::queue_id.eq(queue_id)))
        .execute(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::queue::Delivery;

    fn prepared(content: &str) -> crate::workspace::reference::PreparedContextItem {
        crate::workspace::reference::PreparedContextItem {
            id: "ctx1".into(),
            kind: crate::workspace::reference::MessageContextKind::ProjectFile,
            content: content.into(),
            display_path: Some("src/lib.rs".into()),
            line_start: None,
            line_end: None,
            content_hash: "hash".into(),
            byte_count: content.len() as i32,
            line_count: 1,
            token_count: 1,
            truncated: 0,
            metadata: None,
        }
    }

    #[test]
    fn enqueue_commits_the_prompt_and_its_frozen_snapshot_together() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        crate::db::ops::conversation::create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        let frozen = prepared("old bytes");
        crate::db::ops::queue::enqueue_with_context(
            &mut conn,
            "q1",
            "c1",
            "read @src/lib.rs",
            Delivery::FollowUp,
            &[frozen],
            2,
        )
        .unwrap();

        let got = list_prepared(&mut conn, "q1").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].content, "old bytes");

        crate::db::ops::queue::remove(&mut conn, "c1", "q1").unwrap();
        assert!(list_prepared(&mut conn, "q1").unwrap().is_empty());
    }
}

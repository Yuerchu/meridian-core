use std::collections::HashMap;

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::message_context_item::{MessageContextItemInsert, MessageContextItemRow};
use crate::db::schema::message_context_items;

pub fn insert_many(conn: &mut SqliteConnection, items: &[MessageContextItemInsert<'_>]) -> QueryResult<usize> {
    if items.is_empty() {
        return Ok(0);
    }
    diesel::insert_into(message_context_items::table)
        .values(items)
        .execute(conn)
}

pub fn list_for_message(conn: &mut SqliteConnection, message_id: &str) -> QueryResult<Vec<MessageContextItemRow>> {
    message_context_items::table
        .filter(message_context_items::message_id.eq(message_id))
        .order(message_context_items::position.asc())
        .select(MessageContextItemRow::as_select())
        .load(conn)
}

pub fn list_for_messages(
    conn: &mut SqliteConnection,
    message_ids: &[String],
) -> QueryResult<HashMap<String, Vec<MessageContextItemRow>>> {
    if message_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = message_context_items::table
        .filter(message_context_items::message_id.eq_any(message_ids))
        .order((
            message_context_items::message_id.asc(),
            message_context_items::position.asc(),
        ))
        .select(MessageContextItemRow::as_select())
        .load::<MessageContextItemRow>(conn)?;
    let mut by_message: HashMap<String, Vec<MessageContextItemRow>> = HashMap::new();
    for row in rows {
        by_message.entry(row.message_id.clone()).or_default().push(row);
    }
    Ok(by_message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::message::MessageInsert;
    use crate::db::ops::message::append_message;

    fn seed_message(conn: &mut SqliteConnection, id: &str) {
        crate::db::ops::conversation::create_conversation(conn, "c1", None, None, None, 1).unwrap();
        append_message(
            conn,
            &MessageInsert {
                id,
                conversation_id: "c1",
                role: "user",
                content: "look",
                provider_id: None,
                model_id: None,
                input_tokens: None,
                output_tokens: None,
                tool_calls: None,
                tool_call_id: None,
                sort_order: 0,
                created_at: 1,
                reasoning_content: None,
                rating: None,
                schema_version: 2,
                is_compact_summary: 0,
                sender_id: None,
                parent_id: None,
                compact_anchor_id: None,
                source: None,
                turn_id: None,
                tool_outcome: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
                server_tool_calls: None,
                provider_name: None,
            },
            None,
        )
        .unwrap();
    }

    #[test]
    fn items_are_ordered_and_die_with_their_message() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        seed_message(&mut conn, "m1");
        let make = |id, position| MessageContextItemInsert {
            id,
            message_id: "m1",
            position,
            kind: "project_file",
            content: id,
            display_path: Some(id),
            line_start: None,
            line_end: None,
            content_hash: "hash",
            byte_count: 1,
            line_count: 1,
            token_count: 1,
            truncated: 0,
            metadata: None,
            created_at: 1,
        };
        insert_many(&mut conn, &[make("second", 1), make("first", 0)]).unwrap();
        let got = list_for_message(&mut conn, "m1").unwrap();
        assert_eq!(
            got.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["first", "second"]
        );

        diesel::delete(crate::db::schema::messages::table.find("m1"))
            .execute(&mut conn)
            .unwrap();
        assert!(list_for_message(&mut conn, "m1").unwrap().is_empty());
    }
}

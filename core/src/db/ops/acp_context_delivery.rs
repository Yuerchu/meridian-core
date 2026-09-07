use std::collections::HashSet;

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::acp_context_delivery::AcpContextDeliveryInsert;
use crate::db::schema::acp_context_deliveries;

/// Which candidate context items an ACP prompt has already carried.
pub fn delivered(conn: &mut SqliteConnection, context_item_ids: &[String]) -> QueryResult<HashSet<String>> {
    if context_item_ids.is_empty() {
        return Ok(HashSet::new());
    }
    acp_context_deliveries::table
        .filter(acp_context_deliveries::context_item_id.eq_any(context_item_ids))
        .select(acp_context_deliveries::context_item_id)
        .load::<String>(conn)
        .map(|ids| ids.into_iter().collect())
}

/// Mark a prompt's shell context delivered. Idempotent because a reply can be
/// observed twice while an adapter is shutting down; one receipt is the fact.
pub fn mark_delivered(conn: &mut SqliteConnection, context_item_ids: &[String], now: i64) -> QueryResult<usize> {
    let rows = context_item_ids
        .iter()
        .map(|id| AcpContextDeliveryInsert {
            context_item_id: id,
            delivered_at: now,
        })
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return Ok(0);
    }
    diesel::insert_or_ignore_into(acp_context_deliveries::table)
        .values(&rows)
        .execute(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::message::MessageInsert;
    use crate::db::models::message_context_item::MessageContextItemInsert;

    fn seed(conn: &mut SqliteConnection) {
        crate::db::ops::conversation::create_conversation(conn, "c1", None, None, None, 1).unwrap();
        crate::db::ops::message::append_message(
            conn,
            &MessageInsert {
                id: "m1",
                conversation_id: "c1",
                role: "user",
                content: "!echo hi",
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
                source: Some("shell"),
                turn_id: Some("t1"),
                tool_outcome: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
                server_tool_calls: None,
                provider_name: None,
            },
            None,
        )
        .unwrap();
        crate::db::ops::message_context_item::insert_many(
            conn,
            &[MessageContextItemInsert {
                id: "i1",
                message_id: "m1",
                position: 0,
                kind: "shell_output",
                content: "hi",
                display_path: None,
                line_start: None,
                line_end: None,
                content_hash: "hash",
                byte_count: 2,
                line_count: 1,
                token_count: 1,
                truncated: 0,
                metadata: None,
                created_at: 1,
            }],
        )
        .unwrap();
    }

    #[test]
    fn receipt_is_idempotent_and_cascades_with_the_item() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        seed(&mut conn);
        let ids = vec!["i1".to_string()];
        assert_eq!(mark_delivered(&mut conn, &ids, 2).unwrap(), 1);
        assert_eq!(mark_delivered(&mut conn, &ids, 3).unwrap(), 0);
        assert_eq!(delivered(&mut conn, &ids).unwrap(), HashSet::from(["i1".into()]));

        diesel::delete(crate::db::schema::message_context_items::table.find("i1"))
            .execute(&mut conn)
            .unwrap();
        assert!(delivered(&mut conn, &ids).unwrap().is_empty());
    }
}

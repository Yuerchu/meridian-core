//! What a message referenced — a file, a directory, a command's output,
//! another conversation — frozen as it was when sent.

use std::collections::HashMap;

use sea_orm::{ColumnTrait, DbErr, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder};

use crate::db::entity::message_context_item;
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};

pub async fn insert_many(tx: &WriteTx, items: Vec<message_context_item::Model>) -> Result<u64, DbErr> {
    if items.is_empty() {
        return Ok(0);
    }
    let count = items.len() as u64;
    message_context_item::Entity::insert_many(items.into_iter().map(IntoActiveModel::into_active_model))
        .exec_without_returning(tx.conn()?)
        .await?;
    Ok(count)
}

pub async fn list_for_message(db: &impl Read, message_id: &str) -> Result<Vec<message_context_item::Model>, DbErr> {
    message_context_item::Entity::find()
        .filter(message_context_item::Column::MessageId.eq(message_id))
        .order_by_asc(message_context_item::Column::Position)
        .all(db.conn()?)
        .await
}

/// Every listed message's items, by message, each in position order.
pub async fn list_for_messages(
    db: &impl Read,
    message_ids: &[String],
) -> Result<HashMap<String, Vec<message_context_item::Model>>, DbErr> {
    if message_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = message_context_item::Entity::find()
        .filter(message_context_item::Column::MessageId.is_in(message_ids.iter().map(String::as_str)))
        .order_by_asc(message_context_item::Column::MessageId)
        .order_by_asc(message_context_item::Column::Position)
        .all(db.conn()?)
        .await?;
    let mut by_message: HashMap<String, Vec<message_context_item::Model>> = HashMap::new();
    for row in rows {
        by_message.entry(row.message_id.clone()).or_default().push(row);
    }
    Ok(by_message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::{execute_for_tests, sea_test_db};
    use crate::db::types::SqlBool;
    use crate::workspace::reference::MessageContextKind;

    fn item(id: &str, message_id: &str, position: i32) -> message_context_item::Model {
        message_context_item::Model {
            id: id.into(),
            message_id: message_id.into(),
            position,
            kind: MessageContextKind::ShellOutput,
            content: format!("output {id}"),
            display_path: None,
            line_start: None,
            line_end: None,
            content_hash: "h".into(),
            byte_count: 8,
            line_count: 1,
            token_count: 2,
            truncated: SqlBool::FALSE,
            metadata: None,
            created_at: 1,
        }
    }

    /// Items read back in position order, grouped by message; a message with
    /// none is absent from the map, and its rows go with the message.
    #[tokio::test]
    async fn items_read_back_in_order_by_message() {
        let db = sea_test_db().await;
        execute_for_tests(
            &db,
            "INSERT INTO conversations (id, created_at, updated_at) VALUES ('c1', 1, 1);
             INSERT INTO messages (id, conversation_id, role, content, created_at)
                 VALUES ('m1', 'c1', 'user', 'a', 1), ('m2', 'c1', 'user', 'b', 2), ('m3', 'c1', 'user', 'c', 3)",
        )
        .await
        .unwrap();
        let written = vec![item("i2", "m1", 1), item("i1", "m1", 0), item("i3", "m2", 0)];
        assert_eq!(db.write(async |tx| insert_many(tx, written).await).await.unwrap(), 3);

        let m1: Vec<_> = list_for_message(&db, "m1")
            .await
            .unwrap()
            .into_iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(m1, ["i1", "i2"]);
        let grouped = list_for_messages(&db, &["m1".into(), "m2".into(), "m3".into()])
            .await
            .unwrap();
        assert_eq!((grouped["m1"].len(), grouped["m2"].len()), (2, 1));
        assert!(!grouped.contains_key("m3"));
        assert_eq!(list_for_messages(&db, &[]).await.unwrap().len(), 0);

        execute_for_tests(&db, "DELETE FROM messages WHERE id = 'm1'")
            .await
            .unwrap();
        assert!(list_for_message(&db, "m1").await.unwrap().is_empty());
    }
}

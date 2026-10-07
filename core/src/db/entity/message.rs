//! `messages`: one row of a conversation's message tree.
//!
//! The entity exists ahead of the message ops, which still run on Diesel. The
//! JSON columns stay text here, as the Diesel row keeps them, because none of
//! them decodes on its own: `tool_calls` means one of two encodings depending
//! on `schema_version` (`agent::tool_calls::parse_stored_tool_calls`),
//! `provider_state` carries its own version (`ProviderState::from_storage_json`),
//! and `auto_review` and `tool_diffs` are keyed by the call ids in
//! `tool_calls`. Each gets its type when the ops that read it move.
//!
//! `parent_id` has no foreign key on purpose (migration 21); `role`, `source`
//! and `tool_outcome` have no `CHECK` and are parsed where they are used.

use sea_orm::entity::prelude::*;

use crate::db::types::{EpochMs, SqlBool};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "messages")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub conversation_id: String,
    pub role: String,
    pub content: String,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub input_tokens: Option<i32>,
    pub output_tokens: Option<i32>,
    pub tool_calls: Option<String>,
    pub tool_call_id: Option<String>,
    /// Insertion order within the conversation. An insert that leaves it 0 has
    /// `trg_messages_sort_order` assign the next one.
    pub sort_order: i32,
    pub created_at: EpochMs,
    pub reasoning_content: Option<String>,
    pub rating: Option<i32>,
    pub schema_version: i32,
    pub is_compact_summary: SqlBool,
    pub sender_id: Option<i64>,
    pub parent_id: Option<String>,
    pub compact_anchor_id: Option<String>,
    pub source: Option<String>,
    pub turn_id: Option<String>,
    pub tool_outcome: Option<String>,
    pub cache_read_tokens: Option<i32>,
    pub cache_write_tokens: Option<i32>,
    pub provider_name: Option<String>,
    pub provider_state: Option<String>,
    pub auto_review: Option<String>,
    pub server_tool_calls: Option<i32>,
    pub tool_diffs: Option<String>,
    pub response_model_id: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "Entity",
        from = "Column::CompactAnchorId",
        to = "Column::Id",
        on_delete = "Cascade"
    )]
    CompactAnchor,
    #[sea_orm(
        belongs_to = "super::provider::Entity",
        from = "Column::ProviderId",
        to = "super::provider::Column::Id",
        on_delete = "SetNull"
    )]
    Provider,
    #[sea_orm(
        belongs_to = "super::conversation::Entity",
        from = "Column::ConversationId",
        to = "super::conversation::Column::Id",
        on_delete = "Cascade"
    )]
    Conversation,
}

impl Related<super::provider::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Provider.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

#[cfg(test)]
mod tests {
    use sea_orm::{EntityTrait, IntoActiveModel, QueryOrder};

    use super::*;
    use crate::db::entity::conversation;
    use crate::db::sea::cap::sealed::Access;
    use crate::db::sea::sea_test_db;

    fn conversation_row(id: &str) -> conversation::Model {
        conversation::Model {
            id: id.into(),
            title: None,
            assistant_id: None,
            is_pinned: SqlBool::FALSE,
            is_archived: SqlBool::FALSE,
            message_count: 0,
            created_at: 1,
            updated_at: 1,
            project_id: None,
            thinking_level: None,
            fast_mode: SqlBool::FALSE,
            mode: None,
            head_message_id: None,
            accept_edits: SqlBool::FALSE,
            parent_conversation_id: None,
            spawned_by_message_id: None,
            spawned_by_call_id: None,
            spawned_turn_id: None,
            agent_kind: None,
            agent_provider_id: None,
            agent_model_id: None,
        }
    }

    fn message_row(id: &str, conversation_id: &str, created_at: EpochMs) -> Model {
        Model {
            id: id.into(),
            conversation_id: conversation_id.into(),
            role: "user".into(),
            content: "hi".into(),
            provider_id: None,
            model_id: None,
            input_tokens: None,
            output_tokens: None,
            tool_calls: None,
            tool_call_id: None,
            sort_order: 0,
            created_at,
            reasoning_content: None,
            rating: None,
            schema_version: 2,
            is_compact_summary: SqlBool::FALSE,
            sender_id: None,
            parent_id: None,
            compact_anchor_id: None,
            source: None,
            turn_id: None,
            tool_outcome: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            provider_name: None,
            provider_state: None,
            auto_review: None,
            server_tool_calls: None,
            tool_diffs: None,
            response_model_id: None,
        }
    }

    /// The triggers the Diesel inserts relied on still run for a SeaORM one:
    /// `sort_order` left at 0 is assigned, and the conversation's count and
    /// `updated_at` follow each insert.
    #[tokio::test]
    async fn an_insert_through_the_entity_runs_the_triggers() {
        let db = sea_test_db().await;
        db.write(async |tx| {
            conversation::Entity::insert(conversation_row("c").into_active_model())
                .exec_without_returning(tx.conn()?)
                .await?;
            for (id, at) in [("m1", 10), ("m2", 20)] {
                Entity::insert(message_row(id, "c", at).into_active_model())
                    .exec_without_returning(tx.conn()?)
                    .await?;
            }
            Ok::<_, sea_orm::DbErr>(())
        })
        .await
        .unwrap();

        let rows = db
            .read(async |tx| Entity::find().order_by_asc(Column::CreatedAt).all(tx.conn()?).await)
            .await
            .unwrap();
        let orders: Vec<_> = rows.iter().map(|m| (m.id.as_str(), m.sort_order)).collect();
        assert_eq!(orders, [("m1", 1), ("m2", 2)]);
        let conversation = db
            .read(async |tx| conversation::Entity::find_by_id("c").one(tx.conn()?).await)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((conversation.message_count, conversation.updated_at), (2, 20));
    }
}

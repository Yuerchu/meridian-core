//! `conversations`: one chat, and the head of the path its transcript reads.
//!
//! The entity exists ahead of the conversation ops, which still run on Diesel
//! inside the queue, turn and plan-review transactions: the tables under it
//! (messages, composer drafts, plan documents, …) reference this one, and the
//! drift test checks a reference only when both ends have an entity.
//!
//! `thinking_level`, `mode` and `agent_kind` stay text, as the Diesel row
//! keeps them: none has a `CHECK`, and each is parsed where it is used.

use sea_orm::entity::prelude::*;

use crate::db::types::{EpochMs, SqlBool};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "conversations")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub title: Option<String>,
    pub assistant_id: Option<String>,
    pub is_pinned: SqlBool,
    pub is_archived: SqlBool,
    /// Kept by the `trg_messages_count_*` triggers, never written directly.
    pub message_count: i32,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
    pub project_id: Option<String>,
    pub thinking_level: Option<String>,
    pub fast_mode: SqlBool,
    pub mode: Option<String>,
    /// The leaf the active path ends at; see `db::sea::ops::message::active_context`.
    pub head_message_id: Option<String>,
    pub accept_edits: SqlBool,
    pub parent_conversation_id: Option<String>,
    pub spawned_by_message_id: Option<String>,
    pub spawned_by_call_id: Option<String>,
    pub spawned_turn_id: Option<String>,
    pub agent_kind: Option<String>,
    pub agent_provider_id: Option<String>,
    pub agent_model_id: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::message::Entity",
        from = "Column::HeadMessageId",
        to = "super::message::Column::Id",
        on_delete = "SetNull"
    )]
    HeadMessage,
    #[sea_orm(
        belongs_to = "super::project::Entity",
        from = "Column::ProjectId",
        to = "super::project::Column::Id",
        on_delete = "SetNull"
    )]
    Project,
    #[sea_orm(
        belongs_to = "super::assistant::Entity",
        from = "Column::AssistantId",
        to = "super::assistant::Column::Id",
        on_delete = "SetNull"
    )]
    Assistant,
}

impl Related<super::project::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Project.def()
    }
}

impl Related<super::assistant::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Assistant.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

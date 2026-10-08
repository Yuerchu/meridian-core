//! `queued_prompt_context_items`: the files and transcripts a queued prompt
//! referenced, prepared when it was queued and moved onto the message when it
//! is delivered.

use sea_orm::entity::prelude::*;

use crate::db::types::{EpochMs, SqlBool};
use crate::workspace::reference::MessageContextKind;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "queued_prompt_context_items")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub queue_id: String,
    pub position: i32,
    pub kind: MessageContextKind,
    pub content: String,
    pub display_path: Option<String>,
    pub line_start: Option<i32>,
    pub line_end: Option<i32>,
    pub content_hash: String,
    pub byte_count: i32,
    pub line_count: i32,
    pub token_count: i32,
    pub truncated: SqlBool,
    pub metadata: Option<String>,
    pub created_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::queued_prompt::Entity",
        from = "Column::QueueId",
        to = "super::queued_prompt::Column::Id",
        on_delete = "Cascade"
    )]
    QueuedPrompt,
}

impl Related<super::queued_prompt::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::QueuedPrompt.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

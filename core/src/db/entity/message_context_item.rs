//! `message_context_items`: what a message referenced — a file, a directory,
//! a command's output, another conversation — frozen as it was when sent.

use sea_orm::entity::prelude::*;

use crate::db::types::{EpochMs, SqlBool, text_enum_column};
use crate::workspace::reference::MessageContextKind;

text_enum_column!(MessageContextKind);

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "message_context_items")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub message_id: String,
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
        belongs_to = "super::message::Entity",
        from = "Column::MessageId",
        to = "super::message::Column::Id",
        on_delete = "Cascade"
    )]
    Message,
}

impl Related<super::message::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Message.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

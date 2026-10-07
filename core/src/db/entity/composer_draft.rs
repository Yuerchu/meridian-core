//! `composer_drafts`: what an unsent composer holds, one row per composer.
//!
//! `attachments` and `conversation_refs` decode at the read. The draft is
//! shown back to the person who typed it, so a column that does not decode
//! fails the read rather than coming back as an empty list: guessing at a part
//! of it would be showing them something they did not write.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

use crate::db::types::{EpochMs, Json};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "composer_drafts")]
pub struct Model {
    /// `new` or `conversation:<id>`; see `db::sea::ops::composer_draft::DraftSlot`.
    /// A `CHECK` holds it and `conversation_id` to the same answer.
    #[sea_orm(primary_key, auto_increment = false)]
    pub slot: String,
    pub conversation_id: Option<String>,
    pub body: String,
    pub attachments: Json<Vec<DraftAttachment>>,
    pub conversation_refs: Json<Vec<String>>,
    pub sticker_id: Option<String>,
    pub revision: i64,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    /// `SET NULL`: deleting a sticker takes it off a draft instead of being
    /// refused, and the rest of the draft stays.
    #[sea_orm(
        belongs_to = "super::emoji::Entity",
        from = "Column::StickerId",
        to = "super::emoji::Column::Id",
        on_delete = "SetNull"
    )]
    Emoji,
    /// `CASCADE`: the draft goes with its conversation, through the key rather
    /// than a line in `delete_conversation`.
    #[sea_orm(
        belongs_to = "super::conversation::Entity",
        from = "Column::ConversationId",
        to = "super::conversation::Column::Id",
        on_delete = "Cascade"
    )]
    Conversation,
}

impl ActiveModelBehavior for ActiveModel {}

/// An attachment that can be opened again by path after a restart: one element
/// of `attachments`. Strict both ways — an unknown member in a stored row is a
/// decode error, not something to skip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftAttachment {
    pub path: String,
    pub name: String,
}

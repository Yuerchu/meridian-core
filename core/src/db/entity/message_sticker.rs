//! `message_stickers`: which sticker a message showed, in order.
//!
//! The links are written inside the transaction that writes the message.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "message_stickers")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub message_id: String,
    pub sticker_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub position: i32,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    /// `RESTRICT`: a sticker a message still shows cannot be deleted, which is
    /// what the capture pool's eviction relies on.
    #[sea_orm(
        belongs_to = "super::emoji::Entity",
        from = "Column::StickerId",
        to = "super::emoji::Column::Id",
        on_delete = "Restrict"
    )]
    Emoji,
    #[sea_orm(
        belongs_to = "super::message::Entity",
        from = "Column::MessageId",
        to = "super::message::Column::Id",
        on_delete = "Cascade"
    )]
    Message,
}

impl Related<super::emoji::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Emoji.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

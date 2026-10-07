//! `assistant_emoji_packs`: which packs an assistant may send stickers from.

use sea_orm::entity::prelude::*;

use crate::db::types::EpochMs;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "assistant_emoji_packs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub assistant_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub pack_id: String,
    pub created_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::emoji_pack::Entity",
        from = "Column::PackId",
        to = "super::emoji_pack::Column::Id",
        on_delete = "Cascade"
    )]
    EmojiPack,
    #[sea_orm(
        belongs_to = "super::assistant::Entity",
        from = "Column::AssistantId",
        to = "super::assistant::Column::Id",
        on_delete = "Cascade"
    )]
    Assistant,
}

impl Related<super::emoji_pack::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::EmojiPack.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

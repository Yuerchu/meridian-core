//! `emoji_embeddings`: persisted on-device embedding vector for one sticker.
//!
//! `model_fingerprint` binds the vector to the exact `.litertlm` file and
//! vision mode that produced it; `source_hash` binds it to the sticker's
//! current name, tags, file name and raw image bytes. `vector` stores `dims`
//! little-endian IEEE 754 `f32` values (`vector.len() == dims * 4`).

use sea_orm::entity::prelude::*;

use crate::db::types::EpochMs;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "emoji_embeddings")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub emoji_id: String,
    pub model_fingerprint: String,
    pub source_hash: String,
    pub dims: i32,
    pub vector: Vec<u8>,
    pub updated_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::emoji::Entity",
        from = "Column::EmojiId",
        to = "super::emoji::Column::Id",
        on_delete = "Cascade"
    )]
    Emoji,
}

impl Related<super::emoji::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Emoji.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

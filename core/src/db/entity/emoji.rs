//! `emojis`: one sticker, its file and what it means.
//!
//! A sticker captured from OneBot arrives without a meaning: it is `pending`
//! until a model suggests one (`suggested`) or a person confirms it
//! (`confirmed`), and only a confirmed sticker can be sent. `native_payload` is
//! what the platform needs to send it again natively, a JSON object, decoded
//! at the read. `file_name` and `file_format` are empty for a sticker whose
//! image never downloaded; `tags` is free text.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

use crate::db::types::{EpochMs, Json};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "emojis")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub pack_id: String,
    pub name: String,
    pub tags: Option<String>,
    pub file_name: String,
    pub file_format: String,
    pub sort_order: i32,
    pub created_at: EpochMs,
    pub source: EmojiSource,
    /// What identifies this sticker on its platform, within its pack and
    /// source; unique there (a partial index), so a sticker seen twice is one row.
    pub source_key: Option<String>,
    pub native_payload: Option<Json<serde_json::Map<String, serde_json::Value>>>,
    pub semantic_status: EmojiSemanticStatus,
    pub suggested_name: Option<String>,
    pub suggested_tags: Option<String>,
    pub file_size: i64,
    pub seen_count: i32,
    pub last_seen_at: Option<EpochMs>,
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
}

impl Related<super::emoji_pack::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::EmojiPack.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

impl Model {
    /// The platform payload, `{}` when there is none.
    pub fn native_payload(&self) -> serde_json::Value {
        serde_json::Value::Object(self.native_payload.as_ref().map(|p| p.0.clone()).unwrap_or_default())
    }
}

/// Where a sticker came from. The column has no `CHECK`, so this type is what
/// holds the list closed. `EnumIter` is SeaORM's re-export, which
/// `ActiveEnum` requires.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    EnumIter,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    DeriveActiveEnum,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum EmojiSource {
    /// Imported from a file on this machine.
    #[sea_orm(string_value = "local")]
    Local,
    #[sea_orm(string_value = "onebot_face")]
    OnebotFace,
    #[sea_orm(string_value = "onebot_mface")]
    OnebotMface,
    #[sea_orm(string_value = "onebot_image")]
    OnebotImage,
}

impl EmojiSource {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// How far a sticker's meaning has been settled.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    EnumIter,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    DeriveActiveEnum,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum EmojiSemanticStatus {
    #[sea_orm(string_value = "pending")]
    Pending,
    #[sea_orm(string_value = "suggested")]
    Suggested,
    /// The only status a sticker can be sent in.
    #[sea_orm(string_value = "confirmed")]
    Confirmed,
}

#[cfg(test)]
mod tests {
    use sea_orm::{ActiveEnum, Iterable};

    use super::*;

    fn same_spellings<E>()
    where
        E: ActiveEnum<Value = String> + Iterable + Serialize + std::fmt::Debug,
    {
        for variant in E::iter() {
            let stored = variant.to_value();
            assert_eq!(
                serde_json::to_value(&variant).unwrap().as_str(),
                Some(stored.as_str()),
                "{variant:?}"
            );
            assert_eq!(E::try_from_value(&stored).unwrap().to_value(), stored);
        }
        assert!(E::try_from_value(&"future".to_owned()).is_err());
    }

    #[test]
    fn stored_and_wire_spellings_are_one_list() {
        same_spellings::<EmojiSource>();
        same_spellings::<EmojiSemanticStatus>();
    }
}

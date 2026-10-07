//! `emoji_packs`: a set of stickers — imported by hand, or the pool a OneBot
//! account fills by itself (one per account, held by a partial unique index on
//! `(kind, source_account_id)`).

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

use crate::db::types::{EpochMs, SqlBool};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "emoji_packs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub cover_image: Option<String>,
    pub is_builtin: SqlBool,
    pub sort_order: i32,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
    pub kind: EmojiPackKind,
    /// The OneBot account whose pool this is; `None` for a manual pack.
    pub source_account_id: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::emoji::Entity")]
    Emoji,
    #[sea_orm(has_many = "super::assistant_emoji_pack::Entity")]
    AssistantEmojiPack,
}

impl Related<super::emoji::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Emoji.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

/// Where a pack came from. The column has no `CHECK`, so this type is what
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
pub enum EmojiPackKind {
    #[sea_orm(string_value = "manual")]
    Manual,
    /// Filled by OneBot capture; see `onebot::stickers`.
    #[sea_orm(string_value = "onebot")]
    Onebot,
}

impl EmojiPackKind {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ActiveEnum, Iterable};

    use super::*;

    #[test]
    fn stored_and_wire_spellings_are_one_list() {
        for kind in EmojiPackKind::iter() {
            let stored = kind.to_value();
            assert_eq!(serde_json::to_value(kind).unwrap().as_str(), Some(stored.as_str()));
            assert_eq!(kind.as_str(), stored);
        }
        assert!(EmojiPackKind::try_from_value(&"future".to_owned()).is_err());
    }
}

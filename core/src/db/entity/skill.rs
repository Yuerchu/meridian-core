//! `skills`: the index of skill directories on disk.
//!
//! The filesystem is the truth and this is its cache: `agent::skills::sync_index`
//! rewrites the LLM-facing fields from each `SKILL.md` and drops rows whose
//! directory is gone. What survives a rescan is what the user set here — the
//! display fields and `is_enabled`.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

use crate::db::types::{EpochMs, SqlBool};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "skills")]
pub struct Model {
    /// The directory under the skills root, and the key: a slug, which the
    /// schema checks (`m0002_skill_keys`), so a path has one spelling and appears
    /// once.
    #[sea_orm(primary_key, auto_increment = false)]
    pub dir_name: String,
    pub llm_name: String,
    pub llm_description: String,
    pub display_name: String,
    pub display_description: Option<String>,
    pub source: SkillSource,
    pub is_enabled: SqlBool,
    pub is_builtin: SqlBool,
    pub mtime_hash: Option<String>,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::skill_binding_global::Entity")]
    BindingGlobal,
    #[sea_orm(has_many = "super::skill_binding_project::Entity")]
    BindingProject,
    #[sea_orm(has_many = "super::skill_binding_assistant::Entity")]
    BindingAssistant,
}

impl ActiveModelBehavior for ActiveModel {}

/// Where a skill came from. The column has no `CHECK`, so this type is what
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
pub enum SkillSource {
    /// Shipped with Meridian and rewritten on every launch.
    #[sea_orm(string_value = "official")]
    Official,
    #[sea_orm(string_value = "user")]
    User,
    #[sea_orm(string_value = "assistant")]
    Assistant,
    #[sea_orm(string_value = "imported")]
    Imported,
}

impl SkillSource {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value.parse().map_err(|_| format!("unknown skill source `{value}`"))
    }
}

/// A partial update: a field left `None` is not written. The nullable columns
/// are `Option<Option<_>>`, so `Some(None)` clears one.
#[derive(Debug, Default, DeriveIntoActiveModel)]
pub struct SkillChangeset {
    pub llm_name: Option<String>,
    pub llm_description: Option<String>,
    pub display_name: Option<String>,
    pub display_description: Option<Option<String>>,
    pub source: Option<SkillSource>,
    pub is_enabled: Option<SqlBool>,
    pub mtime_hash: Option<Option<String>>,
    pub updated_at: Option<EpochMs>,
}

#[cfg(test)]
mod tests {
    use sea_orm::{ActiveEnum, Iterable};

    use super::*;

    #[test]
    fn stored_and_wire_spellings_are_one_list() {
        for source in SkillSource::iter() {
            let stored = source.to_value();
            assert_eq!(serde_json::to_value(source).unwrap().as_str(), Some(stored.as_str()));
            assert_eq!(SkillSource::parse(&stored).unwrap(), source);
        }
        assert!(SkillSource::parse("future").is_err());
    }
}

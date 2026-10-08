//! `projects`: a working directory, or the project a OneBot chat is filed
//! under.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

use crate::db::types::EpochMs;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "projects")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub name: String,
    pub path: Option<String>,
    pub source_type: ProjectSource,
    pub source_id: Option<String>,
    pub assistant_id: Option<String>,
    pub description: Option<String>,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::assistant::Entity",
        from = "Column::AssistantId",
        to = "super::assistant::Column::Id",
        on_delete = "SetNull"
    )]
    Assistant,
}

impl Related<super::assistant::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Assistant.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

/// A partial update: a field left `None` is not written. The nullable
/// columns are `Option<Option<_>>`, so `Some(None)` clears one.
#[derive(Debug, Default, DeriveIntoActiveModel)]
pub struct ProjectChangeset {
    pub name: Option<String>,
    pub path: Option<Option<String>>,
    pub assistant_id: Option<Option<String>>,
    pub description: Option<Option<String>>,
    pub updated_at: Option<EpochMs>,
}

/// Where a project came from. The column has no `CHECK`, so this type is what
/// holds the list closed. Re-exported from `db::models::project` while the
/// Diesel row still uses it. `EnumIter` is SeaORM's re-export, which
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
pub enum ProjectSource {
    #[sea_orm(string_value = "local")]
    Local,
    #[sea_orm(string_value = "onebot_private")]
    OnebotPrivate,
    #[sea_orm(string_value = "onebot_group")]
    OnebotGroup,
}

impl ProjectSource {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value.parse().map_err(|_| format!("unknown project source `{value}`"))
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ActiveEnum, Iterable};

    use super::*;

    #[test]
    fn stored_and_wire_spellings_are_one_list() {
        for source in ProjectSource::iter() {
            let stored = source.to_value();
            assert_eq!(serde_json::to_value(source).unwrap().as_str(), Some(stored.as_str()));
            assert_eq!(source.as_str(), stored);
            assert_eq!(ProjectSource::parse(&stored).unwrap(), source);
        }
        assert!(ProjectSource::parse("slack").is_err());
    }
}

//! `redaction_rules`: the patterns the model (or the user) added on top of the
//! built-in redaction rules.
//!
//! Three columns are closed sets with a `CHECK` in the schema and decode into
//! [`RuleScope`], [`RuleCategory`] and [`RuleOrigin`]; `examples` is JSON and
//! decodes into [`RedactionExample`]s. A test below holds each enum's stored
//! spelling, its wire spelling and the `CHECK` list together.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

use crate::db::types::{EpochMs, Json, SqlBool};

/// The `scope_id` a global rule carries; the schema's `CHECK` pairs it with
/// `scope_type = 'global'`, and a project rule may not use it.
pub const GLOBAL_SCOPE_ID: &str = "_";
pub const MAX_RULES_PER_SCOPE: usize = 200;
pub const MAX_PATTERN_LEN: usize = 512;
pub const MAX_EXAMPLES: usize = 16;
pub const MAX_EXAMPLE_LEN: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "redaction_rules")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub scope_type: RuleScope,
    pub scope_id: String,
    pub name: String,
    pub description: String,
    pub pattern: String,
    pub category: RuleCategory,
    pub examples: Json<Vec<RedactionExample>>,
    pub origin: RuleOrigin,
    pub source_conversation_id: Option<String>,
    pub is_enabled: SqlBool,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// Where a rule applies. `EnumIter` is SeaORM's re-export, which `ActiveEnum`
/// requires.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    EnumIter,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
    DeriveActiveEnum,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum RuleScope {
    #[sea_orm(string_value = "global")]
    Global,
    #[sea_orm(string_value = "project")]
    Project,
}

impl RuleScope {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value.parse().map_err(|_| format!("unknown rule scope `{value}`"))
    }
}

/// What a rule protects, which the redaction mode filters on.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    EnumIter,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
    DeriveActiveEnum,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum RuleCategory {
    #[sea_orm(string_value = "secret")]
    Secret,
    #[sea_orm(string_value = "pii")]
    Pii,
    #[sea_orm(string_value = "network")]
    Network,
}

impl RuleCategory {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value.parse().map_err(|_| format!("unknown rule category `{value}`"))
    }
}

/// Who added a rule. The model may remove only its own.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    EnumIter,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
    DeriveActiveEnum,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum RuleOrigin {
    #[sea_orm(string_value = "model")]
    Model,
    #[sea_orm(string_value = "user")]
    User,
}

impl RuleOrigin {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

/// One example a rule was checked against when it was added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedactionExample {
    pub text: String,
    pub should_match: bool,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use sea_orm::{ActiveEnum, Iterable};

    use super::*;

    /// The values the snapshot's `CHECK (<column> IN (…))` allows.
    fn check_list(column: &str) -> BTreeSet<String> {
        let snapshot = include_str!("../../../schema.snapshot.sql");
        let table = snapshot
            .lines()
            .find(|line| line.starts_with("CREATE TABLE \"redaction_rules\""))
            .expect("the snapshot builds redaction_rules");
        let prefix = format!("CHECK ({column} IN (");
        let start = table.find(&prefix).expect("a CHECK on the column") + prefix.len();
        let end = start + table[start..].find("))").expect("the CHECK closes");
        table[start..end]
            .split(',')
            .map(|name| name.trim().trim_matches('\'').to_owned())
            .collect()
    }

    /// Each variant's stored spelling, after checking it is also the wire one.
    fn stored<E>() -> BTreeSet<String>
    where
        E: ActiveEnum<Value = String> + Iterable + Serialize + std::fmt::Debug,
    {
        E::iter()
            .map(|variant| {
                let db = variant.to_value();
                assert_eq!(
                    serde_json::to_value(&variant).unwrap(),
                    serde_json::Value::String(db.clone()),
                    "{variant:?} is spelt one way on the wire and another in the database"
                );
                db
            })
            .collect()
    }

    /// The stored spelling, the wire spelling and the schema's `CHECK` are one
    /// list for each of the three, read out of the snapshot rather than
    /// restated here.
    #[test]
    fn each_enum_is_the_check_constraint_it_is_stored_under() {
        assert_eq!(stored::<RuleScope>(), check_list("scope_type"));
        assert_eq!(stored::<RuleCategory>(), check_list("category"));
        assert_eq!(stored::<RuleOrigin>(), check_list("origin"));
        assert!(RuleCategory::parse("credential").is_err());
        assert!(RuleScope::try_from_value(&"team".to_owned()).is_err());
    }
}

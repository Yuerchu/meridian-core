use diesel::prelude::*;
use serde::{Deserialize, Serialize};
use strum::{EnumIter, IntoEnumIterator};

use crate::db::schema::redaction_rules;

pub const GLOBAL_SCOPE_ID: &str = "_";
pub const MAX_RULES_PER_SCOPE: usize = 200;
pub const MAX_PATTERN_LEN: usize = 512;
pub const MAX_EXAMPLES: usize = 16;
pub const MAX_EXAMPLE_LEN: usize = 512;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, EnumIter, Serialize, Deserialize, strum::IntoStaticStr, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum RuleScope {
    Global,
    Project,
}

impl RuleScope {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value.parse().map_err(|_| format!("unknown rule scope `{value}`"))
    }

    pub fn all() -> Vec<&'static str> {
        Self::iter().map(|v| v.as_str()).collect()
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, EnumIter, Serialize, Deserialize, strum::IntoStaticStr, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum RuleCategory {
    Secret,
    Pii,
    Network,
}

impl RuleCategory {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value.parse().map_err(|_| format!("unknown rule category `{value}`"))
    }

    pub fn all() -> Vec<&'static str> {
        Self::iter().map(|v| v.as_str()).collect()
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, EnumIter, Serialize, Deserialize, strum::IntoStaticStr, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum RuleOrigin {
    Model,
    User,
}

impl RuleOrigin {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value.parse().map_err(|_| format!("unknown rule origin `{value}`"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedactionExample {
    pub text: String,
    pub should_match: bool,
}

pub fn encode_examples(examples: &[RedactionExample]) -> Result<String, String> {
    serde_json::to_string(examples).map_err(|e| format!("failed to encode examples: {e}"))
}

pub fn decode_examples(raw: &str) -> Result<Vec<RedactionExample>, String> {
    serde_json::from_str(raw).map_err(|e| format!("malformed examples JSON: {e}"))
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = redaction_rules)]
pub struct RedactionRuleRow {
    pub id: String,
    pub scope_type: String,
    pub scope_id: String,
    pub name: String,
    pub description: String,
    pub pattern: String,
    pub category: String,
    pub examples: String,
    pub origin: String,
    pub source_conversation_id: Option<String>,
    pub is_enabled: i32,
    pub created_at: i64,
    pub updated_at: i64,
}

impl RedactionRuleRow {
    pub fn examples(&self) -> Result<Vec<RedactionExample>, String> {
        decode_examples(&self.examples)
    }

    pub fn scope(&self) -> Result<RuleScope, String> {
        RuleScope::parse(&self.scope_type)
    }

    pub fn category(&self) -> Result<RuleCategory, String> {
        RuleCategory::parse(&self.category)
    }

    pub fn origin(&self) -> Result<RuleOrigin, String> {
        RuleOrigin::parse(&self.origin)
    }

    pub fn is_enabled(&self) -> bool {
        self.is_enabled != 0
    }
}

#[derive(Debug, Insertable)]
#[diesel(table_name = redaction_rules)]
pub struct RedactionRuleInsert<'a> {
    pub id: &'a str,
    pub scope_type: &'a str,
    pub scope_id: &'a str,
    pub name: &'a str,
    pub description: &'a str,
    pub pattern: &'a str,
    pub category: &'a str,
    pub examples: &'a str,
    pub origin: &'a str,
    pub source_conversation_id: Option<&'a str>,
    pub is_enabled: i32,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Default, AsChangeset)]
#[diesel(table_name = redaction_rules)]
pub struct RedactionRuleChangeset {
    pub description: Option<String>,
    pub pattern: Option<String>,
    pub category: Option<String>,
    pub examples: Option<String>,
    pub is_enabled: Option<i32>,
    pub updated_at: Option<i64>,
}

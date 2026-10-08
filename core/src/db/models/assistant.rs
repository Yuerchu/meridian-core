use diesel::prelude::*;
use serde::Serialize;

use crate::db::schema::assistants;

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = assistants)]
pub struct AssistantRow {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub avatar: Option<String>,
    pub system_prompt: String,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_tokens: Option<i32>,
    pub is_default: i32,
    pub sort_order: i32,
    pub created_at: i64,
    pub updated_at: i64,
    pub context_limit: i32,
    pub compact_keep_recent: i32,
    pub enabled_tools: Option<String>,
    pub thinking_enabled: i32,
    pub thinking_budget: Option<i32>,
    pub tool_preset_id: Option<String>,
    pub auto_compact_enabled: i32,
}

/// A Diesel row as the entity model: flags must be 0 or 1 and the tool list a
/// JSON array of names, or the read fails, as it does on SeaORM.
impl TryFrom<AssistantRow> for crate::db::entity::assistant::Model {
    type Error = String;

    fn try_from(row: AssistantRow) -> Result<Self, String> {
        use crate::db::types::{Json, SqlBool};
        let flag = |name: &str, value: i32| {
            SqlBool::try_from(value).map_err(|error| format!("assistant {} has an invalid {name}: {error}", row.id))
        };
        Ok(Self {
            is_default: flag("is_default", row.is_default)?,
            thinking_enabled: flag("thinking_enabled", row.thinking_enabled)?,
            auto_compact_enabled: flag("auto_compact_enabled", row.auto_compact_enabled)?,
            enabled_tools: row
                .enabled_tools
                .as_deref()
                .map(Json::<Vec<String>>::decode)
                .transpose()
                .map_err(|error| format!("assistant {} has invalid enabled_tools: {error}", row.id))?,
            id: row.id,
            name: row.name,
            description: row.description,
            avatar: row.avatar,
            system_prompt: row.system_prompt,
            provider_id: row.provider_id,
            model_id: row.model_id,
            temperature: row.temperature,
            top_p: row.top_p,
            max_tokens: row.max_tokens,
            sort_order: row.sort_order,
            created_at: row.created_at,
            updated_at: row.updated_at,
            context_limit: row.context_limit,
            compact_keep_recent: row.compact_keep_recent,
            thinking_budget: row.thinking_budget,
            tool_preset_id: row.tool_preset_id,
        })
    }
}

#[derive(Debug, Insertable)]
#[diesel(table_name = assistants)]
pub struct AssistantInsert<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub avatar: Option<&'a str>,
    pub system_prompt: &'a str,
    pub provider_id: Option<&'a str>,
    pub model_id: Option<&'a str>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_tokens: Option<i32>,
    pub is_default: i32,
    pub sort_order: i32,
    pub created_at: i64,
    pub updated_at: i64,
    pub context_limit: i32,
    pub compact_keep_recent: i32,
    pub enabled_tools: Option<&'a str>,
    pub thinking_enabled: i32,
    pub thinking_budget: Option<i32>,
    pub tool_preset_id: Option<&'a str>,
    pub auto_compact_enabled: i32,
}

#[derive(Debug, AsChangeset, Default)]
#[diesel(table_name = assistants)]
pub struct AssistantChangeset {
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub avatar: Option<Option<String>>,
    pub system_prompt: Option<String>,
    pub provider_id: Option<Option<String>>,
    pub model_id: Option<Option<String>>,
    pub temperature: Option<Option<f32>>,
    pub top_p: Option<Option<f32>>,
    pub max_tokens: Option<Option<i32>>,
    pub is_default: Option<i32>,
    pub context_limit: Option<i32>,
    pub compact_keep_recent: Option<i32>,
    pub enabled_tools: Option<Option<String>>,
    pub thinking_enabled: Option<i32>,
    pub thinking_budget: Option<Option<i32>>,
    pub tool_preset_id: Option<Option<String>>,
    pub auto_compact_enabled: Option<i32>,
    pub updated_at: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(enabled_tools: Option<&str>, auto_compact_enabled: i32) -> AssistantRow {
        AssistantRow {
            id: "a1".into(),
            name: "A".into(),
            description: None,
            avatar: None,
            system_prompt: String::new(),
            provider_id: None,
            model_id: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            is_default: 0,
            sort_order: 0,
            created_at: 0,
            updated_at: 0,
            context_limit: 0,
            compact_keep_recent: 10,
            enabled_tools: enabled_tools.map(str::to_string),
            thinking_enabled: 0,
            thinking_budget: None,
            tool_preset_id: None,
            auto_compact_enabled,
        }
    }

    /// A tool list that is not a JSON array of names, or a flag outside 0/1,
    /// fails the read: an allow-list read as "none configured" would hand the
    /// assistant every tool.
    #[test]
    fn a_row_this_build_cannot_read_fails_the_conversion() {
        type Model = crate::db::entity::assistant::Model;
        let ok = Model::try_from(row(Some(r#"["read_file"]"#), 1)).unwrap();
        assert_eq!(
            ok.enabled_tools.map(|tools| tools.0),
            Some(vec!["read_file".to_string()])
        );
        assert!(ok.auto_compact_enabled.get());
        let error = Model::try_from(row(Some("not json"), 0)).unwrap_err();
        assert!(error.contains("invalid enabled_tools"), "{error}");
        assert!(Model::try_from(row(None, 2)).is_err());
    }
}

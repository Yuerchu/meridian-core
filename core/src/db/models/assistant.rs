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

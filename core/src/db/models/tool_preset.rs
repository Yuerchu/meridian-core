use diesel::prelude::*;
use serde::Serialize;

use crate::db::schema::tool_presets;

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = tool_presets)]
pub struct ToolPresetRow {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub tool_names: String,
    pub is_builtin: i32,
    pub sort_order: i32,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = tool_presets)]
pub struct ToolPresetInsert<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub icon: Option<&'a str>,
    pub tool_names: &'a str,
    pub is_builtin: i32,
    pub sort_order: i32,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, AsChangeset, Default)]
#[diesel(table_name = tool_presets)]
pub struct ToolPresetChangeset {
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub icon: Option<Option<String>>,
    pub tool_names: Option<String>,
    pub sort_order: Option<i32>,
    pub updated_at: Option<i64>,
}

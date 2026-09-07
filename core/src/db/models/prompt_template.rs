use diesel::prelude::*;
use serde::Serialize;

use crate::db::schema::prompt_templates;

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = prompt_templates)]
pub struct PromptTemplateRow {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub category: String,
    pub template_text: String,
    pub is_builtin: i32,
    pub sort_order: i32,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = prompt_templates)]
pub struct PromptTemplateInsert<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub category: &'a str,
    pub template_text: &'a str,
    pub is_builtin: i32,
    pub sort_order: i32,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, AsChangeset, Default)]
#[diesel(table_name = prompt_templates)]
pub struct PromptTemplateChangeset {
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub category: Option<String>,
    pub template_text: Option<String>,
    pub sort_order: Option<i32>,
    pub updated_at: Option<i64>,
}

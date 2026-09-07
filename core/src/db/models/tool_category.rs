use diesel::prelude::*;
use serde::Serialize;

use crate::db::schema::tool_categories;

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = tool_categories)]
pub struct ToolCategoryRow {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub sort_order: i32,
    pub created_at: i64,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = tool_categories)]
pub struct ToolCategoryInsert<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub icon: Option<&'a str>,
    pub sort_order: i32,
    pub created_at: i64,
}

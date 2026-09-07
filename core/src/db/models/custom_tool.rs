use diesel::prelude::*;
use serde::Serialize;

use crate::db::schema::custom_tools;

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = custom_tools)]
pub struct CustomToolRow {
    pub id: String,
    pub name: String,
    pub description: String,
    pub category_id: Option<String>,
    pub parameters_schema: String,
    pub command: String,
    pub args_template: Option<String>,
    pub working_directory: Option<String>,
    pub timeout_ms: Option<i32>,
    pub permission: String,
    pub is_enabled: i32,
    pub sort_order: i32,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = custom_tools)]
pub struct CustomToolInsert<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub description: &'a str,
    pub category_id: Option<&'a str>,
    pub parameters_schema: &'a str,
    pub command: &'a str,
    pub args_template: Option<&'a str>,
    pub working_directory: Option<&'a str>,
    pub timeout_ms: Option<i32>,
    pub permission: &'a str,
    pub is_enabled: i32,
    pub sort_order: i32,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, AsChangeset, Default)]
#[diesel(table_name = custom_tools)]
pub struct CustomToolChangeset {
    pub name: Option<String>,
    pub description: Option<String>,
    pub category_id: Option<Option<String>>,
    pub parameters_schema: Option<String>,
    pub command: Option<String>,
    pub args_template: Option<Option<String>>,
    pub working_directory: Option<Option<String>>,
    pub timeout_ms: Option<Option<i32>>,
    pub permission: Option<String>,
    pub is_enabled: Option<i32>,
    pub sort_order: Option<i32>,
    pub updated_at: Option<i64>,
}

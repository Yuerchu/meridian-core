use diesel::prelude::*;
use serde::Serialize;

use crate::db::schema::emojis;

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = emojis)]
pub struct EmojiRow {
    pub id: String,
    pub pack_id: String,
    pub name: String,
    pub tags: Option<String>,
    pub file_name: String,
    pub file_format: String,
    pub sort_order: i32,
    pub created_at: i64,
    pub source: String,
    pub source_key: Option<String>,
    pub native_payload: Option<String>,
    pub semantic_status: String,
    pub suggested_name: Option<String>,
    pub suggested_tags: Option<String>,
    pub file_size: i64,
    pub seen_count: i32,
    pub last_seen_at: Option<i64>,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = emojis)]
pub struct EmojiInsert<'a> {
    pub id: &'a str,
    pub pack_id: &'a str,
    pub name: &'a str,
    pub tags: Option<&'a str>,
    pub file_name: &'a str,
    pub file_format: &'a str,
    pub sort_order: i32,
    pub created_at: i64,
    pub source: &'a str,
    pub source_key: Option<&'a str>,
    pub native_payload: Option<&'a str>,
    pub semantic_status: &'a str,
    pub suggested_name: Option<&'a str>,
    pub suggested_tags: Option<&'a str>,
    pub file_size: i64,
    pub seen_count: i32,
    pub last_seen_at: Option<i64>,
}

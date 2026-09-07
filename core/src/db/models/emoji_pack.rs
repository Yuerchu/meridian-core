use diesel::prelude::*;
use serde::Serialize;

use crate::db::schema::emoji_packs;

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = emoji_packs)]
pub struct EmojiPackRow {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub cover_image: Option<String>,
    pub is_builtin: i32,
    pub sort_order: i32,
    pub created_at: i64,
    pub updated_at: i64,
    pub kind: String,
    pub source_account_id: Option<String>,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = emoji_packs)]
pub struct EmojiPackInsert<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub cover_image: Option<&'a str>,
    pub is_builtin: i32,
    pub sort_order: i32,
    pub created_at: i64,
    pub updated_at: i64,
    pub kind: &'a str,
    pub source_account_id: Option<&'a str>,
}

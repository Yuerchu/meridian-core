use crate::db::schema::message_context_items;
use diesel::prelude::*;

/// A frozen, user-provided context block bound to one message branch.
///
/// `content` and `metadata` are persistence-only fields. The shell maps rows
/// into an explicit descriptor response before anything crosses IPC.
#[derive(Debug, Clone, Queryable, Selectable)]
#[diesel(table_name = message_context_items)]
pub struct MessageContextItemRow {
    pub id: String,
    pub message_id: String,
    pub position: i32,
    pub kind: String,
    pub content: String,
    pub display_path: Option<String>,
    pub line_start: Option<i32>,
    pub line_end: Option<i32>,
    pub content_hash: String,
    pub byte_count: i32,
    pub line_count: i32,
    pub token_count: i32,
    pub truncated: i32,
    pub metadata: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Insertable)]
#[diesel(table_name = message_context_items)]
pub struct MessageContextItemInsert<'a> {
    pub id: &'a str,
    pub message_id: &'a str,
    pub position: i32,
    pub kind: &'a str,
    pub content: &'a str,
    pub display_path: Option<&'a str>,
    pub line_start: Option<i32>,
    pub line_end: Option<i32>,
    pub content_hash: &'a str,
    pub byte_count: i32,
    pub line_count: i32,
    pub token_count: i32,
    pub truncated: i32,
    pub metadata: Option<&'a str>,
    pub created_at: i64,
}

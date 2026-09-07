use crate::db::schema::queued_prompt_context_items;
use diesel::prelude::*;

#[derive(Debug, Clone, Queryable, Selectable)]
#[diesel(table_name = queued_prompt_context_items)]
pub struct QueuedPromptContextItemRow {
    pub id: String,
    pub queue_id: String,
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

impl TryFrom<QueuedPromptContextItemRow> for crate::workspace::reference::PreparedContextItem {
    type Error = String;

    fn try_from(item: QueuedPromptContextItemRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: item.id,
            kind: crate::workspace::reference::MessageContextKind::parse(&item.kind)?,
            content: item.content,
            display_path: item.display_path,
            line_start: item.line_start,
            line_end: item.line_end,
            content_hash: item.content_hash,
            byte_count: item.byte_count,
            line_count: item.line_count,
            token_count: item.token_count,
            truncated: item.truncated,
            metadata: item.metadata,
        })
    }
}

#[derive(Debug, Insertable)]
#[diesel(table_name = queued_prompt_context_items)]
pub struct QueuedPromptContextItemInsert<'a> {
    pub id: &'a str,
    pub queue_id: &'a str,
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

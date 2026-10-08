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

/// A Diesel row as the entity model: the kind must be one this build knows
/// and the truncation flag 0 or 1, or the read fails, as it does on SeaORM.
impl TryFrom<MessageContextItemRow> for crate::db::entity::message_context_item::Model {
    type Error = String;

    fn try_from(row: MessageContextItemRow) -> Result<Self, String> {
        Ok(Self {
            kind: crate::workspace::reference::MessageContextKind::parse(&row.kind)?,
            truncated: crate::db::types::SqlBool::try_from(row.truncated)?,
            id: row.id,
            message_id: row.message_id,
            position: row.position,
            content: row.content,
            display_path: row.display_path,
            line_start: row.line_start,
            line_end: row.line_end,
            content_hash: row.content_hash,
            byte_count: row.byte_count,
            line_count: row.line_count,
            token_count: row.token_count,
            metadata: row.metadata,
            created_at: row.created_at,
        })
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, truncated: i32) -> MessageContextItemRow {
        MessageContextItemRow {
            id: "i1".into(),
            message_id: "m1".into(),
            position: 0,
            kind: kind.into(),
            content: "bytes".into(),
            display_path: None,
            line_start: None,
            line_end: None,
            content_hash: "hash".into(),
            byte_count: 5,
            line_count: 1,
            token_count: 1,
            truncated,
            metadata: None,
            created_at: 1,
        }
    }

    /// An unknown kind or a flag outside 0/1 fails the conversion rather than
    /// reading as some kind, or as not truncated.
    #[test]
    fn a_row_this_build_cannot_read_fails_the_conversion() {
        type Model = crate::db::entity::message_context_item::Model;
        let ok = Model::try_from(row("shell_output", 1)).unwrap();
        assert_eq!(ok.kind, crate::workspace::reference::MessageContextKind::ShellOutput);
        assert!(ok.truncated.get());
        assert!(Model::try_from(row("teleport", 0)).is_err());
        assert!(Model::try_from(row("shell_output", 2)).is_err());
    }
}

use diesel::prelude::*;
use serde::Serialize;

use crate::db::schema::acp_sessions;

/// Which agent session a hosted conversation is, and where it runs.
///
/// The two travel together because neither is any use alone: an id with no
/// directory cannot be resumed, and a directory with no id is what this table
/// replaced.
#[derive(Debug, Clone, Queryable, Selectable, Identifiable, Serialize)]
#[diesel(table_name = acp_sessions)]
#[diesel(primary_key(conversation_id))]
pub struct AcpSessionRow {
    pub conversation_id: String,
    /// `None` means there is nothing to resume, not that something is missing.
    /// A conversation from before this table existed reads this way, and so
    /// does one whose adapter came up but never opened a session.
    pub acp_session_id: Option<String>,
    pub cwd: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = acp_sessions)]
pub struct AcpSessionInsert<'a> {
    pub conversation_id: &'a str,
    pub acp_session_id: Option<&'a str>,
    pub cwd: &'a str,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, AsChangeset)]
#[diesel(table_name = acp_sessions)]
pub struct AcpSessionChangeset<'a> {
    pub acp_session_id: Option<&'a str>,
    pub cwd: &'a str,
    pub updated_at: i64,
}

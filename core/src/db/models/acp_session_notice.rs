use diesel::prelude::*;
use serde::Serialize;

use crate::db::schema::acp_session_notices;

/// One incident a hosted Claude Code session reported about itself, at the
/// latest revision the adapter published for it.
///
/// The adapter's own id is `notice_id`; `id` is this app's row id. Two
/// conversations can each hold an incident with the same `notice_id` — the
/// adapter's ids are scoped to its session — which is why the unique key is
/// the pair and not the adapter's id alone.
#[derive(Debug, Clone, Queryable, Selectable, Identifiable, Serialize)]
#[diesel(table_name = acp_session_notices)]
pub struct AcpSessionNoticeRow {
    pub id: String,
    pub conversation_id: String,
    /// `None` for an incident that belongs to the session rather than to a
    /// turn: noticed between turns, or restored from the agent's own history.
    pub turn_id: Option<String>,
    pub notice_id: String,
    pub revision: i32,
    pub category: String,
    pub severity: String,
    pub title: String,
    pub details: Option<String>,
    pub reason: Option<String>,
    /// A JSON array of action names. Decoded strictly by whoever reads it.
    pub actions: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = acp_session_notices)]
pub struct AcpSessionNoticeInsert<'a> {
    pub id: &'a str,
    pub conversation_id: &'a str,
    pub turn_id: Option<&'a str>,
    pub notice_id: &'a str,
    pub revision: i32,
    pub category: &'a str,
    pub severity: &'a str,
    pub title: &'a str,
    pub details: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub actions: &'a str,
    pub created_at: i64,
    pub updated_at: i64,
}

/// What a higher revision of the same incident is allowed to change. The
/// identity — row id, conversation, `notice_id`, `created_at` — stays.
#[derive(Debug, AsChangeset)]
#[diesel(table_name = acp_session_notices)]
pub struct AcpSessionNoticeChangeset<'a> {
    pub turn_id: Option<Option<&'a str>>,
    pub revision: i32,
    pub category: &'a str,
    pub severity: &'a str,
    pub title: &'a str,
    pub details: Option<Option<&'a str>>,
    pub reason: Option<Option<&'a str>>,
    pub actions: &'a str,
    pub updated_at: i64,
}

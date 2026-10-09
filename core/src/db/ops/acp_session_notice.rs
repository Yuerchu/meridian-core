//! The incidents a hosted Claude Code session reported about itself.
//!
//! Written by the ACP session as the adapter publishes them and read back with
//! the conversation snapshot, so a failure survives a reload — which the bare
//! JSON-RPC error string this replaced never did.

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::acp_session_notice::{AcpSessionNoticeChangeset, AcpSessionNoticeInsert, AcpSessionNoticeRow};
use crate::db::schema::acp_session_notices;

/// [`upsert_if_newer`] without the transaction around it. The caller holds
/// one, or the read and the write it makes can interleave with another
/// revision of the same incident.
pub fn upsert_if_newer_in_transaction(
    conn: &mut SqliteConnection,
    insert: AcpSessionNoticeInsert<'_>,
) -> QueryResult<Option<AcpSessionNoticeRow>> {
    let existing = acp_session_notices::table
        .filter(acp_session_notices::conversation_id.eq(insert.conversation_id))
        .filter(acp_session_notices::notice_id.eq(insert.notice_id))
        .select(AcpSessionNoticeRow::as_select())
        .first(conn)
        .optional()?;

    match existing {
        None => {
            diesel::insert_into(acp_session_notices::table)
                .values(&insert)
                .execute(conn)?;
            acp_session_notices::table
                .find(insert.id)
                .select(AcpSessionNoticeRow::as_select())
                .first(conn)
                .map(Some)
        }
        Some(row) if insert.revision > row.revision => {
            diesel::update(acp_session_notices::table.find(&row.id))
                .set(&AcpSessionNoticeChangeset {
                    // A revision may attach a session-scoped incident to
                    // the turn it ended, but never detach one.
                    turn_id: insert.turn_id.map(Some),
                    revision: insert.revision,
                    category: insert.category,
                    severity: insert.severity,
                    title: insert.title,
                    details: Some(insert.details),
                    reason: Some(insert.reason),
                    actions: insert.actions,
                    updated_at: insert.updated_at,
                })
                .execute(conn)?;
            acp_session_notices::table
                .find(&row.id)
                .select(AcpSessionNoticeRow::as_select())
                .first(conn)
                .map(Some)
        }
        Some(_) => Ok(None),
    }
}

/// Every incident on record for a conversation, oldest first.
pub fn list_for_conversation(
    conn: &mut SqliteConnection,
    conversation_id: &str,
) -> QueryResult<Vec<AcpSessionNoticeRow>> {
    acp_session_notices::table
        .filter(acp_session_notices::conversation_id.eq(conversation_id))
        .order((acp_session_notices::created_at.asc(), acp_session_notices::id.asc()))
        .select(AcpSessionNoticeRow::as_select())
        .load(conn)
}

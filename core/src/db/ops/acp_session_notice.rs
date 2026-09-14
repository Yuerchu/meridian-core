//! The incidents a hosted Claude Code session reported about itself.
//!
//! Written by the ACP session as the adapter publishes them and read back with
//! the conversation snapshot, so a failure survives a reload — which the bare
//! JSON-RPC error string this replaced never did.

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::acp_session_notice::{AcpSessionNoticeChangeset, AcpSessionNoticeInsert, AcpSessionNoticeRow};
use crate::db::schema::acp_session_notices;

/// Record a revision of an incident, keeping only the newest.
///
/// The adapter republishes an incident under the same `notice_id` when it
/// changes — an `api_retry` warning becoming the terminal failure is the same
/// incident at revision 2 — and reissues old ones when a session is loaded
/// again. So the write is conditional: a first sighting inserts, a higher
/// revision updates in place, and anything else is a no-op. `None` is that
/// no-op, and it is the caller's cue to announce nothing.
///
/// The row id in `insert` is only used when a row is created; on an update the
/// existing id stays, so an event carries the id the frontend already holds.
///
/// One transaction, `immediate`, because the decision is read-then-write and
/// two revisions of one incident can arrive back to back. A caller already
/// inside a transaction — the import writer — uses
/// [`upsert_if_newer_in_transaction`], since SQLite cannot open a second
/// immediate one.
pub fn upsert_if_newer(
    conn: &mut SqliteConnection,
    insert: AcpSessionNoticeInsert<'_>,
) -> QueryResult<Option<AcpSessionNoticeRow>> {
    conn.immediate_transaction(|conn| upsert_if_newer_in_transaction(conn, insert))
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    fn conversation(conn: &mut SqliteConnection, id: &str) {
        crate::db::ops::conversation::create_conversation(conn, id, Some("hosted"), None, None, 0).unwrap();
    }

    fn insert<'a>(
        id: &'a str,
        conversation_id: &'a str,
        notice_id: &'a str,
        revision: i32,
        title: &'a str,
        now: i64,
    ) -> AcpSessionNoticeInsert<'a> {
        AcpSessionNoticeInsert {
            id,
            conversation_id,
            turn_id: None,
            notice_id,
            revision,
            category: "limit",
            severity: "warning",
            title,
            details: None,
            reason: None,
            actions: "[]",
            created_at: now,
            updated_at: now,
        }
    }

    /// The ordinary life of an incident: a warning, then the same incident at
    /// a higher revision saying it became the failure — one row throughout.
    #[test]
    fn a_higher_revision_updates_the_incident_in_place() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");

        let first = upsert_if_newer(
            &mut conn,
            insert("n1", "c1", "turn-1:error", 1, "Retrying, attempt 1 of 5.", 100),
        )
        .unwrap()
        .expect("a first sighting is written");
        assert_eq!(first.revision, 1);

        let mut second = insert("n2", "c1", "turn-1:error", 2, "Rate limit reached.", 200);
        second.severity = "error";
        second.actions = r#"["retry"]"#;
        second.turn_id = Some("turn-1");
        let updated = upsert_if_newer(&mut conn, second)
            .unwrap()
            .expect("a higher revision is written");
        assert_eq!(updated.id, "n1", "the row keeps the id the first sighting minted");
        assert_eq!(updated.revision, 2);
        assert_eq!(updated.title, "Rate limit reached.");
        assert_eq!(updated.severity, "error");
        assert_eq!(updated.actions, r#"["retry"]"#);
        assert_eq!(updated.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(updated.created_at, 100, "first seen stays first seen");
        assert_eq!(updated.updated_at, 200);

        assert_eq!(list_for_conversation(&mut conn, "c1").unwrap().len(), 1);
    }

    /// A replay of what is already known changes nothing and announces
    /// nothing: the same revision again, or an older one arriving late.
    #[test]
    fn an_equal_or_lower_revision_is_ignored() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");

        upsert_if_newer(&mut conn, insert("n1", "c1", "s1:notice:1", 2, "Model fallback.", 100)).unwrap();
        assert!(
            upsert_if_newer(
                &mut conn,
                insert("n2", "c1", "s1:notice:1", 2, "Model fallback again.", 200)
            )
            .unwrap()
            .is_none()
        );
        assert!(
            upsert_if_newer(&mut conn, insert("n3", "c1", "s1:notice:1", 1, "Older.", 300))
                .unwrap()
                .is_none()
        );
        let rows = list_for_conversation(&mut conn, "c1").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Model fallback.");
        assert_eq!(rows[0].updated_at, 100);
    }

    /// The adapter's ids are scoped to its session, so the same one in two
    /// conversations is two incidents.
    #[test]
    fn the_same_notice_id_in_two_conversations_is_two_rows() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        conversation(&mut conn, "c2");

        upsert_if_newer(&mut conn, insert("n1", "c1", "shared", 1, "one", 100)).unwrap();
        upsert_if_newer(&mut conn, insert("n2", "c2", "shared", 1, "two", 100)).unwrap();
        assert_eq!(list_for_conversation(&mut conn, "c1").unwrap().len(), 1);
        assert_eq!(list_for_conversation(&mut conn, "c2").unwrap().len(), 1);
    }

    /// Incidents go with the conversation they were about.
    #[test]
    fn deleting_the_conversation_takes_its_incidents() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        upsert_if_newer(&mut conn, insert("n1", "c1", "x", 1, "gone", 100)).unwrap();

        crate::db::ops::conversation::delete_conversation(&mut conn, "c1").unwrap();
        assert!(list_for_conversation(&mut conn, "c1").unwrap().is_empty());
    }
}

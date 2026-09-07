//! Reading and writing which agent session a hosted conversation is.

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::acp_session::{AcpSessionChangeset, AcpSessionInsert, AcpSessionRow};
use crate::db::schema::acp_sessions;

/// What is on record for a conversation, if anything.
///
/// `None` for a conversation that is not hosted at all, and for one whose row
/// was never written — the caller cannot start a session either way, since it
/// would have no directory to start it in.
pub fn get(conn: &mut SqliteConnection, conversation_id: &str) -> QueryResult<Option<AcpSessionRow>> {
    acp_sessions::table
        .find(conversation_id)
        .select(AcpSessionRow::as_select())
        .first(conn)
        .optional()
}

/// Which conversation owns each agent session this app knows about.
///
/// The whole ownership register, and it is small — one row per hosted
/// conversation. Read whole rather than queried per session because the caller
/// is annotating a list the agent just handed over, and a lookup per row would
/// be one statement per session on the machine.
///
/// Rows with no session id are left out: they own nothing, and `None` is a
/// state rather than a gap (see [`upsert`]).
pub fn owners(conn: &mut SqliteConnection) -> QueryResult<Vec<(String, String)>> {
    acp_sessions::table
        .filter(acp_sessions::acp_session_id.is_not_null())
        .select((acp_sessions::acp_session_id, acp_sessions::conversation_id))
        .load::<(Option<String>, String)>(conn)
        .map(|rows| {
            rows.into_iter()
                .filter_map(|(session, conversation)| Some((session?, conversation)))
                .collect()
        })
}

/// Write what a session opened as, whether it is the first one or the tenth.
///
/// Upsert rather than insert-then-update: the row exists for a conversation
/// that has been opened before and does not for one being created, and every
/// caller wants the same thing either way. `created_at` is left alone on a
/// conflict, so it keeps saying when the conversation first had a session.
///
/// `acp_session_id` is what the *agent* answered with. On a resume that is not
/// necessarily the id that was asked for — the SDK decides which session it
/// actually recovered — so writing back the reply is what keeps the next resume
/// asking for something that exists.
pub fn upsert(
    conn: &mut SqliteConnection,
    conversation_id: &str,
    acp_session_id: Option<&str>,
    cwd: &str,
    now: i64,
) -> QueryResult<usize> {
    diesel::insert_into(acp_sessions::table)
        .values(&AcpSessionInsert {
            conversation_id,
            acp_session_id,
            cwd,
            created_at: now,
            updated_at: now,
        })
        .on_conflict(acp_sessions::conversation_id)
        .do_update()
        .set(&AcpSessionChangeset {
            acp_session_id,
            cwd,
            updated_at: now,
        })
        .execute(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    fn conversation(conn: &mut SqliteConnection, id: &str) {
        crate::db::ops::conversation::create_conversation(conn, id, Some("hosted"), None, None, 0).unwrap();
    }

    /// The ordinary life of a row: opened without a session id, then given one,
    /// then given a different one when a resume lands somewhere else.
    #[test]
    fn a_conversations_session_is_rewritten_in_place() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");

        upsert(&mut conn, "c1", None, "/work/meridian", 100).unwrap();
        let row = get(&mut conn, "c1").unwrap().expect("written");
        assert_eq!(row.acp_session_id, None, "no session yet is a state, not a gap");
        assert_eq!(row.cwd, "/work/meridian");
        assert_eq!(row.created_at, 100);

        upsert(&mut conn, "c1", Some("sess-1"), "/work/meridian", 200).unwrap();
        let row = get(&mut conn, "c1").unwrap().unwrap();
        assert_eq!(row.acp_session_id.as_deref(), Some("sess-1"));
        assert_eq!(row.created_at, 100, "first opened stays first opened");
        assert_eq!(row.updated_at, 200);

        // A resume that recovered a different session than the one asked for.
        upsert(&mut conn, "c1", Some("sess-2"), "/work/meridian", 300).unwrap();
        assert_eq!(
            get(&mut conn, "c1").unwrap().unwrap().acp_session_id.as_deref(),
            Some("sess-2")
        );
    }

    /// Two conversations must not name one agent session: they would be two
    /// transcripts written from the same place.
    #[test]
    fn one_session_cannot_belong_to_two_conversations() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        conversation(&mut conn, "c2");

        upsert(&mut conn, "c1", Some("sess-1"), "/a", 1).unwrap();
        assert!(upsert(&mut conn, "c2", Some("sess-1"), "/b", 2).is_err());

        // And "nothing to resume" is not a value that collides with itself.
        upsert(&mut conn, "c2", None, "/b", 2).unwrap();
        conversation(&mut conn, "c3");
        upsert(&mut conn, "c3", None, "/c", 3).unwrap();
    }

    /// The register a session list is annotated against. A conversation with
    /// no session id owns nothing and must not appear — otherwise every
    /// listed session in that directory would be reported as already taken.
    #[test]
    fn the_owner_list_names_only_conversations_that_hold_a_session() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        conversation(&mut conn, "c2");
        conversation(&mut conn, "c3");

        upsert(&mut conn, "c1", Some("sess-1"), "/a", 1).unwrap();
        upsert(&mut conn, "c2", None, "/b", 2).unwrap();
        upsert(&mut conn, "c3", Some("sess-3"), "/c", 3).unwrap();

        let mut owned = owners(&mut conn).unwrap();
        owned.sort();
        assert_eq!(
            owned,
            vec![
                ("sess-1".to_string(), "c1".to_string()),
                ("sess-3".to_string(), "c3".to_string()),
            ]
        );
    }

    /// The row belongs to the conversation and goes with it. Left behind, it
    /// would be a session id nothing can reach and a UNIQUE index entry blocking
    /// a conversation that legitimately resumes it later.
    #[test]
    fn deleting_the_conversation_takes_the_row() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        upsert(&mut conn, "c1", Some("sess-1"), "/a", 1).unwrap();

        crate::db::ops::conversation::delete_conversation(&mut conn, "c1").unwrap();
        assert!(get(&mut conn, "c1").unwrap().is_none());
    }
}

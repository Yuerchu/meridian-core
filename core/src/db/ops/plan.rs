use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::plan::{KIND_PLAN, PlanStatus};
use crate::db::schema::mode_artifacts;

/// Retire the artifact in force because the work it described is finished.
/// Without this an approved plan would keep being injected into every later
/// request, long after it stopped being what the conversation is about.
pub(super) fn complete_active(conn: &mut SqliteConnection, conversation_id: &str, now: i64) -> QueryResult<usize> {
    conn.transaction(|conn| {
        let legacy = retire_approved(conn, conversation_id, KIND_PLAN, PlanStatus::Done, now)?;
        // The old table remains readable during the compatibility release, but
        // the versioned document is the active source of truth.  Completing a
        // conversation must retire both or the approved revision would keep
        // being injected after its legacy mirror says the work is done.
        let documents = diesel::update(
            crate::db::schema::plan_documents::table
                .filter(crate::db::schema::plan_documents::conversation_id.eq(conversation_id))
                .filter(
                    crate::db::schema::plan_documents::state
                        .eq(crate::db::models::plan_review::PlanDocumentState::Approved.as_str()),
                ),
        )
        .set((
            crate::db::schema::plan_documents::state
                .eq(crate::db::models::plan_review::PlanDocumentState::Done.as_str()),
            crate::db::schema::plan_documents::lock_version.eq(crate::db::schema::plan_documents::lock_version + 1),
            crate::db::schema::plan_documents::updated_at.eq(now),
        ))
        .execute(conn)?;
        Ok(legacy + documents)
    })
}

fn retire_approved(
    conn: &mut SqliteConnection,
    conversation_id: &str,
    kind: &str,
    to: PlanStatus,
    now: i64,
) -> QueryResult<usize> {
    diesel::update(
        mode_artifacts::table
            .filter(mode_artifacts::conversation_id.eq(conversation_id))
            .filter(mode_artifacts::kind.eq(kind))
            .filter(mode_artifacts::status.eq(PlanStatus::Approved.as_str())),
    )
    .set((
        mode_artifacts::status.eq(to.as_str()),
        mode_artifacts::updated_at.eq(now),
    ))
    .execute(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::diesel_test_db;

    fn seed_conversation(conn: &mut SqliteConnection, id: &str) {
        use crate::db::schema::conversations;
        diesel::insert_into(conversations::table)
            .values((
                conversations::id.eq(id),
                conversations::created_at.eq(1),
                conversations::updated_at.eq(1),
            ))
            .execute(conn)
            .unwrap();
    }

    #[test]
    fn completing_with_no_active_plan_is_a_no_op() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        seed_conversation(&mut conn, "c1");
        assert_eq!(complete_active(&mut conn, "c1", 10).unwrap(), 0);
    }

    #[test]
    fn statuses_round_trip_through_parse() {
        for s in [
            PlanStatus::Pending,
            PlanStatus::Approved,
            PlanStatus::Rejected,
            PlanStatus::Superseded,
            PlanStatus::Done,
        ] {
            assert_eq!(PlanStatus::parse(s.as_str()).unwrap(), s);
        }
        assert!(PlanStatus::parse("bogus").is_err());
    }
}

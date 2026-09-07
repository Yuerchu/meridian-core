use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::plan::{KIND_PLAN, ModeArtifactInsert, ModeArtifactRow, PlanStatus};
use crate::db::schema::mode_artifacts;

/// Store an artifact the model just proposed. It starts `pending`; the user's
/// decision moves it on.
pub fn record_plan(
    conn: &mut SqliteConnection,
    conversation_id: &str,
    content: &str,
    now: i64,
) -> QueryResult<ModeArtifactRow> {
    let id = uuid::Uuid::new_v4().to_string();
    diesel::insert_into(mode_artifacts::table)
        .values(&ModeArtifactInsert {
            id: &id,
            conversation_id,
            kind: KIND_PLAN,
            content,
            status: PlanStatus::Pending.as_str(),
            created_at: now,
            updated_at: now,
        })
        .execute(conn)?;
    mode_artifacts::table.find(&id).first::<ModeArtifactRow>(conn)
}

/// Accept an artifact, retiring whichever one was previously in force. Only one
/// per conversation and kind is ever `approved` — enforced by a partial unique
/// index as well as by this transaction, so two concurrent approvals cannot
/// both land.
pub fn approve(conn: &mut SqliteConnection, id: &str, now: i64) -> QueryResult<ModeArtifactRow> {
    conn.transaction(|conn| {
        let plan = mode_artifacts::table.find(id).first::<ModeArtifactRow>(conn)?;
        retire_approved(conn, &plan.conversation_id, &plan.kind, PlanStatus::Superseded, now)?;
        set_status(conn, id, PlanStatus::Approved, now)
    })
}

pub fn reject(conn: &mut SqliteConnection, id: &str, now: i64) -> QueryResult<ModeArtifactRow> {
    set_status(conn, id, PlanStatus::Rejected, now)
}

/// Retire the artifact in force because the work it described is finished.
/// Without this an approved plan would keep being injected into every later
/// request, long after it stopped being what the conversation is about.
pub fn complete_active(conn: &mut SqliteConnection, conversation_id: &str, now: i64) -> QueryResult<usize> {
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

fn set_status(conn: &mut SqliteConnection, id: &str, status: PlanStatus, now: i64) -> QueryResult<ModeArtifactRow> {
    diesel::update(mode_artifacts::table.find(id))
        .set((
            mode_artifacts::status.eq(status.as_str()),
            mode_artifacts::updated_at.eq(now),
        ))
        .execute(conn)?;
    mode_artifacts::table.find(id).first::<ModeArtifactRow>(conn)
}

/// The plan currently being implemented, if any.
pub fn get_active(conn: &mut SqliteConnection, conversation_id: &str) -> QueryResult<Option<ModeArtifactRow>> {
    mode_artifacts::table
        .filter(mode_artifacts::conversation_id.eq(conversation_id))
        .filter(mode_artifacts::kind.eq(KIND_PLAN))
        .filter(mode_artifacts::status.eq(PlanStatus::Approved.as_str()))
        .first::<ModeArtifactRow>(conn)
        .optional()
}

/// Every plan row of a conversation, oldest first. Test-only: production reads
/// go through `get_active_plan` / `get_approved_plan`, but tests verify row
/// history directly.
#[cfg(test)]
pub fn list_plans(conn: &mut SqliteConnection, conversation_id: &str) -> QueryResult<Vec<ModeArtifactRow>> {
    mode_artifacts::table
        .filter(mode_artifacts::conversation_id.eq(conversation_id))
        .filter(mode_artifacts::kind.eq(KIND_PLAN))
        .order(mode_artifacts::created_at.asc())
        .load::<ModeArtifactRow>(conn)
}

/// Render the approved plan for the system prompt. Follows the same contract as
/// the checklist block: `None` when there is nothing to say, a leading blank
/// line built in, wrapped in a tag.
pub fn format_plan_block(plan: &ModeArtifactRow) -> Option<String> {
    let content = plan.content.trim();
    if content.is_empty() {
        return None;
    }
    Some(format!("\n\n<approved_plan>\n{content}\n</approved_plan>"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

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

    /// Migrations are plain SQL and Diesel does not check them at compile time,
    /// so this is the only place a broken ALTER TABLE surfaces before runtime.
    #[test]
    fn migrations_apply_and_the_mode_column_round_trips() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_conversation(&mut conn, "c1");

        let conv = crate::db::ops::conversation::get_conversation(&mut conn, "c1").unwrap();
        assert_eq!(conv.mode, None, "defaults to the work mode");

        crate::db::ops::conversation::update_mode(&mut conn, "c1", Some("plan"), 2).unwrap();
        let conv = crate::db::ops::conversation::get_conversation(&mut conn, "c1").unwrap();
        assert_eq!(conv.mode.as_deref(), Some("plan"));

        crate::db::ops::conversation::update_mode(&mut conn, "c1", None, 3).unwrap();
        assert_eq!(
            crate::db::ops::conversation::get_conversation(&mut conn, "c1")
                .unwrap()
                .mode,
            None,
        );
    }

    #[test]
    fn a_recorded_plan_starts_pending_and_is_not_active_yet() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_conversation(&mut conn, "c1");

        let plan = record_plan(&mut conn, "c1", "# Plan\n\nDo the thing.", 10).unwrap();
        assert_eq!(plan.status, "pending");
        assert_eq!(plan.kind, "plan");
        assert!(get_active(&mut conn, "c1").unwrap().is_none());
    }

    #[test]
    fn approving_makes_it_active() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_conversation(&mut conn, "c1");

        let plan = record_plan(&mut conn, "c1", "first", 10).unwrap();
        approve(&mut conn, &plan.id, 20).unwrap();

        let active = get_active(&mut conn, "c1").unwrap().unwrap();
        assert_eq!(active.id, plan.id);
        assert_eq!(active.status, "approved");
    }

    #[test]
    fn approving_a_second_plan_supersedes_the_first() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_conversation(&mut conn, "c1");

        let first = record_plan(&mut conn, "c1", "first", 10).unwrap();
        approve(&mut conn, &first.id, 20).unwrap();
        let second = record_plan(&mut conn, "c1", "second", 30).unwrap();
        approve(&mut conn, &second.id, 40).unwrap();

        let active = get_active(&mut conn, "c1").unwrap().unwrap();
        assert_eq!(active.id, second.id, "only the latest approval is in force");

        let all = list_plans(&mut conn, "c1").unwrap();
        assert_eq!(all.len(), 2, "the earlier plan is kept, not deleted");
        assert_eq!(all[0].status, "superseded");
    }

    /// The application-level transaction is not the only guard: a concurrent
    /// approval that slipped past it must still be refused by the database.
    #[test]
    fn the_database_refuses_a_second_approved_artifact() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_conversation(&mut conn, "c1");

        let first = record_plan(&mut conn, "c1", "first", 10).unwrap();
        approve(&mut conn, &first.id, 20).unwrap();
        let second = record_plan(&mut conn, "c1", "second", 30).unwrap();

        // Bypassing `approve` is the only way to attempt this; the partial
        // unique index is what stops it.
        let forced = diesel::update(mode_artifacts::table.find(&second.id))
            .set(mode_artifacts::status.eq(PlanStatus::Approved.as_str()))
            .execute(&mut conn);

        assert!(forced.is_err());
    }

    #[test]
    fn finishing_the_work_retires_the_plan() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_conversation(&mut conn, "c1");

        let plan = record_plan(&mut conn, "c1", "the plan", 10).unwrap();
        approve(&mut conn, &plan.id, 20).unwrap();
        assert!(get_active(&mut conn, "c1").unwrap().is_some());

        complete_active(&mut conn, "c1", 30).unwrap();

        assert!(get_active(&mut conn, "c1").unwrap().is_none(), "stops being injected");
        let all = list_plans(&mut conn, "c1").unwrap();
        assert_eq!(all[0].status, "done", "kept on file, not deleted");
    }

    #[test]
    fn completing_with_no_active_plan_is_a_no_op() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_conversation(&mut conn, "c1");
        assert_eq!(complete_active(&mut conn, "c1", 10).unwrap(), 0);
    }

    #[test]
    fn rejecting_leaves_the_previous_approval_in_place() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_conversation(&mut conn, "c1");

        let good = record_plan(&mut conn, "c1", "good", 10).unwrap();
        approve(&mut conn, &good.id, 20).unwrap();
        let bad = record_plan(&mut conn, "c1", "bad", 30).unwrap();
        reject(&mut conn, &bad.id, 40).unwrap();

        assert_eq!(get_active(&mut conn, "c1").unwrap().unwrap().id, good.id);
    }

    #[test]
    fn plans_do_not_leak_between_conversations() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_conversation(&mut conn, "c1");
        seed_conversation(&mut conn, "c2");

        let mine = record_plan(&mut conn, "c1", "mine", 10).unwrap();
        approve(&mut conn, &mine.id, 20).unwrap();

        assert!(get_active(&mut conn, "c2").unwrap().is_none());
        // The unique index is scoped per conversation, so c2 can still approve.
        let theirs = record_plan(&mut conn, "c2", "theirs", 30).unwrap();
        assert!(approve(&mut conn, &theirs.id, 40).is_ok());
    }

    #[test]
    fn deleting_a_conversation_cascades_to_its_artifacts() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_conversation(&mut conn, "c1");
        record_plan(&mut conn, "c1", "doomed", 10).unwrap();

        crate::db::ops::conversation::delete_conversation(&mut conn, "c1").unwrap();

        assert!(list_plans(&mut conn, "c1").unwrap().is_empty());
    }

    #[test]
    fn the_block_carries_the_plan_and_skips_empty_ones() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_conversation(&mut conn, "c1");

        let plan = record_plan(&mut conn, "c1", "  # Plan\n\nStep one.  ", 10).unwrap();
        let block = format_plan_block(&plan).unwrap();
        assert!(block.starts_with("\n\n<approved_plan>"));
        assert!(block.ends_with("</approved_plan>"));
        assert!(block.contains("Step one."));

        let blank = record_plan(&mut conn, "c1", "   \n  ", 20).unwrap();
        assert!(format_plan_block(&blank).is_none());
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

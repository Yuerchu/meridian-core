//! The durable half of a turn.
//!
//! A turn runs on the stack of the task driving it, and everything about it —
//! the cancellation token, the approvals waiting on a human — lives in memory.
//! That is fine while the process is alive and useless the moment it is not.
//! These rows are what remains: a turn says what it is about to do *before*
//! doing it, so whatever the last phase says is where it died.
//!
//! Nothing here is written from a destructor. Destructors do not run for a
//! kill, which is the case that matters, so the design leans the other way: a
//! row left at `running` is itself the record that the turn never reached its
//! own ending, and `reconcile_interrupted` says so at the next launch.

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

#[cfg(test)]
use crate::db::models::turn::TurnRow;
use crate::db::models::turn::{TurnInsert, TurnPhase, TurnStatus};
use crate::db::schema::turns;
use crate::turn::{TurnOrigin, TurnTrigger};

/// Record a turn that is starting. Called once the conversation has actually
/// been taken, so a refused turn leaves nothing behind.
///
/// `self_id` names the bot account for a turn a bot started, and is `None`
/// everywhere else. It travels with `origin` rather than separately because the
/// two are one fact — where this turn came from — and a caller that could set
/// one without the other would eventually set only one.
pub fn begin(
    conn: &mut SqliteConnection,
    id: &str,
    conversation_id: &str,
    origin: TurnOrigin,
    self_id: Option<i64>,
    now: i64,
) -> QueryResult<usize> {
    begin_triggered(
        conn,
        id,
        conversation_id,
        origin,
        self_id,
        (TurnTrigger::User, None),
        now,
    )
}

/// [`begin`], for a turn that says what set it going. Everything a person
/// started goes through `begin`, which is this with `TurnTrigger::User`.
fn begin_triggered(
    conn: &mut SqliteConnection,
    id: &str,
    conversation_id: &str,
    origin: TurnOrigin,
    self_id: Option<i64>,
    (trigger, trigger_ref): (TurnTrigger, Option<&str>),
    now: i64,
) -> QueryResult<usize> {
    diesel::insert_into(turns::table)
        .values(&TurnInsert {
            id,
            conversation_id,
            origin: origin.as_str(),
            status: TurnStatus::Running.as_str(),
            phase: Some(TurnPhase::Streaming.as_str()),
            started_at: now,
            updated_at: now,
            self_id,
            trigger: trigger.as_str(),
            trigger_ref,
        })
        .execute(conn)
}

/// Close a turn out. Only the paths that actually reach an ending call this —
/// everything else is left for `reconcile_interrupted`.
pub fn finish(
    conn: &mut SqliteConnection,
    id: &str,
    status: TurnStatus,
    error: Option<&str>,
    now: i64,
) -> QueryResult<usize> {
    diesel::update(running(id))
        .set((
            turns::status.eq(status.as_str()),
            turns::error.eq(error),
            turns::ended_at.eq(Some(now)),
            turns::updated_at.eq(now),
        ))
        .execute(conn)
}

/// A turn only while it is still running.
///
/// Every write goes through this, so the lifecycle is one-way at the SQL level
/// rather than by call-order discipline. Two things it rules out: a phase write
/// that lands after the turn ended, which would leave a finished row claiming
/// to be inside a tool; and a second `finish`, which would overwrite a terminal
/// state with a later opinion. The second is reachable today — the desktop turn
/// records `done` and *then* emits its stop event, and a failed emit sends the
/// caller down the failure path.
fn running(
    id: &str,
) -> diesel::helper_types::Filter<
    diesel::helper_types::Find<turns::table, &str>,
    diesel::dsl::Eq<turns::status, &'static str>,
> {
    turns::table
        .find(id)
        .filter(turns::status.eq(TurnStatus::Running.as_str()))
}

pub use crate::db::sea::ops::turn::{InterruptedCandidate, Ledger};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::diesel_test_db;
    use crate::db::ops::conversation::create_conversation;

    fn conv(conn: &mut SqliteConnection, id: &str) {
        create_conversation(conn, id, Some("t"), None, None, 1000).unwrap();
    }

    fn get(conn: &mut SqliteConnection, id: &str) -> TurnRow {
        turns::table.find(id).first::<TurnRow>(conn).unwrap()
    }

    /// A turn says what set it going, and a person's turn says so without
    /// being asked: every caller of `begin` is one.
    #[test]
    fn a_turn_records_what_set_it_going() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        conv(&mut conn, "c1");

        begin(&mut conn, "asked", "c1", TurnOrigin::Desktop, None, 1000).unwrap();
        begin_triggered(
            &mut conn,
            "woken",
            "c1",
            TurnOrigin::Desktop,
            None,
            (TurnTrigger::TaskCompletion, Some("task-1")),
            1001,
        )
        .unwrap();

        assert_eq!(get(&mut conn, "asked").trigger().unwrap(), TurnTrigger::User);
        let woken = get(&mut conn, "woken");
        assert_eq!(woken.trigger().unwrap(), TurnTrigger::TaskCompletion);
        assert_eq!(woken.trigger_ref.as_deref(), Some("task-1"));
    }

    /// The column takes only the four words the enum has; anything else is a
    /// broken row, refused where it is written rather than discovered where it
    /// is read.
    #[test]
    fn an_unknown_trigger_is_refused_by_the_table() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        conv(&mut conn, "c1");
        let refused = diesel::sql_query(
            "INSERT INTO turns (id, conversation_id, origin, status, started_at, updated_at, trigger) \
             VALUES ('t', 'c1', 'desktop', 'running', 1, 1, 'whenever')",
        )
        .execute(&mut conn);
        assert!(refused.is_err());
    }

    #[test]
    fn a_turn_starts_running_and_streaming() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        conv(&mut conn, "c1");

        begin(&mut conn, "t1", "c1", TurnOrigin::Desktop, None, 1000).unwrap();

        let t = get(&mut conn, "t1");
        assert_eq!(t.status().unwrap(), TurnStatus::Running);
        assert_eq!(t.phase().unwrap(), Some(TurnPhase::Streaming));
        assert_eq!(t.origin, "desktop");
        assert!(t.ended_at.is_none());
    }

    #[test]
    fn a_failed_turn_keeps_what_went_wrong() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        conv(&mut conn, "c1");
        begin(&mut conn, "t1", "c1", TurnOrigin::Desktop, None, 1000).unwrap();

        finish(&mut conn, "t1", TurnStatus::Failed, Some("API Key not set"), 1500).unwrap();

        let t = get(&mut conn, "t1");
        assert_eq!(t.status().unwrap(), TurnStatus::Failed);
        assert_eq!(t.error.as_deref(), Some("API Key not set"));
    }

    /// A second ending cannot overwrite the first. Reachable today: the desktop
    /// records `done` and *then* emits its stop event, and an emit that fails
    /// sends the caller down the failure path with the same turn id.
    #[test]
    fn a_turn_cannot_end_twice() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        conv(&mut conn, "c1");
        begin(&mut conn, "t1", "c1", TurnOrigin::Desktop, None, 1000).unwrap();
        finish(&mut conn, "t1", TurnStatus::Done, None, 1500).unwrap();

        assert_eq!(
            finish(&mut conn, "t1", TurnStatus::Failed, Some("too late"), 2000).unwrap(),
            0,
        );

        let t = get(&mut conn, "t1");
        assert_eq!(t.status().unwrap(), TurnStatus::Done);
        assert_eq!(t.ended_at, Some(1500));
        assert!(t.error.is_none());
    }

    /// A turn cut short by the loop guard did not complete, and its record must
    /// not say it did — the stop event on the same turn says `loop_detected`.
    #[test]
    fn a_turn_the_loop_guard_stopped_is_not_recorded_as_done() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        conv(&mut conn, "c1");
        begin(&mut conn, "t1", "c1", TurnOrigin::Desktop, None, 1000).unwrap();

        finish(
            &mut conn,
            "t1",
            TurnStatus::Failed,
            Some(crate::db::models::turn::ERROR_LOOP_DETECTED),
            1500,
        )
        .unwrap();

        let t = get(&mut conn, "t1");
        assert_eq!(t.status().unwrap(), TurnStatus::Failed);
        assert_eq!(t.error.as_deref(), Some("loop_detected"));
    }

    /// A status this build does not know is a contract error rather than a
    /// value silently reinterpreted as no status.
    #[test]
    fn an_unknown_status_is_rejected() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        conv(&mut conn, "c1");
        begin(&mut conn, "t1", "c1", TurnOrigin::Desktop, None, 1000).unwrap();
        diesel::update(turns::table.find("t1"))
            .set(turns::status.eq("from_the_future"))
            .execute(&mut conn)
            .unwrap();

        let t = get(&mut conn, "t1");
        assert_eq!(t.status(), Err("unknown turn status 'from_the_future'".into()));
    }

    #[test]
    fn an_unknown_phase_is_rejected() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        conv(&mut conn, "c1");
        begin(&mut conn, "t1", "c1", TurnOrigin::Desktop, None, 1000).unwrap();
        diesel::update(turns::table.find("t1"))
            .set(turns::phase.eq("from_the_future"))
            .execute(&mut conn)
            .unwrap();

        let turn = get(&mut conn, "t1");
        assert_eq!(turn.phase(), Err("unknown turn phase 'from_the_future'".into()));
    }
}

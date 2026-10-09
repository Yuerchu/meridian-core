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

use crate::db::entity::turn;
use crate::db::models::turn::{TurnInsert, TurnPhase, TurnRow, TurnStatus};
use crate::db::schema::turns;
use crate::turn::{TurnOrigin, TurnTrigger};

/// A Diesel row as the entity model; a stored value this build cannot read
/// fails the read, as it does on the SeaORM side.
pub(super) fn model(row: TurnRow) -> QueryResult<turn::Model> {
    turn::Model::try_from(row).map_err(|error| {
        diesel::result::Error::DeserializationError(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            error,
        )))
    })
}

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

/// Say what the turn is about to do. Must be committed *before* the thing it
/// names, or the window it was meant to cover is still uncovered.
///
/// `tool` names the call `phase` refers to, and is cleared when it does not.
pub fn set_phase(
    conn: &mut SqliteConnection,
    id: &str,
    phase: TurnPhase,
    tool: Option<&str>,
    now: i64,
) -> QueryResult<usize> {
    diesel::update(running(id))
        .set((
            turns::phase.eq(phase.as_str()),
            turns::phase_tool.eq(tool),
            turns::updated_at.eq(now),
        ))
        .execute(conn)
}

/// Release a turn at the durable human-review boundary.
///
/// Unlike a running turn, this state survives process death honestly: the
/// review row contains everything needed to continue, and no task or lease is
/// expected to remain alive. `ended_at` stays NULL because the tool call has
/// not received its decision yet.
pub(super) fn wait_for_review(conn: &mut SqliteConnection, id: &str, now: i64) -> QueryResult<usize> {
    diesel::update(running(id))
        .set((
            turns::status.eq(TurnStatus::WaitingReview.as_str()),
            turns::phase.eq(Some(TurnPhase::AwaitingApproval.as_str())),
            turns::phase_tool.eq(Some(crate::agent::modes::EXIT_PLAN_TOOL)),
            turns::updated_at.eq(now),
        ))
        .execute(conn)
}

/// Settle a durable review boundary after its transcript tool result has been
/// committed. Callers may wrap both writes in one outer transaction.
pub fn finish_waiting_review(
    conn: &mut SqliteConnection,
    id: &str,
    status: TurnStatus,
    error: Option<&str>,
    now: i64,
) -> QueryResult<usize> {
    assert!(
        matches!(status, TurnStatus::Done | TurnStatus::Cancelled | TurnStatus::Failed),
        "a waiting review may only move to a terminal status"
    );
    diesel::update(
        turns::table
            .find(id)
            .filter(turns::status.eq(TurnStatus::WaitingReview.as_str())),
    )
    .set((
        turns::status.eq(status.as_str()),
        turns::error.eq(error),
        turns::ended_at.eq(Some(now)),
        turns::updated_at.eq(now),
    ))
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

/// Tie-break for turns that started in the same millisecond.
///
/// One conversation's turns are strictly sequential — the coordinator sees to
/// that — but a turn refused by its provider can begin and end inside a
/// millisecond, so two of them sharing a `started_at` is reachable. `id` cannot
/// break the tie: it is a uuid, and its ordering has nothing to do with when
/// the row was written. SQLite's rowid does, being assigned on insert — and
/// both readers here care which turn came first: one caps its answer by
/// recency, the other narrates the interruptions in the order they happened.
fn insertion_order() -> diesel::expression::SqlLiteral<diesel::sql_types::BigInt> {
    diesel::dsl::sql::<diesel::sql_types::BigInt>("rowid")
}

/// Every turn of a conversation, oldest first. For the transcript snapshot,
/// which is what lets the UI say which turn was cut off rather than guessing.
pub fn list_for_conversation(conn: &mut SqliteConnection, conversation_id: &str) -> QueryResult<Vec<turn::Model>> {
    turns::table
        .filter(turns::conversation_id.eq(conversation_id))
        .order((turns::started_at.asc(), insertion_order().asc()))
        .load::<TurnRow>(conn)?
        .into_iter()
        .map(model)
        .collect()
}

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

    /// The phase is the diagnosis, so it has to track what the turn is really
    /// doing — including going back to streaming once a tool returns.
    #[test]
    fn the_phase_follows_the_turn() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        conv(&mut conn, "c1");
        begin(&mut conn, "t1", "c1", TurnOrigin::Desktop, None, 1000).unwrap();

        set_phase(&mut conn, "t1", TurnPhase::AwaitingApproval, Some("run_command"), 1001).unwrap();
        let t = get(&mut conn, "t1");
        assert_eq!(t.phase().unwrap(), Some(TurnPhase::AwaitingApproval));
        assert_eq!(t.phase_tool.as_deref(), Some("run_command"));

        set_phase(&mut conn, "t1", TurnPhase::RunningTool, Some("run_command"), 1002).unwrap();
        assert_eq!(get(&mut conn, "t1").phase().unwrap(), Some(TurnPhase::RunningTool));

        // Back to the model, and the tool is no longer what it is doing.
        set_phase(&mut conn, "t1", TurnPhase::Streaming, None, 1003).unwrap();
        let t = get(&mut conn, "t1");
        assert_eq!(t.phase().unwrap(), Some(TurnPhase::Streaming));
        assert!(
            t.phase_tool.is_none(),
            "a phase that names no tool must not keep the last one"
        );
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

    /// Turns belong to their conversation and go with it.
    #[test]
    fn deleting_a_conversation_takes_its_turns() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        conv(&mut conn, "c1");
        begin(&mut conn, "t1", "c1", TurnOrigin::Desktop, None, 1000).unwrap();

        crate::db::ops::conversation::delete_conversation(&mut conn, "c1").unwrap();

        assert!(list_for_conversation(&mut conn, "c1").unwrap().is_empty());
    }

    /// The lifecycle is one-way, enforced in SQL rather than by call order.
    /// A phase write that lands after the turn ended would leave a finished row
    /// claiming to be inside a tool — and "inside a tool" is the reading that
    /// says side effects may have happened.
    #[test]
    fn a_finished_turn_no_longer_moves() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        conv(&mut conn, "c1");
        begin(&mut conn, "t1", "c1", TurnOrigin::Desktop, None, 1000).unwrap();
        finish(&mut conn, "t1", TurnStatus::Done, None, 1500).unwrap();
        let ended = get(&mut conn, "t1");

        assert_eq!(
            set_phase(&mut conn, "t1", TurnPhase::RunningTool, Some("edit_file"), 2000).unwrap(),
            0,
            "a late phase write must find nothing to update",
        );

        let after = get(&mut conn, "t1");
        assert_eq!(after.phase, ended.phase);
        assert_eq!(after.phase_tool, ended.phase_tool);
        assert_eq!(after.updated_at, ended.updated_at);
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

    /// The desktop's turn ids arrive from the front end, so a replayed one is
    /// reachable without anything being malicious — a retry, a double
    /// dispatch. It must not be able to reopen a turn that has already ended:
    /// the insert conflicts, but everything after it is an update by primary
    /// key and would rewrite that turn's ending while the new turn's messages
    /// filed themselves under it.
    #[test]
    fn a_second_turn_cannot_claim_an_id_that_is_already_on_record() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        conv(&mut conn, "c1");
        begin(&mut conn, "t1", "c1", TurnOrigin::Desktop, None, 1000).unwrap();
        set_phase(&mut conn, "t1", TurnPhase::RunningTool, Some("edit_file"), 1001).unwrap();
        finish(&mut conn, "t1", TurnStatus::Done, None, 1500).unwrap();

        let replayed = begin(&mut conn, "t1", "c1", TurnOrigin::Desktop, None, 2000);

        assert!(
            matches!(
                replayed,
                Err(diesel::result::Error::DatabaseError(
                    diesel::result::DatabaseErrorKind::UniqueViolation,
                    _
                ))
            ),
            "a replayed id must be refused by the database, not merged into the old row",
        );
        // And the finished turn is exactly as it was.
        let t = get(&mut conn, "t1");
        assert_eq!(t.status().unwrap(), TurnStatus::Done);
        assert_eq!(t.started_at, 1000);
        assert_eq!(t.ended_at, Some(1500));
        assert_eq!(list_for_conversation(&mut conn, "c1").unwrap().len(), 1);
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

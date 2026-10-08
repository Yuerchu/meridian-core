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

/// Record that these turns have now been described to the model.
///
/// `updated_at` is left alone on purpose: it says when the turn itself last did
/// something, and the turn is dead. Being talked about is not doing something.
///
/// The `IS NULL` filter keeps the first telling as the recorded one, which
/// matters because two runners can read the same unreported turn before either
/// of them dispatches.
pub fn mark_reported(conn: &mut SqliteConnection, ids: &[String], ledger: Ledger, now: i64) -> QueryResult<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let rows = turns::table.filter(turns::id.eq_any(ids));
    match ledger {
        Ledger::Own => diesel::update(rows.filter(turns::reported_at.is_null()))
            .set(turns::reported_at.eq(Some(now)))
            .execute(conn),
        Ledger::Parent => diesel::update(rows.filter(turns::parent_reported_at.is_null()))
            .set(turns::parent_reported_at.eq(Some(now)))
            .execute(conn),
    }
}

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

/// One turn's record, for a caller that has the id and wants the verdict.
///
/// `None` for an id with no row, which is not an error: a turn can fail before
/// it has written one, and the callers here treat "no record" and "did not
/// reach an ending" the same way.
pub(super) fn get(conn: &mut SqliteConnection, turn_id: &str) -> QueryResult<Option<turn::Model>> {
    turns::table
        .find(turn_id)
        .first::<TurnRow>(conn)
        .optional()?
        .map(model)
        .transpose()
}

/// Mark every turn still recorded as running as interrupted, and report how
/// many there were.
///
/// Sound because a turn only ever runs inside the process that wrote its row:
/// there is no scheduler, no worker pool, nothing that could still be going.
/// So a `running` row seen at startup is not a turn in progress, it is a turn
/// that was killed — and `phase` is the last thing it admitted to doing.
///
/// The one case this does not cover is two copies of the app sharing a
/// database, where one would declare the other's live turns dead. That is
/// already impossible for other reasons (the OneBot listener binds a port, MCP
/// servers are spawned as children) and is not designed for.
/// **It also holds those conversations' queues**, and that is not a second
/// concern bolted on — it is the same fact written in the other place it has to
/// be. The queue's rule is that a turn which did not reach an ending holds
/// everything behind it: "now rename that function" means nothing if the
/// function was never created, and only a person can decide otherwise. A crash
/// is the purest case of a turn that reached no ending, and without this the
/// rule survives everything except the one event it was written for — the next
/// enqueue would pump a follow-up whose premise died with the process.
pub fn reconcile_interrupted(conn: &mut SqliteConnection, now: i64) -> QueryResult<usize> {
    conn.transaction(|conn| {
        // Read before the update, because after it there is nothing left to
        // tell these conversations apart from any other.
        let stranded: Vec<String> = turns::table
            .filter(turns::status.eq(TurnStatus::Running.as_str()))
            .select(turns::conversation_id)
            .distinct()
            .load(conn)?;

        let interrupted = diesel::update(turns::table.filter(turns::status.eq(TurnStatus::Running.as_str())))
            .set((
                turns::status.eq(TurnStatus::Interrupted.as_str()),
                turns::ended_at.eq(Some(now)),
                turns::updated_at.eq(now),
            ))
            .execute(conn)?;

        let mut held = 0;
        for conversation_id in &stranded {
            held += crate::db::ops::queue::hold_all(conn, conversation_id, now)?;
        }
        if held > 0 {
            tracing::info!(
                items = held,
                conversations = stranded.len(),
                "held queued prompts whose turn was cut off",
            );
        }
        Ok(interrupted)
    })
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

    /// The whole point. A turn that was killed left its row at `running` with
    /// the phase it died in; startup turns that into a diagnosis without losing
    /// the phase.
    #[test]
    fn a_turn_left_running_is_interrupted_at_the_next_launch() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        conv(&mut conn, "c1");
        begin(&mut conn, "t1", "c1", TurnOrigin::Desktop, None, 1000).unwrap();
        set_phase(&mut conn, "t1", TurnPhase::RunningTool, Some("edit_file"), 1001).unwrap();

        assert_eq!(reconcile_interrupted(&mut conn, 2000).unwrap(), 1);

        let t = get(&mut conn, "t1");
        assert_eq!(t.status().unwrap(), TurnStatus::Interrupted);
        assert_eq!(t.ended_at, Some(2000));
        assert_eq!(
            t.phase().unwrap(),
            Some(TurnPhase::RunningTool),
            "the phase is the diagnosis; reconciliation must not erase it",
        );
        assert_eq!(t.phase_tool.as_deref(), Some("edit_file"));
    }

    /// **A killed turn holds whatever was queued behind it.**
    ///
    /// The queue's rule is that only a turn reaching an ending lets the next
    /// item go — "now rename that function" means nothing if the function was
    /// never created. A crash is the purest case of a turn that reached no
    /// ending, and it is also the only one where nobody is there to see it, so
    /// without this the rule survived every situation except the one it was
    /// written for.
    #[test]
    fn a_killed_turn_holds_the_queue_that_was_waiting_on_it() {
        use crate::db::models::queue::{Delivery, QueueState};
        use crate::db::ops::queue;

        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        conv(&mut conn, "crashed");
        conv(&mut conn, "idle");
        begin(&mut conn, "t1", "crashed", TurnOrigin::Desktop, None, 1000).unwrap();

        queue::enqueue(&mut conn, "q1", "crashed", "now rename it", Delivery::FollowUp, 1).unwrap();
        // A conversation nothing was running on has nothing to be in the dark
        // about, and its queue must not be swept up along with the other's.
        queue::enqueue(&mut conn, "q2", "idle", "unrelated", Delivery::FollowUp, 1).unwrap();

        assert_eq!(reconcile_interrupted(&mut conn, 2000).unwrap(), 1);

        assert_eq!(
            queue::list(&mut conn, "crashed").unwrap()[0].state(),
            QueueState::Held,
            "and so it waits for a person rather than running on a dead premise",
        );
        assert_eq!(
            queue::list(&mut conn, "idle").unwrap()[0].state(),
            QueueState::Queued,
            "an untouched conversation's queue is not collateral",
        );
    }

    #[test]
    fn reconciliation_leaves_finished_turns_alone() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        conv(&mut conn, "c1");
        for (id, status) in [
            ("done", TurnStatus::Done),
            ("cancelled", TurnStatus::Cancelled),
            ("failed", TurnStatus::Failed),
        ] {
            begin(&mut conn, id, "c1", TurnOrigin::Desktop, None, 1000).unwrap();
            finish(&mut conn, id, status, None, 1500).unwrap();
        }
        begin(&mut conn, "live", "c1", TurnOrigin::OneBot, None, 1000).unwrap();

        assert_eq!(reconcile_interrupted(&mut conn, 2000).unwrap(), 1);

        assert_eq!(get(&mut conn, "done").status().unwrap(), TurnStatus::Done);
        assert_eq!(get(&mut conn, "cancelled").status().unwrap(), TurnStatus::Cancelled);
        assert_eq!(get(&mut conn, "failed").status().unwrap(), TurnStatus::Failed);
        assert_eq!(get(&mut conn, "live").status().unwrap(), TurnStatus::Interrupted);
        // Nothing to do the second time.
        assert_eq!(reconcile_interrupted(&mut conn, 2001).unwrap(), 0);
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
        // And it is not swept up as if it were running.
        assert_eq!(reconcile_interrupted(&mut conn, 2000).unwrap(), 0);
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

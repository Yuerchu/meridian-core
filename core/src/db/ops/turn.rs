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

use crate::db::models::turn::{TurnInsert, TurnPhase, TurnRow, TurnStatus};
use crate::db::schema::turns;
use crate::turn::TurnOrigin;

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
pub fn wait_for_review(conn: &mut SqliteConnection, id: &str, now: i64) -> QueryResult<usize> {
    diesel::update(running(id))
        .set((
            turns::status.eq(TurnStatus::WaitingReview.as_str()),
            turns::phase.eq(Some(TurnPhase::AwaitingApproval.as_str())),
            turns::phase_tool.eq(Some("exit_plan")),
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

/// Turns of a conversation that may still owe the model an explanation, newest
/// first, capped at `limit`.
///
/// `excluding` is the turn asking. By the time a turn wants to know how the
/// previous ones ended it has already opened its own record, so without this it
/// would find itself — running, held by the coordinator, and therefore
/// perfectly fine.
///
/// "May" because `running` is only half an answer here: the coordinator decides
/// whether such a row is a live turn or a dead one. The two statuses selected
/// are the only ones that can be a dead turn at all; `done`, `cancelled` and
/// `failed` each reached an ending and said so in the transcript.
///
/// Deliberately *not* "the most recent turn". The previous design read only the
/// latest row and so treated any later turn as having consumed the notice,
/// including one that failed before sending a single request. `reported_at` is
/// the consumption record instead, and it is only written by a turn that got a
/// reply back and read it to the end.
pub fn unreported_for_conversation(
    conn: &mut SqliteConnection,
    conversation_id: &str,
    excluding: Option<&str>,
    limit: i64,
) -> QueryResult<Vec<InterruptedCandidate>> {
    let mut out: Vec<InterruptedCandidate> = turns::table
        .filter(turns::conversation_id.eq(conversation_id))
        .filter(turns::id.ne(excluding.unwrap_or("")))
        .filter(turns::reported_at.is_null())
        .filter(turns::status.eq_any([TurnStatus::Running.as_str(), TurnStatus::Interrupted.as_str()]))
        .order((turns::started_at.desc(), insertion_order().desc()))
        .limit(limit)
        .load::<TurnRow>(conn)?
        .into_iter()
        .map(|turn| InterruptedCandidate {
            turn,
            ledger: Ledger::Own,
            child_title: None,
        })
        .collect();

    // What the conversation delegated. The parent has to hear about these
    // itself: "a sub-agent was partway through `edit_file`" is the fact that
    // matters, and it lives on a row in a conversation the parent's own history
    // never mentions.
    let children: Vec<(String, Option<String>)> = crate::db::schema::conversations::table
        .filter(crate::db::schema::conversations::parent_conversation_id.eq(conversation_id))
        .select((
            crate::db::schema::conversations::id,
            crate::db::schema::conversations::title,
        ))
        .load(conn)?;
    if !children.is_empty() {
        let ids: Vec<&str> = children.iter().map(|(id, _)| id.as_str()).collect();
        let delegated = turns::table
            .filter(turns::conversation_id.eq_any(&ids))
            // Only the delegated run itself. A follow-up the user typed into the
            // sub-agent's transcript is between them and that conversation — the
            // parent never saw the question and would be left guessing what an
            // interruption there was even about.
            .filter(turns::origin.eq(TurnOrigin::SubAgent.as_str()))
            .filter(turns::parent_reported_at.is_null())
            .filter(turns::status.eq_any([TurnStatus::Running.as_str(), TurnStatus::Interrupted.as_str()]))
            .order((turns::started_at.desc(), insertion_order().desc()))
            .limit(limit)
            .load::<TurnRow>(conn)?;
        out.extend(delegated.into_iter().map(|turn| {
            let child_title = children
                .iter()
                .find(|(id, _)| *id == turn.conversation_id)
                .and_then(|(_, title)| title.clone());
            InterruptedCandidate {
                turn,
                ledger: Ledger::Parent,
                child_title,
            }
        }));
    }

    // Both halves arrive newest first; merging keeps that. A stable sort settles
    // a shared millisecond in favour of the conversation's own turn, which is
    // the one the reader has actually seen.
    out.sort_by_key(|x| std::cmp::Reverse(x.turn.started_at));
    out.truncate(limit as usize);
    Ok(out)
}

/// Which ledger records that a turn has been described.
///
/// A delegated run has two audiences — its own conversation, which the user can
/// open and read, and the one that spawned it — and one column cannot serve
/// both. With a single `reported_at`, opening the sub-agent and typing one
/// message would consume the notice, and the parent would never hear that a tool
/// had been left half-run.
///
/// Two columns rather than a `(turn, recipient)` table because depth is one by
/// construction: a sub-agent is handed no way to delegate, so a turn has at most
/// two audiences. Lift that and this has to become the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ledger {
    /// `turns.reported_at`: the conversation the turn ran in has been told.
    Own,
    /// `turns.parent_reported_at`: the conversation that delegated it has.
    Parent,
}

/// A turn that may still owe an explanation, and who it owes it to.
pub struct InterruptedCandidate {
    pub turn: TurnRow,
    pub ledger: Ledger,
    /// The sub-agent's title — the description the parent gave when it
    /// delegated. Joined here rather than looked up while wording the report:
    /// that side has no connection, and asking it to grow one to fetch a string
    /// it was handed would be a query per line.
    pub child_title: Option<String>,
}

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
pub fn list_for_conversation(conn: &mut SqliteConnection, conversation_id: &str) -> QueryResult<Vec<TurnRow>> {
    turns::table
        .filter(turns::conversation_id.eq(conversation_id))
        .order((turns::started_at.asc(), insertion_order().asc()))
        .load::<TurnRow>(conn)
}

/// One turn's record, for a caller that has the id and wants the verdict.
///
/// `None` for an id with no row, which is not an error: a turn can fail before
/// it has written one, and the callers here treat "no record" and "did not
/// reach an ending" the same way.
pub fn get(conn: &mut SqliteConnection, turn_id: &str) -> QueryResult<Option<TurnRow>> {
    turns::table.find(turn_id).first::<TurnRow>(conn).optional()
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
    use crate::db::ops::conversation::create_conversation;
    use crate::db::test_db;

    fn conv(conn: &mut SqliteConnection, id: &str) {
        create_conversation(conn, id, Some("t"), None, None, 1000).unwrap();
    }

    fn get(conn: &mut SqliteConnection, id: &str) -> TurnRow {
        turns::table.find(id).first::<TurnRow>(conn).unwrap()
    }

    #[test]
    fn a_turn_starts_running_and_streaming() {
        let pool = test_db();
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
        let pool = test_db();
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
        let pool = test_db();
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

        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conv(&mut conn, "crashed");
        conv(&mut conn, "idle");
        begin(&mut conn, "t1", "crashed", TurnOrigin::Desktop, None, 1000).unwrap();

        queue::enqueue(&mut conn, "q1", "crashed", "now rename it", Delivery::FollowUp, 1).unwrap();
        // A conversation nothing was running on has nothing to be in the dark
        // about, and its queue must not be swept up along with the other's.
        queue::enqueue(&mut conn, "q2", "idle", "unrelated", Delivery::FollowUp, 1).unwrap();

        assert_eq!(reconcile_interrupted(&mut conn, 2000).unwrap(), 1);

        assert_eq!(queue::list(&mut conn, "crashed").unwrap()[0].state(), QueueState::Held);
        assert!(
            queue::next_pending(&mut conn, "crashed").unwrap().is_none(),
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
        let pool = test_db();
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
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conv(&mut conn, "c1");
        begin(&mut conn, "t1", "c1", TurnOrigin::Desktop, None, 1000).unwrap();

        finish(&mut conn, "t1", TurnStatus::Failed, Some("API Key not set"), 1500).unwrap();

        let t = get(&mut conn, "t1");
        assert_eq!(t.status().unwrap(), TurnStatus::Failed);
        assert_eq!(t.error.as_deref(), Some("API Key not set"));
    }

    fn ids(candidates: Vec<InterruptedCandidate>) -> Vec<String> {
        candidates.into_iter().map(|c| c.turn.id).collect()
    }

    #[test]
    fn unreported_turns_come_back_newest_first_within_their_conversation() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conv(&mut conn, "c1");
        conv(&mut conn, "c2");
        begin(&mut conn, "old", "c1", TurnOrigin::Desktop, None, 1000).unwrap();
        begin(&mut conn, "new", "c1", TurnOrigin::Desktop, None, 2000).unwrap();
        begin(&mut conn, "other", "c2", TurnOrigin::Desktop, None, 3000).unwrap();

        assert_eq!(
            ids(unreported_for_conversation(&mut conn, "c1", None, 10).unwrap()),
            ["new", "old"]
        );
        assert_eq!(
            ids(unreported_for_conversation(&mut conn, "c2", None, 10).unwrap()),
            ["other"]
        );
        assert!(
            unreported_for_conversation(&mut conn, "nope", None, 10)
                .unwrap()
                .is_empty()
        );
        // The turn asking is never one of the answers.
        assert_eq!(
            ids(unreported_for_conversation(&mut conn, "c1", Some("new"), 10).unwrap()),
            ["old"]
        );
        // And the limit keeps the newest, which is where the useful detail is.
        assert_eq!(
            ids(unreported_for_conversation(&mut conn, "c1", None, 1).unwrap()),
            ["new"]
        );

        let all: Vec<String> = list_for_conversation(&mut conn, "c1")
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(all, vec!["old", "new"]);
    }

    /// Only a turn that never reached an ending can owe an explanation. The
    /// other three said how they ended, in the transcript, where the model can
    /// already see it.
    #[test]
    fn a_turn_that_reached_an_ending_owes_nothing() {
        let pool = test_db();
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
        begin(&mut conn, "cut-off", "c1", TurnOrigin::Desktop, None, 2000).unwrap();
        begin(&mut conn, "reconciled", "c1", TurnOrigin::Desktop, None, 3000).unwrap();
        reconcile_interrupted(&mut conn, 3500).unwrap();

        assert_eq!(
            ids(unreported_for_conversation(&mut conn, "c1", None, 10).unwrap()),
            ["reconciled", "cut-off"]
        );
    }

    /// The record of having been told, which is what stops the same warning
    /// from being repeated forever — and what stops it from being lost when the
    /// turn that read it dies on the way to the provider.
    #[test]
    fn a_reported_turn_leaves_the_queue_and_keeps_its_first_telling() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conv(&mut conn, "c1");
        begin(&mut conn, "t1", "c1", TurnOrigin::Desktop, None, 1000).unwrap();
        begin(&mut conn, "t2", "c1", TurnOrigin::Desktop, None, 2000).unwrap();

        assert_eq!(
            mark_reported(&mut conn, &["t1".to_string()], Ledger::Own, 5000).unwrap(),
            1
        );
        assert_eq!(
            ids(unreported_for_conversation(&mut conn, "c1", None, 10).unwrap()),
            ["t2"]
        );

        // Told once. A second telling finds nothing to record, and the first
        // timestamp stands.
        assert_eq!(
            mark_reported(&mut conn, &["t1".to_string()], Ledger::Own, 9000).unwrap(),
            0
        );
        assert_eq!(get(&mut conn, "t1").reported_at, Some(5000));
        assert_eq!(mark_reported(&mut conn, &[], Ledger::Own, 9000).unwrap(), 0);

        // Reconciliation is about how a turn ended, and does not un-tell it.
        reconcile_interrupted(&mut conn, 9500).unwrap();
        assert_eq!(get(&mut conn, "t1").reported_at, Some(5000));
        assert_eq!(get(&mut conn, "t1").status().unwrap(), TurnStatus::Interrupted);
    }

    /// Turns belong to their conversation and go with it.
    #[test]
    fn deleting_a_conversation_takes_its_turns() {
        let pool = test_db();
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
        let pool = test_db();
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
        let pool = test_db();
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
        let pool = test_db();
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

    /// A turn refused by its provider can start and finish inside the same
    /// millisecond, so `started_at` alone does not say which of two turns came
    /// last — and a uuid primary key cannot break the tie, because its order
    /// has nothing to do with when it was written. Insertion order does.
    #[test]
    fn turns_from_the_same_millisecond_still_have_an_order() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conv(&mut conn, "c1");
        // Ids chosen so that ordering by `id` would put them the wrong way
        // round: "aaa" sorts before "zzz" but was written second.
        begin(&mut conn, "zzz-first", "c1", TurnOrigin::Desktop, None, 1000).unwrap();
        begin(&mut conn, "aaa-second", "c1", TurnOrigin::Desktop, None, 1000).unwrap();

        assert_eq!(
            ids(unreported_for_conversation(&mut conn, "c1", None, 10).unwrap()),
            ["aaa-second", "zzz-first"],
            "the latest turn is the one written last, not the one sorting last",
        );
        let all: Vec<String> = list_for_conversation(&mut conn, "c1")
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(all, vec!["zzz-first", "aaa-second"]);
    }

    /// The desktop's turn ids arrive from the front end, so a replayed one is
    /// reachable without anything being malicious — a retry, a double
    /// dispatch. It must not be able to reopen a turn that has already ended:
    /// the insert conflicts, but everything after it is an update by primary
    /// key and would rewrite that turn's ending while the new turn's messages
    /// filed themselves under it.
    #[test]
    fn a_second_turn_cannot_claim_an_id_that_is_already_on_record() {
        let pool = test_db();
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
        let pool = test_db();
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
        let pool = test_db();
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

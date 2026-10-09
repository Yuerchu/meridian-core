//! The durable half of a turn, on SeaORM.
//!
//! A turn runs on the stack of the task driving it; these rows are what
//! remains when the process does not. A turn says what it is about to do
//! *before* doing it, so whatever the last phase says is where it died, and
//! nothing is written from a destructor: a row left at `running` is itself the
//! record that the turn never reached its own ending, and
//! `reconcile_interrupted` says so at the next launch.
//!
//! Every write takes the caller's `WriteTx`.

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Select};

use crate::db::entity::turn::{TurnPhase, TurnStatus};
use crate::db::entity::{conversation, turn};
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, Snapshot, WriteTx};
use crate::db::types::EpochMs;
use crate::turn::{TurnOrigin, TurnTrigger};

/// Record a turn that is starting, set going by a person. Called once the
/// conversation has actually been taken, so a refused turn leaves nothing.
/// `self_id` names the bot account for a turn a bot started.
pub async fn begin(
    tx: &WriteTx,
    id: &str,
    conversation_id: &str,
    origin: TurnOrigin,
    self_id: Option<i64>,
    now: EpochMs,
) -> Result<(), DbErr> {
    begin_triggered(tx, id, conversation_id, origin, self_id, (TurnTrigger::User, None), now).await
}

/// [`begin`], for a turn that says what set it going.
pub async fn begin_triggered(
    tx: &WriteTx,
    id: &str,
    conversation_id: &str,
    origin: TurnOrigin,
    self_id: Option<i64>,
    (trigger, trigger_ref): (TurnTrigger, Option<&str>),
    now: EpochMs,
) -> Result<(), DbErr> {
    turn::Entity::insert(turn::ActiveModel {
        id: Set(id.to_owned()),
        conversation_id: Set(conversation_id.to_owned()),
        origin: Set(origin),
        status: Set(TurnStatus::Running),
        phase: Set(Some(TurnPhase::Streaming)),
        phase_tool: Set(None),
        error: Set(None),
        started_at: Set(now),
        updated_at: Set(now),
        ended_at: Set(None),
        reported_at: Set(None),
        parent_reported_at: Set(None),
        self_id: Set(self_id),
        trigger: Set(trigger),
        trigger_ref: Set(trigger_ref.map(str::to_owned)),
    })
    .exec_without_returning(tx.conn()?)
    .await?;
    Ok(())
}

/// Writes `row`'s set columns to the turn `id` while it is still running, and
/// says how many rows that touched. Every lifecycle write goes through here,
/// so the lifecycle is one-way at the SQL level: a phase write cannot land
/// after the turn ended, and a second `finish` cannot overwrite a terminal
/// state with a later opinion.
async fn update_running(tx: &WriteTx, id: &str, row: turn::ActiveModel) -> Result<u64, DbErr> {
    Ok(turn::Entity::update_many()
        .set(row)
        .filter(turn::Column::Id.eq(id))
        .filter(turn::Column::Status.eq(TurnStatus::Running))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

/// Say what the turn is about to do. Must be committed *before* the thing it
/// names. `tool` names the call `phase` refers to, and is cleared when it does
/// not.
pub async fn set_phase(
    tx: &WriteTx,
    id: &str,
    phase: TurnPhase,
    tool: Option<&str>,
    now: EpochMs,
) -> Result<u64, DbErr> {
    let row = turn::ActiveModel {
        phase: Set(Some(phase)),
        phase_tool: Set(tool.map(str::to_owned)),
        updated_at: Set(now),
        ..Default::default()
    };
    update_running(tx, id, row).await
}

/// Release a turn at the durable human-review boundary. `ended_at` stays NULL
/// because the tool call has not received its decision yet.
pub async fn wait_for_review(tx: &WriteTx, id: &str, now: EpochMs) -> Result<u64, DbErr> {
    let row = turn::ActiveModel {
        status: Set(TurnStatus::WaitingReview),
        phase: Set(Some(TurnPhase::AwaitingApproval)),
        phase_tool: Set(Some(crate::agent::modes::EXIT_PLAN_TOOL.to_owned())),
        updated_at: Set(now),
        ..Default::default()
    };
    update_running(tx, id, row).await
}

/// Settle a durable review boundary after its transcript tool result has been
/// committed, in the same write.
pub async fn finish_waiting_review(
    tx: &WriteTx,
    id: &str,
    status: TurnStatus,
    error: Option<&str>,
    now: EpochMs,
) -> Result<u64, DbErr> {
    assert!(
        matches!(status, TurnStatus::Done | TurnStatus::Cancelled | TurnStatus::Failed),
        "a waiting review may only move to a terminal status"
    );
    Ok(turn::Entity::update_many()
        .set(turn::ActiveModel {
            status: Set(status),
            error: Set(error.map(str::to_owned)),
            ended_at: Set(Some(now)),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(turn::Column::Id.eq(id))
        .filter(turn::Column::Status.eq(TurnStatus::WaitingReview))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

/// Close a turn out. Only the paths that actually reach an ending call this;
/// everything else is left for `reconcile_interrupted`.
pub async fn finish(
    tx: &WriteTx,
    id: &str,
    status: TurnStatus,
    error: Option<&str>,
    now: EpochMs,
) -> Result<u64, DbErr> {
    let row = turn::ActiveModel {
        status: Set(status),
        error: Set(error.map(str::to_owned)),
        ended_at: Set(Some(now)),
        updated_at: Set(now),
        ..Default::default()
    };
    update_running(tx, id, row).await
}

/// Which ledger records that a turn has been described.
///
/// A delegated run has two audiences — its own conversation, which the user can
/// open and read, and the one that spawned it — and one column cannot serve
/// both. Two columns rather than a `(turn, recipient)` table because depth is
/// one by construction: a sub-agent is handed no way to delegate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ledger {
    /// `turns.reported_at`: the conversation the turn ran in has been told.
    Own,
    /// `turns.parent_reported_at`: the conversation that delegated it has.
    Parent,
}

/// A turn that may still owe an explanation, and who it owes it to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptedCandidate {
    pub turn: turn::Model,
    pub ledger: Ledger,
    /// The sub-agent's title — the description the parent gave when it
    /// delegated — for a turn owed to the parent.
    pub child_title: Option<String>,
}

/// Tie-break for turns that started in the same millisecond: SQLite's rowid,
/// assigned on insert. A turn refused by its provider can begin and end
/// inside a millisecond, and the id is a uuid whose order means nothing.
// backend: sqlite-only — `rowid`; PostgreSQL needs an insertion sequence column.
fn by_insertion(select: Select<turn::Entity>, newest_first: bool) -> Select<turn::Entity> {
    if newest_first {
        select
            .order_by_desc(turn::Column::StartedAt)
            .order_by_desc(Expr::cust("rowid"))
    } else {
        select
            .order_by_asc(turn::Column::StartedAt)
            .order_by_asc(Expr::cust("rowid"))
    }
}

/// Turns of a conversation that may still owe the model an explanation, newest
/// first, capped at `limit`: its own `running` or `interrupted` turns whose
/// telling is not recorded, and the delegated runs of its sub-agents not yet
/// told to it. `excluding` is the turn asking, which has already opened its
/// own record. Several statements, so it takes a snapshot.
pub async fn unreported_for_conversation(
    db: &impl Snapshot,
    conversation_id: &str,
    excluding: Option<&str>,
    limit: u64,
) -> Result<Vec<InterruptedCandidate>, DbErr> {
    let dead = [TurnStatus::Running, TurnStatus::Interrupted];
    let own = turn::Entity::find()
        .filter(turn::Column::ConversationId.eq(conversation_id))
        .filter(turn::Column::Id.ne(excluding.unwrap_or("")))
        .filter(turn::Column::ReportedAt.is_null())
        .filter(turn::Column::Status.is_in(dead));
    let mut out: Vec<InterruptedCandidate> = by_insertion(own, true)
        .limit(limit)
        .all(db.conn()?)
        .await?
        .into_iter()
        .map(|turn| InterruptedCandidate {
            turn,
            ledger: Ledger::Own,
            child_title: None,
        })
        .collect();

    let children: Vec<(String, Option<String>)> = conversation::Entity::find()
        .filter(conversation::Column::ParentConversationId.eq(conversation_id))
        .select_only()
        .column(conversation::Column::Id)
        .column(conversation::Column::Title)
        .into_tuple()
        .all(db.conn()?)
        .await?;
    if !children.is_empty() {
        // Only the delegated run itself: a follow-up the user typed into the
        // sub-agent's transcript is between them and that conversation.
        let delegated = turn::Entity::find()
            .filter(turn::Column::ConversationId.is_in(children.iter().map(|(id, _)| id.as_str())))
            .filter(turn::Column::Origin.eq(TurnOrigin::SubAgent))
            .filter(turn::Column::ParentReportedAt.is_null())
            .filter(turn::Column::Status.is_in(dead));
        let delegated = by_insertion(delegated, true).limit(limit).all(db.conn()?).await?;
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

    // Both halves arrive newest first; a stable sort keeps that and settles a
    // shared millisecond in favour of the conversation's own turn.
    out.sort_by_key(|x| std::cmp::Reverse(x.turn.started_at));
    out.truncate(limit as usize);
    Ok(out)
}

/// Record that these turns have now been described to the model. The
/// `IS NULL` filter keeps the first telling as the recorded one; `updated_at`
/// is left alone, because being talked about is not the turn doing something.
pub async fn mark_reported(tx: &WriteTx, ids: &[String], ledger: Ledger, now: EpochMs) -> Result<u64, DbErr> {
    if ids.is_empty() {
        return Ok(0);
    }
    let (column, row) = match ledger {
        Ledger::Own => (
            turn::Column::ReportedAt,
            turn::ActiveModel {
                reported_at: Set(Some(now)),
                ..Default::default()
            },
        ),
        Ledger::Parent => (
            turn::Column::ParentReportedAt,
            turn::ActiveModel {
                parent_reported_at: Set(Some(now)),
                ..Default::default()
            },
        ),
    };
    Ok(turn::Entity::update_many()
        .set(row)
        .filter(turn::Column::Id.is_in(ids))
        .filter(column.is_null())
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

/// Every turn of a conversation, oldest first.
pub async fn list_for_conversation(db: &impl Read, conversation_id: &str) -> Result<Vec<turn::Model>, DbErr> {
    by_insertion(
        turn::Entity::find().filter(turn::Column::ConversationId.eq(conversation_id)),
        false,
    )
    .all(db.conn()?)
    .await
}

/// One turn's record; `None` for an id with no row, which callers treat like
/// a turn that did not reach an ending.
pub async fn get(db: &impl Read, turn_id: &str) -> Result<Option<turn::Model>, DbErr> {
    turn::Entity::find_by_id(turn_id).one(db.conn()?).await
}

/// Mark every turn still recorded as running as interrupted, hold the queues
/// of the conversations they belonged to, and say how many turns there were.
///
/// Sound because a turn only ever runs inside the process that wrote its row,
/// so a `running` row seen at startup was killed. The queue half is the same
/// fact written in the other place it has to be: a turn that reached no ending
/// holds everything queued behind it, and a crash is the purest such turn.
pub async fn reconcile_interrupted(tx: &WriteTx, now: EpochMs) -> Result<u64, DbErr> {
    // Read before the update: after it nothing tells these conversations apart.
    let stranded: Vec<String> = turn::Entity::find()
        .filter(turn::Column::Status.eq(TurnStatus::Running))
        .select_only()
        .column(turn::Column::ConversationId)
        .distinct()
        .into_tuple()
        .all(tx.conn()?)
        .await?;
    let interrupted = turn::Entity::update_many()
        .col_expr(turn::Column::Status, Expr::value(TurnStatus::Interrupted))
        .col_expr(turn::Column::EndedAt, Expr::value(now))
        .col_expr(turn::Column::UpdatedAt, Expr::value(now))
        .filter(turn::Column::Status.eq(TurnStatus::Running))
        .exec(tx.conn()?)
        .await?
        .rows_affected;
    let mut held = 0;
    for conversation_id in &stranded {
        held += crate::db::sea::ops::queue::hold_all(tx, conversation_id, now).await?;
    }
    if held > 0 {
        tracing::info!(
            items = held,
            conversations = stranded.len(),
            "held queued prompts whose turn was cut off",
        );
    }
    Ok(interrupted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::entity::queued_prompt::{Delivery, QueueState};
    use crate::db::sea::cap::Db;
    use crate::db::sea::ops::queue;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    async fn with_conversations(ids: &[&str]) -> Db {
        let db = sea_test_db().await;
        for id in ids {
            execute_for_tests(
                &db,
                &format!(
                    "INSERT INTO conversations (id, title, created_at, updated_at) VALUES ('{id}', 't', 1000, 1000)"
                ),
            )
            .await
            .unwrap();
        }
        db
    }

    async fn start(db: &Db, id: &str, conversation: &str, origin: TurnOrigin, now: EpochMs) -> Result<(), DbErr> {
        db.write(async |tx| begin(tx, id, conversation, origin, None, now).await)
            .await
    }

    async fn turn_row(db: &Db, id: &str) -> turn::Model {
        get(db, id).await.unwrap().unwrap()
    }

    async fn finish_as(db: &Db, id: &str, status: TurnStatus, error: Option<&str>, now: EpochMs) -> u64 {
        db.write(async |tx| finish(tx, id, status, error, now).await)
            .await
            .unwrap()
    }

    async fn phase(db: &Db, id: &str, phase: TurnPhase, tool: Option<&str>, now: EpochMs) -> u64 {
        db.write(async |tx| set_phase(tx, id, phase, tool, now).await)
            .await
            .unwrap()
    }

    async fn reconcile(db: &Db, now: EpochMs) -> u64 {
        db.write(async |tx| reconcile_interrupted(tx, now).await).await.unwrap()
    }

    async fn unreported(db: &Db, conversation: &str, excluding: Option<&str>, limit: u64) -> Vec<String> {
        db.read(async |tx| unreported_for_conversation(tx, conversation, excluding, limit).await)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.turn.id)
            .collect()
    }

    async fn listed(db: &Db, conversation: &str) -> Vec<String> {
        list_for_conversation(db, conversation)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect()
    }

    /// A turn says what set it going, and a person's turn says so without
    /// being asked: every caller of `begin` is one.
    #[tokio::test]
    async fn a_turn_records_what_set_it_going() {
        let db = with_conversations(&["c1"]).await;
        start(&db, "asked", "c1", TurnOrigin::Desktop, 1000).await.unwrap();
        db.write(async |tx| {
            begin_triggered(
                tx,
                "woken",
                "c1",
                TurnOrigin::Desktop,
                None,
                (TurnTrigger::TaskCompletion, Some("task-1")),
                1001,
            )
            .await
        })
        .await
        .unwrap();

        assert_eq!(turn_row(&db, "asked").await.trigger, TurnTrigger::User);
        let woken = turn_row(&db, "woken").await;
        assert_eq!(
            (woken.trigger, woken.trigger_ref.as_deref()),
            (TurnTrigger::TaskCompletion, Some("task-1"))
        );
    }

    #[tokio::test]
    async fn a_turn_starts_running_and_streaming() {
        let db = with_conversations(&["c1"]).await;
        start(&db, "t1", "c1", TurnOrigin::Desktop, 1000).await.unwrap();
        let t = turn_row(&db, "t1").await;
        assert_eq!(
            (t.status, t.phase, t.origin, t.ended_at),
            (
                TurnStatus::Running,
                Some(TurnPhase::Streaming),
                TurnOrigin::Desktop,
                None
            )
        );
    }

    /// The phase is the diagnosis, so it has to track what the turn is really
    /// doing — including going back to streaming once a tool returns.
    #[tokio::test]
    async fn the_phase_follows_the_turn() {
        let db = with_conversations(&["c1"]).await;
        start(&db, "t1", "c1", TurnOrigin::Desktop, 1000).await.unwrap();

        phase(&db, "t1", TurnPhase::AwaitingApproval, Some("run_command"), 1001).await;
        let t = turn_row(&db, "t1").await;
        assert_eq!(
            (t.phase, t.phase_tool.as_deref()),
            (Some(TurnPhase::AwaitingApproval), Some("run_command"))
        );
        phase(&db, "t1", TurnPhase::RunningTool, Some("run_command"), 1002).await;
        assert_eq!(turn_row(&db, "t1").await.phase, Some(TurnPhase::RunningTool));

        phase(&db, "t1", TurnPhase::Streaming, None, 1003).await;
        let t = turn_row(&db, "t1").await;
        assert_eq!(t.phase, Some(TurnPhase::Streaming));
        assert_eq!(
            t.phase_tool, None,
            "a phase that names no tool must not keep the last one"
        );
    }

    /// A killed turn's row is turned into a diagnosis at the next launch
    /// without losing the phase it died in.
    #[tokio::test]
    async fn a_turn_left_running_is_interrupted_at_the_next_launch() {
        let db = with_conversations(&["c1"]).await;
        start(&db, "t1", "c1", TurnOrigin::Desktop, 1000).await.unwrap();
        phase(&db, "t1", TurnPhase::RunningTool, Some("edit_file"), 1001).await;

        assert_eq!(reconcile(&db, 2000).await, 1);
        let t = turn_row(&db, "t1").await;
        assert_eq!(
            (t.status, t.ended_at, t.phase, t.phase_tool.as_deref()),
            (
                TurnStatus::Interrupted,
                Some(2000),
                Some(TurnPhase::RunningTool),
                Some("edit_file")
            ),
            "the phase is the diagnosis; reconciliation must not erase it"
        );
    }

    /// A killed turn holds whatever was queued behind it, and only in its own
    /// conversation.
    #[tokio::test]
    async fn a_killed_turn_holds_the_queue_that_was_waiting_on_it() {
        let db = with_conversations(&["crashed", "idle"]).await;
        start(&db, "t1", "crashed", TurnOrigin::Desktop, 1000).await.unwrap();
        db.write(async |tx| {
            queue::enqueue(tx, "q1", "crashed", "now rename it", Delivery::FollowUp, 1).await?;
            queue::enqueue(tx, "q2", "idle", "unrelated", Delivery::FollowUp, 1).await
        })
        .await
        .unwrap();

        assert_eq!(reconcile(&db, 2000).await, 1);
        assert_eq!(queue::list(&db, "crashed").await.unwrap()[0].state(), QueueState::Held);
        assert_eq!(
            queue::next_pending(&db, "crashed").await.unwrap(),
            None,
            "and so it waits for a person rather than running on a dead premise"
        );
        assert_eq!(
            queue::list(&db, "idle").await.unwrap()[0].state(),
            QueueState::Queued,
            "an untouched conversation's queue is not collateral"
        );
    }

    #[tokio::test]
    async fn reconciliation_leaves_finished_turns_alone() {
        let db = with_conversations(&["c1"]).await;
        let ended = [
            ("done", TurnStatus::Done),
            ("cancelled", TurnStatus::Cancelled),
            ("failed", TurnStatus::Failed),
        ];
        for (id, status) in ended {
            start(&db, id, "c1", TurnOrigin::Desktop, 1000).await.unwrap();
            finish_as(&db, id, status, None, 1500).await;
        }
        start(&db, "live", "c1", TurnOrigin::OneBot, 1000).await.unwrap();

        assert_eq!(reconcile(&db, 2000).await, 1);
        for (id, status) in ended {
            assert_eq!(turn_row(&db, id).await.status, status);
        }
        assert_eq!(turn_row(&db, "live").await.status, TurnStatus::Interrupted);
        assert_eq!(reconcile(&db, 2001).await, 0, "nothing to do the second time");
    }

    #[tokio::test]
    async fn a_failed_turn_keeps_what_went_wrong() {
        let db = with_conversations(&["c1"]).await;
        start(&db, "t1", "c1", TurnOrigin::Desktop, 1000).await.unwrap();
        finish_as(&db, "t1", TurnStatus::Failed, Some("API Key not set"), 1500).await;
        let t = turn_row(&db, "t1").await;
        assert_eq!(
            (t.status, t.error.as_deref()),
            (TurnStatus::Failed, Some("API Key not set"))
        );
    }

    #[tokio::test]
    async fn unreported_turns_come_back_newest_first_within_their_conversation() {
        let db = with_conversations(&["c1", "c2"]).await;
        start(&db, "old", "c1", TurnOrigin::Desktop, 1000).await.unwrap();
        start(&db, "new", "c1", TurnOrigin::Desktop, 2000).await.unwrap();
        start(&db, "other", "c2", TurnOrigin::Desktop, 3000).await.unwrap();

        assert_eq!(unreported(&db, "c1", None, 10).await, ["new", "old"]);
        assert_eq!(unreported(&db, "c2", None, 10).await, ["other"]);
        assert!(unreported(&db, "nope", None, 10).await.is_empty());
        assert_eq!(
            unreported(&db, "c1", Some("new"), 10).await,
            ["old"],
            "the turn asking is never an answer"
        );
        assert_eq!(
            unreported(&db, "c1", None, 1).await,
            ["new"],
            "the limit keeps the newest"
        );
        assert_eq!(listed(&db, "c1").await, ["old", "new"]);
    }

    /// Only a turn that never reached an ending can owe an explanation.
    #[tokio::test]
    async fn a_turn_that_reached_an_ending_owes_nothing() {
        let db = with_conversations(&["c1"]).await;
        for (id, status) in [
            ("done", TurnStatus::Done),
            ("cancelled", TurnStatus::Cancelled),
            ("failed", TurnStatus::Failed),
        ] {
            start(&db, id, "c1", TurnOrigin::Desktop, 1000).await.unwrap();
            finish_as(&db, id, status, None, 1500).await;
        }
        start(&db, "cut-off", "c1", TurnOrigin::Desktop, 2000).await.unwrap();
        start(&db, "reconciled", "c1", TurnOrigin::Desktop, 3000).await.unwrap();
        reconcile(&db, 3500).await;
        assert_eq!(unreported(&db, "c1", None, 10).await, ["reconciled", "cut-off"]);
    }

    /// A sub-agent's interrupted run is owed to the parent on its own ledger,
    /// with the child's title; a follow-up typed into the child is not.
    #[tokio::test]
    async fn a_delegated_run_is_owed_to_its_parent_on_the_parent_ledger() {
        let db = with_conversations(&["parent"]).await;
        execute_for_tests(
            &db,
            "INSERT INTO conversations (id, title, parent_conversation_id, created_at, updated_at)
                 VALUES ('child', 'Look into the build', 'parent', 1000, 1000)",
        )
        .await
        .unwrap();
        start(&db, "run", "child", TurnOrigin::SubAgent, 1000).await.unwrap();
        start(&db, "typed", "child", TurnOrigin::Desktop, 2000).await.unwrap();
        start(&db, "own", "parent", TurnOrigin::Desktop, 1500).await.unwrap();

        let owed = db
            .read(async |tx| unreported_for_conversation(tx, "parent", None, 10).await)
            .await
            .unwrap();
        let shape: Vec<_> = owed
            .iter()
            .map(|c| (c.turn.id.as_str(), c.ledger, c.child_title.as_deref()))
            .collect();
        assert_eq!(
            shape,
            [
                ("own", Ledger::Own, None),
                ("run", Ledger::Parent, Some("Look into the build"))
            ]
        );

        // Telling the child does not tell the parent.
        db.write(async |tx| mark_reported(tx, &["run".to_string()], Ledger::Own, 3000).await)
            .await
            .unwrap();
        assert_eq!(unreported(&db, "parent", None, 10).await, ["own", "run"]);
        db.write(async |tx| mark_reported(tx, &["run".to_string()], Ledger::Parent, 3001).await)
            .await
            .unwrap();
        assert_eq!(unreported(&db, "parent", None, 10).await, ["own"]);
    }

    /// The record of having been told, which keeps its first telling.
    #[tokio::test]
    async fn a_reported_turn_leaves_the_queue_and_keeps_its_first_telling() {
        let db = with_conversations(&["c1"]).await;
        start(&db, "t1", "c1", TurnOrigin::Desktop, 1000).await.unwrap();
        start(&db, "t2", "c1", TurnOrigin::Desktop, 2000).await.unwrap();
        let tell = |ids: Vec<String>, now| {
            let db = db.clone();
            async move {
                db.write(async |tx| mark_reported(tx, &ids, Ledger::Own, now).await)
                    .await
                    .unwrap()
            }
        };

        assert_eq!(tell(vec!["t1".into()], 5000).await, 1);
        assert_eq!(unreported(&db, "c1", None, 10).await, ["t2"]);
        assert_eq!(tell(vec!["t1".into()], 9000).await, 0);
        assert_eq!(turn_row(&db, "t1").await.reported_at, Some(5000));
        assert_eq!(tell(vec![], 9000).await, 0);

        // Reconciliation is about how a turn ended, and does not un-tell it.
        reconcile(&db, 9500).await;
        let t1 = turn_row(&db, "t1").await;
        assert_eq!((t1.reported_at, t1.status), (Some(5000), TurnStatus::Interrupted));
    }

    #[tokio::test]
    async fn deleting_a_conversation_takes_its_turns() {
        let db = with_conversations(&["c1"]).await;
        start(&db, "t1", "c1", TurnOrigin::Desktop, 1000).await.unwrap();
        execute_for_tests(&db, "DELETE FROM conversations WHERE id = 'c1'")
            .await
            .unwrap();
        assert!(listed(&db, "c1").await.is_empty());
    }

    /// The lifecycle is one-way, enforced in SQL rather than by call order.
    #[tokio::test]
    async fn a_finished_turn_no_longer_moves() {
        let db = with_conversations(&["c1"]).await;
        start(&db, "t1", "c1", TurnOrigin::Desktop, 1000).await.unwrap();
        finish_as(&db, "t1", TurnStatus::Done, None, 1500).await;
        let ended = turn_row(&db, "t1").await;

        assert_eq!(
            phase(&db, "t1", TurnPhase::RunningTool, Some("edit_file"), 2000).await,
            0,
            "a late phase write must find nothing to update"
        );
        assert_eq!(turn_row(&db, "t1").await, ended);
    }

    #[tokio::test]
    async fn a_turn_cannot_end_twice() {
        let db = with_conversations(&["c1"]).await;
        start(&db, "t1", "c1", TurnOrigin::Desktop, 1000).await.unwrap();
        finish_as(&db, "t1", TurnStatus::Done, None, 1500).await;
        assert_eq!(
            finish_as(&db, "t1", TurnStatus::Failed, Some("too late"), 2000).await,
            0
        );
        let t = turn_row(&db, "t1").await;
        assert_eq!((t.status, t.ended_at, t.error), (TurnStatus::Done, Some(1500), None));
    }

    /// A durable review boundary parks the turn, and only a terminal status
    /// releases it.
    #[tokio::test]
    async fn a_turn_waiting_for_review_is_settled_once() {
        let db = with_conversations(&["c1"]).await;
        start(&db, "t1", "c1", TurnOrigin::Desktop, 1000).await.unwrap();
        assert_eq!(
            db.write(async |tx| wait_for_review(tx, "t1", 1100).await)
                .await
                .unwrap(),
            1
        );
        let waiting = turn_row(&db, "t1").await;
        assert_eq!(
            (
                waiting.status,
                waiting.phase,
                waiting.phase_tool.as_deref(),
                waiting.ended_at
            ),
            (
                TurnStatus::WaitingReview,
                Some(TurnPhase::AwaitingApproval),
                Some(crate::agent::modes::EXIT_PLAN_TOOL),
                None
            )
        );
        assert_eq!(
            reconcile(&db, 1150).await,
            0,
            "a durable boundary is not an interruption"
        );
        assert_eq!(
            finish_as(&db, "t1", TurnStatus::Done, None, 1200).await,
            0,
            "finish needs a running turn"
        );

        let settle = |now| {
            let db = db.clone();
            async move {
                db.write(async |tx| finish_waiting_review(tx, "t1", TurnStatus::Done, None, now).await)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(settle(1300).await, 1);
        assert_eq!(settle(1400).await, 0);
        let t = turn_row(&db, "t1").await;
        assert_eq!((t.status, t.ended_at), (TurnStatus::Done, Some(1300)));
    }

    /// Two turns in one millisecond are told apart by insertion order, not by
    /// id: "aaa" sorts before "zzz" but was written second.
    #[tokio::test]
    async fn turns_from_the_same_millisecond_still_have_an_order() {
        let db = with_conversations(&["c1"]).await;
        start(&db, "zzz-first", "c1", TurnOrigin::Desktop, 1000).await.unwrap();
        start(&db, "aaa-second", "c1", TurnOrigin::Desktop, 1000).await.unwrap();
        assert_eq!(unreported(&db, "c1", None, 10).await, ["aaa-second", "zzz-first"]);
        assert_eq!(listed(&db, "c1").await, ["zzz-first", "aaa-second"]);
    }

    /// A replayed id is refused by the database rather than merged into the
    /// turn already on record.
    #[tokio::test]
    async fn a_second_turn_cannot_claim_an_id_that_is_already_on_record() {
        let db = with_conversations(&["c1"]).await;
        start(&db, "t1", "c1", TurnOrigin::Desktop, 1000).await.unwrap();
        finish_as(&db, "t1", TurnStatus::Done, None, 1500).await;
        let ended = turn_row(&db, "t1").await;

        assert!(start(&db, "t1", "c1", TurnOrigin::Desktop, 2000).await.is_err());
        assert_eq!(turn_row(&db, "t1").await, ended);
        assert_eq!(listed(&db, "c1").await.len(), 1);
    }

    /// A status or phase this build does not know fails the read rather than
    /// being reinterpreted, and an unknown status is not swept up as running.
    #[tokio::test]
    async fn an_unknown_status_or_phase_fails_the_read() {
        let db = with_conversations(&["c1"]).await;
        start(&db, "t1", "c1", TurnOrigin::Desktop, 1000).await.unwrap();
        execute_for_tests(&db, "UPDATE turns SET status = 'from_the_future' WHERE id = 't1'")
            .await
            .unwrap();
        let error = get(&db, "t1").await.unwrap_err().to_string();
        assert!(error.contains("unknown turn status 'from_the_future'"), "{error}");
        assert_eq!(reconcile(&db, 2000).await, 0);

        execute_for_tests(
            &db,
            "UPDATE turns SET status = 'running', phase = 'from_the_future' WHERE id = 't1'",
        )
        .await
        .unwrap();
        let error = get(&db, "t1").await.unwrap_err().to_string();
        assert!(error.contains("unknown turn phase 'from_the_future'"), "{error}");
    }

    /// The column takes only the words the enum has.
    #[tokio::test]
    async fn an_unknown_trigger_is_refused_by_the_table() {
        let db = with_conversations(&["c1"]).await;
        let refused = execute_for_tests(
            &db,
            "INSERT INTO turns (id, conversation_id, origin, status, started_at, updated_at, trigger)
                 VALUES ('t', 'c1', 'desktop', 'running', 1, 1, 'whenever')",
        )
        .await;
        assert!(refused.is_err());
    }

    /// A turn cut short by the loop guard did not complete, and its record must
    /// not say it did — the stop event on the same turn says `loop_detected`.
    #[tokio::test]
    async fn a_turn_the_loop_guard_stopped_is_not_recorded_as_done() {
        let db = crate::db::sea::sea_test_db().await;
        db.write(async |tx| {
            crate::db::sea::ops::conversation::create_conversation(tx, "c1", None, None, None, 1).await?;
            begin(tx, "t1", "c1", TurnOrigin::Desktop, None, 1000).await?;
            finish(
                tx,
                "t1",
                TurnStatus::Failed,
                Some(crate::db::models::turn::ERROR_LOOP_DETECTED),
                1500,
            )
            .await
        })
        .await
        .unwrap();
        let t = get(&db, "t1").await.unwrap().unwrap();
        assert_eq!(t.status, TurnStatus::Failed);
        assert_eq!(t.error.as_deref(), Some("loop_detected"));
    }
}

//! Reading and writing background tasks. See `crate::background`.
//!
//! No function here opens a transaction: a write takes the caller's `WriteTx`,
//! which is the `BEGIN IMMEDIATE` that makes a count and the insert it guards,
//! or a claim and the row it writes, one step.

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, DbErr, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, Set};

use crate::db::entity::background_task::{self, BackgroundRunner, BackgroundState};
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::types::EpochMs;

/// A task about to start.
pub struct BackgroundTaskInsert<'a> {
    pub id: &'a str,
    pub conversation_id: &'a str,
    pub runner: BackgroundRunner,
    pub kind: background_task::BackgroundKind,
    pub spawned_turn_id: Option<&'a str>,
    pub command: Option<&'a str>,
    pub description: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub sandbox: Option<&'a str>,
    pub output_path: Option<&'a str>,
    pub started_at: EpochMs,
}

/// How a task ended, written once.
pub struct BackgroundTaskChangeset<'a> {
    pub state: BackgroundState,
    pub exit_code: Option<i32>,
    pub ended_reason: Option<&'a str>,
    pub output_bytes: i64,
    pub output_truncated: bool,
    pub ended_at: EpochMs,
}

pub async fn insert(tx: &WriteTx, row: &BackgroundTaskInsert<'_>) -> Result<(), DbErr> {
    let model = background_task::ActiveModel {
        id: Set(row.id.to_string()),
        conversation_id: Set(row.conversation_id.to_string()),
        runner: Set(row.runner),
        external_id: Set(None),
        spawned_turn_id: Set(row.spawned_turn_id.map(str::to_string)),
        spawned_call_id: Set(None),
        kind: Set(row.kind),
        command: Set(row.command.map(str::to_string)),
        description: Set(row.description.map(str::to_string)),
        cwd: Set(row.cwd.map(str::to_string)),
        sandbox: Set(row.sandbox.map(str::to_string)),
        state: Set(BackgroundState::Running),
        exit_code: Set(None),
        ended_reason: Set(None),
        output_path: Set(row.output_path.map(str::to_string)),
        output_bytes: Set(0),
        output_truncated: Set(false.into()),
        started_at: Set(row.started_at),
        ended_at: Set(None),
        notified_at: Set(None),
        notified_turn_id: Set(None),
    };
    background_task::Entity::insert(model).exec(tx.conn()?).await?;
    Ok(())
}

pub async fn get(db: &impl Read, id: &str) -> Result<Option<background_task::Model>, DbErr> {
    background_task::Entity::find_by_id(id).one(db.conn()?).await
}

/// A conversation's tasks, oldest first.
pub async fn list_for_conversation(
    db: &impl Read,
    conversation_id: &str,
) -> Result<Vec<background_task::Model>, DbErr> {
    background_task::Entity::find()
        .filter(background_task::Column::ConversationId.eq(conversation_id))
        .order_by_asc(background_task::Column::StartedAt)
        .order_by_asc(background_task::Column::Id)
        .all(db.conn()?)
        .await
}

/// How many of a conversation's tasks are still running.
pub async fn count_running(db: &impl Read, conversation_id: &str) -> Result<u64, DbErr> {
    background_task::Entity::find()
        .filter(background_task::Column::ConversationId.eq(conversation_id))
        .filter(background_task::Column::State.eq(BackgroundState::Running))
        .count(db.conn()?)
        .await
}

/// Write how a task ended — once. A task that is no longer `running` has
/// already been given an ending (a stop, or the reconcile at startup) and this
/// answers 0 rather than overwriting it.
pub async fn finish(tx: &WriteTx, id: &str, ending: &BackgroundTaskChangeset<'_>) -> Result<u64, DbErr> {
    let done = background_task::Entity::update_many()
        .col_expr(background_task::Column::State, Expr::value(ending.state))
        .col_expr(background_task::Column::ExitCode, Expr::value(ending.exit_code))
        .col_expr(
            background_task::Column::EndedReason,
            Expr::value(ending.ended_reason.map(str::to_string)),
        )
        .col_expr(background_task::Column::OutputBytes, Expr::value(ending.output_bytes))
        .col_expr(
            background_task::Column::OutputTruncated,
            Expr::value(crate::db::types::SqlBool::from(ending.output_truncated)),
        )
        .col_expr(background_task::Column::EndedAt, Expr::value(ending.ended_at))
        .filter(background_task::Column::Id.eq(id))
        .filter(background_task::Column::State.eq(BackgroundState::Running))
        .exec(tx.conn()?)
        .await?;
    Ok(done.rows_affected)
}

/// Every native task a previous process was watching, marked `lost`.
///
/// Called once at startup, before anything can start a task of its own, so
/// every `running` native row found here belonged to a process that is gone.
/// Its command may well have finished; nobody saw how. A hosted session's tasks
/// are the adapter's to report and are left to it.
pub async fn reconcile_lost(tx: &WriteTx, now: EpochMs) -> Result<u64, DbErr> {
    let done = background_task::Entity::update_many()
        .col_expr(background_task::Column::State, Expr::value(BackgroundState::Lost))
        .col_expr(
            background_task::Column::EndedReason,
            Expr::value("Meridian stopped while it was running"),
        )
        .col_expr(background_task::Column::EndedAt, Expr::value(now))
        .filter(background_task::Column::Runner.eq(BackgroundRunner::Native))
        .filter(background_task::Column::State.eq(BackgroundState::Running))
        .exec(tx.conn()?)
        .await?;
    Ok(done.rows_affected)
}

/// Native tasks that ended and that the model has not been told about, oldest
/// ending first.
///
/// `waking` narrows it to the ones worth starting a turn for: a completion or
/// a failure this process saw. A stop was somebody's decision, and a task lost
/// to a restart ended with nobody there — both wait for the next turn rather
/// than starting one.
pub async fn unnotified(
    db: &impl Read,
    conversation_id: &str,
    waking: bool,
) -> Result<Vec<background_task::Model>, DbErr> {
    let mut query = background_task::Entity::find()
        .filter(background_task::Column::ConversationId.eq(conversation_id))
        .filter(background_task::Column::Runner.eq(BackgroundRunner::Native))
        .filter(background_task::Column::State.ne(BackgroundState::Running))
        .filter(background_task::Column::NotifiedAt.is_null());
    if waking {
        query =
            query.filter(background_task::Column::State.is_in([BackgroundState::Completed, BackgroundState::Failed]));
    }
    query
        .order_by_asc(background_task::Column::EndedAt)
        .order_by_asc(background_task::Column::Id)
        .all(db.conn()?)
        .await
}

/// Record that the model was told. The affected-row count is the claim: two
/// turns reading the same task both try, and only one gets a 1.
pub async fn mark_notified(tx: &WriteTx, id: &str, turn_id: Option<&str>, now: EpochMs) -> Result<u64, DbErr> {
    let done = background_task::Entity::update_many()
        .col_expr(background_task::Column::NotifiedAt, Expr::value(now))
        .col_expr(
            background_task::Column::NotifiedTurnId,
            Expr::value(turn_id.map(str::to_string)),
        )
        .filter(background_task::Column::Id.eq(id))
        .filter(background_task::Column::NotifiedAt.is_null())
        .exec(tx.conn()?)
        .await?;
    Ok(done.rows_affected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::entity::background_task::BackgroundKind;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    async fn db_with_conversation() -> crate::db::sea::Db {
        let db = sea_test_db().await;
        execute_for_tests(
            &db,
            "INSERT INTO conversations (id, title, is_pinned, is_archived, message_count, created_at, updated_at, fast_mode)
             VALUES ('c1', 't', 0, 0, 0, 0, 0, 0)",
        )
        .await
        .unwrap();
        db
    }

    async fn start(db: &crate::db::sea::Db, id: &str, runner: BackgroundRunner, at: i64) {
        db.write(async |tx| {
            insert(
                tx,
                &BackgroundTaskInsert {
                    id,
                    conversation_id: "c1",
                    runner,
                    kind: BackgroundKind::Command,
                    spawned_turn_id: None,
                    command: Some("sleep 1"),
                    description: None,
                    cwd: None,
                    sandbox: None,
                    output_path: None,
                    started_at: at,
                },
            )
            .await
        })
        .await
        .unwrap();
    }

    async fn end(db: &crate::db::sea::Db, id: &str, state: BackgroundState, at: i64) -> u64 {
        db.write(async |tx| {
            finish(
                tx,
                id,
                &BackgroundTaskChangeset {
                    state,
                    exit_code: Some(0),
                    ended_reason: None,
                    output_bytes: 0,
                    output_truncated: false,
                    ended_at: at,
                },
            )
            .await
        })
        .await
        .unwrap()
    }

    /// An ending is written once. The reconcile at startup and a stop both
    /// get there first sometimes, and the exit arriving after them must not
    /// relabel a stopped task as completed.
    #[tokio::test]
    async fn an_ending_is_written_once() {
        let db = db_with_conversation().await;
        start(&db, "t1", BackgroundRunner::Native, 1).await;
        assert_eq!(end(&db, "t1", BackgroundState::Stopped, 2).await, 1);
        assert_eq!(end(&db, "t1", BackgroundState::Completed, 3).await, 0);
        assert_eq!(get(&db, "t1").await.unwrap().unwrap().state, BackgroundState::Stopped);
    }

    /// Only native tasks are lost with this process; a hosted session's are
    /// the adapter's to report.
    #[tokio::test]
    async fn a_restart_loses_only_what_it_was_watching() {
        let db = db_with_conversation().await;
        start(&db, "n1", BackgroundRunner::Native, 1).await;
        start(&db, "h1", BackgroundRunner::ClaudeCode, 1).await;
        assert_eq!(db.write(async |tx| reconcile_lost(tx, 5).await).await.unwrap(), 1);
        assert_eq!(get(&db, "n1").await.unwrap().unwrap().state, BackgroundState::Lost);
        assert_eq!(get(&db, "h1").await.unwrap().unwrap().state, BackgroundState::Running);
    }

    /// What is owed and what is worth waking for are different lists, and a
    /// telling is claimed exactly once.
    #[tokio::test]
    async fn a_debt_is_listed_until_it_is_paid_once() {
        let db = db_with_conversation().await;
        start(&db, "done", BackgroundRunner::Native, 1).await;
        start(&db, "stopped", BackgroundRunner::Native, 2).await;
        start(&db, "still", BackgroundRunner::Native, 3).await;
        end(&db, "done", BackgroundState::Completed, 10).await;
        end(&db, "stopped", BackgroundState::Stopped, 11).await;

        let owed: Vec<_> = unnotified(&db, "c1", false)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(owed, ["done", "stopped"], "every ended task, oldest ending first");
        let waking: Vec<_> = unnotified(&db, "c1", true)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(waking, ["done"], "a stop is somebody's decision and wakes nothing");

        let first = db
            .write(async |tx| mark_notified(tx, "done", Some("turn-a"), 20).await)
            .await
            .unwrap();
        let second = db
            .write(async |tx| mark_notified(tx, "done", Some("turn-b"), 21).await)
            .await
            .unwrap();
        assert_eq!((first, second), (1, 0), "claimed once");
        assert!(unnotified(&db, "c1", true).await.unwrap().is_empty());
        assert_eq!(count_running(&db, "c1").await.unwrap(), 1);
    }
}

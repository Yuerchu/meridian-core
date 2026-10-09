//! Keeping the durable half of a turn up to date, from either runner.
//!
//! Thin wrappers over `db::sea::ops::turn`, here rather than in that module
//! because both loops need the same policy around them: which failures end a
//! turn (none but a replayed id) and which are logged and carried past.

use sea_orm::SqlErr;

use crate::db::models::turn::{TurnPhase, TurnStatus};
use crate::db::sea::cap::Db;
use crate::db::sea::ops::turn as turn_ops;
use crate::turn::TurnOrigin;
use crate::util::now_ms;

/// Record a turn that is starting.
///
/// `Err` means one thing only: a turn with this id is already on record. The
/// desktop's ids are minted in the front end, and a replayed one would not fail
/// loudly on its own — the insert conflicts, but every `set_phase` and `finish`
/// after it is an update by primary key and would land on the *finished* turn
/// instead, rewriting its ending while the new turn's messages filed themselves
/// under it. A turn cannot be run twice, so the second attempt is refused.
///
/// Every other failure is logged and swallowed. A turn that cannot be recorded
/// still runs: losing the record costs the diagnosis if this process is killed,
/// whereas refusing to answer costs the user the reply they are waiting for.
pub async fn begin(
    db: &Db,
    turn_id: &str,
    conversation_id: &str,
    origin: TurnOrigin,
    self_id: Option<i64>,
) -> Result<(), String> {
    begin_triggered(
        db,
        turn_id,
        conversation_id,
        origin,
        self_id,
        crate::turn::TurnTrigger::User,
        None,
    )
    .await
}

/// [`begin`], for a turn that says what set it going.
pub async fn begin_triggered(
    db: &Db,
    turn_id: &str,
    conversation_id: &str,
    origin: TurnOrigin,
    self_id: Option<i64>,
    trigger: crate::turn::TurnTrigger,
    trigger_ref: Option<&str>,
) -> Result<(), String> {
    let written = db
        .write(async |tx| {
            turn_ops::begin_triggered(
                tx,
                turn_id,
                conversation_id,
                origin,
                self_id,
                (trigger, trigger_ref),
                now_ms(),
            )
            .await
        })
        .await;
    match written {
        Ok(()) => Ok(()),
        Err(e) if matches!(e.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))) => {
            tracing::error!(turn_id, "refusing a turn whose id is already on record");
            Err("This turn has already been run.".into())
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not record the start of this turn");
            Ok(())
        }
    }
}

/// Write down what the turn is about to do, before it does it.
///
/// The ordering is the whole point: a phase recorded afterwards describes a
/// window that has already closed. What is stored when the process dies is the
/// diagnosis — `RunningTool` in particular means a tool had started and the
/// world outside the database may already have changed.
pub(crate) async fn note_phase(db: &Db, turn_id: &str, phase: TurnPhase, tool: Option<&str>) {
    if let Err(e) = db
        .write(async |tx| turn_ops::set_phase(tx, turn_id, phase, tool, now_ms()).await)
        .await
    {
        tracing::warn!(error = %e, "could not record the turn phase");
    }
}

/// Close the turn's record out.
///
/// Only the paths that actually reach an ending call this. Anything else leaves
/// the row at `running`, which is exactly what the next launch reads as "this
/// was killed" — which is why none of this is attempted from a destructor.
///
/// Only ever called with an id `begin` returned `Ok` for. Called with any other
/// it would rewrite the ending of whichever turn really owns that id, which is
/// the same corruption `begin` refuses — reached by the back door.
pub async fn finish(db: &Db, turn_id: &str, status: TurnStatus, error: Option<&str>) {
    if let Err(e) = db
        .write(async |tx| turn_ops::finish(tx, turn_id, status, error, now_ms()).await)
        .await
    {
        tracing::warn!(error = %e, "could not record how the turn ended");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    /// The second half of refusing a replayed id. `begin` says no, but the
    /// caller then has to *not* close the turn out — the failure path used to
    /// run unconditionally, so a duplicate id was refused and then immediately
    /// marked the historical turn `failed`, which is the corruption it was
    /// refused to prevent.
    #[tokio::test]
    async fn a_refused_duplicate_leaves_the_turn_it_named_untouched() {
        let db = sea_test_db().await;
        execute_for_tests(
            &db,
            "INSERT INTO conversations (id, title, created_at, updated_at) VALUES ('c1', 't', 1, 1)",
        )
        .await
        .unwrap();

        begin(&db, "t1", "c1", TurnOrigin::Desktop, None).await.expect("first");
        note_phase(&db, "t1", TurnPhase::RunningTool, Some("edit_file")).await;
        finish(&db, "t1", TurnStatus::Done, None).await;
        let before = turn_ops::get(&db, "t1").await.unwrap().unwrap();

        // The same id arrives again.
        let replay = begin(&db, "t1", "c1", TurnOrigin::Desktop, None).await;
        assert!(replay.is_err(), "a turn cannot be run twice");

        // The caller must stop here; the finished turn is exactly as it was.
        assert_eq!(turn_ops::get(&db, "t1").await.unwrap().unwrap(), before);
    }

    /// A record that cannot be written must not take the turn down with it:
    /// losing the diagnosis is cheaper than losing the answer.
    #[tokio::test]
    async fn a_turn_whose_record_cannot_be_written_still_runs() {
        let db = sea_test_db().await;
        // No conversation row, so the foreign key refuses the insert. Not a
        // duplicate — the turn should be allowed to carry on regardless.
        assert!(begin(&db, "t1", "missing", TurnOrigin::Desktop, None).await.is_ok());
    }
}

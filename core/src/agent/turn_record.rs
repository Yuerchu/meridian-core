//! Keeping the durable half of a turn up to date, from either runner.
//!
//! Thin async wrappers over `db::ops::turn`, here rather than in that module
//! because both loops need them and `db::ops` is otherwise entirely
//! synchronous. Every write goes through `spawn_blocking`: these are pooled
//! connections, and a turn must never block a runtime worker on one.

use crate::db::DbPool;
use crate::db::models::turn::{TurnPhase, TurnStatus};
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
    pool: &DbPool,
    turn_id: &str,
    conversation_id: &str,
    origin: TurnOrigin,
    self_id: Option<i64>,
) -> Result<(), String> {
    let pool = pool.clone();
    let id = turn_id.to_string();
    let conv = conversation_id.to_string();
    let written = tokio::task::spawn_blocking(move || {
        let mut conn = pool.get().map_err(|e| Failure::Unrecorded(e.to_string()))?;
        crate::db::ops::turn::begin(&mut conn, &id, &conv, origin, self_id, now_ms()).map_err(|e| match e {
            diesel::result::Error::DatabaseError(diesel::result::DatabaseErrorKind::UniqueViolation, _) => {
                Failure::Duplicate
            }
            other => Failure::Unrecorded(other.to_string()),
        })
    })
    .await;

    match written {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(Failure::Duplicate)) => {
            tracing::error!(turn_id, "refusing a turn whose id is already on record");
            Err("This turn has already been run.".into())
        }
        Ok(Err(Failure::Unrecorded(e))) => {
            tracing::warn!(error = %e, "could not record the start of this turn");
            Ok(())
        }
        Err(e) => {
            tracing::warn!(error = %e, "recording the start of this turn panicked");
            Ok(())
        }
    }
}

enum Failure {
    /// The id names a turn that has already happened.
    Duplicate,
    /// The record could not be written, which costs the diagnosis and nothing
    /// else.
    Unrecorded(String),
}

/// Write down what the turn is about to do, before it does it.
///
/// The ordering is the whole point: a phase recorded afterwards describes a
/// window that has already closed. What is stored when the process dies is the
/// diagnosis — `RunningTool` in particular means a tool had started and the
/// world outside the database may already have changed.
pub(crate) async fn note_phase(pool: &DbPool, turn_id: &str, phase: TurnPhase, tool: Option<&str>) {
    let pool = pool.clone();
    let id = turn_id.to_string();
    let tool = tool.map(str::to_string);
    report(
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| e.to_string())?;
            crate::db::ops::turn::set_phase(&mut conn, &id, phase, tool.as_deref(), now_ms()).map_err(|e| e.to_string())
        })
        .await,
        "could not record the turn phase",
    );
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
pub async fn finish(pool: &DbPool, turn_id: &str, status: TurnStatus, error: Option<&str>) {
    let pool = pool.clone();
    let id = turn_id.to_string();
    let error = error.map(str::to_string);
    report(
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| e.to_string())?;
            crate::db::ops::turn::finish(&mut conn, &id, status, error.as_deref(), now_ms()).map_err(|e| e.to_string())
        })
        .await,
        "could not record how the turn ended",
    );
}

fn report(outcome: Result<Result<usize, String>, tokio::task::JoinError>, what: &str) {
    match outcome {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "{what}"),
        Err(e) => tracing::warn!(error = %e, "{what} (the write panicked)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::turn::TurnPhase;
    use crate::db::test_db;

    fn conv(pool: &DbPool, id: &str) {
        let mut conn = pool.get().unwrap();
        crate::db::ops::conversation::create_conversation(&mut conn, id, Some("t"), None, None, 1).unwrap();
    }

    fn stored(pool: &DbPool, id: &str) -> crate::db::models::turn::TurnRow {
        use diesel::prelude::*;
        let mut conn = pool.get().unwrap();
        crate::db::schema::turns::table.find(id).first(&mut conn).unwrap()
    }

    /// The second half of refusing a replayed id. `begin` says no, but the
    /// caller then has to *not* close the turn out — the failure path used to
    /// run unconditionally, so a duplicate id was refused and then immediately
    /// marked the historical turn `failed`, which is the corruption it was
    /// refused to prevent.
    #[tokio::test]
    async fn a_refused_duplicate_leaves_the_turn_it_named_untouched() {
        let pool = test_db();
        conv(&pool, "c1");

        begin(&pool, "t1", "c1", TurnOrigin::Desktop, None)
            .await
            .expect("first");
        note_phase(&pool, "t1", TurnPhase::RunningTool, Some("edit_file")).await;
        finish(&pool, "t1", TurnStatus::Done, None).await;
        let before = stored(&pool, "t1");

        // The same id arrives again.
        let replay = begin(&pool, "t1", "c1", TurnOrigin::Desktop, None).await;
        assert!(replay.is_err(), "a turn cannot be run twice");

        // The caller must stop here. If it went on to close the turn out — as
        // the unconditional failure path did — this is what would change.
        let after = stored(&pool, "t1");
        assert_eq!(after.status, before.status);
        assert_eq!(after.ended_at, before.ended_at);
        assert_eq!(after.phase, before.phase);
        assert_eq!(after.error, before.error);
    }

    /// A record that cannot be written must not take the turn down with it:
    /// losing the diagnosis is cheaper than losing the answer.
    #[tokio::test]
    async fn a_turn_whose_record_cannot_be_written_still_runs() {
        let pool = test_db();
        // No conversation row, so the foreign key refuses the insert. Not a
        // duplicate — the turn should be allowed to carry on regardless.
        assert!(begin(&pool, "t1", "missing", TurnOrigin::Desktop, None).await.is_ok());
    }
}

//! An insertion sequence on `turns`, replacing `rowid` as the tie-break.
//!
//! Two turns can start in the same millisecond — a turn its provider refuses
//! begins and ends inside one — and `turns.id` is a uuid whose order means
//! nothing. The order was SQLite's `rowid`, which no second backend has:
//! PostgreSQL's `ctid` is physical and moves when a row is updated, and a turn
//! is updated several times before it ends.
//!
//! So the order becomes a column. `ops::turn::begin_triggered` fills it from
//! `MAX(seq) + 1` inside the write transaction that inserts the row — which is
//! why there is no trigger and no `AUTOINCREMENT` here. Writes are serialized
//! (`BEGIN IMMEDIATE` on SQLite, an advisory lock on PostgreSQL), so the read
//! and the insert cannot interleave with another writer's.
//!
//! `DEFAULT 0` is for the existing rows this migration backfills and for a
//! writer that forgets: zero sorts before every real sequence rather than
//! after, so a forgotten `seq` shows up as a turn stuck at the bottom of the
//! list rather than as one that silently jumped to the top.

pub fn sqlite_statements() -> Vec<String> {
    vec![
        "ALTER TABLE turns ADD COLUMN seq INTEGER NOT NULL DEFAULT 0".into(),
        // `rowid` is exactly the order this column is taking over from, so
        // existing rows keep the order they were read in before the migration.
        "UPDATE turns SET seq = rowid".into(),
        // `MAX(seq)` runs once per turn; without this it is a table scan.
        "CREATE INDEX idx_turns_seq ON turns (seq)".into(),
    ]
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Database};

    /// The backfill keeps insertion order, and the new column is not nullable.
    #[tokio::test]
    async fn existing_turns_keep_their_insertion_order() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        for sql in super::super::m0001_baseline::sqlite_statements() {
            db.execute_unprepared(&sql).await.unwrap();
        }
        db.execute_unprepared("INSERT INTO conversations (id, created_at, updated_at) VALUES ('c', 1, 1)")
            .await
            .unwrap();
        for id in ["zzz-first", "aaa-second"] {
            db.execute_unprepared(&format!(
                "INSERT INTO turns (id, conversation_id, origin, status, started_at, updated_at) \
                 VALUES ('{id}', 'c', 'desktop', 'running', 1000, 1000)"
            ))
            .await
            .unwrap();
        }

        for sql in super::sqlite_statements() {
            db.execute_unprepared(&sql).await.unwrap();
        }

        let rows = db
            .query_all_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                "SELECT id FROM turns ORDER BY seq",
            ))
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get::<String>("", "id").unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows, ["zzz-first", "aaa-second"]);
    }
}

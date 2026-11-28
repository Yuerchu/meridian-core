//! Background Auto-Dream memory consolidation tables (`dream_watermarks` and
//! `dream_runs`).
//!
//! `dream_watermarks` tracks per-scope turn accumulation and the last
//! consolidated message cursor so Auto-Dream evaluates its 3-gate trigger
//! (cooldown elapsed, minimum turns reached, daily budget remaining) without
//! rescanning the whole conversation history.
//!
//! `dream_runs` records every consolidation attempt — status, counts, model
//! used, and the exact soft-deleted memory IDs (`pruned_memory_ids_json`) so
//! the user can review or undo a prune pass with one click.

pub fn sqlite_statements() -> Vec<String> {
    vec![
        "CREATE TABLE dream_watermarks (\
            scope_type TEXT NOT NULL, \
            scope_id TEXT NOT NULL, \
            last_dream_at INTEGER, \
            last_message_id TEXT, \
            turns_since_dream INTEGER NOT NULL DEFAULT 0, \
            updated_at INTEGER NOT NULL, \
            PRIMARY KEY (scope_type, scope_id)\
        )"
        .into(),
        "CREATE TABLE dream_runs (\
            id TEXT PRIMARY KEY NOT NULL, \
            scope_type TEXT NOT NULL, \
            scope_id TEXT NOT NULL, \
            status TEXT NOT NULL CHECK (status IN ('running', 'done', 'failed', 'skipped')), \
            trigger TEXT NOT NULL CHECK (trigger IN ('auto', 'manual')), \
            model_id TEXT, \
            started_at INTEGER NOT NULL, \
            finished_at INTEGER, \
            turns_reviewed INTEGER NOT NULL DEFAULT 0, \
            memories_created INTEGER NOT NULL DEFAULT 0, \
            memories_updated INTEGER NOT NULL DEFAULT 0, \
            memories_pruned INTEGER NOT NULL DEFAULT 0, \
            proposals_created INTEGER NOT NULL DEFAULT 0, \
            pruned_memory_ids_json TEXT, \
            summary TEXT, \
            error TEXT\
        )"
        .into(),
        "CREATE INDEX idx_dream_runs_scope_started ON dream_runs (scope_type, scope_id, started_at DESC)".into(),
        "CREATE INDEX idx_dream_runs_started ON dream_runs (started_at DESC)".into(),
    ]
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Database};

    #[tokio::test]
    async fn creates_dream_tables_and_enforces_status_and_trigger_checks() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        for sql in super::sqlite_statements() {
            db.execute_unprepared(&sql).await.unwrap();
        }

        db.execute_unprepared(
            "INSERT INTO dream_watermarks (scope_type, scope_id, turns_since_dream, updated_at) \
             VALUES ('project', 'p1', 3, 1000)",
        )
        .await
        .unwrap();

        db.execute_unprepared(
            "INSERT INTO dream_runs (id, scope_type, scope_id, status, trigger, started_at) \
             VALUES ('r1', 'project', 'p1', 'done', 'auto', 1000)",
        )
        .await
        .unwrap();

        assert!(
            db.execute_unprepared(
                "INSERT INTO dream_runs (id, scope_type, scope_id, status, trigger, started_at) \
                 VALUES ('r2', 'project', 'p1', 'bogus', 'auto', 1000)"
            )
            .await
            .is_err()
        );
        assert!(
            db.execute_unprepared(
                "INSERT INTO dream_runs (id, scope_type, scope_id, status, trigger, started_at) \
                 VALUES ('r3', 'project', 'p1', 'done', 'cron', 1000)"
            )
            .await
            .is_err()
        );
    }
}

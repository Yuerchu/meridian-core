//! Per-round stream timing on assistant rows.
//!
//! TTFT is the wait from the request going out to the first model token —
//! visible text, thinking, or a tool-call start. Generation is the stretch
//! from that token to the stream ending. Tokens-per-second is derived from
//! generation plus `output_tokens` at read time, so the two columns stay the
//! facts and the rate cannot disagree with them.
//!
//! NULL is a row written before this existed, a round that never produced a
//! token, or a hosted session this app does not time. `0` is a first token
//! that landed in the same millisecond the request went out.

pub fn sqlite_statements() -> Vec<String> {
    vec![
        "ALTER TABLE messages ADD COLUMN ttft_ms INTEGER".into(),
        "ALTER TABLE messages ADD COLUMN generation_ms INTEGER".into(),
    ]
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Database};

    #[tokio::test]
    async fn existing_rows_have_unknown_timing() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        for sql in super::super::m0001_baseline::sqlite_statements() {
            db.execute_unprepared(&sql).await.unwrap();
        }
        db.execute_unprepared("INSERT INTO conversations (id, created_at, updated_at) VALUES ('c', 1, 1)")
            .await
            .unwrap();
        db.execute_unprepared(
            "INSERT INTO messages (id, conversation_id, role, content, created_at) \
             VALUES ('m', 'c', 'assistant', 'hi', 1)",
        )
        .await
        .unwrap();

        for sql in super::sqlite_statements() {
            db.execute_unprepared(&sql).await.unwrap();
        }

        let row = db
            .query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                "SELECT ttft_ms, generation_ms FROM messages WHERE id='m'",
            ))
            .await
            .unwrap()
            .unwrap();
        assert!(row.try_get_by_index::<Option<i64>>(0).unwrap().is_none());
        assert!(row.try_get_by_index::<Option<i64>>(1).unwrap().is_none());
    }
}

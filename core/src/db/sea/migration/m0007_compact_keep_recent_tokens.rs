//! 压缩保留量按 token 计，而不是按消息对数。
//!
//! The old column counted "pairs of messages kept after compaction", which
//! says nothing about how much context survives: two tool results can outweigh
//! forty short exchanges. The replacement is a token budget, matching Pi's
//! `keepRecentTokens`, so the retry sees a stable amount of context whatever
//! shape the history has.
//!
//! The old value is not converted. Ten pairs of messages is not a token count,
//! and any arithmetic that pretended otherwise would be a guess wearing a
//! migration's authority. Every existing row gets the new default.

pub fn sqlite_statements() -> Vec<String> {
    vec![
        "ALTER TABLE assistants ADD COLUMN compact_keep_recent_tokens INTEGER NOT NULL DEFAULT 20000".into(),
        "ALTER TABLE assistants DROP COLUMN compact_keep_recent".into(),
    ]
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Database};

    #[tokio::test]
    async fn every_existing_assistant_gets_the_default_budget() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        for sql in super::super::m0001_baseline::sqlite_statements() {
            db.execute_unprepared(&sql).await.unwrap();
        }
        db.execute_unprepared(
            "INSERT INTO assistants (id,name,system_prompt,is_default,sort_order,created_at,updated_at,\
             context_limit,compact_keep_recent,thinking_enabled,auto_compact_enabled) \
             VALUES ('a','A','',0,0,1,1,128000,6,0,1)",
        )
        .await
        .unwrap();

        for sql in super::sqlite_statements() {
            db.execute_unprepared(&sql).await.unwrap();
        }

        let row = db
            .query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                "SELECT compact_keep_recent_tokens FROM assistants WHERE id='a'",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get_by_index::<i64>(0).unwrap(), 20000);

        // The old spelling is gone rather than left beside the new one.
        assert!(
            db.execute_unprepared("SELECT compact_keep_recent FROM assistants LIMIT 1")
                .await
                .is_err()
        );
    }
}

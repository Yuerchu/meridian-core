//! Which of a connected MCP server's tools the model is offered.
//!
//! A server's `tools/list` is the catalogue; this column is the set the user
//! has turned off. Absence from the list means on, so a tool that appears after
//! a reconnect is offered until someone disables it — the same default as
//! "connect and send everything". The names are the server's own, not the
//! `mcp__…` qualified form, which follows the server's display name.

pub fn sqlite_statements() -> Vec<String> {
    vec!["ALTER TABLE mcp_servers ADD COLUMN disabled_tools TEXT NOT NULL DEFAULT '[]'".into()]
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Database};

    #[tokio::test]
    async fn existing_servers_start_with_nothing_disabled() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        for sql in super::super::m0001_baseline::sqlite_statements() {
            db.execute_unprepared(&sql).await.unwrap();
        }
        db.execute_unprepared(
            "INSERT INTO mcp_servers (id, name, transport_type, is_enabled, sort_order, created_at, updated_at) \
             VALUES ('s', 's', 'stdio', 0, 0, 1, 1)",
        )
        .await
        .unwrap();

        for sql in super::sqlite_statements() {
            db.execute_unprepared(&sql).await.unwrap();
        }

        let row = db
            .query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                "SELECT disabled_tools FROM mcp_servers WHERE id='s'",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get_by_index::<String>(0).unwrap(), "[]");
    }
}

//! Three-tier MCP policy columns on `mcp_servers`.
//!
//! Adds nullable JSON override columns so every existing server seamlessly
//! inherits the global default (`local_only` / `deferred` / `request_approve`)
//! until the operator overrides at server or per-tool granularity.
//! `approved_always_tools_json` snapshots tool names acknowledged under an
//! `always` server/global policy to guard against upstream supply-chain drift.

pub fn sqlite_statements() -> Vec<String> {
    vec![
        "ALTER TABLE mcp_servers ADD COLUMN default_policy_json TEXT".into(),
        "ALTER TABLE mcp_servers ADD COLUMN tool_policies_json TEXT".into(),
        "ALTER TABLE mcp_servers ADD COLUMN approved_always_tools_json TEXT".into(),
    ]
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Database};

    #[tokio::test]
    async fn existing_servers_start_with_null_policy_columns() {
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
                "SELECT default_policy_json, tool_policies_json, approved_always_tools_json FROM mcp_servers WHERE id='s'",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get_by_index::<Option<String>>(0).unwrap(), None);
        assert_eq!(row.try_get_by_index::<Option<String>>(1).unwrap(), None);
        assert_eq!(row.try_get_by_index::<Option<String>>(2).unwrap(), None);
    }
}

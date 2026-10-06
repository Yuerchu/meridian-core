//! Reading and writing `mcp_servers`.
//!
//! No function here opens a transaction of its own: a write takes the caller's
//! `WriteTx`, and the caller's `Db::write` is the `BEGIN IMMEDIATE`.

use sea_orm::ActiveValue::Unchanged;
use sea_orm::{ActiveModelTrait, ColumnTrait, DbErr, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder};

use crate::db::entity::mcp_server;
use crate::db::entity::mcp_server::McpServerChangeset;
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::types::SqlBool;

pub async fn list_mcp_servers(db: &impl Read) -> Result<Vec<mcp_server::Model>, DbErr> {
    mcp_server::Entity::find()
        .order_by_asc(mcp_server::Column::SortOrder)
        .order_by_asc(mcp_server::Column::Id)
        .all(db.conn()?)
        .await
}

/// The servers marked to connect at startup.
pub async fn list_enabled_mcp_servers(db: &impl Read) -> Result<Vec<mcp_server::Model>, DbErr> {
    mcp_server::Entity::find()
        .filter(mcp_server::Column::IsEnabled.eq(SqlBool::TRUE))
        .order_by_asc(mcp_server::Column::SortOrder)
        .order_by_asc(mcp_server::Column::Id)
        .all(db.conn()?)
        .await
}

pub async fn get_mcp_server(db: &impl Read, id: &str) -> Result<Option<mcp_server::Model>, DbErr> {
    mcp_server::Entity::find_by_id(id).one(db.conn()?).await
}

fn not_found(id: &str) -> DbErr {
    DbErr::RecordNotFound(format!("MCP server `{id}`"))
}

/// Inserts the row the caller built and reads it back.
pub async fn create_mcp_server(tx: &WriteTx, model: mcp_server::Model) -> Result<mcp_server::Model, DbErr> {
    let id = model.id.clone();
    mcp_server::Entity::insert(model.into_active_model())
        .exec_without_returning(tx.conn()?)
        .await?;
    get_mcp_server(tx, &id).await?.ok_or_else(|| not_found(&id))
}

/// `RecordNotFound` when there is no such row, before anything is written.
pub async fn update_mcp_server(
    tx: &WriteTx,
    id: &str,
    changeset: McpServerChangeset,
) -> Result<mcp_server::Model, DbErr> {
    let existing = get_mcp_server(tx, id).await?.ok_or_else(|| not_found(id))?;
    let mut row = changeset.into_active_model();
    row.id = Unchanged(existing.id);
    row.update(tx.conn()?).await
}

/// How many rows went: 0 for an id that was already gone.
pub async fn delete_mcp_server(tx: &WriteTx, id: &str) -> Result<u64, DbErr> {
    Ok(mcp_server::Entity::delete_by_id(id)
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::db::entity::mcp_server::McpTransport;
    use crate::db::sea::cap::Db;
    use crate::db::sea::{execute_for_tests, sea_test_db};
    use crate::db::types::Json;

    fn server(id: &str, sort_order: i32, enabled: bool) -> mcp_server::Model {
        mcp_server::Model {
            id: id.to_owned(),
            name: id.to_owned(),
            transport_type: McpTransport::Stdio,
            command: Some("npx".into()),
            args: Some(Json(vec!["-y".into(), "server".into()])),
            env: Some(Json(BTreeMap::from([("TOKEN".into(), "t".into())]))),
            url: None,
            headers: None,
            is_enabled: enabled.into(),
            sort_order,
            created_at: 1,
            updated_at: 1,
        }
    }

    async fn insert(db: &Db, model: mcp_server::Model) -> mcp_server::Model {
        db.write(async |tx| create_mcp_server(tx, model).await).await.unwrap()
    }

    #[tokio::test]
    async fn a_created_row_reads_back_as_it_was_written() {
        let db = sea_test_db().await;
        let written = server("s1", 0, false);
        assert_eq!(insert(&db, written.clone()).await, written);
        assert_eq!(get_mcp_server(&db, "s1").await.unwrap(), Some(written));
        assert_eq!(get_mcp_server(&db, "missing").await.unwrap(), None);
    }

    #[tokio::test]
    async fn lists_are_in_sort_order_and_enabled_filters() {
        let db = sea_test_db().await;
        insert(&db, server("b", 1, true)).await;
        insert(&db, server("a", 2, false)).await;
        insert(&db, server("c", 0, true)).await;

        let all: Vec<_> = list_mcp_servers(&db).await.unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(all, ["c", "b", "a"]);
        let enabled: Vec<_> = list_enabled_mcp_servers(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(enabled, ["c", "b"]);
    }

    /// `Some(None)` clears a nullable column; a field left `None` is untouched.
    #[tokio::test]
    async fn an_update_writes_only_what_it_names() {
        let db = sea_test_db().await;
        insert(&db, server("s1", 0, false)).await;
        let updated = db
            .write(async |tx| {
                update_mcp_server(
                    tx,
                    "s1",
                    McpServerChangeset {
                        env: Some(None),
                        is_enabled: Some(SqlBool::TRUE),
                        updated_at: Some(2),
                        ..Default::default()
                    },
                )
                .await
            })
            .await
            .unwrap();
        assert_eq!(updated.env, None);
        assert_eq!(updated.args, server("s1", 0, false).args);
        assert!(updated.is_enabled.get());
        assert_eq!(updated.updated_at, 2);

        let missing = db
            .write(async |tx| update_mcp_server(tx, "nope", McpServerChangeset::default()).await)
            .await;
        assert!(matches!(missing, Err(DbErr::RecordNotFound(_))), "{missing:?}");
    }

    #[tokio::test]
    async fn delete_reports_what_it_removed() {
        let db = sea_test_db().await;
        insert(&db, server("s1", 0, false)).await;
        assert_eq!(db.write(async |tx| delete_mcp_server(tx, "s1").await).await.unwrap(), 1);
        assert_eq!(db.write(async |tx| delete_mcp_server(tx, "s1").await).await.unwrap(), 0);
    }

    /// A row the writers could not have produced fails the read, rather than
    /// arriving as a server with no environment or an unknown transport.
    #[tokio::test]
    async fn a_malformed_row_fails_the_read() {
        for (column, value) in [
            ("env", "'{\"TOKEN\": 1}'"),
            ("args", "'not json'"),
            ("transport_type", "'sse'"),
        ] {
            let db = sea_test_db().await;
            insert(&db, server("s1", 0, true)).await;
            execute_for_tests(&db, &format!("UPDATE mcp_servers SET {column} = {value}"))
                .await
                .unwrap();
            assert!(get_mcp_server(&db, "s1").await.is_err(), "{column} = {value} decoded");
            assert!(
                list_enabled_mcp_servers(&db).await.is_err(),
                "{column} = {value} listed"
            );
        }
    }
}

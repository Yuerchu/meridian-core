use std::sync::Arc;

use diesel::RunQueryDsl;
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement};

use super::cap::sealed::Access;
use super::legacy::LEGACY;
use super::{open, sea_test_db};
use crate::db::types::SqlBool;

fn raw(sql: &str) -> Statement {
    Statement::from_string(DbBackend::Sqlite, sql)
}

#[test]
fn every_migration_directory_is_embedded() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    on_disk.sort();
    let embedded: Vec<&str> = LEGACY.iter().map(|(name, _)| *name).collect();
    assert_eq!(embedded, on_disk, "embedded list and migrations/ disagree");
    for (name, sql) in LEGACY {
        let file = std::fs::read_to_string(dir.join(name).join("up.sql")).unwrap();
        assert_eq!(*sql, file, "{name} embeds a different file");
    }
}

#[derive(diesel::QueryableByName, Debug, PartialEq)]
struct SchemaObject {
    #[diesel(sql_type = diesel::sql_types::Text)]
    kind: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    sql: Option<String>,
}

const SCHEMA: &str = "SELECT type AS kind, name, sql FROM sqlite_master \
     WHERE name NOT LIKE 'sqlite_%' AND name <> '__diesel_schema_migrations' ORDER BY type, name";

/// The replay goes through sqlx, the migrations through Diesel: the two must
/// leave the same schema, every trigger and partial index included. This is
/// what notices a replay that stops short of the end of a file.
#[tokio::test]
async fn sea_test_db_has_the_schema_diesel_builds() {
    let diesel = crate::db::diesel_test_db();
    let expected: Vec<SchemaObject> = diesel::sql_query(SCHEMA).load(&mut diesel.get().unwrap()).unwrap();

    let db = sea_test_db().await;
    let actual: Vec<SchemaObject> = db
        .conn()
        .unwrap()
        .query_all_raw(raw(SCHEMA))
        .await
        .unwrap()
        .iter()
        .map(|row| SchemaObject {
            kind: row.try_get("", "kind").unwrap(),
            name: row.try_get("", "name").unwrap(),
            sql: row.try_get("", "sql").unwrap(),
        })
        .collect();

    assert!(
        expected.iter().any(|o| o.kind == "trigger"),
        "the comparison covers triggers"
    );
    assert_eq!(actual, expected);
}

/// Every connection the production pool hands out carries the pragmas, not
/// only the first one: five held at once are five distinct connections.
#[tokio::test(flavor = "multi_thread", worker_threads = 5)]
async fn every_pooled_connection_waits_for_locks_and_enforces_foreign_keys() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir.path().join("pool.sqlite")).await.unwrap();
    let all_held = Arc::new(tokio::sync::Barrier::new(5));

    let readers: Vec<_> = (0..5)
        .map(|_| {
            let db = db.clone();
            let all_held = all_held.clone();
            tokio::spawn(async move {
                db.read(async |tx| {
                    let conn = tx.conn()?;
                    let pragma_int = async |name: &str| -> Result<i64, DbErr> {
                        conn.query_one_raw(raw(&format!("PRAGMA {name}")))
                            .await?
                            .unwrap()
                            .try_get_by_index(0)
                    };
                    let journal: String = conn
                        .query_one_raw(raw("PRAGMA journal_mode"))
                        .await?
                        .unwrap()
                        .try_get_by_index(0)?;
                    let seen = (
                        pragma_int("busy_timeout").await?,
                        pragma_int("foreign_keys").await?,
                        journal,
                    );
                    all_held.wait().await;
                    Ok::<_, DbErr>(seen)
                })
                .await
                .unwrap()
            })
        })
        .collect();

    for reader in readers {
        let (busy, fk, journal) = reader.await.unwrap();
        assert_eq!((busy, fk, journal.as_str()), (5000, 1, "wal"));
    }
}

#[tokio::test]
async fn a_flag_column_holding_anything_but_0_or_1_fails_the_row() {
    let db = sea_test_db().await;
    let conn = db.conn().unwrap();
    conn.execute_unprepared("CREATE TABLE flags (id TEXT PRIMARY KEY NOT NULL, f INTEGER NOT NULL)")
        .await
        .unwrap();
    for (id, flag) in [("off", SqlBool::FALSE), ("on", SqlBool::TRUE)] {
        conn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO flags (id, f) VALUES (?, ?)",
            [id.into(), flag.into()],
        ))
        .await
        .unwrap();
    }
    conn.execute_unprepared("INSERT INTO flags (id, f) VALUES ('two', 2)")
        .await
        .unwrap();

    let read = async |id: &str| {
        let row = conn
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT f FROM flags WHERE id = ?",
                [id.into()],
            ))
            .await
            .unwrap()
            .unwrap();
        row.try_get::<SqlBool>("", "f")
    };
    assert_eq!(read("off").await.unwrap(), SqlBool::FALSE);
    assert!(read("on").await.unwrap().get());
    let error = read("two").await.unwrap_err();
    assert!(error.to_string().contains("holds 2"), "{error}");
}

/// Each call is its own database: an in-memory database belongs to its
/// connection, and the factory never shares one.
#[tokio::test]
async fn each_sea_test_db_is_private() {
    let first = sea_test_db().await;
    let second = sea_test_db().await;
    first
        .conn()
        .unwrap()
        .execute_unprepared("INSERT INTO preferences (key, value, updated_at) VALUES ('k', 'v', 1)")
        .await
        .unwrap();
    let count = |db: super::cap::Db| async move {
        let row = db
            .conn()
            .unwrap()
            .query_one_raw(raw("SELECT count(*) AS n FROM preferences WHERE key = 'k'"))
            .await
            .unwrap()
            .unwrap();
        row.try_get::<i64>("", "n").unwrap()
    };
    assert_eq!((count(first).await, count(second).await), (1, 0));
}

//! Rehearses the bridge on a copy of a real database and reports what it did.
//!
//! ```text
//! cargo run -p meridian-core --example bridge_check --features test-support -- <path to meridian.db>
//! ```
//!
//! The source is opened read-only and copied with `VACUUM INTO` — a consistent
//! snapshot even while the app has the file open in WAL — into a temporary
//! directory that is removed when this exits. Nothing is ever written to the
//! source, or next to it. On the copy, the bridge runs as it would at the next
//! start, and then four things are checked: the schema is the one the baseline
//! builds, every table has the row count it had, SQLite's integrity and
//! foreign-key checks say what they said before, and the previous release's
//! Diesel harness finds nothing left to run. Any difference is a non-zero exit.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use diesel::Connection as _;
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};
use meridian_core::db::sea::bridge::{Ledger, migrate_file};
use meridian_core::db::sea::introspect::Schema;
use meridian_core::db::sea::{memory_connection, migration};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, SqlxSqliteConnector, Statement};
use sea_orm_migration::MigratorTrait;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

/// What the previous release embedded, for the downgrade check.
const DIESEL_MIGRATIONS: EmbeddedMigrations = embed_migrations!();

#[tokio::main]
async fn main() {
    let source = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .expect("usage: bridge_check <path to meridian.db>");
    if !source.is_file() {
        panic!("{} is not a file", source.display());
    }
    let name = source.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if name.ends_with("-wal") || name.ends_with("-shm") {
        panic!("point this at the database file, not its WAL or shared-memory file");
    }

    let scratch = tempfile::tempdir().expect("a temporary directory");
    let copy = scratch.path().join("copy.sqlite");
    snapshot(&source, &copy).await;
    println!("snapshot of {} taken into a temporary directory", source.display());

    let before = Facts::read(&copy).await;
    println!(
        "before: {} tables, {} rows, diesel ledger {:?}, integrity {:?}, {} foreign-key violations",
        before.rows.len(),
        before.rows.values().sum::<i64>(),
        before.diesel_versions.len(),
        before.integrity,
        before.foreign_key_violations
    );

    let started = Instant::now();
    let ledger = migrate_file(&copy).await.expect("the bridge failed");
    println!("bridge: {ledger:?} in {:?}", started.elapsed());

    let after = Facts::read(&copy).await;
    let mut failures = Vec::new();

    let expected = fresh_baseline_schema().await;
    if after.schema.normalized() != expected {
        failures.push("the bridged schema is not the one the baseline builds".to_owned());
    }
    for (table, count) in &before.rows {
        match after.rows.get(table) {
            Some(now) if now == count => {}
            Some(now) => failures.push(format!("{table}: {count} rows before, {now} after")),
            None => failures.push(format!("{table}: gone")),
        }
    }
    if after.integrity != "ok" {
        failures.push(format!("integrity_check: {}", after.integrity));
    }
    if after.foreign_key_violations != before.foreign_key_violations {
        failures.push(format!(
            "foreign_key_check: {} violations before, {} after",
            before.foreign_key_violations, after.foreign_key_violations
        ));
    }
    if matches!(ledger, Ledger::Diesel { .. }) && after.diesel_versions.len() != 65 {
        failures.push(format!(
            "the Diesel ledger lists {} migrations after the bridge, not 65",
            after.diesel_versions.len()
        ));
    }
    if after.sea_versions.first().map(String::as_str) != Some(migration::BASELINE_NAME) {
        failures.push(format!("the SeaORM ledger reads {:?}", after.sea_versions));
    }

    let pending = {
        let path = copy.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = diesel::SqliteConnection::establish(path.to_str().unwrap()).unwrap();
            conn.pending_migrations(DIESEL_MIGRATIONS).unwrap().len()
        })
        .await
        .unwrap()
    };
    if matches!(ledger, Ledger::Diesel { .. }) && pending != 0 {
        failures.push(format!(
            "the previous release would still want to run {pending} migrations on this database"
        ));
    }

    println!(
        "after: {} tables, {} rows, seaql {:?}, previous release sees {pending} pending",
        after.rows.len(),
        after.rows.values().sum::<i64>(),
        after.sea_versions
    );
    if failures.is_empty() {
        println!("OK");
    } else {
        for failure in &failures {
            println!("FAIL: {failure}");
        }
        std::process::exit(1);
    }
}

/// A consistent copy of `source`, read through a read-only connection.
async fn snapshot(source: &Path, copy: &Path) {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().filename(source).read_only(true))
        .await
        .expect("open the source read-only");
    sqlx::query("VACUUM INTO ?")
        .bind(copy.to_str().expect("a UTF-8 temporary path"))
        .execute(&pool)
        .await
        .expect("VACUUM INTO the copy");
    pool.close().await;
}

struct Facts {
    schema: Schema,
    rows: BTreeMap<String, i64>,
    diesel_versions: Vec<String>,
    sea_versions: Vec<String>,
    integrity: String,
    foreign_key_violations: usize,
}

impl Facts {
    async fn read(path: &Path) -> Self {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(path).read_only(true))
            .await
            .expect("open the copy read-only");
        let conn = SqlxSqliteConnector::from_sqlx_sqlite_pool(pool);
        let schema = Schema::read(&conn).await.expect("read the schema");
        let mut rows = BTreeMap::new();
        for table in &schema.tables {
            rows.insert(
                table.name.clone(),
                scalar::<i64>(&conn, &format!("SELECT count(*) FROM \"{}\"", table.name)).await,
            );
        }
        let facts = Self {
            schema,
            rows,
            diesel_versions: versions(&conn, "__diesel_schema_migrations").await,
            sea_versions: versions(&conn, "seaql_migrations").await,
            integrity: scalar::<String>(&conn, "PRAGMA integrity_check").await,
            foreign_key_violations: conn
                .query_all_raw(Statement::from_string(DbBackend::Sqlite, "PRAGMA foreign_key_check"))
                .await
                .unwrap()
                .len(),
        };
        conn.close().await.unwrap();
        facts
    }
}

async fn scalar<T: sea_orm::TryGetable>(conn: &DatabaseConnection, sql: &str) -> T {
    conn.query_one_raw(Statement::from_string(DbBackend::Sqlite, sql))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap()
}

/// The ledger's versions, or nothing when the table does not exist.
async fn versions(conn: &DatabaseConnection, table: &str) -> Vec<String> {
    let exists = scalar::<i64>(
        conn,
        &format!("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = '{table}'"),
    )
    .await
        > 0;
    if !exists {
        return Vec::new();
    }
    conn.query_all_raw(Statement::from_string(
        DbBackend::Sqlite,
        format!("SELECT version FROM {table} ORDER BY version"),
    ))
    .await
    .unwrap()
    .iter()
    .map(|row| row.try_get_by_index::<String>(0).unwrap())
    .collect()
}

async fn fresh_baseline_schema() -> Schema {
    let conn = memory_connection().await;
    conn.execute_unprepared("PRAGMA foreign_keys = OFF").await.unwrap();
    migration::Migrator::up(&conn, None).await.unwrap();
    Schema::read(&conn).await.unwrap().normalized()
}

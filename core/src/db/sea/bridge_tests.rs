//! The bridge, one ledger state at a time: what it does to each kind of
//! database, what it refuses, and that a refusal writes nothing.

use pretty_assertions::assert_eq;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement};
use sea_orm_migration::{MigrationName, MigrationTrait, MigratorTrait, SchemaManager};
use sqlx::sqlite::SqlitePoolOptions;

use super::bridge::{Ledger, classify, migrate, migrate_file, migrate_with, previous_release_file, replay_as_diesel};
use super::introspect::Schema;
use super::legacy::{LEGACY, replay_all};
use super::memory_connection;
use super::migration::Migrator;
use super::migration::m0001_baseline::sqlite_statements;

const DIESEL_LEDGER: &str = "__diesel_schema_migrations";

fn raw(sql: &str) -> Statement {
    Statement::from_string(DbBackend::Sqlite, sql)
}

async fn blank() -> DatabaseConnection {
    let conn = memory_connection().await;
    // As the migration connection in production: foreign keys off.
    conn.execute_unprepared("PRAGMA foreign_keys = OFF").await.unwrap();
    conn
}

/// A database Diesel had migrated up to and including migration `applied`.
async fn diesel_at(applied: usize) -> DatabaseConnection {
    let conn = blank().await;
    replay_as_diesel(&conn, applied).await.unwrap();
    conn
}

async fn fresh() -> DatabaseConnection {
    let conn = blank().await;
    assert_eq!(migrate(&conn).await.unwrap(), Ledger::Fresh);
    conn
}

async fn strings(conn: &DatabaseConnection, sql: &str) -> Vec<String> {
    conn.query_all_raw(raw(sql))
        .await
        .unwrap()
        .iter()
        .map(|row| row.try_get_by_index::<String>(0).unwrap())
        .collect()
}

async fn versions(conn: &DatabaseConnection, table: &str) -> Vec<String> {
    strings(conn, &format!("SELECT version FROM {table} ORDER BY version")).await
}

async fn table_names(conn: &DatabaseConnection) -> Vec<String> {
    strings(
        conn,
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )
    .await
}

/// Everything a refusal must leave alone: every schema object and every
/// ledger row.
async fn snapshot(conn: &DatabaseConnection) -> Vec<String> {
    let mut rows = strings(
        conn,
        "SELECT type || '|' || name || '|' || coalesce(sql, '') FROM sqlite_master ORDER BY type, name",
    )
    .await;
    for table in ["seaql_migrations", DIESEL_LEDGER] {
        if table_names(conn).await.iter().any(|name| name == table) {
            rows.extend(
                versions(conn, table)
                    .await
                    .into_iter()
                    .map(|version| format!("{table}|{version}")),
            );
        }
    }
    rows
}

async fn insert_diesel(conn: &DatabaseConnection, version: &str) {
    conn.execute_unprepared(&format!("INSERT INTO {DIESEL_LEDGER} (version) VALUES ('{version}')"))
        .await
        .unwrap();
}

fn all_legacy_versions() -> Vec<String> {
    LEGACY
        .iter()
        .map(|(name, _)| name.split('_').next().unwrap().to_owned())
        .collect()
}

/// The migrations this build ships, by name, in the order they run: what a
/// database is recorded as once `migrate` is done with it.
fn shipped() -> Vec<String> {
    Migrator::migrations().iter().map(|m| m.name().to_owned()).collect()
}

/// A migration after every shipped one, for the tests that need a later
/// migration to exist.
struct Probe;

impl MigrationName for Probe {
    fn name(&self) -> &str {
        "m9999_probe"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Probe {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("CREATE TABLE probe (id INTEGER PRIMARY KEY)")
            .await?;
        Ok(())
    }
}

struct WithProbe;

#[async_trait::async_trait]
impl MigratorTrait for WithProbe {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        let mut all = Migrator::migrations();
        all.push(Box::new(Probe));
        all
    }
}

#[tokio::test]
async fn a_fresh_database_is_built_by_the_baseline_and_recorded_as_such() {
    let conn = fresh().await;
    assert_eq!(classify(&conn).await.unwrap(), Ledger::Bridged);
    assert_eq!(versions(&conn, "seaql_migrations").await, shipped());
    let tables = table_names(&conn).await;
    assert!(tables.iter().any(|t| t == "conversations"), "{tables:?}");
    assert!(
        !tables.iter().any(|t| t == DIESEL_LEDGER),
        "a database the baseline built has no Diesel ledger, and no downgrade"
    );
}

/// `0` is an install that created the ledger and crashed before migration 1;
/// `65` is a replay that finished and crashed before the baseline was
/// recorded; the rest are ordinary old databases. Each ends as a bridged
/// database with the schema the baseline builds.
#[tokio::test]
async fn a_diesel_database_is_replayed_to_the_end_and_then_recorded() {
    let expected = Schema::read(&fresh().await).await.unwrap().normalized();
    for applied in [0, 1, 30, 64, 65] {
        let conn = diesel_at(applied).await;
        assert_eq!(migrate(&conn).await.unwrap(), Ledger::Diesel { applied }, "{applied}");
        assert_eq!(versions(&conn, DIESEL_LEDGER).await, all_legacy_versions(), "{applied}");
        assert_eq!(versions(&conn, "seaql_migrations").await, shipped(), "{applied}");
        assert_eq!(classify(&conn).await.unwrap(), Ledger::Bridged, "{applied}");
        assert_eq!(Schema::read(&conn).await.unwrap().normalized(), expected, "{applied}");
    }
}

#[tokio::test]
async fn migrating_again_changes_nothing() {
    for conn in [fresh().await, diesel_at(10).await] {
        migrate(&conn).await.unwrap();
        let before = snapshot(&conn).await;
        assert_eq!(migrate(&conn).await.unwrap(), Ledger::Bridged);
        assert_eq!(snapshot(&conn).await, before);
    }
}

/// A later migration runs after the bridge on a new database and on an old
/// one, and a database bridged by this build picks it up when the next build
/// opens it.
#[tokio::test]
async fn a_migration_after_the_baseline_runs_once_on_every_kind_of_database() {
    for conn in [blank().await, diesel_at(60).await, fresh().await] {
        migrate_with::<WithProbe>(&conn).await.unwrap();
        let mut expected = shipped();
        expected.push("m9999_probe".into());
        assert_eq!(versions(&conn, "seaql_migrations").await, expected);
        assert!(table_names(&conn).await.iter().any(|t| t == "probe"));
        let before = snapshot(&conn).await;
        assert_eq!(migrate_with::<WithProbe>(&conn).await.unwrap(), Ledger::Bridged);
        assert_eq!(snapshot(&conn).await, before);
    }
}

#[tokio::test]
async fn ledgers_no_release_produced_are_refused_before_any_write() {
    let later_without_baseline = diesel_at(65).await;
    Migrator::install(&later_without_baseline).await.unwrap();
    later_without_baseline
        .execute_unprepared("INSERT INTO seaql_migrations (version, applied_at) VALUES ('m9999_probe', 1)")
        .await
        .unwrap();

    let hole = diesel_at(65).await;
    hole.execute_unprepared(&format!("DELETE FROM {DIESEL_LEDGER} WHERE version = '00000000000030'"))
        .await
        .unwrap();

    let unknown_in_place_of_the_last = diesel_at(64).await;
    insert_diesel(&unknown_in_place_of_the_last, "99999999999999").await;

    let one_too_many = diesel_at(65).await;
    insert_diesel(&one_too_many, "99999999999999").await;

    let tables_without_a_ledger = blank().await;
    replay_all(&tables_without_a_ledger).await.unwrap();

    let cases = [
        (later_without_baseline, "but not m0001_baseline"),
        (
            hole,
            "not a prefix of the known migrations: found 00000000000031, expected 00000000000030",
        ),
        (
            unknown_in_place_of_the_last,
            "not a prefix of the known migrations: found 99999999999999, expected 00000000000065",
        ),
        (one_too_many, "records 66 migrations, more than the 65"),
        (tables_without_a_ledger, "tables but no migration ledger"),
    ];
    for (conn, reason) in cases {
        // Creating the SeaORM ledger is the one write allowed before the
        // check, so the snapshot is taken after it.
        Migrator::install(&conn).await.unwrap();
        let before = snapshot(&conn).await;
        let error = migrate(&conn).await.unwrap_err().to_string();
        assert!(error.contains(reason), "{error:?} does not say {reason:?}");
        assert_eq!(snapshot(&conn).await, before, "{reason}");
    }
}

/// The baseline is one transaction: a failure at table N leaves no table
/// behind, and the retry starts from nothing. The failure is injected by
/// capping the database's page count low enough that the statements run out
/// of room part-way through; the first half of the test establishes that the
/// cap really does bite in the middle rather than on the first statement.
#[tokio::test]
async fn a_baseline_that_fails_midway_leaves_nothing_behind() {
    const CAP: &str = "PRAGMA max_page_count = 40";

    let probe = blank().await;
    probe.execute_unprepared(CAP).await.unwrap();
    let statements = sqlite_statements();
    let mut ran = 0;
    for statement in &statements {
        if probe.execute_unprepared(statement).await.is_err() {
            break;
        }
        ran += 1;
    }
    assert!(
        ran > 0 && ran < statements.len(),
        "the cap must stop the statements part-way, not at {ran} of {}",
        statements.len()
    );

    let conn = blank().await;
    conn.execute_unprepared(CAP).await.unwrap();
    let error = migrate(&conn).await.unwrap_err().to_string();
    assert!(error.contains("full"), "{error}");
    assert_eq!(table_names(&conn).await, ["seaql_migrations"]);
    assert_eq!(versions(&conn, "seaql_migrations").await, Vec::<String>::new());

    conn.execute_unprepared("PRAGMA max_page_count = 1073741823")
        .await
        .unwrap();
    assert_eq!(migrate(&conn).await.unwrap(), Ledger::Fresh);
    assert!(table_names(&conn).await.iter().any(|t| t == "conversations"));
}

/// The migration connection sets WAL on the file and is gone when the call
/// returns — on Windows an open handle would make the remove below fail.
#[tokio::test]
async fn migrate_file_leaves_a_wal_file_and_no_open_connection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.sqlite");
    assert_eq!(migrate_file(&path).await.unwrap(), Ledger::Fresh);
    assert_eq!(migrate_file(&path).await.unwrap(), Ledger::Bridged);

    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(super::options(&path))
        .await
        .unwrap();
    let conn = sea_orm::SqlxSqliteConnector::from_sqlx_sqlite_pool(pool);
    assert_eq!(strings(&conn, "PRAGMA journal_mode").await, ["wal"]);
    conn.close().await.unwrap();

    std::fs::remove_file(&path).expect("nothing holds the file open");
}

/// The downgrade that is promised: the previous release opens a bridged
/// database, finds all 65 migrations recorded, and runs none.
#[tokio::test]
async fn the_previous_release_finds_nothing_pending_on_a_bridged_database() {
    use diesel::Connection;
    use diesel_migrations::MigrationHarness;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.sqlite");
    previous_release_file(&path, 60).await.unwrap();

    assert_eq!(migrate_file(&path).await.unwrap(), Ledger::Diesel { applied: 60 });

    let pending = tokio::task::spawn_blocking(move || {
        let mut conn = diesel::SqliteConnection::establish(path.to_str().unwrap()).unwrap();
        let pending = conn.pending_migrations(crate::db::MIGRATIONS).unwrap().len();
        let ran = conn.run_pending_migrations(crate::db::MIGRATIONS).unwrap().len();
        (pending, ran)
    })
    .await
    .unwrap();
    assert_eq!(pending, (0, 0));
}

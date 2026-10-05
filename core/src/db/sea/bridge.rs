//! Bringing a database to the baseline, whichever way it got its schema.
//!
//! Three kinds of database arrive here, and each is told apart by its ledgers
//! before anything is written:
//!
//! - **Fresh** — no tables, no ledger. The baseline migration builds it.
//! - **Diesel** — `__diesel_schema_migrations` lists the first *k* of the 65
//!   migrations, `0 ≤ k ≤ 65`. The remaining ones are replayed exactly as
//!   Diesel would have run them (each in its own transaction, recorded in the
//!   same table), and then the baseline is *recorded* rather than run: the
//!   schema is already there. From then on the database is bridged.
//! - **Bridged** — `seaql_migrations` records the baseline. Only migrations
//!   after it can be pending.
//!
//! Anything else — a SeaORM ledger with later migrations but no baseline, a
//! Diesel ledger with a hole or a version this build has never heard of,
//! tables with no ledger at all — is refused before the first write, and the
//! error says what was found. A database in such a state was not produced by
//! any release of this app, and guessing would overwrite what someone may
//! still be able to recover.
//!
//! The Diesel ledger is never dropped. A bridged database opened by the
//! previous release finds all 65 migrations applied and runs none, which is
//! what makes downgrading a bridged database safe — and the only downgrade
//! that is promised: a database the baseline built has no Diesel ledger, and
//! an older build would try to run migration 1 against existing tables.

use std::path::Path;
use std::time::SystemTime;

use sea_orm::{
    ConnectionTrait, DatabaseConnection, DbBackend, DbErr, SqliteTransactionMode, SqlxSqliteConnector, Statement,
    TransactionOptions, TransactionTrait,
};
use sea_orm_migration::MigratorTrait;
use sqlx::sqlite::{SqliteJournalMode, SqlitePoolOptions};

use super::legacy::LEGACY;
use super::migration::{BASELINE_NAME, Migrator};

const DIESEL_LEDGER: &str = "__diesel_schema_migrations";

/// Diesel's own definition (`diesel/src/migration/setup_migration_table.sql`),
/// so that a database bridged from `k = 0` is indistinguishable from one Diesel
/// migrated itself.
#[cfg(any(test, feature = "test-support"))]
const DIESEL_LEDGER_DDL: &str = "CREATE TABLE IF NOT EXISTS __diesel_schema_migrations (\n       \
     version VARCHAR(50) PRIMARY KEY NOT NULL,\n       \
     run_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP\n)";

/// What the ledgers said about a database when it was opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ledger {
    Fresh,
    /// Diesel had applied the first `applied` migrations.
    Diesel {
        applied: usize,
    },
    Bridged,
}

fn raw(sql: &str) -> Statement {
    Statement::from_string(DbBackend::Sqlite, sql)
}

fn refuse(reason: String) -> DbErr {
    DbErr::Custom(format!(
        "the database cannot be brought to the SeaORM baseline: {reason}"
    ))
}

/// The 14-digit version Diesel records for a migration directory name.
fn version_of(name: &str) -> &str {
    name.split('_').next().expect("a migration name")
}

async fn versions(conn: &impl ConnectionTrait, table: &str) -> Result<Vec<String>, DbErr> {
    conn.query_all_raw(raw(&format!("SELECT version FROM {table} ORDER BY version")))
        .await?
        .iter()
        .map(|row| row.try_get("", "version"))
        .collect()
}

async fn has_table(conn: &impl ConnectionTrait, name: &str) -> Result<bool, DbErr> {
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT count(*) AS n FROM sqlite_master WHERE type = 'table' AND name = ?",
            [name.into()],
        ))
        .await?
        .expect("count(*) always yields a row");
    Ok(row.try_get::<i64>("", "n")? > 0)
}

/// Reads the ledgers and says which kind of database this is, or why it is
/// none of them. Reads only; `seaql_migrations` must already exist.
pub async fn classify(conn: &impl ConnectionTrait) -> Result<Ledger, DbErr> {
    let sea = versions(conn, "seaql_migrations").await?;
    if sea.iter().any(|version| version == BASELINE_NAME) {
        return Ok(Ledger::Bridged);
    }
    if !sea.is_empty() {
        return Err(refuse(format!(
            "the SeaORM ledger records {sea:?} but not {BASELINE_NAME}"
        )));
    }

    if has_table(conn, DIESEL_LEDGER).await? {
        let applied = versions(conn, DIESEL_LEDGER).await?;
        let known: Vec<&str> = LEGACY.iter().map(|(name, _)| version_of(name)).collect();
        if applied.len() > known.len() {
            return Err(refuse(format!(
                "the Diesel ledger records {} migrations, more than the {} this build knows",
                applied.len(),
                known.len()
            )));
        }
        if let Some((found, expected)) = applied.iter().zip(&known).find(|(found, expected)| found != expected) {
            return Err(refuse(format!(
                "the Diesel ledger is not a prefix of the known migrations: found {found}, expected {expected}"
            )));
        }
        return Ok(Ledger::Diesel { applied: applied.len() });
    }

    let tables = conn
        .query_one_raw(raw("SELECT count(*) AS n FROM sqlite_master WHERE type = 'table' \
             AND name NOT LIKE 'sqlite_%' AND name <> 'seaql_migrations'"))
        .await?
        .expect("count(*) always yields a row")
        .try_get::<i64>("", "n")?;
    if tables > 0 {
        return Err(refuse(format!("it has {tables} tables but no migration ledger")));
    }
    Ok(Ledger::Fresh)
}

/// Runs one legacy migration as Diesel did: its SQL and its ledger row in one
/// transaction, so a crash leaves either both or neither.
async fn apply_legacy(conn: &DatabaseConnection, name: &str, sql: &str) -> Result<(), DbErr> {
    let tx = conn
        .begin_with_options(TransactionOptions {
            sqlite_transaction_mode: Some(SqliteTransactionMode::Immediate),
            ..Default::default()
        })
        .await?;
    tx.execute_unprepared(sql).await?;
    tx.execute_raw(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO __diesel_schema_migrations (version) VALUES (?)",
        [version_of(name).into()],
    ))
    .await?;
    tx.commit().await
}

/// Records the baseline as applied without running it. Idempotent, so a
/// restart between the last replayed migration and this row simply repeats
/// it.
async fn record_baseline(conn: &impl ConnectionTrait) -> Result<(), DbErr> {
    let applied_at = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0);
    conn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO seaql_migrations (version, applied_at) VALUES (?, ?) ON CONFLICT (version) DO NOTHING",
        [BASELINE_NAME.into(), applied_at.into()],
    ))
    .await?;
    Ok(())
}

/// Brings the database on `conn` to the head of `M`'s migrations, bridging a
/// Diesel-migrated one first. Returns what the ledgers said on arrival.
///
/// `conn` must have foreign keys off: the legacy migrations rebuild tables by
/// dropping them, and `ON DELETE` actions must not fire while they do. The
/// pragma is per connection and must be set before the first transaction
/// begins, which is why the callers set it on the connection options or on
/// a single-connection pool before calling.
pub async fn migrate_with<M: MigratorTrait>(conn: &DatabaseConnection) -> Result<Ledger, DbErr> {
    M::install(conn).await?;
    let ledger = classify(conn).await?;
    if let Ledger::Diesel { applied } = ledger {
        for (name, sql) in LEGACY.iter().skip(applied) {
            apply_legacy(conn, name, sql).await?;
        }
        record_baseline(conn).await?;
    }
    M::up(conn, None).await?;
    Ok(ledger)
}

/// [`migrate_with`] for the migrations this build ships.
pub async fn migrate(conn: &DatabaseConnection) -> Result<Ledger, DbErr> {
    migrate_with::<Migrator>(conn).await
}

/// Migrates the database file at `path` on a connection of its own, which is
/// closed before this returns: foreign keys off for the migration run only,
/// and WAL set on the file before any pool opens it.
pub async fn migrate_file(path: &Path) -> Result<Ledger, DbErr> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            super::options(path)
                .journal_mode(SqliteJournalMode::Wal)
                .foreign_keys(false),
        )
        .await
        .map_err(super::connection_error)?;
    let conn = SqlxSqliteConnector::from_sqlx_sqlite_pool(pool);
    let ledger = migrate(&conn).await?;
    conn.close().await?;
    Ok(ledger)
}

/// What a database looks like after Diesel applied the first `applied`
/// migrations: the ledger table, and each migration with its row. For tests
/// of the bridge, and for the generator's replay.
#[cfg(any(test, feature = "test-support"))]
pub async fn replay_as_diesel(conn: &DatabaseConnection, applied: usize) -> Result<(), DbErr> {
    conn.execute_unprepared(DIESEL_LEDGER_DDL).await?;
    for (name, sql) in LEGACY.iter().take(applied) {
        apply_legacy(conn, name, sql).await?;
    }
    Ok(())
}

/// A database file as the previous release left it: Diesel's ledger and the
/// first `applied` migrations, nothing of SeaORM's. For tests that start a
/// build of today against yesterday's file.
#[cfg(any(test, feature = "test-support"))]
pub async fn previous_release_file(path: &Path, applied: usize) -> Result<(), DbErr> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(super::options(path).foreign_keys(false))
        .await
        .map_err(super::connection_error)?;
    let conn = SqlxSqliteConnector::from_sqlx_sqlite_pool(pool);
    replay_as_diesel(&conn, applied).await?;
    conn.close().await
}

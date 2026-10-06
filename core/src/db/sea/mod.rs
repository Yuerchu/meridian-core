//! The SeaORM side of the persistence layer, alongside Diesel while modules
//! move over one transaction root at a time.
//!
//! `schema_drift` holds every registered entity (`db::entity`) against the
//! live schema and pins the DDL the migrations build to `schema.snapshot.sql`.

pub mod bridge;
pub mod cap;
/// Public for `examples/gen_baseline.rs`, which replays it; nothing outside this
/// crate should otherwise need the legacy SQL.
pub mod legacy;
pub mod migration;
pub mod ops;

#[cfg(any(test, feature = "test-support"))]
pub mod baseline_gen;
#[cfg(any(test, feature = "test-support"))]
pub mod introspect;

#[cfg(test)]
mod bridge_tests;
#[cfg(test)]
mod equivalence_tests;
#[cfg(test)]
mod legacy_tests;
#[cfg(test)]
mod poc;
#[cfg(test)]
mod schema_drift;

use std::path::Path;
use std::time::Duration;

use sea_orm::{DbErr, SqlxSqliteConnector};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{ConnectOptions, Connection};

use self::cap::Db;
use super::{BUSY_TIMEOUT_MS, POOL_ACQUIRE_TIMEOUT};

/// The figures the Diesel pool uses (`max_size(5)`, see `init_db` for why that
/// number has no measurement behind it yet). Built by hand rather than through
/// `Database::connect`, which caps an SQLite pool at one connection unless told
/// otherwise.
const MAX_CONNECTIONS: u32 = 5;

/// Pragmas sqlx applies to every connection it opens, which is what Diesel's
/// `ConnectionCustomizer` does by hand: `busy_timeout` first, so a connection
/// meeting a held lock waits instead of failing on the spot, and foreign keys
/// on, because SQLite leaves them off per connection.
///
/// Statement logging is off: a slow-statement warning would carry the SQL into
/// the log file, and what leaves the machine is lengths, not text.
fn options(filename: impl AsRef<Path>) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(filename)
        .create_if_missing(true)
        .busy_timeout(Duration::from_millis(u64::from(BUSY_TIMEOUT_MS)))
        .foreign_keys(true)
        .disable_statement_logging()
}

fn connection_error(error: sqlx::Error) -> DbErr {
    DbErr::Conn(sea_orm::RuntimeErr::SqlxError(error.into()))
}

/// Opens the database file as the SeaORM pool. Migrations are not run here:
/// `bridge::migrate_file` does that first, on a connection of its own.
///
/// WAL is set once, on a connection of its own, as `init_db` does, and the
/// pool's connections do not ask for it. The mode is stored in the file and
/// every later connection inherits it. Asking again on each new connection is
/// a no-op while the file is WAL, and the file cannot leave WAL while any
/// connection has it open (both measured, SQLite 3.51) — so with the pool
/// holding one connection this placement changes nothing observable. It only
/// keeps a new connection from trying to switch the mode back, which takes a
/// lock `busy_timeout` does not wait for, should something outside ever switch
/// the file while the pool happened to hold no connection at all.
pub async fn open(path: &Path) -> Result<Db, DbErr> {
    let setup = options(path)
        .journal_mode(SqliteJournalMode::Wal)
        .connect()
        .await
        .map_err(connection_error)?;
    setup.close().await.map_err(connection_error)?;

    let pool = SqlitePoolOptions::new()
        .max_connections(MAX_CONNECTIONS)
        .min_connections(1)
        .acquire_timeout(POOL_ACQUIRE_TIMEOUT)
        .connect_with(options(path))
        .await
        .map_err(connection_error)?;
    Ok(Db::new(SqlxSqliteConnector::from_sqlx_sqlite_pool(pool)))
}

/// A private, fully migrated in-memory database, for tests.
///
/// An in-memory database lives exactly as long as its connection, so the pool
/// holds one connection and never lets it go: no idle timeout, no lifetime cap,
/// and the migrations run on that same connection rather than on a second pool
/// that would be a second, empty database. It takes the same path a new
/// install does: the bridge finds a fresh database and the baseline builds it.
#[cfg(any(test, feature = "test-support"))]
pub async fn sea_test_db() -> Db {
    use sea_orm::ConnectionTrait;

    use self::cap::sealed::Access;

    let db = Db::new(memory_connection().await);
    let conn = db.conn().expect("no transaction is open yet");
    // As in production: the migration connection has foreign keys off.
    conn.execute_unprepared("PRAGMA foreign_keys = OFF").await.unwrap();
    bridge::migrate(conn)
        .await
        .expect("failed to migrate the test database");
    conn.execute_unprepared("PRAGMA foreign_keys = ON").await.unwrap();
    db
}

/// A private in-memory database with nothing in it, on a single connection
/// that is never recycled. What `sea_test_db` starts from; the bridge and
/// equivalence tests build their own fixtures on it.
#[cfg(any(test, feature = "test-support"))]
pub async fn memory_connection() -> sea_orm::DatabaseConnection {
    let pool = SqlitePoolOptions::new()
        .min_connections(1)
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(options(":memory:"))
        .await
        .expect("failed to open the in-memory test database");
    SqlxSqliteConnector::from_sqlx_sqlite_pool(pool)
}

/// One migrated file in `dir`, open through both pools, for tests that need a
/// Diesel transaction and a SeaORM one to meet on the same database.
#[cfg(any(test, feature = "test-support"))]
pub async fn shared_test_db(dir: &Path) -> (super::DbPool, Db) {
    let path = dir.join("shared.sqlite");
    bridge::migrate_file(&path)
        .await
        .expect("failed to migrate the shared test database");
    let diesel = super::init_db(path.to_str().expect("a UTF-8 temp path"));
    let sea = open(&path).await.expect("failed to open the shared test database");
    (diesel, sea)
}

/// Runs one raw statement on the pool, for tests that need to break the
/// schema under a loader (`DROP TABLE preferences`) and nothing else. The
/// sealed connection is reachable only from inside `crate::db`, which is why
/// this lives here rather than beside the test that wants it.
#[cfg(any(test, feature = "test-support"))]
pub async fn execute_for_tests(db: &Db, sql: &str) -> Result<(), DbErr> {
    use sea_orm::ConnectionTrait;

    use self::cap::sealed::Access;

    db.conn()?.execute_unprepared(sql).await?;
    Ok(())
}

#[cfg(test)]
mod tests;

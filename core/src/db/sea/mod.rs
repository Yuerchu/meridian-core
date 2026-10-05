//! The SeaORM side of the persistence layer, alongside Diesel while modules
//! move over one transaction root at a time.

pub mod cap;
// Only the test factories replay it so far; the bridge that brings old
// databases up to the baseline will, and then this gate goes.
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod legacy;

#[cfg(test)]
mod poc;

use std::path::Path;
use std::time::Duration;

use sea_orm::{DbErr, SqlxSqliteConnector};
use sqlx::ConnectOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

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

/// Opens the database file as the SeaORM pool. Migrations are not run here;
/// until the baseline lands, Diesel's `init_db` still owns the schema.
pub async fn open(path: &Path) -> Result<Db, DbErr> {
    let pool = SqlitePoolOptions::new()
        .max_connections(MAX_CONNECTIONS)
        .min_connections(1)
        .acquire_timeout(POOL_ACQUIRE_TIMEOUT)
        .connect_with(options(path).journal_mode(SqliteJournalMode::Wal))
        .await
        .map_err(|error| DbErr::Conn(sea_orm::RuntimeErr::SqlxError(error.into())))?;
    Ok(Db::new(SqlxSqliteConnector::from_sqlx_sqlite_pool(pool)))
}

/// A private, fully migrated in-memory database, for tests.
///
/// An in-memory database lives exactly as long as its connection, so the pool
/// holds one connection and never lets it go: no idle timeout, no lifetime cap,
/// and the migrations run on that same connection rather than on a second pool
/// that would be a second, empty database.
#[cfg(any(test, feature = "test-support"))]
pub async fn sea_test_db() -> Db {
    use sea_orm::ConnectionTrait;

    use self::cap::sealed::Access;

    let pool = SqlitePoolOptions::new()
        .min_connections(1)
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(options(":memory:"))
        .await
        .expect("failed to open the in-memory test database");
    let db = Db::new(SqlxSqliteConnector::from_sqlx_sqlite_pool(pool));
    let conn = db.conn().expect("no transaction is open yet");
    // As in production: table rebuilds must not fire `ON DELETE` actions.
    conn.execute_unprepared("PRAGMA foreign_keys = OFF").await.unwrap();
    legacy::replay_all(conn).await.expect("failed to replay the migrations");
    conn.execute_unprepared("PRAGMA foreign_keys = ON").await.unwrap();
    db
}

/// One migrated file in `dir`, open through both pools, for tests that need a
/// Diesel transaction and a SeaORM one to meet on the same database.
#[cfg(any(test, feature = "test-support"))]
pub async fn shared_test_db(dir: &Path) -> (super::DbPool, Db) {
    let path = dir.join("shared.sqlite");
    let diesel = super::init_db(path.to_str().expect("a UTF-8 temp path"));
    let sea = open(&path).await.expect("failed to open the shared test database");
    (diesel, sea)
}

#[cfg(test)]
mod tests;

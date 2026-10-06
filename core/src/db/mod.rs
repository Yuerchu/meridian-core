pub mod entity;
pub mod models;
pub mod ops;
pub mod schema;
pub mod sea;
pub mod types;

use diesel::RunQueryDsl;
use diesel::r2d2::{ConnectionManager, Pool, PooledConnection};
use diesel::sqlite::SqliteConnection;
#[cfg(test)]
use diesel_migrations::{EmbeddedMigrations, embed_migrations};

/// The Diesel migrations as the previous release embedded them. Production
/// no longer runs them — `sea::bridge` replays the SQL through sqlx — and
/// they stay here for one proof: that this harness finds nothing pending on a
/// database the bridge has handled, which is what makes downgrading one safe.
#[cfg(test)]
const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

pub type DbPool = Pool<ConnectionManager<SqliteConnection>>;
pub type PooledConn = PooledConnection<ConnectionManager<SqliteConnection>>;

/// How long a connection waits for a lock before giving up.
///
/// Long enough to sit through any write this app makes — they are single-row
/// inserts and updates — while still failing rather than hanging if something
/// holds the write lock indefinitely.
pub(crate) const BUSY_TIMEOUT_MS: u32 = 5_000;

/// How long `pool.get()` waits for a free connection.
///
/// r2d2 defaults this to 30 seconds, which is long enough that an exhausted
/// pool reads as the app having frozen rather than as an error. Every caller
/// here either reports the failure or falls back within a request, so failing
/// fast is strictly better than waiting.
pub(crate) const POOL_ACQUIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// SQLite pragmas are per-connection, so they must run on every connection the
/// pool hands out — running them once on a single connection leaves the other
/// pooled connections without foreign key enforcement.
#[derive(Debug)]
struct ConnectionCustomizer;

impl diesel::r2d2::CustomizeConnection<SqliteConnection, diesel::r2d2::Error> for ConnectionCustomizer {
    fn on_acquire(&self, conn: &mut SqliteConnection) -> Result<(), diesel::r2d2::Error> {
        // First, so that the statement after it is covered too.
        //
        // SQLite defaults this to zero: a connection that meets a held lock
        // fails on the spot with "database is locked" instead of waiting. WAL
        // lets readers run alongside a writer, but two writers still collide,
        // and a single turn writes from several places — the user row, the
        // assistant placeholder, then a row per tool result, all while the
        // frontend is polling context size. That is what put the error in the
        // log, raised from r2d2 handing out a connection rather than from any
        // one query, because even this pragma run below could not get in.
        diesel::sql_query(format!("PRAGMA busy_timeout={BUSY_TIMEOUT_MS}"))
            .execute(conn)
            .map_err(diesel::r2d2::Error::QueryError)?;
        diesel::sql_query("PRAGMA foreign_keys=ON")
            .execute(conn)
            .map_err(diesel::r2d2::Error::QueryError)?;
        Ok(())
    }
}

pub fn init_db(db_path: &str) -> DbPool {
    let manager = ConnectionManager::<SqliteConnection>::new(db_path);
    // max_size is deliberately left where it was: it is a capacity figure with
    // no measurement behind it, and the acquire timeout above is what turns
    // exhaustion from a hang into a visible, logged failure. Raise it once the
    // logs say how often the pool actually runs dry.
    let pool = Pool::builder()
        .max_size(5)
        .connection_timeout(POOL_ACQUIRE_TIMEOUT)
        .connection_customizer(Box::new(ConnectionCustomizer))
        .build(manager)
        .expect("failed to create db pool");

    let mut conn = pool.get().expect("failed to get db connection");
    // journal_mode is persistent (stored in the db file), one connection suffices.
    diesel::sql_query("PRAGMA journal_mode=WAL").execute(&mut conn).ok();

    // The schema is not this pool's business any more. `db::sea::bridge`
    // migrates the file before this is called — bridging a database Diesel
    // migrated, or building a new one from the SeaORM baseline — and the
    // repairs that need the migrated schema are `bootstrap::startup_recovery`,
    // which runs right after this returns. A test or tool that opens a file
    // through `init_db` alone gets a pool on whatever schema the file has.
    pool
}

#[cfg(any(test, feature = "test-support"))]
pub fn diesel_test_db() -> DbPool {
    let manager = ConnectionManager::<SqliteConnection>::new(":memory:");
    let pool = Pool::builder()
        .max_size(1)
        .connection_customizer(Box::new(ConnectionCustomizer))
        .build(manager)
        .expect("failed to create test db pool");

    // The statements the SeaORM baseline runs, run here through Diesel: one
    // source for the schema while both ORMs read it. A migration after the
    // baseline adds its own statements to this list.
    let mut conn = pool.get().expect("failed to get test db connection");
    let schema = sea::migration::m0001_baseline::sqlite_statements().join(";\n") + ";";
    diesel::connection::SimpleConnection::batch_execute(&mut *conn, &schema).expect("failed to build the test schema");

    pool
}

#[cfg(test)]
mod pool_tests {
    use super::*;
    use diesel::prelude::*;
    use diesel::sql_types::Integer;

    #[derive(QueryableByName)]
    struct BusyTimeout {
        #[diesel(sql_type = Integer)]
        timeout: i32,
    }

    /// Without this every pooled connection fails the moment it meets a lock,
    /// which is what "database is locked" in the log was.
    #[test]
    fn test_pooled_connections_wait_for_locks() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        let rows: Vec<BusyTimeout> = diesel::sql_query("PRAGMA busy_timeout").load(&mut conn).unwrap();
        assert_eq!(rows[0].timeout, BUSY_TIMEOUT_MS as i32);
    }
}

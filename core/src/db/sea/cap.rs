//! What a piece of code is allowed to do to the database is written in its
//! argument type.
//!
//! SeaORM's `ConnectionTrait` carries `execute_unprepared` and friends, so any
//! value that hands one out can write. These wrappers hand one out only to code
//! inside `crate::db`, and only through [`sealed::Access`]:
//!
//! - [`Db`] — the pool. Reads on autocommit, and is where transactions start.
//! - [`ReadTx`] — `BEGIN DEFERRED`. Reads only; there is no way to write from it.
//! - [`WriteTx`] — `BEGIN IMMEDIATE`, or a SAVEPOINT inside one. The only thing
//!   a write op accepts.
//!
//! Why `IMMEDIATE` for every write, even a single statement: a deferred
//! transaction that has read takes the write lock only when it first writes,
//! and if another connection committed in between SQLite answers
//! `SQLITE_BUSY_SNAPSHOT` at once — `busy_timeout` does not apply. And a
//! SAVEPOINT cannot fix that from inside, because SeaORM drops the mode for a
//! nested begin (`transaction.rs`, depth ≠ 0). So a write can only start at the
//! top, as `IMMEDIATE`, or inside something that already holds the lock.
//!
//! The types stop one mistake at compile time: passing [`Db`] or [`ReadTx`] to a
//! write op. They cannot stop a closure from capturing the [`Db`] it was
//! started from and going back to the pool — which takes a second connection,
//! reads a snapshot the transaction cannot see, or waits on its own lock. In
//! test and debug builds every way back into the pool checks a task-local for an
//! open transaction and fails at once; the transaction-graph checker rejects the
//! capture statically.

use sea_orm::{
    DatabaseConnection, DatabaseTransaction, DbErr, SqliteTransactionMode, TransactionOptions, TransactionTrait,
};

/// Which kind of transaction the current task is inside, for the guard and its
/// message. Set for the duration of a [`Db::read`]/[`Db::write`] closure; a task
/// spawned from inside one does not inherit it, on purpose — a spawned task
/// that touches the database opens its own transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxMode {
    Read,
    Write,
}

tokio::task_local! {
    static ACTIVE_TX: TxMode;
}

/// Refuses a way back into the pool while this task has a transaction open.
fn refuse_inside_transaction(entry: &str) -> Result<(), DbErr> {
    if !cfg!(any(test, debug_assertions)) {
        return Ok(());
    }
    match ACTIVE_TX.try_with(|mode| *mode) {
        Ok(mode) => Err(DbErr::Custom(format!(
            "{entry} went through Db while a {mode:?} transaction is open on this task; \
             use the transaction instead"
        ))),
        Err(_) => Ok(()),
    }
}

/// The connection behind a capability, for ops inside `crate::db` only.
///
/// The module is private to `crate::db`, so nothing outside can name the trait
/// and so nothing outside can call `conn`.
pub(in crate::db) mod sealed {
    use sea_orm::{ConnectionTrait, DbErr};

    pub trait Access {
        type Conn: ConnectionTrait;
        fn conn(&self) -> Result<&Self::Conn, DbErr>;
    }
}

/// Anything a read op accepts.
pub trait Read: sealed::Access + Sync {}

/// A read that sees one moment of the database: a [`ReadTx`] or a [`WriteTx`],
/// never the pool.
///
/// An op that issues more than one statement and joins their answers needs
/// this. On the pool each statement autocommits on its own snapshot, so a
/// write that lands between the first and the second is half-visible — a clip
/// in the second read whose blob was not in the first. Inside a transaction
/// SQLite (WAL) fixes the snapshot at the first read and keeps it until the
/// end, so the two answers agree. A one-statement read stays on [`Read`].
pub trait Snapshot: Read {}

/// The pool.
#[derive(Clone, Debug)]
pub struct Db(DatabaseConnection);

/// A deferred transaction. Reads only.
#[derive(Debug)]
pub struct ReadTx(DatabaseTransaction);

/// An immediate transaction, or a savepoint inside one.
#[derive(Debug)]
pub struct WriteTx(DatabaseTransaction);

impl Db {
    pub fn new(conn: DatabaseConnection) -> Self {
        Self(conn)
    }

    /// Runs `f` inside `BEGIN IMMEDIATE`: committed on `Ok`, rolled back on `Err`.
    pub async fn write<T, E>(&self, f: impl AsyncFnOnce(&WriteTx) -> Result<T, E>) -> Result<T, E>
    where
        E: From<DbErr>,
    {
        refuse_inside_transaction("Db::write")?;
        let options = TransactionOptions {
            sqlite_transaction_mode: Some(SqliteTransactionMode::Immediate),
            ..Default::default()
        };
        let tx = WriteTx(self.0.begin_with_options(options).await?);
        let out = ACTIVE_TX.scope(TxMode::Write, f(&tx)).await;
        settle(tx.0, out).await
    }

    /// Runs `f` inside `BEGIN DEFERRED`, for reads that must see one snapshot.
    pub async fn read<T, E>(&self, f: impl AsyncFnOnce(&ReadTx) -> Result<T, E>) -> Result<T, E>
    where
        E: From<DbErr>,
    {
        refuse_inside_transaction("Db::read")?;
        let options = TransactionOptions {
            sqlite_transaction_mode: Some(SqliteTransactionMode::Deferred),
            ..Default::default()
        };
        let tx = ReadTx(self.0.begin_with_options(options).await?);
        let out = ACTIVE_TX.scope(TxMode::Read, f(&tx)).await;
        settle(tx.0, out).await
    }
}

impl WriteTx {
    /// Runs `f` inside a SAVEPOINT of this transaction: released on `Ok`,
    /// rolled back to on `Err`, leaving the outer transaction open either way.
    pub async fn nested<T, E>(&self, f: impl AsyncFnOnce(&WriteTx) -> Result<T, E>) -> Result<T, E>
    where
        E: From<DbErr>,
    {
        let tx = WriteTx(self.0.begin().await?);
        let out = f(&tx).await;
        settle(tx.0, out).await
    }
}

async fn settle<T, E: From<DbErr>>(tx: DatabaseTransaction, out: Result<T, E>) -> Result<T, E> {
    match out {
        Ok(value) => {
            tx.commit().await?;
            Ok(value)
        }
        Err(error) => {
            tx.rollback().await?;
            Err(error)
        }
    }
}

impl sealed::Access for Db {
    type Conn = DatabaseConnection;
    fn conn(&self) -> Result<&DatabaseConnection, DbErr> {
        refuse_inside_transaction("a pool connection")?;
        Ok(&self.0)
    }
}

impl sealed::Access for ReadTx {
    type Conn = DatabaseTransaction;
    fn conn(&self) -> Result<&DatabaseTransaction, DbErr> {
        Ok(&self.0)
    }
}

impl sealed::Access for WriteTx {
    type Conn = DatabaseTransaction;
    fn conn(&self) -> Result<&DatabaseTransaction, DbErr> {
        Ok(&self.0)
    }
}

// `ConnectionTrait` is deliberately implemented for none of the three: that
// trait is how SeaORM writes, and implementing it would hand writing back to
// whoever holds a `Db` or a `ReadTx`.
impl Read for Db {}
impl Read for ReadTx {}
impl Read for WriteTx {}
impl Snapshot for ReadTx {}
impl Snapshot for WriteTx {}

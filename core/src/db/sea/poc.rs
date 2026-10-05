//! Phase 0 of the SeaORM migration: the experiments the plan depends on, run
//! against real files where the claim is about locking.

use std::path::Path;
use std::str::FromStr;
use std::time::{Duration, Instant};

use sea_orm::entity::prelude::*;
use sea_orm::{
    ActiveValue::Set, ConnectionTrait, DbBackend, SqliteTransactionMode, SqlxSqliteConnector, Statement,
    TransactionOptions, TransactionTrait,
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous};

use super::cap::{Db, Read, WriteTx, sealed::Access};
use crate::db::ops::conversation as diesel_conversation;
use crate::decimal::Decimal;

/// The two flags the toggle race is about, and nothing else of the row.
mod flags {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "conversations")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub is_pinned: i32,
        pub is_archived: i32,
        pub updated_at: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

async fn toggle_pin(tx: &WriteTx, id: &str, now: i64) -> Result<flags::Model, DbErr> {
    let conn = tx.conn()?;
    let row = flags::Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound(id.to_owned()))?;
    let flipped = if row.is_pinned == 0 { 1 } else { 0 };
    let mut active: flags::ActiveModel = row.into();
    active.is_pinned = Set(flipped);
    active.updated_at = Set(now);
    active.update(conn).await
}

async fn toggle_archive(tx: &WriteTx, id: &str, now: i64) -> Result<flags::Model, DbErr> {
    let conn = tx.conn()?;
    let row = flags::Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound(id.to_owned()))?;
    let flipped = if row.is_archived == 0 { 1 } else { 0 };
    let mut active: flags::ActiveModel = row.into();
    active.is_archived = Set(flipped);
    active.updated_at = Set(now);
    active.update(conn).await
}

/// A migrated file with one conversation in it, written through Diesel.
async fn diesel_file(dir: &Path) -> (std::path::PathBuf, crate::db::DbPool) {
    let path = dir.join("poc.sqlite");
    super::bridge::migrate_file(&path).await.unwrap();
    let pool = crate::db::init_db(path.to_str().unwrap());
    diesel_conversation::create_conversation(&mut pool.get().unwrap(), "c1", None, None, None, 1).unwrap();
    (path, pool)
}

/// The sqlx pool the plan describes, minus durability: `synchronous=OFF` as
/// the Diesel race test does, so the run waits on locks rather than the disk.
async fn sqlx_file(path: &Path) -> SqlitePool {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5))
        .synchronous(SqliteSynchronous::Off);
    SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .unwrap()
}

fn is_locked(error: &DbErr) -> bool {
    error.to_string().contains("database is locked")
}

#[derive(Clone, Copy)]
enum Flag {
    Pinned,
    Archived,
}

/// One toggle, asked again until it lands. A writer starved past
/// `busy_timeout` is refused and changed nothing, as in the Diesel race test.
///
/// A plain function rather than a helper taking an async closure: a closure
/// that borrows the `Db` and is passed through a generic `AsyncFnMut` bound is
/// not provably `Send` for every lifetime, so it cannot live in a spawned task.
async fn toggle_until_applied(db: &Db, flag: Flag, now: i64) {
    loop {
        let result = match flag {
            Flag::Pinned => db.write(async |tx| toggle_pin(tx, "c1", now).await).await,
            Flag::Archived => db.write(async |tx| toggle_archive(tx, "c1", now).await).await,
        };
        match result {
            Ok(_) => return,
            Err(error) if is_locked(&error) => {}
            Err(error) => panic!("{error}"),
        }
    }
}

async fn flags_of(db: &Db) -> (i32, i32) {
    let row = flags::Entity::find_by_id("c1")
        .one(db.conn().unwrap())
        .await
        .unwrap()
        .unwrap();
    (row.is_archived, row.is_pinned)
}

/// (a) The Diesel race test, ported: four tasks, one row, every toggle must land.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_toggles_are_each_applied() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _diesel) = diesel_file(dir.path()).await;
    let db = Db::new(SqlxSqliteConnector::from_sqlx_sqlite_pool(sqlx_file(&path).await));

    const ROUNDS: i64 = 2000;
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let db = db.clone();
            tokio::spawn(async move {
                for i in 0..ROUNDS {
                    toggle_until_applied(&db, Flag::Archived, i).await;
                    toggle_until_applied(&db, Flag::Pinned, i).await;
                }
            })
        })
        .collect();
    for worker in workers {
        worker.await.unwrap();
    }

    assert_eq!(
        flags_of(&db).await,
        (0, 0),
        "an even number of toggles lands back where it started"
    );
}

/// (a) What `IMMEDIATE` buys over a deferred transaction. Both keep a toggle
/// from being lost — a deferred one that read a stale snapshot is refused with
/// `SQLITE_BUSY_SNAPSHOT` rather than overwriting — so the race test above
/// cannot tell them apart, and it does not try to. The difference is who
/// waits: under `IMMEDIATE` another writer queues behind this transaction for
/// up to `busy_timeout`; under `DEFERRED` it commits in between and this
/// transaction's write fails on the spot, an error no caller here retries.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_holds_the_lock_from_its_first_read() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _diesel) = diesel_file(dir.path()).await;
    let pool = sqlx_file(&path).await;
    let db = Db::new(SqlxSqliteConnector::from_sqlx_sqlite_pool(pool.clone()));
    let read_done = std::sync::Arc::new(tokio::sync::Notify::new());

    let other = tokio::spawn({
        let read_done = read_done.clone();
        async move {
            read_done.notified().await;
            sqlx::query("UPDATE conversations SET is_pinned = 1 WHERE id = 'c1'")
                .execute(&pool)
                .await
        }
    });

    let ours = db
        .write(async |tx| {
            let row = flags::Entity::find_by_id("c1").one(tx.conn()?).await?.unwrap();
            read_done.notify_one();
            tokio::time::sleep(Duration::from_millis(300)).await;
            let mut active: flags::ActiveModel = row.into();
            active.is_archived = Set(1);
            active.update(tx.conn()?).await
        })
        .await;

    ours.expect("the other writer waited instead of committing in between");
    other.await.unwrap().expect("and then got its turn");
    assert_eq!(flags_of(&db).await, (1, 1));
}

/// (a') Why every write starts as `IMMEDIATE`: a savepoint inside a deferred
/// transaction that has already read cannot take the write lock once another
/// connection has committed, and it fails at once rather than waiting.
#[tokio::test(flavor = "multi_thread")]
async fn a_savepoint_in_a_deferred_read_fails_once_another_connection_commits() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _diesel) = diesel_file(dir.path()).await;
    let conn = SqlxSqliteConnector::from_sqlx_sqlite_pool(sqlx_file(&path).await);

    let deferred = TransactionOptions {
        sqlite_transaction_mode: Some(SqliteTransactionMode::Deferred),
        ..Default::default()
    };
    let outer = conn.begin_with_options(deferred).await.unwrap();
    flags::Entity::find_by_id("c1").one(&outer).await.unwrap();

    conn.execute_unprepared("UPDATE conversations SET is_pinned = 1 WHERE id = 'c1'")
        .await
        .unwrap();

    let inner = outer.begin().await.unwrap();
    let started = Instant::now();
    let error = inner
        .execute_unprepared("UPDATE conversations SET is_archived = 1 WHERE id = 'c1'")
        .await
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "refused at once, not after busy_timeout"
    );
    assert!(is_locked(&error), "{error}");
}

/// (a') The same sequence with the outer transaction `IMMEDIATE`: the other
/// connection's write waits for it instead of getting in between.
#[tokio::test(flavor = "multi_thread")]
async fn a_savepoint_in_an_immediate_transaction_keeps_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _diesel) = diesel_file(dir.path()).await;
    let conn = SqlxSqliteConnector::from_sqlx_sqlite_pool(sqlx_file(&path).await);

    let immediate = TransactionOptions {
        sqlite_transaction_mode: Some(SqliteTransactionMode::Immediate),
        ..Default::default()
    };
    let outer = conn.begin_with_options(immediate).await.unwrap();
    flags::Entity::find_by_id("c1").one(&outer).await.unwrap();

    let other = tokio::spawn({
        let conn = conn.clone();
        async move {
            conn.execute_unprepared("UPDATE conversations SET is_pinned = 1 WHERE id = 'c1'")
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let inner = outer.begin().await.unwrap();
    inner
        .execute_unprepared("UPDATE conversations SET is_archived = 1 WHERE id = 'c1'")
        .await
        .unwrap();
    inner.commit().await.unwrap();
    outer.commit().await.unwrap();
    other.await.unwrap().unwrap();
}

/// A private in-memory database on one connection that is never recycled: an
/// in-memory database lives exactly as long as its connection.
async fn memory_db() -> Db {
    let options = SqliteConnectOptions::from_str("sqlite::memory:").unwrap();
    let pool = SqlitePoolOptions::new()
        .min_connections(1)
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(options)
        .await
        .unwrap();
    Db::new(SqlxSqliteConnector::from_sqlx_sqlite_pool(pool))
}

/// A read op in the shape the ops will have.
async fn one(reader: &impl Read) -> Result<i64, DbErr> {
    let row = reader
        .conn()?
        .query_one_raw(Statement::from_string(DbBackend::Sqlite, "SELECT 1 AS one"))
        .await?
        .unwrap();
    row.try_get("", "one")
}

/// Asserts the guard refused at once: on this one-connection pool a second
/// acquire would otherwise wait for the connection the transaction holds.
fn assert_refused<T: std::fmt::Debug>(result: Result<Result<T, DbErr>, tokio::time::error::Elapsed>) {
    let error = result
        .expect("refused at once, not after waiting for a connection")
        .unwrap_err();
    assert!(error.to_string().contains("went through Db"), "{error}");
}

#[tokio::test]
async fn going_back_to_the_pool_inside_a_transaction_is_refused() {
    let db = memory_db().await;
    let quick = Duration::from_secs(1);

    db.write(async |_tx| {
        assert_refused(tokio::time::timeout(quick, one(&db)).await);
        assert_refused(tokio::time::timeout(quick, db.write(async |_| Ok::<_, DbErr>(()))).await);
        Ok::<_, DbErr>(())
    })
    .await
    .unwrap();

    db.read(async |_tx| {
        assert_refused(tokio::time::timeout(quick, one(&db)).await);
        assert_refused(tokio::time::timeout(quick, db.write(async |_| Ok::<_, DbErr>(()))).await);
        Ok::<_, DbErr>(())
    })
    .await
    .unwrap();

    // And the transaction itself, and the pool once it is closed, still work.
    assert_eq!(db.write(async |tx| one(tx).await).await.unwrap(), 1);
    assert_eq!(db.read(async |tx| one(tx).await).await.unwrap(), 1);
    assert_eq!(one(&db).await.unwrap(), 1);
}

mod prices {
    use sea_orm::entity::prelude::*;

    use crate::decimal::Decimal;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "prices")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub amount: Option<Decimal>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// (b) Money survives the trip at its full eighteen fractional digits, is
/// stored as TEXT, and a stored value that is not canonical fails the row.
#[tokio::test]
async fn decimals_round_trip_as_canonical_text() {
    let db = memory_db().await;
    let widest = "99999999999999999999.999999999999999999";
    db.write(async |tx| {
        let conn = tx.conn()?;
        conn.execute_unprepared("CREATE TABLE prices (id TEXT PRIMARY KEY NOT NULL, amount TEXT)")
            .await?;
        for (id, amount) in [
            ("wide", Some(widest)),
            ("small", Some("0.000000000000000001")),
            ("none", None),
        ] {
            prices::ActiveModel {
                id: Set(id.to_owned()),
                amount: Set(amount.map(|raw| Decimal::from_str(raw).unwrap())),
            }
            .insert(conn)
            .await?;
        }
        Ok::<_, DbErr>(())
    })
    .await
    .unwrap();

    let conn = db.conn().unwrap();
    let read = |id: &'static str| async move { prices::Entity::find_by_id(id).one(conn).await };
    let wide = read("wide").await.unwrap().unwrap().amount.unwrap();
    assert_eq!(wide.canonical(), widest);
    let small = read("small").await.unwrap().unwrap().amount.unwrap();
    assert_eq!(small.canonical(), "0.000000000000000001");
    assert_eq!(read("none").await.unwrap().unwrap().amount, None);

    let types: Vec<String> = conn
        .query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT typeof(amount) AS t FROM prices WHERE amount IS NOT NULL",
        ))
        .await
        .unwrap()
        .iter()
        .map(|row| row.try_get("", "t").unwrap())
        .collect();
    assert_eq!(types, ["text", "text"]);

    db.write(async |tx| {
        tx.conn()?
            .execute_unprepared("INSERT INTO prices (id, amount) VALUES ('loose', '1.250')")
            .await
    })
    .await
    .unwrap();
    let error = read("loose").await.unwrap_err();
    assert!(error.to_string().contains("canonical"), "{error}");
}

/// (c) The claim the whole incremental plan rests on: a Diesel transaction and
/// a SeaORM one on the same file are serialised by SQLite's own lock, whichever
/// pool they came from. Two Diesel threads and two SeaORM tasks toggle the same
/// row; every toggle must land.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn diesel_and_seaorm_writers_on_one_file_do_not_lose_each_others_writes() {
    use diesel::RunQueryDsl;

    let dir = tempfile::tempdir().unwrap();
    let (path, diesel) = diesel_file(dir.path()).await;
    let pool = sqlx_file(&path).await;
    let db = Db::new(SqlxSqliteConnector::from_sqlx_sqlite_pool(pool.clone()));

    const ROUNDS: i64 = 1000;
    let diesel_workers: Vec<_> = (0..2)
        .map(|_| {
            let diesel = diesel.clone();
            tokio::task::spawn_blocking(move || {
                let mut conn = diesel.get().unwrap();
                diesel::sql_query("PRAGMA synchronous=OFF").execute(&mut conn).unwrap();
                let mut until_applied = |op: &mut dyn FnMut(&mut _) -> diesel::QueryResult<_>| loop {
                    match op(&mut conn) {
                        Ok(_) => return,
                        Err(diesel::result::Error::DatabaseError(_, info))
                            if info.message() == "database is locked" => {}
                        Err(error) => panic!("{error}"),
                    }
                };
                for i in 0..ROUNDS {
                    until_applied(&mut |conn| diesel_conversation::toggle_archive(conn, "c1", i));
                    until_applied(&mut |conn| diesel_conversation::toggle_pin(conn, "c1", i));
                }
            })
        })
        .collect();
    let sea_workers: Vec<_> = (0..2)
        .map(|_| {
            let db = db.clone();
            tokio::spawn(async move {
                for i in 0..ROUNDS {
                    toggle_until_applied(&db, Flag::Archived, i).await;
                    toggle_until_applied(&db, Flag::Pinned, i).await;
                }
            })
        })
        .collect();
    for worker in diesel_workers {
        worker.await.unwrap();
    }
    for worker in sea_workers {
        worker.await.unwrap();
    }
    assert_eq!(flags_of(&db).await, (0, 0), "an even number of toggles from each side");

    // Every connection the sqlx pool hands out has foreign keys on and sees WAL.
    let mut held = Vec::new();
    for _ in 0..5 {
        let mut conn = pool.acquire().await.unwrap();
        let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!((fk, mode.as_str()), (1, "wal"));
        held.push(conn);
    }
}

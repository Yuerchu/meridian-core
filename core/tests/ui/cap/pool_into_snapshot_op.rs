// The pool cannot be handed to a read that joins two statements: each would
// autocommit on its own snapshot. Such a read starts with `Db::read`.
use meridian_core::db::sea::cap::{Db, Snapshot};

async fn joined_read(_: &impl Snapshot) {}

async fn caller(db: &Db) {
    joined_read(db).await;
}

fn main() {}

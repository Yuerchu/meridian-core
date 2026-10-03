// The pool cannot be handed to a write op: a write starts with `Db::write`.
use meridian_core::db::sea::cap::{Db, WriteTx};

async fn write_op(_: &WriteTx) {}

async fn caller(db: &Db) {
    write_op(db).await;
}

fn main() {}

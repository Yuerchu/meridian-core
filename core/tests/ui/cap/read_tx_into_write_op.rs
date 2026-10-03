// A deferred read transaction cannot be handed to a write op.
use meridian_core::db::sea::cap::{Db, WriteTx};

async fn write_op(_: &WriteTx) {}

async fn caller(db: &Db) {
    let _ = db
        .read(async |tx| {
            write_op(tx).await;
            Ok::<_, sea_orm::DbErr>(())
        })
        .await;
}

fn main() {}

// A read transaction is not a SeaORM connection, so raw SQL cannot run on it.
use meridian_core::db::sea::cap::Db;
use sea_orm::ConnectionTrait;

async fn caller(db: &Db) {
    let _ = db
        .read(async |tx| {
            ConnectionTrait::execute_unprepared(tx, "DELETE FROM conversations").await?;
            Ok::<_, sea_orm::DbErr>(())
        })
        .await;
}

fn main() {}

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::cached_model::{CachedModelInsert, CachedModelRow};
use crate::db::schema::cached_models;

pub fn list_by_provider(conn: &mut SqliteConnection, pid: &str) -> QueryResult<Vec<CachedModelRow>> {
    cached_models::table
        .filter(cached_models::provider_id.eq(pid))
        .order(cached_models::model_id.asc())
        .load::<CachedModelRow>(conn)
}

pub fn replace_models(conn: &mut SqliteConnection, pid: &str, models: &[CachedModelInsert]) -> QueryResult<()> {
    conn.transaction(|conn| {
        diesel::delete(cached_models::table.filter(cached_models::provider_id.eq(pid))).execute(conn)?;
        diesel::insert_into(cached_models::table).values(models).execute(conn)?;
        Ok(())
    })
}

pub fn delete_by_provider(conn: &mut SqliteConnection, pid: &str) -> QueryResult<usize> {
    diesel::delete(cached_models::table.filter(cached_models::provider_id.eq(pid))).execute(conn)
}

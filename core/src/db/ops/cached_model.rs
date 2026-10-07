//! What is left of the Diesel cached-model ops: the two that run on a Diesel
//! connection beside other Diesel work. `list_by_provider` is read by
//! `agent::sub_agents::catalog` with the provider and model-config reads;
//! `delete_by_provider` runs inside the provider commands' plan-barrier
//! transactions. Everything else is `db::sea::ops::cached_model`.

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::cached_model::CachedModelRow;
use crate::db::schema::cached_models;

pub fn list_by_provider(conn: &mut SqliteConnection, pid: &str) -> QueryResult<Vec<CachedModelRow>> {
    cached_models::table
        .filter(cached_models::provider_id.eq(pid))
        .order(cached_models::model_id.asc())
        .load::<CachedModelRow>(conn)
}

pub fn delete_by_provider(conn: &mut SqliteConnection, pid: &str) -> QueryResult<usize> {
    diesel::delete(cached_models::table.filter(cached_models::provider_id.eq(pid))).execute(conn)
}

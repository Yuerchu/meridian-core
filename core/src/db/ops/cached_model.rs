//! What is left of the Diesel cached-model ops: `list_by_provider`, read by
//! `agent::sub_agents::catalog` on a Diesel connection beside the provider and
//! model-config reads. Everything else is `db::sea::ops::cached_model`.

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

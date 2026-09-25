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

/// What is cached for one provider, and nothing else: no request leaves the
/// machine. For the settings page, which only needs to say which configured
/// models the provider's list names, and must not make a network request (or
/// report an offline error) merely because somebody opened it.
///
/// **An empty list is an answer, not a failure** — nothing has been fetched
/// yet. The provider's absence is the one error, and it is checked in the same
/// transaction so the two cannot be confused: `NotFound` never means "empty".
pub fn list_cached_for_provider(conn: &mut SqliteConnection, pid: &str) -> QueryResult<Vec<CachedModelRow>> {
    use crate::db::schema::providers;
    conn.transaction(|conn| {
        providers::table
            .filter(providers::id.eq(pid))
            .select(providers::id)
            .first::<String>(conn)?;
        list_by_provider(conn, pid)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::provider::ProviderInsert;

    fn provider(conn: &mut SqliteConnection, id: &str) {
        crate::db::ops::provider::create_provider(
            conn,
            &ProviderInsert {
                id,
                name: "P",
                provider_type: "openai",
                base_url: "https://example.invalid",
                is_enabled: 1,
                sort_order: 0,
                created_at: 0,
                updated_at: 0,
                api_format: "chat_completions",
                catalog_id: None,
                credential_kind: "api_key",
                transport_profile: "standard",
                icon: None,
                codex_request_shape: 0,
            },
        )
        .unwrap();
    }

    #[test]
    fn nothing_fetched_yet_is_an_empty_list() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        provider(&mut conn, "p1");
        assert!(list_cached_for_provider(&mut conn, "p1").unwrap().is_empty());
    }

    #[test]
    fn an_unknown_provider_is_an_error_and_not_an_empty_cache() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        assert_eq!(
            list_cached_for_provider(&mut conn, "missing").unwrap_err(),
            diesel::result::Error::NotFound
        );
    }

    #[test]
    fn the_cache_is_read_for_that_provider_only_in_model_order() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        provider(&mut conn, "p1");
        provider(&mut conn, "p2");
        let row = |pid, model| CachedModelInsert {
            provider_id: pid,
            model_id: model,
            model_name: model,
            fetched_at: 7,
        };
        replace_models(&mut conn, "p1", &[row("p1", "zeta"), row("p1", "alpha")]).unwrap();
        replace_models(&mut conn, "p2", &[row("p2", "other")]).unwrap();

        let got: Vec<_> = list_cached_for_provider(&mut conn, "p1")
            .unwrap()
            .into_iter()
            .map(|r| (r.model_id, r.fetched_at))
            .collect();
        assert_eq!(got, [("alpha".to_string(), 7), ("zeta".to_string(), 7)]);
    }
}

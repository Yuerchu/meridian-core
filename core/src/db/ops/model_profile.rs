use diesel::dsl::count_star;
use diesel::prelude::*;

use crate::db::models::model_profile::{ModelProfileChangeset, ModelProfileInsert, ModelProfileRow};
use crate::db::schema::{model_configs, model_profiles};

pub fn get(conn: &mut SqliteConnection, id: &str) -> QueryResult<Option<ModelProfileRow>> {
    model_profiles::table.find(id).first(conn).optional()
}

/// Every profile with the number of provider rows pointing at it.
///
/// The count is what the editor needs to say "changing this affects three
/// providers" before somebody edits a window shared by all of them, so it is
/// answered here in one grouped query rather than by asking per row.
pub fn list_with_model_counts(conn: &mut SqliteConnection) -> QueryResult<Vec<(ModelProfileRow, i64)>> {
    let profiles: Vec<ModelProfileRow> = model_profiles::table.order(model_profiles::name.asc()).load(conn)?;
    let counts: Vec<(String, i64)> = model_configs::table
        .group_by(model_configs::profile_id)
        .select((model_configs::profile_id, count_star()))
        .load(conn)?;
    Ok(profiles
        .into_iter()
        .map(|profile| {
            let count = counts
                .iter()
                .find(|(id, _)| *id == profile.id)
                .map_or(0, |(_, count)| *count);
            (profile, count)
        })
        .collect())
}

pub fn insert(conn: &mut SqliteConnection, new: &ModelProfileInsert) -> QueryResult<ModelProfileRow> {
    diesel::insert_into(model_profiles::table).values(new).execute(conn)?;
    model_profiles::table.find(new.id).first(conn)
}

pub fn update(conn: &mut SqliteConnection, id: &str, changes: &ModelProfileChangeset) -> QueryResult<ModelProfileRow> {
    diesel::update(model_profiles::table.find(id))
        .set(changes)
        .execute(conn)?;
    model_profiles::table.find(id).first(conn)
}

/// Drops a profile only while nothing points at it, and says whether it went.
///
/// The column carries no `ON DELETE` on purpose: a config without a profile has
/// no context window to run a turn with, so the database refuses rather than
/// cascading or nulling. This is the collector for the other side of that —
/// when the last model moves to a different profile, the one it left behind has
/// nothing to describe and would otherwise sit in the picker for ever.
pub fn delete_if_unreferenced(conn: &mut SqliteConnection, id: &str) -> QueryResult<bool> {
    let referenced: i64 = model_configs::table
        .filter(model_configs::profile_id.eq(id))
        .select(count_star())
        .first(conn)?;
    if referenced > 0 {
        return Ok(false);
    }
    Ok(diesel::delete(model_profiles::table.find(id)).execute(conn)? > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::model_config::ModelConfigInsert;
    use crate::db::models::provider::ProviderInsert;
    use crate::db::test_db;

    fn profile<'a>(id: &'a str, name: &'a str) -> ModelProfileInsert<'a> {
        ModelProfileInsert {
            id,
            name,
            context_window: 128_000,
            compact_threshold: 100_000,
            max_output_tokens: None,
            input_price: None,
            output_price: None,
            cache_read_price: None,
            cache_write_price: None,
            pricing_tiers: None,
            capability_overrides: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn seed_provider(conn: &mut SqliteConnection, id: &str) {
        diesel::insert_into(crate::db::schema::providers::table)
            .values(&ProviderInsert {
                id,
                name: "Acme",
                provider_type: "anthropic",
                base_url: "https://example.invalid",
                is_enabled: 1,
                sort_order: 0,
                created_at: 0,
                updated_at: 0,
                api_format: "messages",
                catalog_id: None,
                credential_kind: "api_key",
                transport_profile: "standard",
                icon: None,
                codex_request_shape: 0,
            })
            .execute(conn)
            .unwrap();
    }

    fn point_at(conn: &mut SqliteConnection, id: &str, provider_id: &str, profile_id: &str) {
        diesel::insert_into(model_configs::table)
            .values(&ModelConfigInsert {
                id,
                provider_id,
                model_id: "claude-sonnet-5",
                profile_id,
                overrides_pricing: false,
                input_price: None,
                output_price: None,
                cache_read_price: None,
                cache_write_price: None,
                pricing_tiers: None,
                server_tools: None,
                server_tool_price: None,
                created_at: 0,
                updated_at: 0,
            })
            .execute(conn)
            .unwrap();
    }

    /// The count is the whole reason the list is not a plain `load`.
    #[test]
    fn a_profile_carries_how_many_providers_reach_it() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_provider(&mut conn, "p1");
        seed_provider(&mut conn, "p2");
        insert(&mut conn, &profile("shared", "Claude Sonnet 5")).unwrap();
        insert(&mut conn, &profile("lonely", "Zephyr")).unwrap();
        point_at(&mut conn, "mc1", "p1", "shared");
        point_at(&mut conn, "mc2", "p2", "shared");

        let listed = list_with_model_counts(&mut conn).unwrap();
        let counts: Vec<(String, i64)> = listed.into_iter().map(|(p, n)| (p.id, n)).collect();
        assert_eq!(counts, vec![("shared".to_string(), 2), ("lonely".to_string(), 0)]);
    }

    /// Collecting the profile a model just left, and refusing to collect one
    /// that is still somebody's only description of a model.
    #[test]
    fn a_profile_is_collected_only_once_nothing_points_at_it() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_provider(&mut conn, "p1");
        insert(&mut conn, &profile("held", "Held")).unwrap();
        insert(&mut conn, &profile("free", "Free")).unwrap();
        point_at(&mut conn, "mc1", "p1", "held");

        assert!(!delete_if_unreferenced(&mut conn, "held").unwrap());
        assert!(get(&mut conn, "held").unwrap().is_some());
        assert!(delete_if_unreferenced(&mut conn, "free").unwrap());
        assert!(get(&mut conn, "free").unwrap().is_none());
    }

    /// An edit that clears a price has to clear it. Diesel's default reading of
    /// a `None` in a changeset is "leave this column alone", which would keep
    /// billing at a rate the user just deleted.
    #[test]
    fn an_edit_can_empty_a_price_again() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        insert(
            &mut conn,
            &ModelProfileInsert {
                input_price: Some("3".parse().unwrap()),
                output_price: Some("15".parse().unwrap()),
                capability_overrides: Some(r#"{"supports_fast":true}"#),
                ..profile("p", "Claude")
            },
        )
        .unwrap();

        let after = update(
            &mut conn,
            "p",
            &ModelProfileChangeset {
                name: "Claude",
                context_window: 200_000,
                compact_threshold: 150_000,
                max_output_tokens: None,
                input_price: None,
                output_price: None,
                cache_read_price: None,
                cache_write_price: None,
                pricing_tiers: None,
                capability_overrides: None,
                updated_at: 7,
            },
        )
        .unwrap();

        assert_eq!(after.input_price, None);
        assert_eq!(after.output_price, None);
        assert_eq!(after.capability_overrides, None);
        assert_eq!(after.context_window, 200_000);
        assert_eq!(after.updated_at, 7);
    }
}

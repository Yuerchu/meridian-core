//! What is left of the Diesel model-config ops: the two joined reads behind
//! `agent::model_config::load` (the turn loop, the price snapshot, the usage
//! report) and the sub-agent catalog, and `seed_flat` for the Diesel tests.
//! Writes are SeaORM's (`db::sea::ops::model_config`).

use diesel::prelude::*;

use crate::db::models::model_config::ModelConfigRow;
use crate::db::models::model_profile::ModelProfileRow;
use crate::db::schema::{model_configs, model_profiles};

/// A provider's models with the profile each one describes.
///
/// Everything that wants a row wants the profile too — there is no context
/// window, no capability patch and usually no price without it — so the join is
/// the ordinary read and the bare `list_by_provider` above is the exception.
pub fn list_by_provider_with_profiles(
    conn: &mut SqliteConnection,
    provider_id: &str,
) -> QueryResult<Vec<(ModelConfigRow, ModelProfileRow)>> {
    model_configs::table
        .inner_join(model_profiles::table)
        .filter(model_configs::provider_id.eq(provider_id))
        .order(model_configs::model_id.asc())
        .select((ModelConfigRow::as_select(), ModelProfileRow::as_select()))
        .load(conn)
}

pub fn get_with_profile(
    conn: &mut SqliteConnection,
    provider_id: &str,
    model_id: &str,
) -> QueryResult<Option<(ModelConfigRow, ModelProfileRow)>> {
    model_configs::table
        .inner_join(model_profiles::table)
        .filter(model_configs::provider_id.eq(provider_id))
        .filter(model_configs::model_id.eq(model_id))
        .select((ModelConfigRow::as_select(), ModelProfileRow::as_select()))
        .first(conn)
        .optional()
}

/// The flat shape model configuration used to have, for tests that are about
/// something else.
///
/// Every price used to live on one row, and dozens of tests seed a model in
/// passing on the way to asserting about audit rows, usage reports or turn
/// parameters. Rewriting each of them to insert a profile and then a config
/// would bury what they are actually testing under six lines of setup, so the
/// old shape survives here as a seeder: prices, window and capability patch go
/// to a profile, the provider-side tools stay on the row, and nothing
/// overrides. Tests about the split itself build the two rows explicitly.
#[cfg(any(test, feature = "test-support"))]
pub struct FlatModelConfig<'a> {
    pub id: &'a str,
    pub provider_id: &'a str,
    pub model_id: &'a str,
    pub display_name: Option<&'a str>,
    pub context_window: i32,
    pub compact_threshold: i32,
    pub max_output_tokens: Option<i32>,
    pub input_price: Option<crate::decimal::Decimal>,
    pub output_price: Option<crate::decimal::Decimal>,
    pub cache_read_price: Option<crate::decimal::Decimal>,
    pub cache_write_price: Option<crate::decimal::Decimal>,
    pub capability_overrides: Option<&'a str>,
    pub pricing_tiers: Option<&'a str>,
    pub server_tools: Option<&'a str>,
    pub server_tool_price: Option<crate::decimal::Decimal>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[cfg(any(test, feature = "test-support"))]
impl<'a> Default for FlatModelConfig<'a> {
    fn default() -> Self {
        Self {
            id: "mc1",
            provider_id: "p1",
            model_id: "m1",
            display_name: None,
            context_window: 128_000,
            compact_threshold: 100_000,
            max_output_tokens: None,
            input_price: None,
            output_price: None,
            cache_read_price: None,
            cache_write_price: None,
            capability_overrides: None,
            pricing_tiers: None,
            server_tools: None,
            server_tool_price: None,
            created_at: 0,
            updated_at: 0,
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn seed_flat(conn: &mut SqliteConnection, flat: &FlatModelConfig) -> QueryResult<ModelConfigRow> {
    use crate::db::models::model_config::ModelConfigInsert;
    use crate::db::models::model_profile::{ModelProfileChangeset, ModelProfileInsert};
    let profile_id = format!("{}-profile", flat.id);
    let name = flat.display_name.unwrap_or(flat.model_id);
    // Seeding the same model twice is a price change, which is what several
    // callers are testing — so both rows are written, not just inserted.
    diesel::insert_into(model_profiles::table)
        .values(&ModelProfileInsert {
            id: &profile_id,
            name,
            context_window: flat.context_window,
            compact_threshold: flat.compact_threshold,
            max_output_tokens: flat.max_output_tokens,
            input_price: flat.input_price.clone(),
            output_price: flat.output_price.clone(),
            cache_read_price: flat.cache_read_price.clone(),
            cache_write_price: flat.cache_write_price.clone(),
            pricing_tiers: flat.pricing_tiers,
            capability_overrides: flat.capability_overrides,
            created_at: flat.created_at,
            updated_at: flat.updated_at,
        })
        .on_conflict(model_profiles::id)
        .do_update()
        .set(&ModelProfileChangeset {
            name,
            context_window: flat.context_window,
            compact_threshold: flat.compact_threshold,
            max_output_tokens: flat.max_output_tokens,
            input_price: flat.input_price.clone(),
            output_price: flat.output_price.clone(),
            cache_read_price: flat.cache_read_price.clone(),
            cache_write_price: flat.cache_write_price.clone(),
            pricing_tiers: flat.pricing_tiers,
            capability_overrides: flat.capability_overrides,
            updated_at: flat.updated_at,
        })
        .execute(conn)?;
    diesel::insert_into(model_configs::table)
        .values(&ModelConfigInsert {
            id: flat.id,
            provider_id: flat.provider_id,
            model_id: flat.model_id,
            profile_id: &profile_id,
            overrides_pricing: false,
            input_price: None,
            output_price: None,
            cache_read_price: None,
            cache_write_price: None,
            pricing_tiers: None,
            server_tools: flat.server_tools,
            server_tool_price: flat.server_tool_price.clone(),
            created_at: flat.created_at,
            updated_at: flat.updated_at,
        })
        .on_conflict((model_configs::provider_id, model_configs::model_id))
        .do_update()
        .set((
            model_configs::profile_id.eq(&profile_id),
            model_configs::server_tools.eq(flat.server_tools),
            model_configs::server_tool_price.eq(flat.server_tool_price.as_ref()),
            model_configs::updated_at.eq(flat.updated_at),
        ))
        .execute(conn)?;
    model_configs::table
        .filter(model_configs::provider_id.eq(flat.provider_id))
        .filter(model_configs::model_id.eq(flat.model_id))
        .first(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::diesel_test_db;
    use crate::db::models::provider::ProviderInsert;

    fn seed_provider(conn: &mut SqliteConnection) {
        diesel::insert_into(crate::db::schema::providers::table)
            .values(&ProviderInsert {
                id: "p1",
                name: "Acme",
                provider_type: "xai",
                base_url: "https://example.invalid",
                is_enabled: 1,
                sort_order: 0,
                created_at: 0,
                updated_at: 0,
                api_format: "responses",
                catalog_id: None,
                credential_kind: "api_key",
                transport_profile: "standard",
                icon: None,
                codex_request_shape: 0,
            })
            .execute(conn)
            .unwrap();
    }

    /// The join the Diesel readers still want; and seeding the same model
    /// again moves its price on the one row rather than adding a second.
    #[test]
    fn a_model_reads_back_beside_the_profile_that_describes_it() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        seed_provider(&mut conn);
        let flat = FlatModelConfig {
            provider_id: "p1",
            model_id: "grok-4.6",
            ..FlatModelConfig::default()
        };
        seed_flat(&mut conn, &flat).unwrap();
        seed_flat(
            &mut conn,
            &FlatModelConfig {
                input_price: Some("3".parse().unwrap()),
                server_tools: Some(r#"["web_search"]"#),
                updated_at: 5,
                ..flat
            },
        )
        .unwrap();

        let (config, profile) = get_with_profile(&mut conn, "p1", "grok-4.6").unwrap().unwrap();
        assert_eq!(config.id, "mc1");
        assert_eq!(config.server_tools.as_deref(), Some(r#"["web_search"]"#));
        assert_eq!(
            (profile.context_window, profile.input_price),
            (128_000, Some("3".parse().unwrap()))
        );
        assert_eq!(list_by_provider_with_profiles(&mut conn, "p1").unwrap().len(), 1);
    }
}

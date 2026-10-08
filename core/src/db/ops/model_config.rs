//! What is left of the Diesel model-config ops: the two joined reads behind
//! `agent::model_config::load` (the turn loop, the price snapshot, the usage
//! report) and the sub-agent catalog, and `seed_flat` for the Diesel tests.
//! Writes are SeaORM's (`db::sea::ops::model_config`).

use diesel::prelude::*;

use crate::db::models::model_config::ModelConfigRow;
use crate::db::models::model_profile::ModelProfileRow;
use crate::db::schema::{model_configs, model_profiles};

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
pub use crate::db::sea::ops::model_config::FlatModelConfig;

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

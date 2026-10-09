//! What is left of the Diesel model-config ops: the two joined reads behind
//! `agent::model_config::load` (the turn loop, the price snapshot, the usage
//! report) and the sub-agent catalog.
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

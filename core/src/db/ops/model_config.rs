use diesel::prelude::*;

use crate::db::models::model_config::{ModelConfigInsert, ModelConfigRow};
use crate::db::models::model_profile::ModelProfileRow;
use crate::db::ops::model_profile;
use crate::db::schema::{model_configs, model_profiles};

pub fn get(conn: &mut SqliteConnection, id: &str) -> QueryResult<Option<ModelConfigRow>> {
    model_configs::table.find(id).first(conn).optional()
}

pub fn list_by_provider(conn: &mut SqliteConnection, provider_id: &str) -> QueryResult<Vec<ModelConfigRow>> {
    model_configs::table
        .filter(model_configs::provider_id.eq(provider_id))
        .order(model_configs::model_id.asc())
        .load(conn)
}

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

pub fn get_by_provider_and_model(
    conn: &mut SqliteConnection,
    provider_id: &str,
    model_id: &str,
) -> QueryResult<Option<ModelConfigRow>> {
    model_configs::table
        .filter(model_configs::provider_id.eq(provider_id))
        .filter(model_configs::model_id.eq(model_id))
        .first(conn)
        .optional()
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

pub fn upsert(conn: &mut SqliteConnection, new: &ModelConfigInsert) -> QueryResult<ModelConfigRow> {
    let existing = model_configs::table
        .filter(model_configs::provider_id.eq(new.provider_id))
        .filter(model_configs::model_id.eq(new.model_id))
        .first::<ModelConfigRow>(conn)
        .optional()?;

    if let Some(existing) = existing {
        diesel::update(model_configs::table.find(&existing.id))
            .set((
                model_configs::profile_id.eq(new.profile_id),
                model_configs::overrides_pricing.eq(new.overrides_pricing),
                model_configs::input_price.eq(new.input_price.as_ref()),
                model_configs::output_price.eq(new.output_price.as_ref()),
                model_configs::cache_read_price.eq(new.cache_read_price.as_ref()),
                model_configs::cache_write_price.eq(new.cache_write_price.as_ref()),
                model_configs::pricing_tiers.eq(new.pricing_tiers),
                model_configs::server_tools.eq(new.server_tools),
                model_configs::server_tool_price.eq(new.server_tool_price.as_ref()),
                model_configs::updated_at.eq(new.updated_at),
            ))
            .execute(conn)?;
        // The profile it was pointing at a moment ago may now describe nothing.
        if existing.profile_id != new.profile_id {
            model_profile::delete_if_unreferenced(conn, &existing.profile_id)?;
        }
        model_configs::table.find(&existing.id).first(conn)
    } else {
        diesel::insert_into(model_configs::table).values(new).execute(conn)?;
        model_configs::table.find(new.id).first(conn)
    }
}

/// Points one model at a different profile, collecting the one it left.
///
/// Both halves in one call because they are one decision: a caller that moves
/// the pointer and forgets to collect leaves a profile nothing describes in
/// front of everybody choosing one, and a caller that collects first has
/// nothing left to move.
pub fn replace_profile(conn: &mut SqliteConnection, config_id: &str, profile_id: &str, now: i64) -> QueryResult<bool> {
    let Some(existing) = get(conn, config_id)? else {
        return Ok(false);
    };
    if existing.profile_id == profile_id {
        return Ok(false);
    }
    diesel::update(model_configs::table.find(config_id))
        .set((
            model_configs::profile_id.eq(profile_id),
            model_configs::updated_at.eq(now),
        ))
        .execute(conn)?;
    model_profile::delete_if_unreferenced(conn, &existing.profile_id)?;
    Ok(true)
}

pub fn delete(conn: &mut SqliteConnection, id: &str) -> QueryResult<usize> {
    let profile_id = get(conn, id)?.map(|row| row.profile_id);
    let deleted = diesel::delete(model_configs::table.find(id)).execute(conn)?;
    if let Some(profile_id) = profile_id {
        model_profile::delete_if_unreferenced(conn, &profile_id)?;
    }
    Ok(deleted)
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
    use crate::db::models::model_profile::ModelProfileInsert;
    let profile_id = format!("{}-profile", flat.id);
    // Seeding the same model twice is a price change, which is what several
    // callers are testing — so the profile is written, not inserted.
    let profile = ModelProfileInsert {
        id: &profile_id,
        name: flat.display_name.unwrap_or(flat.model_id),
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
    };
    if crate::db::ops::model_profile::get(conn, &profile_id)?.is_some() {
        crate::db::ops::model_profile::update(
            conn,
            &profile_id,
            &crate::db::models::model_profile::ModelProfileChangeset {
                name: profile.name,
                context_window: profile.context_window,
                compact_threshold: profile.compact_threshold,
                max_output_tokens: profile.max_output_tokens,
                input_price: profile.input_price.clone(),
                output_price: profile.output_price.clone(),
                cache_read_price: profile.cache_read_price.clone(),
                cache_write_price: profile.cache_write_price.clone(),
                pricing_tiers: profile.pricing_tiers,
                capability_overrides: profile.capability_overrides,
                updated_at: profile.updated_at,
            },
        )?;
    } else {
        crate::db::ops::model_profile::insert(conn, &profile)?;
    }
    upsert(
        conn,
        &ModelConfigInsert {
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
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::model_profile::ModelProfileInsert;
    use crate::db::models::provider::ProviderInsert;
    use crate::db::test_db;

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
            })
            .execute(conn)
            .unwrap();
    }

    fn seed_profile(conn: &mut SqliteConnection, id: &str) {
        model_profile::insert(
            conn,
            &ModelProfileInsert {
                id,
                name: id,
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
            },
        )
        .unwrap();
    }

    fn decimal(raw: &str) -> crate::decimal::Decimal {
        raw.parse().unwrap()
    }

    fn blank<'a>() -> ModelConfigInsert<'a> {
        ModelConfigInsert {
            id: "mc1",
            provider_id: "p1",
            model_id: "grok-4.6",
            profile_id: "prof1",
            overrides_pricing: false,
            input_price: None,
            output_price: None,
            cache_read_price: None,
            cache_write_price: None,
            pricing_tiers: None,
            server_tools: None,
            server_tool_price: None,
            created_at: 100,
            updated_at: 100,
        }
    }

    /// Every column an edit was given has to actually land.
    ///
    /// A whole-struct comparison on purpose. The update is a hand-written column
    /// list, and that list is exactly where a new column gets silently
    /// forgotten: `pricing_tiers` and `server_tools` were both added to the table,
    /// the model, the command and the form — and saved correctly the first time
    /// a model was configured, because that path inserts the whole row. Every
    /// edit after that dropped them, with no error anywhere. The switch simply
    /// went back to off, which reads as the feature not working.
    ///
    /// Field-by-field assertions would have missed it the same way the omission
    /// did. Comparing the row as a whole is what makes the next added column
    /// fail here instead of in somebody's settings panel.
    #[test]
    fn an_edit_writes_every_column_it_was_given() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_provider(&mut conn);
        seed_profile(&mut conn, "prof1");
        upsert(&mut conn, &blank()).unwrap();

        let edited = ModelConfigInsert {
            overrides_pricing: true,
            input_price: Some(decimal("2")),
            output_price: Some(decimal("6")),
            cache_read_price: Some(decimal("0.5")),
            cache_write_price: Some(decimal("1.5")),
            pricing_tiers: Some(
                r#"[{"min_prompt_tokens":200000,"input_price":"4","output_price":"12","cache_read_price":null,"cache_write_price":null}]"#,
            ),
            server_tools: Some(r#"["web_search"]"#),
            server_tool_price: Some(decimal("5")),
            // The two an update must *not* move, whatever it is passed.
            created_at: 999,
            updated_at: 200,
            ..blank()
        };
        let after = upsert(&mut conn, &edited).unwrap();

        assert_eq!(
            after,
            ModelConfigRow {
                id: "mc1".into(),
                provider_id: "p1".into(),
                model_id: "grok-4.6".into(),
                profile_id: "prof1".into(),
                overrides_pricing: true,
                input_price: Some(decimal("2")),
                output_price: Some(decimal("6")),
                cache_read_price: Some(decimal("0.5")),
                cache_write_price: Some(decimal("1.5")),
                pricing_tiers: Some(
                    r#"[{"min_prompt_tokens":200000,"input_price":"4","output_price":"12","cache_read_price":null,"cache_write_price":null}]"#.into(),
                ),
                server_tools: Some(r#"["web_search"]"#.into()),
                server_tool_price: Some(decimal("5")),
                // When the row first appeared, not when it was last touched.
                created_at: 100,
                updated_at: 200,
            },
        );
    }

    /// Clearing a setting has to clear it. An update that only ever writes
    /// `Some` leaves the old value behind, and the switch the user just turned
    /// off goes on being sent.
    #[test]
    fn an_edit_can_empty_a_column_again() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_provider(&mut conn);
        seed_profile(&mut conn, "prof1");
        upsert(
            &mut conn,
            &ModelConfigInsert {
                overrides_pricing: true,
                server_tools: Some(r#"["web_search"]"#),
                pricing_tiers: Some(
                    r#"[{"min_prompt_tokens":200000,"input_price":"4","output_price":"12","cache_read_price":null,"cache_write_price":null}]"#,
                ),
                cache_read_price: Some(decimal("0.5")),
                ..blank()
            },
        )
        .unwrap();

        let cleared = upsert(&mut conn, &blank()).unwrap();
        assert!(!cleared.overrides_pricing);
        assert_eq!(cleared.server_tools, None);
        assert_eq!(cleared.server_tool_price, None);
        assert_eq!(cleared.pricing_tiers, None);
        assert_eq!(cleared.cache_read_price, None);
    }

    /// Moving the last model off a profile collects it; moving one off a shared
    /// profile leaves it standing for the others.
    #[test]
    fn moving_a_model_collects_the_profile_it_emptied() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_provider(&mut conn);
        seed_profile(&mut conn, "prof1");
        seed_profile(&mut conn, "prof2");
        upsert(&mut conn, &blank()).unwrap();
        upsert(
            &mut conn,
            &ModelConfigInsert {
                id: "mc2",
                model_id: "grok-4.6-mini",
                profile_id: "prof2",
                ..blank()
            },
        )
        .unwrap();

        // prof2 still describes mc2, so it survives mc1 arriving and leaving.
        assert!(replace_profile(&mut conn, "mc1", "prof2", 300).unwrap());
        assert!(model_profile::get(&mut conn, "prof1").unwrap().is_none());
        assert!(model_profile::get(&mut conn, "prof2").unwrap().is_some());
        // Nothing moved, nothing collected.
        assert!(!replace_profile(&mut conn, "mc1", "prof2", 400).unwrap());
        assert_eq!(get(&mut conn, "mc1").unwrap().unwrap().profile_id, "prof2");
    }

    /// The join every reader actually wants.
    #[test]
    fn a_model_reads_back_beside_the_profile_that_describes_it() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_provider(&mut conn);
        seed_profile(&mut conn, "prof1");
        upsert(&mut conn, &blank()).unwrap();

        let (config, profile) = get_with_profile(&mut conn, "p1", "grok-4.6").unwrap().unwrap();
        assert_eq!(config.id, "mc1");
        assert_eq!(profile.context_window, 128_000);
        assert_eq!(list_by_provider_with_profiles(&mut conn, "p1").unwrap().len(), 1);
    }

    /// Deleting the only model on a profile takes the profile with it.
    #[test]
    fn deleting_the_last_model_collects_its_profile() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_provider(&mut conn);
        seed_profile(&mut conn, "prof1");
        upsert(&mut conn, &blank()).unwrap();

        delete(&mut conn, "mc1").unwrap();
        assert!(model_profile::get(&mut conn, "prof1").unwrap().is_none());
    }
}

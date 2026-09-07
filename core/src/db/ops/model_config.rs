use diesel::prelude::*;

use crate::db::models::model_config::{ModelConfigInsert, ModelConfigRow};
use crate::db::schema::model_configs;

pub fn get(conn: &mut SqliteConnection, id: &str) -> QueryResult<Option<ModelConfigRow>> {
    model_configs::table.find(id).first(conn).optional()
}

pub fn list_by_provider(conn: &mut SqliteConnection, provider_id: &str) -> QueryResult<Vec<ModelConfigRow>> {
    model_configs::table
        .filter(model_configs::provider_id.eq(provider_id))
        .order(model_configs::model_id.asc())
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

pub fn upsert(conn: &mut SqliteConnection, new: &ModelConfigInsert) -> QueryResult<ModelConfigRow> {
    let existing = model_configs::table
        .filter(model_configs::provider_id.eq(new.provider_id))
        .filter(model_configs::model_id.eq(new.model_id))
        .first::<ModelConfigRow>(conn)
        .optional()?;

    if let Some(existing) = existing {
        diesel::update(model_configs::table.find(&existing.id))
            .set((
                model_configs::display_name.eq(new.display_name),
                model_configs::context_window.eq(new.context_window),
                model_configs::compact_threshold.eq(new.compact_threshold),
                model_configs::max_output_tokens.eq(new.max_output_tokens),
                model_configs::input_price.eq(new.input_price.as_ref()),
                model_configs::output_price.eq(new.output_price.as_ref()),
                model_configs::cache_read_price.eq(new.cache_read_price.as_ref()),
                model_configs::cache_write_price.eq(new.cache_write_price.as_ref()),
                model_configs::capability_overrides.eq(new.capability_overrides),
                model_configs::pricing_tiers.eq(new.pricing_tiers),
                model_configs::server_tools.eq(new.server_tools),
                model_configs::server_tool_price.eq(new.server_tool_price.as_ref()),
                model_configs::updated_at.eq(new.updated_at),
            ))
            .execute(conn)?;
        model_configs::table.find(&existing.id).first(conn)
    } else {
        diesel::insert_into(model_configs::table).values(new).execute(conn)?;
        model_configs::table.find(new.id).first(conn)
    }
}

pub fn delete(conn: &mut SqliteConnection, id: &str) -> QueryResult<usize> {
    diesel::delete(model_configs::table.find(id)).execute(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
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
            })
            .execute(conn)
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
            display_name: None,
            context_window: 128_000,
            compact_threshold: 100_000,
            max_output_tokens: None,
            input_price: None,
            output_price: None,
            cache_read_price: None,
            cache_write_price: None,
            created_at: 100,
            updated_at: 100,
            capability_overrides: None,
            pricing_tiers: None,
            server_tools: None,
            server_tool_price: None,
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
        upsert(&mut conn, &blank()).unwrap();

        let edited = ModelConfigInsert {
            display_name: Some("Grok 4.6"),
            context_window: 500_000,
            compact_threshold: 400_000,
            max_output_tokens: Some(64_000),
            input_price: Some(decimal("2")),
            output_price: Some(decimal("6")),
            cache_read_price: Some(decimal("0.5")),
            cache_write_price: Some(decimal("1.5")),
            capability_overrides: Some(r#"{"supports_fast":true}"#),
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
                display_name: Some("Grok 4.6".into()),
                context_window: 500_000,
                compact_threshold: 400_000,
                max_output_tokens: Some(64_000),
                input_price: Some(decimal("2")),
                output_price: Some(decimal("6")),
                cache_read_price: Some(decimal("0.5")),
                cache_write_price: Some(decimal("1.5")),
                capability_overrides: Some(r#"{"supports_fast":true}"#.into()),
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
        upsert(
            &mut conn,
            &ModelConfigInsert {
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
        assert_eq!(cleared.server_tools, None);
        assert_eq!(cleared.server_tool_price, None);
        assert_eq!(cleared.pricing_tiers, None);
        assert_eq!(cleared.cache_read_price, None);
    }
}

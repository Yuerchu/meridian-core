//! What a model configuration actually says, once the profile and the
//! provider's overrides have been put together.
//!
//! The split between `model_profiles` and `model_configs` exists so one model
//! reached through three providers is described once. The cost of that is a
//! question every reader would otherwise have to answer for itself — *do these
//! prices come from the profile or from this row?* — and a reader that answers
//! it differently from the billing path is a bill nobody can reproduce.
//!
//! So nothing outside this module reads `overrides_pricing`. A row and its
//! profile go in, an [`EffectiveModelConfig`] comes out, and every consumer —
//! the turn loop, the price snapshot, the usage report, the settings panel —
//! reads the same answer.

use diesel::prelude::*;

use crate::db::models::model_config::ModelConfigRow;
use crate::db::models::model_profile::ModelProfileRow;
use crate::db::ops::model_config;
use crate::decimal::Decimal;

/// One model, as one provider serves it, with every value resolved.
///
/// Deliberately shaped like the flat `model_configs` row this replaced: the
/// consumers were already right about what they needed, and only the question
/// of where it came from moved.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveModelConfig {
    pub config_id: String,
    pub provider_id: String,
    pub model_id: String,
    pub profile_id: String,
    /// The profile's name — what a person calls this model, which may differ
    /// from the wire id this provider answers to.
    pub name: String,
    pub context_window: i32,
    pub compact_threshold: i32,
    pub max_output_tokens: Option<i32>,
    pub capability_overrides: Option<String>,
    pub input_price: Option<Decimal>,
    pub output_price: Option<Decimal>,
    pub cache_read_price: Option<Decimal>,
    pub cache_write_price: Option<Decimal>,
    pub pricing_tiers: Option<String>,
    pub server_tools: Option<String>,
    pub server_tool_price: Option<Decimal>,
}

/// Puts a row together with the profile it points at.
///
/// The rates are all-or-nothing: an overriding row supplies the whole set, a
/// row that does not override supplies none of it. Filling in gaps from the
/// profile column by column looks more helpful and is not — a relay that
/// charges its own input rate and nothing for cache reads would inherit the
/// vendor's cache rate and bill against a cache the relay may not even keep.
///
/// `server_tools` and `server_tool_price` are never the profile's: whether this
/// upstream runs a search on its own side, and what it charges per call for it,
/// is a fact about the upstream.
pub fn effective(config: &ModelConfigRow, profile: &ModelProfileRow) -> EffectiveModelConfig {
    let (input_price, output_price, cache_read_price, cache_write_price, pricing_tiers) = if config.overrides_pricing {
        (
            config.input_price.clone(),
            config.output_price.clone(),
            config.cache_read_price.clone(),
            config.cache_write_price.clone(),
            config.pricing_tiers.clone(),
        )
    } else {
        (
            profile.input_price.clone(),
            profile.output_price.clone(),
            profile.cache_read_price.clone(),
            profile.cache_write_price.clone(),
            profile.pricing_tiers.clone(),
        )
    };

    EffectiveModelConfig {
        config_id: config.id.clone(),
        provider_id: config.provider_id.clone(),
        model_id: config.model_id.clone(),
        profile_id: profile.id.clone(),
        name: profile.name.clone(),
        context_window: profile.context_window,
        compact_threshold: profile.compact_threshold,
        max_output_tokens: profile.max_output_tokens,
        capability_overrides: profile.capability_overrides.clone(),
        input_price,
        output_price,
        cache_read_price,
        cache_write_price,
        pricing_tiers,
        server_tools: config.server_tools.clone(),
        server_tool_price: config.server_tool_price.clone(),
    }
}

/// The one read every turn and every price snapshot makes.
pub fn load(
    conn: &mut SqliteConnection,
    provider_id: &str,
    model_id: &str,
) -> QueryResult<Option<EffectiveModelConfig>> {
    Ok(model_config::get_with_profile(conn, provider_id, model_id)?
        .map(|(config, profile)| effective(&config, &profile)))
}

/// Every configured model on this machine, for readers that price many rows at
/// once rather than one turn.
pub fn load_all(conn: &mut SqliteConnection) -> QueryResult<Vec<EffectiveModelConfig>> {
    use crate::db::schema::{model_configs, model_profiles};
    let rows: Vec<(ModelConfigRow, ModelProfileRow)> = model_configs::table
        .inner_join(model_profiles::table)
        .select((ModelConfigRow::as_select(), ModelProfileRow::as_select()))
        .load(conn)?;
    Ok(rows.iter().map(|(c, p)| effective(c, p)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decimal(raw: &str) -> Decimal {
        raw.parse().unwrap()
    }

    fn profile() -> ModelProfileRow {
        ModelProfileRow {
            id: "prof".into(),
            name: "Claude Sonnet 5".into(),
            context_window: 200_000,
            compact_threshold: 150_000,
            max_output_tokens: Some(64_000),
            input_price: Some(decimal("3")),
            output_price: Some(decimal("15")),
            cache_read_price: Some(decimal("0.3")),
            cache_write_price: Some(decimal("3.75")),
            pricing_tiers: Some(r#"[{"min_prompt_tokens":200000}]"#.into()),
            capability_overrides: Some(r#"{"supports_fast":true}"#.into()),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn config() -> ModelConfigRow {
        ModelConfigRow {
            id: "mc".into(),
            provider_id: "vertex".into(),
            model_id: "claude-sonnet-5@20260514".into(),
            profile_id: "prof".into(),
            overrides_pricing: false,
            input_price: Some(decimal("4")),
            output_price: Some(decimal("20")),
            cache_read_price: None,
            cache_write_price: None,
            pricing_tiers: None,
            server_tools: Some(r#"["web_search"]"#.into()),
            server_tool_price: Some(decimal("5")),
            created_at: 0,
            updated_at: 0,
        }
    }

    /// The window and the capability patch are the model's, whichever door was
    /// used to reach it.
    #[test]
    fn a_profile_describes_the_model_through_every_provider() {
        let resolved = effective(&config(), &profile());
        assert_eq!(resolved.context_window, 200_000);
        assert_eq!(resolved.compact_threshold, 150_000);
        assert_eq!(resolved.max_output_tokens, Some(64_000));
        assert_eq!(
            resolved.capability_overrides.as_deref(),
            Some(r#"{"supports_fast":true}"#)
        );
        assert_eq!(resolved.name, "Claude Sonnet 5");
        assert_eq!(resolved.model_id, "claude-sonnet-5@20260514");
    }

    /// Prices come from the profile while the switch is off, even though the
    /// row is carrying its own — which is exactly the state an override that
    /// was turned off again leaves behind.
    #[test]
    fn a_row_that_does_not_override_is_priced_by_its_profile() {
        let resolved = effective(&config(), &profile());
        assert_eq!(resolved.input_price, Some(decimal("3")));
        assert_eq!(resolved.output_price, Some(decimal("15")));
        assert_eq!(resolved.cache_read_price, Some(decimal("0.3")));
        assert_eq!(
            resolved.pricing_tiers.as_deref(),
            Some(r#"[{"min_prompt_tokens":200000}]"#)
        );
    }

    /// Overriding takes the whole rate set, gaps included. A relay charging its
    /// own input price does not thereby inherit the vendor's cache rate.
    #[test]
    fn an_overriding_row_replaces_the_rates_whole() {
        let resolved = effective(
            &ModelConfigRow {
                overrides_pricing: true,
                ..config()
            },
            &profile(),
        );
        assert_eq!(resolved.input_price, Some(decimal("4")));
        assert_eq!(resolved.output_price, Some(decimal("20")));
        assert_eq!(resolved.cache_read_price, None);
        assert_eq!(resolved.cache_write_price, None);
        assert_eq!(resolved.pricing_tiers, None);
    }

    /// Provider-side tools belong to the provider under either setting.
    #[test]
    fn the_provider_keeps_its_own_tools_and_their_rate() {
        for overriding in [false, true] {
            let resolved = effective(
                &ModelConfigRow {
                    overrides_pricing: overriding,
                    ..config()
                },
                &profile(),
            );
            assert_eq!(resolved.server_tools.as_deref(), Some(r#"["web_search"]"#));
            assert_eq!(resolved.server_tool_price, Some(decimal("5")));
        }
    }
}

use diesel::prelude::*;

use crate::db::schema::model_configs;
use crate::decimal::Decimal;

#[derive(Debug, Clone, PartialEq, Queryable, Selectable)]
#[diesel(table_name = model_configs)]
pub struct ModelConfigRow {
    pub id: String,
    pub provider_id: String,
    pub model_id: String,
    pub display_name: Option<String>,
    pub context_window: i32,
    pub compact_threshold: i32,
    pub max_output_tokens: Option<i32>,
    /// `None` means no rate has been configured. `Some(0)` is an explicit free
    /// rate; these states must not collapse into the same database value.
    pub input_price: Option<Decimal>,
    pub output_price: Option<Decimal>,
    pub cache_read_price: Option<Decimal>,
    pub created_at: i64,
    pub updated_at: i64,
    /// User-authored JSON patch over the built-in catalog. Read only through
    /// the strict capability override decoder; malformed or unknown fields are
    /// contract errors.
    pub capability_overrides: Option<String>,
    /// What a cache *write* costs per million, when it costs more than ordinary
    /// input. Anthropic charges 1.25x for a five-minute entry and 2x for an
    /// hour; most upstreams charge nothing extra, which is what `None` means.
    pub cache_write_price: Option<Decimal>,
    /// Rates that take over above a prompt size, as a JSON array — see
    /// `agent::pricing::PriceTier`. `None` means one price at every size, which
    /// is most models. Read only through `parse_tiers`, which validates the
    /// complete closed document before sorting it.
    pub pricing_tiers: Option<String>,
    /// Provider-side tools switched on for this model, as a JSON array of wire
    /// type names. Narrowed against `ProviderCapabilities::server_tools` at turn
    /// time, so a name here cannot outlive the support it refers to.
    pub server_tools: Option<String>,
    /// What one provider-side tool invocation costs, per **thousand** calls —
    /// the unit the upstreams publish it in. `None` means nobody has said, which
    /// is not zero.
    pub server_tool_price: Option<Decimal>,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = model_configs)]
pub struct ModelConfigInsert<'a> {
    pub id: &'a str,
    pub provider_id: &'a str,
    pub model_id: &'a str,
    pub display_name: Option<&'a str>,
    pub context_window: i32,
    pub compact_threshold: i32,
    pub max_output_tokens: Option<i32>,
    pub input_price: Option<Decimal>,
    pub output_price: Option<Decimal>,
    pub cache_read_price: Option<Decimal>,
    pub created_at: i64,
    pub updated_at: i64,
    pub capability_overrides: Option<&'a str>,
    pub cache_write_price: Option<Decimal>,
    pub pricing_tiers: Option<&'a str>,
    pub server_tools: Option<&'a str>,
    pub server_tool_price: Option<Decimal>,
}

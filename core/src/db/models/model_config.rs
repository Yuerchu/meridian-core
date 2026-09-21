use diesel::prelude::*;

use crate::db::schema::model_configs;
use crate::decimal::Decimal;

/// One provider's door to a model: what it is called on the wire there, which
/// [`ModelProfileRow`](super::model_profile::ModelProfileRow) describes it, and
/// what — if anything — is different about reaching it this way.
///
/// The window, the capability patch and the base prices are the profile's. Only
/// two things are genuinely per-provider: the rates, when a relay really does
/// charge its own, and the provider-side tools, which are a fact about that
/// upstream rather than about the model.
#[derive(Debug, Clone, PartialEq, Queryable, Selectable)]
#[diesel(table_name = model_configs)]
pub struct ModelConfigRow {
    pub id: String,
    pub provider_id: String,
    pub model_id: String,
    pub profile_id: String,
    /// Whether the four prices and the tiers below are read at all. Off is the
    /// ordinary case and means the profile's rates apply; the columns stay
    /// blank rather than holding a copy, because a number kept in two places is
    /// a number that comes to disagree.
    pub overrides_pricing: bool,
    /// `None` means no rate has been configured. `Some(0)` is an explicit free
    /// rate; these states must not collapse into the same database value.
    /// Read only through `agent::model_config::effective`, which is the one
    /// place that knows the switch above decides between these and the
    /// profile's.
    pub input_price: Option<Decimal>,
    pub output_price: Option<Decimal>,
    pub cache_read_price: Option<Decimal>,
    pub cache_write_price: Option<Decimal>,
    /// Rates that take over above a prompt size — see `agent::pricing::PriceTier`.
    pub pricing_tiers: Option<String>,
    /// Provider-side tools switched on for this model, as a JSON array of wire
    /// type names. Narrowed against `ProviderCapabilities::server_tools` at turn
    /// time, so a name here cannot outlive the support it refers to. Not the
    /// profile's: whether the upstream will search the web on its own side is a
    /// fact about the upstream.
    pub server_tools: Option<String>,
    /// What one provider-side tool invocation costs, per **thousand** calls —
    /// the unit the upstreams publish it in. `None` means nobody has said, which
    /// is not zero. Independent of `overrides_pricing`, which governs the token
    /// rates alone: this one has no profile-level counterpart to fall back to.
    pub server_tool_price: Option<Decimal>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = model_configs)]
pub struct ModelConfigInsert<'a> {
    pub id: &'a str,
    pub provider_id: &'a str,
    pub model_id: &'a str,
    pub profile_id: &'a str,
    pub overrides_pricing: bool,
    pub input_price: Option<Decimal>,
    pub output_price: Option<Decimal>,
    pub cache_read_price: Option<Decimal>,
    pub cache_write_price: Option<Decimal>,
    pub pricing_tiers: Option<&'a str>,
    pub server_tools: Option<&'a str>,
    pub server_tool_price: Option<Decimal>,
    pub created_at: i64,
    pub updated_at: i64,
}

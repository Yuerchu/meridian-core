use diesel::prelude::*;

use crate::db::schema::model_profiles;
use crate::decimal::Decimal;

/// What a model *is*, independent of who serves it.
///
/// The same Claude answers on Anthropic, Vertex and Azure, and the same
/// DeepSeek on its own API and on a dozen relays. Its context window, what it
/// can be asked to do and — usually — what a token costs are facts about the
/// model, so they are held once and pointed at by every `model_configs` row
/// that reaches it. A provider that genuinely charges differently overrides the
/// prices on its own row; nothing else is overridable, because nothing else
/// differs between two doors to one model.
#[derive(Debug, Clone, PartialEq, Queryable, Selectable)]
#[diesel(table_name = model_profiles)]
pub struct ModelProfileRow {
    pub id: String,
    /// What a person calls this model. Free text: it names the profile in the
    /// picker, and two providers' wire ids for one model are rarely the same
    /// string.
    pub name: String,
    pub context_window: i32,
    pub compact_threshold: i32,
    pub max_output_tokens: Option<i32>,
    /// `None` means no rate has been configured. `Some(0)` is an explicit free
    /// rate; these states must not collapse into the same database value.
    pub input_price: Option<Decimal>,
    pub output_price: Option<Decimal>,
    pub cache_read_price: Option<Decimal>,
    /// What a cache *write* costs per million, when it costs more than ordinary
    /// input. Anthropic charges 1.25x for a five-minute entry and 2x for an
    /// hour; most upstreams charge nothing extra, which is what `None` means.
    pub cache_write_price: Option<Decimal>,
    /// Rates that take over above a prompt size, as a JSON array — see
    /// `agent::pricing::PriceTier`. `None` means one price at every size, which
    /// is most models. Read only through `parse_tiers`, which validates the
    /// complete closed document before sorting it.
    pub pricing_tiers: Option<String>,
    /// User-authored JSON patch over the built-in catalog. Read only through
    /// the strict capability override decoder; malformed or unknown fields are
    /// contract errors.
    ///
    /// A patch, not a replacement: the base it lands on is resolved per
    /// provider from `(provider_type, api_format, transport_profile, model)`,
    /// so one profile's override can correct the same thing on several doors
    /// without asserting they resolve identically.
    pub capability_overrides: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = model_profiles)]
pub struct ModelProfileInsert<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub context_window: i32,
    pub compact_threshold: i32,
    pub max_output_tokens: Option<i32>,
    pub input_price: Option<Decimal>,
    pub output_price: Option<Decimal>,
    pub cache_read_price: Option<Decimal>,
    pub cache_write_price: Option<Decimal>,
    pub pricing_tiers: Option<&'a str>,
    pub capability_overrides: Option<&'a str>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Every field is written on every edit, `None` included: the editor submits a
/// whole profile, so an absent price means "cleared" rather than "unchanged".
/// `Option<Option<_>>` would be the shape for a partial patch, and there is no
/// caller that wants one.
/// `treat_none_as_null` is what makes that true: without it Diesel reads a
/// `None` as "leave this column alone", and clearing a price would silently
/// keep the old one.
#[derive(Debug, AsChangeset)]
#[diesel(table_name = model_profiles, treat_none_as_null = true)]
pub struct ModelProfileChangeset<'a> {
    pub name: &'a str,
    pub context_window: i32,
    pub compact_threshold: i32,
    pub max_output_tokens: Option<i32>,
    pub input_price: Option<Decimal>,
    pub output_price: Option<Decimal>,
    pub cache_read_price: Option<Decimal>,
    pub cache_write_price: Option<Decimal>,
    pub pricing_tiers: Option<&'a str>,
    pub capability_overrides: Option<&'a str>,
    pub updated_at: i64,
}

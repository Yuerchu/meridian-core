use diesel::prelude::*;
use serde::Serialize;

use crate::db::schema::providers;

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = providers)]
pub struct ProviderRow {
    pub id: String,
    pub name: String,
    pub provider_type: String,
    pub base_url: String,
    pub is_enabled: i32,
    pub sort_order: i32,
    pub created_at: i64,
    pub updated_at: i64,
    pub api_format: String,
    /// Which entry in `provider_catalog.json` this row is an instance of, or
    /// `None` for one the catalog does not describe.
    ///
    /// Display and prefill only — it never reaches `create_provider`, which
    /// picks an adapter from `provider_type` and `api_format`. `None` is
    /// ordinary: hand-made providers and anything pointing at a relay have it,
    /// and both behave exactly as they did before the column existed.
    pub catalog_id: Option<String>,
    /// Where the credential comes from: `api_key`, or one of the ChatGPT logins.
    ///
    /// Not an input to adapter selection — two ways of signing in to ChatGPT
    /// produce the same token on the same wire, so they must not produce two
    /// adapters. See `agent::provider_config::resolve_credential`.
    pub credential_kind: String,
    /// How requests are shaped and what the model can be asked to do.
    ///
    /// The third input to picking an adapter, beside `provider_type` and
    /// `api_format`. It exists because the format cannot carry the distinction
    /// alone: OpenAI's API and ChatGPT's Codex backend are both `responses`.
    pub transport_profile: String,
    /// Which logo to draw, or `None` to derive one from `catalog_id`.
    ///
    /// A logo's name in the front end's icon set, which is a third-party list
    /// that changes between releases — so this is free text, checked nowhere,
    /// read by nothing but the renderer. A name the set no longer knows draws
    /// a generic mark; it cannot make a row stop working.
    pub icon: Option<String>,
    /// Whether requests to this row are shaped exactly the way Codex shapes its
    /// own — see migration 63.
    ///
    /// Separate from `transport_profile` because they answer different
    /// questions: that one says how we authenticate and where we go, this one
    /// says what the body and headers look like once we are there. A Codex
    /// backend behind an ordinary API key is exactly the row where the two come
    /// apart, and it is the row this exists for.
    pub codex_request_shape: i32,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = providers)]
pub struct ProviderInsert<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub provider_type: &'a str,
    pub base_url: &'a str,
    pub is_enabled: i32,
    pub sort_order: i32,
    pub created_at: i64,
    pub updated_at: i64,
    pub api_format: &'a str,
    pub catalog_id: Option<&'a str>,
    pub credential_kind: &'a str,
    pub transport_profile: &'a str,
    pub icon: Option<&'a str>,
    pub codex_request_shape: i32,
}

/// Pointing a row at a relay does not stop it being the vendor the user picked
/// — they are reaching OpenAI through a proxy, and the panel should keep saying
/// OpenAI. The id is a claim about *whose service this is*, not an assertion
/// about the address, so editing the address must not silently retract it.
#[derive(Debug, Default, AsChangeset)]
#[diesel(table_name = providers)]
pub struct ProviderChangeset {
    pub name: Option<String>,
    pub provider_type: Option<String>,
    pub base_url: Option<String>,
    pub is_enabled: Option<i32>,
    pub sort_order: Option<i32>,
    pub updated_at: Option<i64>,
    pub api_format: Option<String>,
    /// Editable, unlike `catalog_id` below: which login a provider uses is a
    /// setting, not an identity. Changing it is how a row moves between an API
    /// key and a ChatGPT session.
    pub credential_kind: Option<String>,
    pub transport_profile: Option<String>,
    /// Never a user input — the command layer recomputes it when, and only
    /// when, `provider_type` changes. A new address can still be the same
    /// vendor behind a relay; a new *type* cannot, and a row created as the
    /// catalog's default and then re-typed used to keep the old vendor's
    /// identity forever: OpenAI's logo and key page on an Anthropic row.
    /// Two layers because the honest recomputed answer is often "no identity"
    /// (`Some(None)`), which one layer cannot say without meaning "leave it".
    pub catalog_id: Option<Option<String>>,
    /// Two layers for the reason `catalog_id` above has them: clearing the
    /// choice back to "follow the catalog" is `Some(None)`, and one layer
    /// cannot tell that from "leave it alone".
    pub icon: Option<Option<String>>,
    /// One layer, unlike the two above: the column is `NOT NULL` and has only
    /// two values, so "off" is a value the user chose rather than an absence
    /// that has to be told apart from "not mentioned".
    pub codex_request_shape: Option<i32>,
}

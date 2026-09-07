//! The vendor catalog: what a provider looks like, and what to prefill when
//! creating one.
//!
//! This is the display half of a deliberate split. `capabilities.rs` reads
//! `model_catalog.json` to answer *how do I send a request* — by longest-prefix
//! match, where one `gpt-5` catch-all covers every model nobody has listed yet.
//! This file answers *what do I show the user*, where a model id has to be
//! exact because it becomes a row they can click. Merging the two would sooner
//! or later render a catch-all prefix as a model that does not exist.
//!
//! Nothing here decides behaviour. `registry::create_provider`,
//! `capabilities::resolve` and `balance::supports_balance` remain the
//! authorities; [`CatalogEntry::balance`] exists only to keep a "check balance"
//! button off panels where it could never do anything, which is the same reason
//! the frontend's `BALANCE_TYPES` gave for existing. A drift between the two is
//! meant to be visible rather than silent.
//!
//! An entry is a **creation preset**. It is not consulted again once a provider
//! row exists: a relay URL the user typed is theirs to keep, and a catalog
//! update must never turn an existing configuration into an illegal one.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    #[serde(rename = "version")]
    _version: u32,
    #[serde(rename = "_comment")]
    _comment: Vec<String>,
    providers: Vec<CatalogEntry>,
}

/// One vendor.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogEntry {
    /// Stable identity, and what a provider row's `catalog_id` points at.
    ///
    /// Distinct from `provider_type` on purpose: several vendors share one
    /// adapter (they are all OpenAI-compatible) while each keeps its own name,
    /// icon and key-issuing page. That separation is what lets a vendor be
    /// added without adding a match arm.
    pub id: String,
    /// Which family of adapter this vendor is served by. Must be a value
    /// `registry::create_provider` accepts.
    pub provider_type: String,
    pub name: String,
    pub icon: String,
    /// Whether this vendor publishes an account balance at all.
    pub balance: bool,
    pub websites: Websites,
    /// The ways of signing in, each carrying the endpoint and dialect that come
    /// with it. See [`AuthOption`].
    pub auth: Vec<AuthOption>,
    /// Preset model list, grouped for display. Empty means "ask the provider",
    /// which is what every vendor with a working `/models` does.
    pub models: Vec<ModelGroup>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Websites {
    #[serde(deserialize_with = "crate::events::deserialize_required_nullable")]
    pub official: Option<String>,
    /// Where the user goes to obtain a key — the one link a settings panel
    /// actually needs.
    #[serde(deserialize_with = "crate::events::deserialize_required_nullable")]
    pub api_key: Option<String>,
    #[serde(deserialize_with = "crate::events::deserialize_required_nullable")]
    pub docs: Option<String>,
    #[serde(deserialize_with = "crate::events::deserialize_required_nullable")]
    pub models: Option<String>,
}

/// One way of signing in to a vendor.
///
/// The endpoint and the dialect live **here** rather than on the entry, because
/// they belong to the login method: OpenAI's API and ChatGPT's Codex backend are
/// both `responses`, yet differ in base URL and in what the wire supports. A
/// flat `default_base_url` keyed by format cannot hold both, and the fallback —
/// hardcoding "when Codex is chosen, switch the URL" into the settings panel —
/// is exactly the vendor-specific branching this catalog exists to delete.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthOption {
    pub id: String,
    /// Where the credential comes from. Deliberately *not* an input to adapter
    /// selection: two ways of signing in to ChatGPT yield the same token on the
    /// same wire, so they must not produce two adapters.
    pub credential_kind: String,
    /// How requests are shaped and what the model can be asked to do. This is
    /// what picks the adapter, alongside `provider_type` and the format.
    pub transport_profile: String,
    /// Dialects available under this login. A single element means the dialect
    /// is not a choice, which is what a settings panel reads to omit the
    /// selector.
    pub api_formats: Vec<String>,
    /// Prefilled base URL per dialect. Every key must appear in `api_formats`.
    pub default_base_url: HashMap<String, String>,
}

/// A display grouping of model ids, e.g. everything in the `gpt-5.1` family.
///
/// Written down rather than derived from the id. Splitting ids on punctuation
/// is how `gpt-5` and `gpt-5.1` end up in two groups while `gpt-5-mini` joins
/// the first — an artefact of the separator, not a statement about the models.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelGroup {
    pub family: String,
    pub ids: Vec<String>,
}

static CATALOG: LazyLock<Catalog> = LazyLock::new(|| {
    // Parsed once at first use. A malformed catalog is an authoring error the
    // checker should have caught, not something to degrade around at runtime —
    // same stance as `model_catalog.json`.
    let catalog: Catalog =
        serde_json::from_str(include_str!("provider_catalog.json")).expect("provider_catalog.json is malformed");
    assert_eq!(catalog._version, 1, "unsupported provider_catalog.json version");
    catalog
});

/// Every vendor, in the order the file lists them (which is the order a picker
/// shows them in).
pub fn entries() -> &'static [CatalogEntry] {
    &CATALOG.providers
}

/// Look up one vendor by catalog id.
///
/// A miss is ordinary: a provider row may carry a `catalog_id` from a build
/// that knew a vendor this one does not, and a row with no `catalog_id` at all
/// is the common case for anything hand-made.
pub fn find(id: &str) -> Option<&'static CatalogEntry> {
    CATALOG.providers.iter().find(|entry| entry.id == id)
}

/// A trailing slash and a capital letter are the same address. Nothing beyond
/// that is normalised, because anything further starts being a guess.
fn normalize_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_ascii_lowercase()
}

/// Which vendor a provider row belongs to, judged only from what it holds.
///
/// Used when a row is created, so a fresh provider carries the identity that
/// gives it a logo and a key-issuing link. Deliberately conservative: it answers
/// only when a vendor's *own* base URL is present verbatim, and returns `None`
/// the moment two entries would both fit. `provider_type` cannot decide this on
/// its own — `openai` covers the official API, a self-hosted proxy and every
/// compatible reseller — and a wrong id is worse than none: it shows the wrong
/// logo and offers a key page for a service the user is not talking to.
///
/// Migration 40 backfills existing rows with the same rule spelled out in SQL.
/// The duplication is intended and the two are allowed to drift apart over
/// time: a migration has to replay to the same result years from now, so it is
/// pinned to the URLs rows *actually hold*, while this function follows the
/// catalog as it is edited. They agree on the day the migration ships, which is
/// the only day they both run on the same data.
pub fn identify(provider_type: &str, base_url: &str) -> Option<&'static str> {
    let wanted = normalize_url(base_url);
    let mut found: Option<&'static str> = None;
    for entry in entries() {
        if entry.provider_type != provider_type {
            continue;
        }
        let fits = entry
            .auth
            .iter()
            .any(|option| option.default_base_url.values().any(|url| normalize_url(url) == wanted));
        if fits {
            if found.is_some() {
                // Two vendors claim this address. Nothing here can break the
                // tie, so refuse rather than pick.
                return None;
            }
            found = Some(entry.id.as_str());
        }
    }
    found
}

impl CatalogEntry {
    /// The login method a fresh row should start from: the first listed. Order
    /// in the file is editorial.
    pub fn default_auth(&self) -> Option<&AuthOption> {
        self.auth.first()
    }

    pub fn auth_option(&self, id: &str) -> Option<&AuthOption> {
        self.auth.iter().find(|option| option.id == id)
    }
}

impl AuthOption {
    /// The dialect a fresh row should start from, or `None` if this login
    /// offers none (which the checker rejects, so it cannot happen in practice).
    pub fn default_api_format(&self) -> Option<&str> {
        self.api_formats.first().map(String::as_str)
    }

    /// Whether the user gets to pick a dialect under this login at all.
    pub fn format_is_a_choice(&self) -> bool {
        self.api_formats.len() > 1
    }

    pub fn base_url_for(&self, api_format: &str) -> Option<&str> {
        self.default_base_url.get(api_format).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped file parses, and every vendor is reachable by its own id.
    #[test]
    fn catalog_parses_and_is_addressable() {
        let all = entries();
        assert!(!all.is_empty(), "catalog is empty");
        for entry in all {
            assert!(find(&entry.id).is_some(), "{} is not findable by its own id", entry.id);
        }
    }

    #[test]
    fn catalog_does_not_invent_missing_fields() {
        let complete: serde_json::Value =
            serde_json::from_str(include_str!("provider_catalog.json")).expect("catalog fixture");

        for field in ["balance", "websites", "models"] {
            let mut missing = complete.clone();
            missing["providers"][0].as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<Catalog>(missing).is_err(),
                "providers[].{field} must be present"
            );
        }

        let mut missing_website_key = complete.clone();
        missing_website_key["providers"][0]["websites"]
            .as_object_mut()
            .unwrap()
            .remove("official");
        assert!(serde_json::from_value::<Catalog>(missing_website_key).is_err());

        let mut missing_base_urls = complete;
        missing_base_urls["providers"][0]["auth"][0]
            .as_object_mut()
            .unwrap()
            .remove("default_base_url");
        assert!(serde_json::from_value::<Catalog>(missing_base_urls).is_err());
    }

    /// The five vendors the settings panel hardcoded before this file existed.
    /// They are what PR2a-2 will read instead, so losing one is a regression in
    /// the UI rather than in the data.
    #[test]
    fn the_original_five_are_present() {
        for id in ["openai", "anthropic", "deepseek", "xai", "google"] {
            assert!(find(id).is_some(), "{id} missing from catalog");
        }
    }

    /// `balance` mirrors the frontend's old `BALANCE_TYPES`, which listed
    /// DeepSeek alone: Anthropic and xAI publish nothing and OpenAI withdrew the
    /// endpoint. `provider::balance` stays the authority — this only decides
    /// whether a button is drawn.
    #[test]
    fn only_deepseek_advertises_a_balance() {
        for entry in entries() {
            assert_eq!(
                entry.balance,
                entry.id == "deepseek",
                "{} disagrees with supports_balance",
                entry.id
            );
        }
    }

    /// A single dialect means the selector is omitted; this is what
    /// `SINGLE_FORMAT_TYPES` said about Anthropic and `DUAL_FORMAT_TYPES` about
    /// xAI and DeepSeek.
    #[test]
    fn dialect_is_a_choice_only_where_it_used_to_be() {
        let choice = |id: &str| {
            find(id)
                .and_then(CatalogEntry::default_auth)
                .expect("entry with a default login")
                .format_is_a_choice()
        };
        assert!(!choice("anthropic"), "Anthropic's adapter ignores the format");
        assert!(choice("xai"), "xAI speaks both dialects");
        assert!(choice("deepseek"), "DeepSeek speaks both dialects");
    }

    /// A vendor's own address identifies it, in the forms it is really written
    /// in — the frontend's old `PROVIDER_DEFAULT_URLS` is what put these into
    /// existing rows, and a trailing slash or a capital letter must not change
    /// the answer.
    #[test]
    fn a_vendors_own_url_identifies_it() {
        assert_eq!(identify("openai", "https://api.openai.com/v1"), Some("openai"));
        assert_eq!(identify("openai", "https://api.openai.com/v1/"), Some("openai"));
        assert_eq!(identify("openai", "HTTPS://API.OPENAI.COM/v1"), Some("openai"));
        assert_eq!(identify("anthropic", "https://api.anthropic.com"), Some("anthropic"));
        assert_eq!(identify("deepseek", "https://api.deepseek.com"), Some("deepseek"));
        assert_eq!(identify("xai", "https://api.x.ai/v1"), Some("xai"));
    }

    /// Google reaches the same vendor down either dialect's address.
    #[test]
    fn both_google_addresses_identify_google() {
        assert_eq!(
            identify("google", "https://generativelanguage.googleapis.com"),
            Some("google")
        );
        assert_eq!(
            identify("google", "https://generativelanguage.googleapis.com/v1beta/openai"),
            Some("google")
        );
    }

    /// A relay is the case this must not guess at. `openai` says nothing about
    /// whose service is behind the address, and a wrong id shows the wrong logo
    /// and a key page for a service the user is not talking to.
    #[test]
    fn a_relay_stays_unidentified() {
        assert_eq!(identify("openai", "https://codex-api.foxline.cn"), None);
        assert_eq!(identify("openai", "https://api.openai.com/v1/proxy"), None);
        assert_eq!(identify("openai", ""), None);
    }

    /// The type has to agree too: the right address under the wrong family is
    /// not that vendor.
    #[test]
    fn the_address_alone_is_not_enough() {
        assert_eq!(identify("anthropic", "https://api.openai.com/v1"), None);
        assert_eq!(identify("nonesuch", "https://api.openai.com/v1"), None);
    }

    /// Prefills are addressable by the dialect they belong to — the property
    /// PR2a-2 depends on when it fills the base-URL field.
    #[test]
    fn every_declared_dialect_has_a_prefilled_url() {
        for entry in entries() {
            for option in &entry.auth {
                for format in &option.api_formats {
                    super::super::registry::validate_stored_contract(
                        &entry.provider_type,
                        format,
                        &option.transport_profile,
                        &option.credential_kind,
                    )
                    .unwrap_or_else(|error| panic!("{}/{} has an invalid contract: {error}", entry.id, option.id));
                    assert!(
                        option.base_url_for(format).is_some(),
                        "{}/{} declares {format} without a URL",
                        entry.id,
                        option.id
                    );
                }
            }
        }
    }
}

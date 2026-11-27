//! The vendor catalog: what a provider looks like, and what to prefill when
//! creating one.
//!
//! Backed directly by the per-vendor directories under `src/vendors/<id>/manifest.json`
//! (auto-discovered at build time by `build.rs`). Adding an L1 declarative vendor
//! requires only adding a `manifest.json` file — zero Rust or frontend code changes.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

use meridian_provider_sdk::domain::CapabilityDomain;
use meridian_provider_sdk::schema::{ConfigFieldSpec, ModelsDevMapping, VendorManifest};

/// One chat vendor entry projected from its [`VendorManifest`].
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogEntry {
    pub id: String,
    pub provider_type: String,
    pub name: String,
    pub icon: String,
    pub domains: Vec<CapabilityDomain>,
    pub balance: bool,
    pub websites: Websites,
    pub auth: Vec<AuthOption>,
    pub config_fields: Vec<ConfigFieldSpec>,
    #[serde(deserialize_with = "meridian_provider_sdk::wire::deserialize_required_nullable")]
    pub models_dev: Option<ModelsDevMapping>,
    pub models: Vec<ModelGroup>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Websites {
    #[serde(deserialize_with = "meridian_provider_sdk::wire::deserialize_required_nullable")]
    pub official: Option<String>,
    #[serde(deserialize_with = "meridian_provider_sdk::wire::deserialize_required_nullable")]
    pub api_key: Option<String>,
    #[serde(deserialize_with = "meridian_provider_sdk::wire::deserialize_required_nullable")]
    pub docs: Option<String>,
    #[serde(deserialize_with = "meridian_provider_sdk::wire::deserialize_required_nullable")]
    pub models: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthOption {
    pub id: String,
    pub credential_kind: String,
    pub transport_profile: String,
    pub api_formats: Vec<String>,
    pub default_base_url: HashMap<String, String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelGroup {
    pub family: String,
    pub ids: Vec<String>,
}

impl From<&VendorManifest> for CatalogEntry {
    fn from(manifest: &VendorManifest) -> Self {
        let provider_type = manifest
            .provider_type
            .clone()
            .unwrap_or_else(|| panic!("chat vendor manifest `{}` is missing provider_type", manifest.id));
        Self {
            id: manifest.id.clone(),
            provider_type,
            name: manifest.name.clone(),
            icon: manifest.icon.clone(),
            domains: manifest.domains.clone(),
            balance: manifest.balance,
            websites: Websites {
                official: manifest.websites.official.clone(),
                api_key: manifest.websites.api_key.clone(),
                docs: manifest.websites.docs.clone(),
                models: manifest.websites.models.clone(),
            },
            auth: manifest
                .auth
                .iter()
                .map(|a| AuthOption {
                    id: a.id.clone(),
                    credential_kind: a.credential_kind.clone(),
                    transport_profile: a.transport_profile.clone(),
                    api_formats: a.api_formats.clone(),
                    default_base_url: a.default_base_url.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                })
                .collect(),
            config_fields: manifest.config_fields.clone(),
            models_dev: manifest.models_dev.clone(),
            models: manifest
                .models
                .iter()
                .map(|m| ModelGroup {
                    family: m.family.clone(),
                    ids: m.ids.clone(),
                })
                .collect(),
        }
    }
}

static PROVIDER_CATALOG: LazyLock<Vec<CatalogEntry>> = LazyLock::new(|| {
    crate::vendors::all_manifests()
        .iter()
        .filter(|manifest| manifest.provider_type.is_some())
        .map(CatalogEntry::from)
        .collect()
});

/// Every interactive provider vendor (`chat`, `image_gen`, `video_gen`), in the manifest `sort_order` order.
pub fn entries() -> &'static [CatalogEntry] {
    &PROVIDER_CATALOG
}

/// Every vendor manifest across all capability domains (`chat`, `web_search`, `tts`, ...).
pub fn all_manifests() -> &'static [VendorManifest] {
    crate::vendors::all_manifests()
}

/// Look up one provider vendor by catalog id.
pub fn find(id: &str) -> Option<&'static CatalogEntry> {
    PROVIDER_CATALOG.iter().find(|entry| entry.id == id)
}

fn normalize_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_ascii_lowercase()
}

/// Which vendor a provider row belongs to, judged only from what it holds.
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
                return None;
            }
            found = Some(entry.id.as_str());
        }
    }
    found
}

impl CatalogEntry {
    pub fn default_auth(&self) -> Option<&AuthOption> {
        self.auth.first()
    }

    pub fn auth_option(&self, id: &str) -> Option<&AuthOption> {
        self.auth.iter().find(|option| option.id == id)
    }
}

impl AuthOption {
    pub fn default_api_format(&self) -> Option<&str> {
        self.api_formats.first().map(String::as_str)
    }

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
        let sample = serde_json::to_value(&entries()[0]).expect("serialize entry");

        for field in ["balance", "websites", "models", "config_fields", "domains"] {
            let mut missing = sample.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<CatalogEntry>(missing).is_err(),
                "CatalogEntry.{field} must be present"
            );
        }

        let mut missing_website_key = sample.clone();
        missing_website_key["websites"]
            .as_object_mut()
            .unwrap()
            .remove("official");
        assert!(serde_json::from_value::<CatalogEntry>(missing_website_key).is_err());

        let mut missing_base_urls = sample;
        missing_base_urls["auth"][0]
            .as_object_mut()
            .unwrap()
            .remove("default_base_url");
        assert!(serde_json::from_value::<CatalogEntry>(missing_base_urls).is_err());
    }

    #[test]
    fn the_original_five_plus_openrouter_are_present() {
        for id in ["openai", "anthropic", "deepseek", "xai", "google", "openrouter"] {
            assert!(find(id).is_some(), "{id} missing from catalog");
        }
    }

    #[test]
    fn the_balance_flag_agrees_with_the_vendor_that_implements_it() {
        for entry in entries() {
            assert_eq!(
                entry.balance,
                super::super::balance::BalanceVendor::from_catalog_id(&entry.id).is_some(),
                "{} disagrees with provider::balance",
                entry.id
            );
        }
    }

    #[test]
    fn the_upstreams_that_publish_a_balance_still_do() {
        for id in ["deepseek", "moonshot", "siliconflow"] {
            assert!(find(id).is_some_and(|entry| entry.balance), "{id} lost its balance");
        }
        for id in ["openai", "anthropic", "xai", "google", "openrouter"] {
            assert!(
                find(id).is_some_and(|entry| !entry.balance),
                "{id} publishes no balance"
            );
        }
    }

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
        assert!(choice("openrouter"), "OpenRouter speaks chat_completions and responses");
    }

    #[test]
    fn a_vendors_own_url_identifies_it() {
        assert_eq!(identify("openai", "https://api.openai.com/v1"), Some("openai"));
        assert_eq!(identify("openai", "https://api.openai.com/v1/"), Some("openai"));
        assert_eq!(identify("openai", "HTTPS://API.OPENAI.COM/v1"), Some("openai"));
        assert_eq!(identify("openai", "https://openrouter.ai/api/v1"), Some("openrouter"));
        assert_eq!(identify("anthropic", "https://api.anthropic.com"), Some("anthropic"));
        assert_eq!(identify("deepseek", "https://api.deepseek.com"), Some("deepseek"));
        assert_eq!(identify("xai", "https://api.x.ai/v1"), Some("xai"));
    }

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

    #[test]
    fn a_relay_stays_unidentified() {
        assert_eq!(identify("openai", "https://codex-api.foxline.cn"), None);
        assert_eq!(identify("openai", "https://api.openai.com/v1/proxy"), None);
        assert_eq!(identify("openai", ""), None);
    }

    #[test]
    fn the_address_alone_is_not_enough() {
        assert_eq!(identify("anthropic", "https://api.openai.com/v1"), None);
        assert_eq!(identify("nonesuch", "https://api.openai.com/v1"), None);
    }

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

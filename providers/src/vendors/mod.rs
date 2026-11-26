//! Vendor directories (`vendors/<vendor_id>/manifest.json` + optional Rust
//! extensions).
//!
//! Adding an L1 declarative vendor (any OpenAI-compatible or standard-protocol
//! vendor with custom URLs, websites, models.dev mapping, and `config_fields`)
//! requires ONLY adding `src/vendors/<id>/manifest.json` — `build.rs` discovers
//! and embeds it automatically with zero Rust or frontend code changes.

pub mod deepseek;
pub mod google;
pub mod openai;
pub mod xai;

use std::sync::LazyLock;

use meridian_provider_sdk::domain::CapabilityDomain;
use meridian_provider_sdk::schema::VendorManifest;

include!(concat!(env!("OUT_DIR"), "/vendor_manifests.rs"));

static MANIFESTS: LazyLock<Vec<VendorManifest>> = LazyLock::new(|| {
    let mut manifests = Vec::with_capacity(VENDOR_MANIFEST_SOURCES.len());
    for &(dir_id, raw) in VENDOR_MANIFEST_SOURCES {
        let manifest: VendorManifest = serde_json::from_str(raw)
            .unwrap_or_else(|err| panic!("vendors/{dir_id}/manifest.json is malformed: {err}"));
        assert_eq!(
            manifest.version, 1,
            "vendors/{dir_id}/manifest.json has unsupported version {}",
            manifest.version
        );
        assert_eq!(
            manifest.id, dir_id,
            "vendors/{dir_id}/manifest.json id `{}` must match directory name `{dir_id}`",
            manifest.id
        );
        manifests.push(manifest);
    }
    manifests.sort_by(|a, b| a.sort_order.cmp(&b.sort_order).then_with(|| a.id.cmp(&b.id)));
    manifests
});

/// All embedded vendor manifests across every capability domain, ordered by
/// `sort_order` then `id`.
pub fn all_manifests() -> &'static [VendorManifest] {
    &MANIFESTS
}

/// Vendor manifests declaring support for `domain`.
pub fn manifests_for_domain(domain: CapabilityDomain) -> Vec<&'static VendorManifest> {
    MANIFESTS.iter().filter(|m| m.supports_domain(domain)).collect()
}

/// Look up one vendor manifest by id.
pub fn find_manifest(id: &str) -> Option<&'static VendorManifest> {
    MANIFESTS.iter().find(|m| m.id == id)
}

/// Resolve ordered `models.dev` provider IDs for a vendor and base URL from its
/// manifest (eliminating hardcoded vendor match tables in `meridian-core`).
pub fn models_dev_provider_ids(vendor_id: &str, base_url: &str) -> &'static [String] {
    find_manifest(vendor_id)
        .and_then(|manifest| manifest.models_dev.as_ref())
        .map(|mapping| mapping.provider_ids_for_base_url(base_url))
        .unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_manifests_parse_and_cover_every_domain() {
        let all = all_manifests();
        assert!(all.len() >= 12, "expected chat + web_search + tts manifests, got {}", all.len());
        assert!(find_manifest("openrouter").is_some(), "L1 declarative openrouter vendor");
        assert!(find_manifest("brave").is_some(), "web_search brave vendor");
        assert!(find_manifest("tavily").is_some());
        assert!(find_manifest("zhipu").is_some());
        assert!(find_manifest("fish_audio").is_some());
        assert!(manifests_for_domain(CapabilityDomain::Chat).len() >= 9);
        assert!(manifests_for_domain(CapabilityDomain::WebSearch).len() >= 3);
        assert!(manifests_for_domain(CapabilityDomain::Tts).len() >= 1);
    }

    #[test]
    fn models_dev_ids_come_from_vendor_manifests() {
        assert_eq!(
            models_dev_provider_ids("moonshot", "https://api.moonshot.cn/v1"),
            &["moonshotai-cn".to_string(), "moonshotai".to_string()]
        );
        assert_eq!(
            models_dev_provider_ids("moonshot", "https://api.moonshot.ai/v1"),
            &["moonshotai".to_string(), "moonshotai-cn".to_string()]
        );
        assert_eq!(
            models_dev_provider_ids("openrouter", "https://openrouter.ai/api/v1"),
            &["openrouter".to_string()]
        );
        assert!(models_dev_provider_ids("litert_lm", "local://").is_empty());
    }
}

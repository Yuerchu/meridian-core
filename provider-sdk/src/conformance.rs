//! Reusable conformance test kit for external vendor PRs.
//!
//! Enable via `meridian-provider-sdk = { features = ["conformance"] }` in
//! `[dev-dependencies]` and invoke these assertions from your vendor crate's
//! `#[cfg(test)]` suite.

use std::collections::BTreeSet;

use crate::chat::ChatProvider;
use crate::domain::{CapabilityDomain, ProtocolCompliance, SwitchImpact};
use crate::schema::{ConfigFieldKind, VendorManifest};
use crate::tts::TtsProvider;
use crate::web_search::WebSearchProvider;

/// Assert that a [`VendorManifest`] obeys Meridian's strict schema, URL/format
/// parity, and secret-isolation contracts.
pub fn assert_manifest_conformance(manifest: &VendorManifest) {
    assert_eq!(manifest.version, 1, "manifest `{}` must have version 1", manifest.id);
    assert!(!manifest.id.trim().is_empty(), "manifest id must not be empty");
    assert!(
        manifest
            .id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
        "manifest id `{}` must be lowercase snake_case",
        manifest.id
    );
    assert!(
        !manifest.name.trim().is_empty(),
        "manifest `{}` missing name",
        manifest.id
    );
    assert!(
        !manifest.icon.trim().is_empty(),
        "manifest `{}` missing icon",
        manifest.id
    );
    assert!(
        !manifest.domains.is_empty(),
        "manifest `{}` must declare at least one capability domain",
        manifest.id
    );

    if manifest.supports_domain(CapabilityDomain::Chat) {
        assert!(
            manifest.provider_type.is_some(),
            "chat manifest `{}` must declare provider_type",
            manifest.id
        );
        assert!(
            !manifest.auth.is_empty(),
            "chat manifest `{}` must declare at least one auth method",
            manifest.id
        );
    }

    let mut auth_ids = BTreeSet::new();
    for auth in &manifest.auth {
        assert!(
            auth_ids.insert(auth.id.as_str()),
            "manifest `{}` has duplicate auth id `{}`",
            manifest.id,
            auth.id
        );
        assert!(
            !auth.api_formats.is_empty(),
            "manifest `{}` auth `{}` must declare at least one api_format",
            manifest.id,
            auth.id
        );
        for fmt in &auth.api_formats {
            assert!(
                auth.default_base_url.contains_key(fmt),
                "manifest `{}` auth `{}` missing default_base_url for `{fmt}`",
                manifest.id,
                auth.id
            );
        }
        for key in auth.default_base_url.keys() {
            assert!(
                auth.api_formats.contains(key),
                "manifest `{}` auth `{}` has unused default_base_url key `{key}`",
                manifest.id,
                auth.id
            );
        }
    }

    let mut config_keys = BTreeSet::new();
    for field in &manifest.config_fields {
        assert!(
            config_keys.insert(field.key.as_str()),
            "manifest `{}` has duplicate config_field `{}`",
            manifest.id,
            field.key
        );
        if field.secret {
            assert!(
                matches!(field.kind, ConfigFieldKind::Secret | ConfigFieldKind::MultilineSecret),
                "manifest `{}` secret field `{}` must use secret or multiline_secret kind",
                manifest.id,
                field.key
            );
            assert!(
                field.default_value.is_none(),
                "manifest `{}` secret field `{}` must not declare a default_value",
                manifest.id,
                field.key
            );
        }
        if field.kind == ConfigFieldKind::Select {
            assert!(
                !field.options.is_empty(),
                "manifest `{}` select field `{}` must declare non-empty options",
                manifest.id,
                field.key
            );
        }
    }
}

/// Assert baseline invariants on any [`ChatProvider`] implementation.
pub fn assert_chat_provider_conformance(provider: &dyn ChatProvider) {
    let name = provider.adapter_name();
    assert!(!name.trim().is_empty(), "ChatProvider::adapter_name must not be empty");
    // Multi-turn chat providers default to Stateful (never silently fail over).
    assert!(
        !provider.switch_impact().allows_automatic_failover(),
        "ChatProvider `{name}` must not allow silent automatic failover across stateful turns"
    );
}

/// Assert baseline invariants on any [`WebSearchProvider`] implementation.
pub fn assert_web_search_provider_conformance(provider: &dyn WebSearchProvider) {
    assert!(!provider.provider_id().trim().is_empty());
    assert_eq!(
        provider.switch_impact(),
        SwitchImpact::Stateless,
        "WebSearchProvider `{}` must declare Stateless switch impact",
        provider.provider_id()
    );
}

/// Assert baseline invariants on any [`TtsProvider`] implementation.
pub fn assert_tts_provider_conformance(provider: &dyn TtsProvider) {
    assert!(!provider.provider_id().trim().is_empty());
    assert_eq!(
        provider.switch_impact(),
        SwitchImpact::Stateless,
        "TtsProvider `{}` must declare Stateless switch impact",
        provider.provider_id()
    );
}

/// Assert Official Standard Protocol Policy for a `(model, protocol)` pair.
pub fn assert_official_protocol_compliance(
    model_id: &str,
    actual_protocol: &str,
    compliance: &ProtocolCompliance,
    allow_degraded: bool,
) {
    match compliance {
        ProtocolCompliance::Standard | ProtocolCompliance::Unknown => {}
        ProtocolCompliance::Degraded {
            expected,
            lost_capabilities,
        } => {
            assert!(
                !expected.is_empty(),
                "Degraded compliance for `{model_id}` on `{actual_protocol}` must list expected official protocols"
            );
            assert!(
                !lost_capabilities.is_empty(),
                "Degraded compliance for `{model_id}` on `{actual_protocol}` must list concrete lost capabilities"
            );
            assert!(
                allow_degraded,
                "Official preset violation: model `{model_id}` uses non-standard protocol `{actual_protocol}`; expected one of {expected:?}"
            );
        }
    }
}

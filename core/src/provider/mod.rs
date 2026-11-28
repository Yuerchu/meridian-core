//! Provider facade re-exporting pure protocol implementations from
//! `meridian-providers` and SDK contracts from `meridian-provider-sdk`, plus
//! database-coupled helpers (`codex_metadata::for_install`, `local_model`,
//! `models`, `models_dev`).

pub mod codex_metadata;
pub mod local_model;
pub mod metering;
pub mod models;
pub mod models_dev;

pub use meridian_provider_sdk::chat::*;
pub use meridian_provider_sdk::chat_state as state;
pub use meridian_provider_sdk::domain::{CapabilityDomain, ProtocolCompliance, RoutingPolicy, SwitchImpact};
pub use meridian_provider_sdk::schema::{ConfigFieldKind, ConfigFieldOption, ConfigFieldSpec, VendorManifest};
pub use meridian_provider_sdk::wire as dto;
pub use meridian_providers::models as canonical_models;
pub use meridian_providers::{
    anthropic, balance, capabilities, catalog, codex, codex_identity, deepseek, gemma_tool, google_generate_content,
    google_interactions, litert_lm_provider, mixins, openai_compat, openai_responses, protocols, registry, vendors,
};

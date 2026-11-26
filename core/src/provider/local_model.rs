//! Database-aware LiteRT-LM local model helpers layered over
//! `meridian_providers::local_model`.

#[cfg(test)]
use std::path::PathBuf;

use litert_lm::model_info::ModelInfo;
pub use meridian_provider_sdk::chat::LocalInferenceBackend;
pub use meridian_providers::local_model::{
    Backend, FileStamp, LocalModelSnapshot, Modality, ModelKind, NpuBrand, SamplerType, engine_config, inspect,
    inspect_async, narrow_to_adapter, native_backend, runtime_version, stamp,
};

pub async fn for_provider(
    db: &crate::db::sea::cap::Db,
    provider_id: &str,
    refresh: bool,
) -> Result<LocalModelSnapshot, String> {
    let provider = crate::db::sea::ops::provider::get(db, provider_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("provider {provider_id} not found"))?;
    if provider.provider_type != super::registry::ProviderType::LitertLm {
        return Err("provider is not LiteRT-LM".into());
    }
    let preference = provider.local_backend.ok_or("LiteRT-LM requires local_backend")?;
    inspect_async(provider.base_url, preference, refresh).await
}

pub async fn apply_capabilities(
    db: &crate::db::sea::cap::Db,
    provider_id: Option<&str>,
    provider_type: &str,
    caps: &mut super::ProviderCapabilities,
) -> Result<Option<ModelInfo>, String> {
    if provider_type != "litert_lm" {
        return Ok(None);
    }
    let provider_id = provider_id.ok_or("local model capability resolution requires provider_id")?;
    let snapshot = for_provider(db, provider_id, false).await?;
    snapshot
        .info
        .select_backend(native_backend(snapshot.requested_backend))
        .map_err(|error| error.to_string())?;
    caps.supports_tools = snapshot.info.supports_function_calling;
    caps.supports_streaming_tools = caps.supports_tools;
    caps.supports_thinking = snapshot.info.supports_thinking;
    caps.supports_thinking_off = caps.supports_thinking;
    Ok(Some(snapshot.info))
}

#[cfg(test)]
#[path = "local_model_tests.rs"]
mod tests;

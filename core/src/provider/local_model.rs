use super::registry::LocalInferenceBackend;
pub use litert_lm::engine::Backend;
use litert_lm::engine::EngineConfig;
use litert_lm::model_info::ModelInfo;
pub use litert_lm::model_info::{Modality, ModelKind, NpuBrand, SamplerType};
pub use litert_lm::runtime::runtime_version;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

#[derive(Clone)]
struct CachedModel {
    stamp: FileStamp,
    revision: u64,
    info: ModelInfo,
}

static MODELS: Mutex<BTreeMap<PathBuf, CachedModel>> = Mutex::new(BTreeMap::new());
static NEXT_REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct LocalModelSnapshot {
    pub file_name: String,
    pub file_size: u64,
    pub revision: u64,
    pub info: ModelInfo,
    pub selected_backend: Option<Backend>,
    pub requested_backend: LocalInferenceBackend,
    pub(crate) path: PathBuf,
}

fn stamp(path: &Path) -> Result<FileStamp, String> {
    let metadata = path
        .metadata()
        .map_err(|error| format!("could not read model file: {error}"))?;
    if !metadata.is_file() {
        return Err("model path must point to a regular file".into());
    }
    Ok(FileStamp {
        path: path.to_path_buf(),
        size: metadata.len(),
        modified: metadata.modified().map_err(|error| error.to_string())?,
    })
}

pub fn inspect(path: &str, preference: LocalInferenceBackend, refresh: bool) -> Result<LocalModelSnapshot, String> {
    if path.is_empty() || path == "local://" || !Path::new(path).is_absolute() {
        return Err("select an absolute .litertlm model file path first".into());
    }
    if path.contains('\0') {
        return Err("model path contains NUL".into());
    }
    let path = std::fs::canonicalize(path).map_err(|error| format!("could not open model file: {error}"))?;
    let before = stamp(&path)?;
    let mut cache = MODELS.lock().map_err(|_| "local model cache lock was poisoned")?;
    let cached = match cache.get(&path).filter(|cached| !refresh && cached.stamp == before) {
        Some(cached) => cached.clone(),
        None => {
            let text = path.to_str().ok_or("model path is not valid UTF-8")?;
            let info = super::litert_lm_provider::get_or_init_runtime()?
                .inspect_model(text)
                .map_err(|error| error.to_string())?;
            if stamp(&path)? != before {
                return Err("model file changed during inspection; inspect it again".into());
            }
            let cached = CachedModel {
                stamp: before,
                revision: NEXT_REVISION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                info,
            };
            cache.insert(path.clone(), cached.clone());
            cached
        }
    };
    let selected_backend = if cached.info.kind == ModelKind::Llm {
        Some(
            cached
                .info
                .select_backend(preference.native())
                .map_err(|error| error.to_string())?,
        )
    } else {
        None
    };
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("model file has no UTF-8 filename")?
        .to_string();
    Ok(LocalModelSnapshot {
        file_name,
        file_size: cached.stamp.size,
        revision: cached.revision,
        info: cached.info,
        selected_backend,
        requested_backend: preference,
        path,
    })
}

pub async fn inspect_async(
    path: String,
    preference: LocalInferenceBackend,
    refresh: bool,
) -> Result<LocalModelSnapshot, String> {
    tokio::task::spawn_blocking(move || inspect(&path, preference, refresh))
        .await
        .map_err(|error| error.to_string())?
}

pub fn engine_config(
    snapshot: &LocalModelSnapshot,
    preference: LocalInferenceBackend,
    context: i32,
) -> Result<EngineConfig, String> {
    let backend = snapshot
        .info
        .select_backend(preference.native())
        .map_err(|error| error.to_string())?;
    snapshot
        .info
        .validate_context(context)
        .map_err(|error| error.to_string())?;
    let path = snapshot.path.to_str().ok_or("model path is not valid UTF-8")?;
    let mut config = EngineConfig::new(path, backend, context);
    config.vision_backend = snapshot.info.backends(Modality::Vision).first().copied();
    config.audio_backend = snapshot.info.backends(Modality::Audio).first().copied();
    Ok(config)
}

pub async fn for_provider(
    db: &crate::db::sea::cap::Db,
    provider_id: &str,
    refresh: bool,
) -> Result<LocalModelSnapshot, String> {
    let provider = crate::db::sea::ops::provider::get_provider(db, provider_id)
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
        .select_backend(snapshot.requested_backend.native())
        .map_err(|error| error.to_string())?;
    caps.supports_tools = snapshot.info.supports_function_calling;
    caps.supports_streaming_tools = caps.supports_tools;
    caps.supports_thinking = snapshot.info.supports_thinking;
    caps.supports_thinking_off = caps.supports_thinking;
    Ok(Some(snapshot.info))
}

pub fn narrow_to_adapter(caps: &mut super::ProviderCapabilities) {
    // 模型文件能力不能打开当前适配器尚未实现的图像/音频或采样参数。
    caps.supports_images = false;
    caps.supports_pdf = false;
    caps.supports_temperature = false;
    caps.supports_top_p = false;
    caps.max_temperature = None;
    caps.max_context_tokens = None;
    caps.max_output_tokens = None;
}

#[cfg(test)]
#[path = "local_model_tests.rs"]
mod tests;

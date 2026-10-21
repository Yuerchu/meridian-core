use crate::LiteRtError;
use crate::engine::Backend;
use crate::runtime::{Runtime, runtime_version};
use crate::sys::{self, LiteRtLmLoadedFile};
use serde::Serialize;
use std::ptr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelKind {
    Unknown,
    Llm,
    Embedding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    Text,
    Vision,
    Audio,
    Video,
}

impl Modality {
    const ALL: [Self; 4] = [Self::Text, Self::Vision, Self::Audio, Self::Video];

    fn native(self) -> i32 {
        match self {
            Self::Text => 0,
            Self::Vision => 1,
            Self::Audio => 2,
            Self::Video => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NpuBrand {
    Qualcomm,
    GoogleTensor,
    Mediatek,
    Intel,
    Samsung,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SamplerType {
    TopK,
    TopP,
    Greedy,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SamplerDefaults {
    pub sampler_type: SamplerType,
    pub temperature: f64,
    pub top_k: i32,
    pub top_p: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModalityBackends {
    pub modality: Modality,
    pub preferred_backends: Vec<Backend>,
    pub npu_brand: Option<NpuBrand>,
    pub soc_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelInfo {
    pub kind: ModelKind,
    pub modalities: Vec<ModalityBackends>,
    pub supports_thinking: bool,
    pub supports_function_calling: bool,
    pub supports_speculative_decoding: bool,
    pub sampler_defaults: Option<SamplerDefaults>,
    pub max_context_tokens: Option<u32>,
    pub is_dynamic_context: bool,
    pub max_vision_token_budget: Option<i32>,
    pub vision_signature_lengths: Option<Vec<i32>>,
    pub embedding_signature_lengths: Option<Vec<i32>>,
    pub embedding_dimension: Option<i32>,
    pub min_runtime_version: Option<String>,
}

impl ModelInfo {
    pub fn backends(&self, modality: Modality) -> &[Backend] {
        self.modalities
            .iter()
            .find(|value| value.modality == modality)
            .map_or(&[], |value| value.preferred_backends.as_slice())
    }

    pub fn select_backend(&self, requested: Option<Backend>) -> Result<Backend, LiteRtError> {
        if self.kind != ModelKind::Llm {
            return Err(LiteRtError::InvalidMetadata(
                "chat requires a text LLM, not an embedding or unknown model".into(),
            ));
        }
        let backends = self.backends(Modality::Text);
        let selected = requested
            .or_else(|| backends.first().copied())
            .ok_or_else(|| LiteRtError::InvalidMetadata("no text backend is reported by this model".into()))?;
        if !backends.contains(&selected) {
            return Err(LiteRtError::InvalidMetadata(format!(
                "model does not allow the requested {} backend",
                selected.as_str()
            )));
        }
        self.check_runtime_version()?;
        Ok(selected)
    }

    pub fn check_runtime_version(&self) -> Result<(), LiteRtError> {
        if let Some(ref required) = self.min_runtime_version {
            let required_version = semver::Version::parse(required).map_err(|error| {
                LiteRtError::InvalidMetadata(format!("invalid minimum runtime version {required}: {error}"))
            })?;
            let current = semver::Version::parse(runtime_version())
                .map_err(|error| LiteRtError::RuntimeIntegrity(error.to_string()))?;
            if required_version > current {
                return Err(LiteRtError::InvalidMetadata(format!(
                    "model requires LiteRT-LM {required}, installed {}",
                    runtime_version()
                )));
            }
        }
        Ok(())
    }

    pub fn validate_context(&self, tokens: i32) -> Result<(), LiteRtError> {
        if tokens <= 0 {
            return Err(LiteRtError::InvalidMetadata(
                "set a positive runtime context budget".into(),
            ));
        }
        if let Some(maximum) = self.max_context_tokens
            && (tokens as u32 > maximum || (!self.is_dynamic_context && tokens as u32 != maximum))
        {
            return Err(LiteRtError::InvalidMetadata(format!(
                "context {tokens} is incompatible with model context {maximum} (dynamic={})",
                self.is_dynamic_context
            )));
        }
        Ok(())
    }
}

struct LoadedFile<'a> {
    rt: &'a Runtime,
    ptr: *mut LiteRtLmLoadedFile,
}

impl Drop for LoadedFile<'_> {
    fn drop(&mut self) {
        unsafe { (self.rt.sym().loaded_file_delete)(self.ptr) };
    }
}

fn optional_nonnegative(value: i32, field: &str) -> Result<Option<i32>, LiteRtError> {
    match value {
        -1 => Ok(None),
        0.. => Ok(Some(value)),
        _ => Err(LiteRtError::InvalidMetadata(format!("invalid {field}: {value}"))),
    }
}

// 限制的是不可信元数据的分配量，不是模型的 token 或推理预算。
const MAX_SIGNATURE_COUNT: i32 = 4096;

fn signature_lengths(mut query: impl FnMut(*mut i32, i32) -> i32) -> Result<Option<Vec<i32>>, LiteRtError> {
    let count = query(ptr::null_mut(), 0);
    if count == -1 {
        return Ok(None);
    }
    if !(0..=MAX_SIGNATURE_COUNT).contains(&count) {
        return Err(LiteRtError::InvalidMetadata(format!("invalid signature count {count}")));
    }
    let mut values = vec![0; count as usize];
    if count > 0 && query(values.as_mut_ptr(), count) != count {
        return Err(LiteRtError::InvalidMetadata(
            "signature count changed during inspection".into(),
        ));
    }
    if values.iter().any(|value| *value <= 0) {
        return Err(LiteRtError::InvalidMetadata("non-positive signature length".into()));
    }
    Ok(Some(values))
}

impl Runtime {
    pub fn inspect_model(&self, model_path: &str) -> Result<ModelInfo, LiteRtError> {
        let path = sys::cstr(model_path)?;
        self.clear_error();
        let ptr = unsafe { (self.sym().loaded_file_create)(path.as_ptr()) };
        if ptr.is_null() {
            return Err(self.failure("loaded_file_create"));
        }
        let file = LoadedFile { rt: self, ptr };
        let sym = self.sym();
        let kind = match unsafe { (sym.loaded_file_model_type)(file.ptr) } {
            0 => ModelKind::Unknown,
            1 => ModelKind::Llm,
            2 => ModelKind::Embedding,
            value => return Err(LiteRtError::InvalidMetadata(format!("unknown model kind {value}"))),
        };
        let mut modalities = Vec::new();
        for modality in Modality::ALL {
            if !unsafe { (sym.loaded_file_supports_input_modality)(ptr, modality.native()) } {
                continue;
            }
            let count =
                unsafe { (sym.loaded_file_modality_supported_backends)(ptr, modality.native(), ptr::null_mut(), 0) };
            if !(0..=3).contains(&count) {
                return Err(LiteRtError::InvalidMetadata(format!("invalid backend count {count}")));
            }
            let mut native_backends = vec![0i32; count as usize];
            if count > 0
                && unsafe {
                    (sym.loaded_file_modality_supported_backends)(
                        ptr,
                        modality.native(),
                        native_backends.as_mut_ptr(),
                        count,
                    )
                } != count
            {
                return Err(LiteRtError::InvalidMetadata(
                    "backend count changed during inspection".into(),
                ));
            }
            let mut preferred_backends = Vec::new();
            for value in native_backends {
                let backend = Backend::from_native(value)?;
                if preferred_backends.contains(&backend) {
                    return Err(LiteRtError::InvalidMetadata("duplicate backend in metadata".into()));
                }
                preferred_backends.push(backend);
            }
            let npu_brand = match unsafe { (sym.loaded_file_modality_npu_brand)(ptr, modality.native()) } {
                0 => None,
                1 => Some(NpuBrand::Qualcomm),
                2 => Some(NpuBrand::GoogleTensor),
                3 => Some(NpuBrand::Mediatek),
                4 => Some(NpuBrand::Intel),
                5 => Some(NpuBrand::Samsung),
                value => return Err(LiteRtError::InvalidMetadata(format!("unknown NPU brand {value}"))),
            };
            let soc_name =
                sys::read_optional_cstr(unsafe { (sym.loaded_file_modality_soc_name)(ptr, modality.native()) })?;
            modalities.push(ModalityBackends {
                modality,
                preferred_backends,
                npu_brand,
                soc_name,
            });
        }
        let sampler_defaults = if kind == ModelKind::Llm {
            let sampler_type = match unsafe { (sym.loaded_file_sampler_type)(ptr) } {
                0 => None,
                1 => Some(SamplerType::TopK),
                2 => Some(SamplerType::TopP),
                3 => Some(SamplerType::Greedy),
                value => return Err(LiteRtError::InvalidMetadata(format!("unknown sampler {value}"))),
            };
            match sampler_type {
                Some(sampler_type) => {
                    let temperature = unsafe { (sym.loaded_file_sampler_temperature)(ptr) } as f64;
                    let top_p = unsafe { (sym.loaded_file_sampler_top_p)(ptr) } as f64;
                    let top_k = unsafe { (sym.loaded_file_sampler_top_k)(ptr) };
                    if !temperature.is_finite() || !top_p.is_finite() || top_k < 0 {
                        return Err(LiteRtError::InvalidMetadata("invalid default sampler values".into()));
                    }
                    Some(SamplerDefaults {
                        sampler_type,
                        temperature,
                        top_p,
                        top_k,
                    })
                }
                None => None,
            }
        } else {
            None
        };
        let maximum = unsafe { (sym.loaded_file_max_context_tokens)(ptr) };
        let info = ModelInfo {
            kind,
            modalities,
            supports_thinking: unsafe { (sym.loaded_file_supports_thinking)(ptr) },
            supports_function_calling: unsafe { (sym.loaded_file_supports_function_calling)(ptr) },
            supports_speculative_decoding: unsafe { (sym.loaded_file_has_speculative_decoding_support)(ptr) },
            sampler_defaults,
            max_context_tokens: if maximum == 0 { None } else { Some(maximum) },
            is_dynamic_context: unsafe { (sym.loaded_file_is_dynamic_context)(ptr) },
            max_vision_token_budget: optional_nonnegative(
                unsafe { (sym.loaded_file_max_vision_token_budget)(ptr) },
                "vision token budget",
            )?,
            vision_signature_lengths: signature_lengths(|out, count| unsafe {
                (sym.loaded_file_vision_signature_selection)(ptr, out, count)
            })?,
            embedding_signature_lengths: signature_lengths(|out, count| unsafe {
                (sym.loaded_file_embedding_signature_selection)(ptr, out, count)
            })?,
            embedding_dimension: optional_nonnegative(
                unsafe { (sym.loaded_file_embedding_dimension)(ptr) },
                "embedding dimension",
            )?,
            min_runtime_version: sys::read_optional_cstr(unsafe { (sym.loaded_file_min_runtime_version)(ptr) })?,
        };
        Ok(info)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn model(backends: &[Backend]) -> ModelInfo {
        ModelInfo {
            kind: ModelKind::Llm,
            modalities: vec![ModalityBackends {
                modality: Modality::Text,
                preferred_backends: backends.to_vec(),
                npu_brand: None,
                soc_name: None,
            }],
            supports_thinking: false,
            supports_function_calling: false,
            supports_speculative_decoding: false,
            sampler_defaults: None,
            max_context_tokens: Some(8192),
            is_dynamic_context: true,
            max_vision_token_budget: None,
            vision_signature_lengths: None,
            embedding_signature_lengths: None,
            embedding_dimension: None,
            min_runtime_version: None,
        }
    }

    #[test]
    fn automatic_selection_obeys_model_order_and_explicit_selection_is_checked() {
        for backend in [Backend::Cpu, Backend::Gpu, Backend::Npu] {
            let info = model(&[backend]);
            assert_eq!(info.select_backend(None).unwrap(), backend);
            assert_eq!(info.select_backend(Some(backend)).unwrap(), backend);
        }
        let info = model(&[Backend::Gpu, Backend::Cpu]);
        assert_eq!(info.select_backend(None).unwrap(), Backend::Gpu);
        assert_eq!(info.select_backend(Some(Backend::Cpu)).unwrap(), Backend::Cpu);
        assert!(info.select_backend(Some(Backend::Npu)).is_err());
        assert!(model(&[]).select_backend(None).is_err());
        assert!(model(&[Backend::Gpu]).select_backend(Some(Backend::Cpu)).is_err());
    }

    #[test]
    fn model_kind_runtime_version_and_context_are_not_guessed() {
        let mut info = model(&[Backend::Gpu]);
        info.min_runtime_version = Some("0.19.0".into());
        assert!(info.select_backend(None).is_err());
        info.min_runtime_version = Some("broken".into());
        assert!(info.check_runtime_version().is_err());
        info.min_runtime_version = Some("0.18.0".into());
        assert!(info.check_runtime_version().is_ok());
        info.kind = ModelKind::Embedding;
        assert!(info.select_backend(None).is_err());
        assert!(info.validate_context(0).is_err());
        assert!(info.validate_context(8193).is_err());
        assert!(info.validate_context(1024).is_ok());
        info.is_dynamic_context = false;
        assert!(info.validate_context(1024).is_err());
        assert!(info.validate_context(8192).is_ok());
    }

    #[test]
    fn invalid_or_changing_signature_sizes_fail() {
        assert_eq!(signature_lengths(|_, _| -1).unwrap(), None);
        assert!(signature_lengths(|_, _| MAX_SIGNATURE_COUNT + 1).is_err());
        let mut first = true;
        assert!(
            signature_lengths(|_, _| {
                if first {
                    first = false;
                    2
                } else {
                    1
                }
            })
            .is_err()
        );
    }
}

use crate::LiteRtError;
use crate::conversation::{Conversation, ConversationConfig};
use crate::runtime::Runtime;
use crate::sys::{self, LiteRtLmEngine, LiteRtLmEngineSettings};
use serde::{Deserialize, Serialize};
use std::ptr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Cpu,
    Gpu,
    Npu,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Gpu => "gpu",
            Self::Npu => "npu",
        }
    }

    pub(crate) fn from_native(value: i32) -> Result<Self, LiteRtError> {
        match value {
            1 => Ok(Self::Cpu),
            2 => Ok(Self::Gpu),
            3 => Ok(Self::Npu),
            _ => Err(LiteRtError::InvalidMetadata(format!("unknown backend {value}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineConfig {
    pub model_path: String,
    pub backend: Backend,
    pub vision_backend: Option<Backend>,
    pub audio_backend: Option<Backend>,
    pub max_tokens: i32,
    pub num_threads: Option<i32>,
    pub cache_dir: Option<String>,
}

impl EngineConfig {
    pub fn new(model_path: impl Into<String>, backend: Backend, max_tokens: i32) -> Self {
        Self {
            model_path: model_path.into(),
            backend,
            vision_backend: None,
            audio_backend: None,
            max_tokens,
            num_threads: None,
            cache_dir: None,
        }
    }
}

pub struct Engine {
    rt: Runtime,
    ptr: *mut LiteRtLmEngine,
}

unsafe impl Send for Engine {}
unsafe impl Sync for Engine {}

struct SettingsHandle<'a> {
    rt: &'a Runtime,
    ptr: *mut LiteRtLmEngineSettings,
}

impl Drop for SettingsHandle<'_> {
    fn drop(&mut self) {
        unsafe { (self.rt.sym().engine_settings_delete)(self.ptr) };
    }
}

impl Engine {
    pub fn new(rt: &Runtime, config: &EngineConfig) -> Result<Self, LiteRtError> {
        if config.max_tokens <= 0 {
            return Err(LiteRtError::OperationFailed(
                "a positive runtime context budget is required".into(),
            ));
        }
        let model_path = sys::cstr(&config.model_path)?;
        let backend = sys::cstr(config.backend.as_str())?;
        let vision = config
            .vision_backend
            .map(|value| sys::cstr(value.as_str()))
            .transpose()?;
        let audio = config
            .audio_backend
            .map(|value| sys::cstr(value.as_str()))
            .transpose()?;
        let create_error = |source| LiteRtError::EngineCreate {
            backend: config.backend.as_str().into(),
            model: config.model_path.clone(),
            source: Box::new(source),
        };
        rt.clear_error();
        let ptr = unsafe {
            (rt.sym().engine_settings_create)(
                model_path.as_ptr(),
                backend.as_ptr(),
                vision.as_ref().map_or(ptr::null(), |value| value.as_ptr()),
                audio.as_ref().map_or(ptr::null(), |value| value.as_ptr()),
            )
        };
        if ptr.is_null() {
            return Err(create_error(rt.failure("engine_settings_create")));
        }
        let settings = SettingsHandle { rt, ptr };
        rt.clear_error();
        unsafe { (rt.sym().engine_settings_set_max_num_tokens)(settings.ptr, config.max_tokens) };
        if let Some(threads) = config.num_threads {
            unsafe { (rt.sym().engine_settings_set_num_threads)(settings.ptr, threads) };
        }
        if let Some(ref dir) = config.cache_dir {
            let dir = sys::cstr(dir)?;
            unsafe { (rt.sym().engine_settings_set_cache_dir)(settings.ptr, dir.as_ptr()) };
        }
        if unsafe { (rt.sym().get_last_error_code)() } != 0 {
            return Err(create_error(rt.failure("engine_settings")));
        }
        rt.clear_error();
        let engine = unsafe { (rt.sym().engine_create)(settings.ptr) };
        if engine.is_null() {
            // 在 SettingsHandle::drop 可能覆盖线程局部错误之前复制诊断。
            return Err(create_error(rt.failure("engine_create")));
        }
        tracing::info!(
            backend = config.backend.as_str(),
            model = %config.model_path,
            max_tokens = config.max_tokens,
            "LiteRT-LM engine created"
        );
        Ok(Self {
            rt: rt.clone(),
            ptr: engine,
        })
    }

    pub fn create_conversation(&self, config: ConversationConfig) -> Result<Conversation, LiteRtError> {
        Conversation::new(&self.rt, self.ptr, config)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        unsafe { (self.rt.sym().engine_delete)(self.ptr) };
        tracing::debug!("LiteRT-LM engine destroyed");
    }
}

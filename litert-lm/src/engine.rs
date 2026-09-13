use crate::conversation::{Conversation, ConversationConfig};
use crate::runtime::Runtime;
use crate::sys::{self, LiteRtLmEngine};
use crate::LiteRtError;
use std::ptr;

/// Backend selection for the LiteRT-LM engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Cpu,
    Gpu,
}

impl Backend {
    fn as_cstr(&self) -> &'static str {
        match self {
            Backend::Cpu => "cpu",
            Backend::Gpu => "gpu",
        }
    }
}

/// Configuration for creating an [`Engine`].
pub struct EngineConfig {
    pub model_path: String,
    pub backend: Backend,
    pub max_tokens: i32,
    pub num_threads: Option<i32>,
    pub cache_dir: Option<String>,
}

impl EngineConfig {
    pub fn new(model_path: impl Into<String>, backend: Backend) -> Self {
        Self {
            model_path: model_path.into(),
            backend,
            max_tokens: 4096,
            num_threads: None,
            cache_dir: None,
        }
    }
}

/// A loaded LLM model, ready to create conversations.
///
/// One `Engine` per model per process. Creating an engine loads the model into
/// memory (CPU RAM or GPU VRAM) and is the most expensive operation (~1–3s).
///
/// `Engine` is `Send + Sync` — the C API internally synchronises access.
pub struct Engine {
    rt: Runtime,
    ptr: *mut LiteRtLmEngine,
}

unsafe impl Send for Engine {}
unsafe impl Sync for Engine {}

impl Engine {
    /// Create a new engine with the given configuration.
    pub fn new(rt: &Runtime, config: &EngineConfig) -> Result<Self, LiteRtError> {
        let sym = rt.sym();

        let model_path = sys::cstr(&config.model_path);
        let backend = sys::cstr(config.backend.as_cstr());

        let settings = unsafe {
            (sym.engine_settings_create)(
                model_path.as_ptr(),
                backend.as_ptr(),
                ptr::null(),
                ptr::null(),
            )
        };
        if settings.is_null() {
            return Err(LiteRtError::EngineCreate {
                backend: config.backend.as_cstr().into(),
                model: config.model_path.clone(),
            });
        }

        unsafe {
            (sym.engine_settings_set_max_num_tokens)(settings, config.max_tokens);
        }

        if let Some(threads) = config.num_threads {
            unsafe { (sym.engine_settings_set_num_threads)(settings, threads) };
        }

        if let Some(ref dir) = config.cache_dir {
            let dir_c = sys::cstr(dir);
            unsafe { (sym.engine_settings_set_cache_dir)(settings, dir_c.as_ptr()) };
        }

        let engine = unsafe { (sym.engine_create)(settings) };
        unsafe { (sym.engine_settings_delete)(settings) };

        if engine.is_null() {
            return Err(LiteRtError::EngineCreate {
                backend: config.backend.as_cstr().into(),
                model: config.model_path.clone(),
            });
        }

        tracing::info!(
            backend = config.backend.as_cstr(),
            model = %config.model_path,
            max_tokens = config.max_tokens,
            "LiteRT-LM engine created"
        );

        Ok(Self {
            rt: rt.clone(),
            ptr: engine,
        })
    }

    /// Create a new conversation on this engine.
    pub fn create_conversation(
        &self,
        config: ConversationConfig,
    ) -> Result<Conversation, LiteRtError> {
        Conversation::new(&self.rt, self.ptr, config)
    }

    pub(crate) fn ptr(&self) -> *mut LiteRtLmEngine {
        self.ptr
    }

    pub(crate) fn runtime(&self) -> &Runtime {
        &self.rt
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { (self.rt.sym().engine_delete)(self.ptr) };
            tracing::debug!("LiteRT-LM engine destroyed");
        }
    }
}

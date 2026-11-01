use crate::LiteRtError;
use crate::engine::Backend;
use crate::runtime::Runtime;
use crate::sys::{
    self, INPUT_DATA_TYPE_IMAGE, INPUT_DATA_TYPE_TEXT, LiteRtLmEmbeddingEngine, LiteRtLmEmbeddingEngineSettings,
};
use std::ptr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingConfig {
    pub model_path: String,
    pub backend: Backend,
    pub vision_backend: Option<Backend>,
    pub cache_dir: Option<String>,
    pub num_threads: Option<i32>,
}

impl EmbeddingConfig {
    pub fn new(model_path: impl Into<String>, backend: Backend) -> Self {
        Self {
            model_path: model_path.into(),
            backend,
            vision_backend: None,
            cache_dir: None,
            num_threads: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum EmbeddingPart<'a> {
    Text(&'a str),
    Image(&'a [u8]),
}

pub struct EmbeddingEngine {
    rt: Runtime,
    ptr: *mut LiteRtLmEmbeddingEngine,
}

unsafe impl Send for EmbeddingEngine {}
unsafe impl Sync for EmbeddingEngine {}

struct SettingsHandle<'a> {
    rt: &'a Runtime,
    ptr: *mut LiteRtLmEmbeddingEngineSettings,
}

impl Drop for SettingsHandle<'_> {
    fn drop(&mut self) {
        unsafe { (self.rt.sym().embedding_engine_settings_delete)(self.ptr) };
    }
}

struct InputGuard<'a> {
    rt: &'a Runtime,
    ptr: *mut sys::LiteRtLmInputData,
}

impl Drop for InputGuard<'_> {
    fn drop(&mut self) {
        unsafe { (self.rt.sym().input_data_delete)(self.ptr) };
    }
}

impl EmbeddingEngine {
    pub fn new(rt: &Runtime, config: &EmbeddingConfig) -> Result<Self, LiteRtError> {
        let model_path = sys::cstr(&config.model_path)?;
        let backend = sys::cstr(config.backend.as_str())?;
        let vision = config
            .vision_backend
            .map(|value| sys::cstr(value.as_str()))
            .transpose()?;
        let create_error = |source| LiteRtError::EngineCreate {
            backend: config.backend.as_str().into(),
            model: config.model_path.clone(),
            source: Box::new(source),
        };
        rt.clear_error();
        let ptr = unsafe {
            (rt.sym().embedding_engine_settings_create)(
                model_path.as_ptr(),
                backend.as_ptr(),
                vision.as_ref().map_or(ptr::null(), |value| value.as_ptr()),
                ptr::null(),
            )
        };
        if ptr.is_null() {
            return Err(create_error(rt.failure("embedding_engine_settings_create")));
        }
        let settings = SettingsHandle { rt, ptr };
        if let Some(threads) = config.num_threads {
            unsafe { (rt.sym().embedding_engine_settings_set_num_threads)(settings.ptr, threads) };
        }
        if let Some(ref dir) = config.cache_dir {
            let dir = sys::cstr(dir)?;
            unsafe { (rt.sym().embedding_engine_settings_set_cache_dir)(settings.ptr, dir.as_ptr()) };
        }
        if unsafe { (rt.sym().get_last_error_code)() } != 0 {
            return Err(create_error(rt.failure("embedding_engine_settings")));
        }
        rt.clear_error();
        let engine = unsafe { (rt.sym().embedding_engine_create)(settings.ptr) };
        if engine.is_null() {
            return Err(create_error(rt.failure("embedding_engine_create")));
        }
        Ok(Self {
            rt: rt.clone(),
            ptr: engine,
        })
    }

    pub fn embed_text(&self, text: &str) -> Result<Vec<f32>, LiteRtError> {
        self.embed_parts(&[EmbeddingPart::Text(text)])
    }

    pub fn embed_image(&self, bytes: &[u8]) -> Result<Vec<f32>, LiteRtError> {
        self.embed_parts(&[EmbeddingPart::Image(bytes)])
    }

    pub fn embed_parts(&self, parts: &[EmbeddingPart<'_>]) -> Result<Vec<f32>, LiteRtError> {
        if parts.is_empty() {
            return Err(LiteRtError::OperationFailed("embedding input was empty".into()));
        }
        self.rt.clear_error();
        let mut inputs = Vec::with_capacity(parts.len());
        for part in parts {
            let (kind, data, size) = match *part {
                EmbeddingPart::Text(text) => (INPUT_DATA_TYPE_TEXT, text.as_ptr().cast(), text.len()),
                EmbeddingPart::Image(bytes) => (INPUT_DATA_TYPE_IMAGE, bytes.as_ptr().cast(), bytes.len()),
            };
            let input = unsafe { (self.rt.sym().input_data_create)(kind, data, size) };
            if input.is_null() {
                return Err(self.rt.failure("input_data_create"));
            }
            inputs.push(InputGuard {
                rt: &self.rt,
                ptr: input,
            });
        }

        let options = unsafe { (self.rt.sym().embedding_options_create)() };
        if options.is_null() {
            return Err(self.rt.failure("embedding_options_create"));
        }
        struct OptionsGuard<'a> {
            rt: &'a Runtime,
            ptr: *mut sys::LiteRtLmEmbeddingOptions,
        }
        impl Drop for OptionsGuard<'_> {
            fn drop(&mut self) {
                unsafe { (self.rt.sym().embedding_options_delete)(self.ptr) };
            }
        }
        let options = OptionsGuard {
            rt: &self.rt,
            ptr: options,
        };
        unsafe { (self.rt.sym().embedding_options_set_normalize)(options.ptr, true) };

        let ptrs: Vec<*const sys::LiteRtLmInputData> = inputs.iter().map(|input| input.ptr as *const _).collect();
        let response = unsafe {
            (self.rt.sym().embedding_engine_compute_embedding)(self.ptr, ptrs.as_ptr(), ptrs.len(), options.ptr)
        };
        if response.is_null() {
            return Err(self.rt.failure("embedding_engine_compute_embedding"));
        }
        struct ResponseGuard<'a> {
            rt: &'a Runtime,
            ptr: *mut sys::LiteRtLmEmbeddingResponse,
        }
        impl Drop for ResponseGuard<'_> {
            fn drop(&mut self) {
                unsafe { (self.rt.sym().embedding_response_delete)(self.ptr) };
            }
        }
        let response = ResponseGuard {
            rt: &self.rt,
            ptr: response,
        };
        let size = unsafe { (self.rt.sym().embedding_response_get_size)(response.ptr) };
        let values = unsafe { (self.rt.sym().embedding_response_get_values)(response.ptr) };
        if size == 0 || values.is_null() {
            return Err(LiteRtError::OperationFailed("embedding response was empty".into()));
        }
        Ok(unsafe { std::slice::from_raw_parts(values, size) }.to_vec())
    }
}

impl Drop for EmbeddingEngine {
    fn drop(&mut self) {
        unsafe { (self.rt.sym().embedding_engine_delete)(self.ptr) };
    }
}

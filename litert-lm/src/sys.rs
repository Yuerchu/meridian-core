#![allow(non_camel_case_types)]

use std::os::raw::{c_char, c_int, c_void};

// Opaque C types — never instantiated on the Rust side.
macro_rules! opaque {
    ($($name:ident),+ $(,)?) => {
        $(pub enum $name {})+
    };
}

opaque! {
    LiteRtLmEngineSettings,
    LiteRtLmEngine,
    LiteRtLmConversationConfig,
    LiteRtLmConversation,
    LiteRtLmConversationOptionalArgs,
    LiteRtLmJsonResponse,
    LiteRtLmStreamChunk,
    LiteRtLmThinkingConfig,
}

pub type StreamCallback = unsafe extern "C" fn(user_data: *mut c_void, chunk: *const LiteRtLmStreamChunk);

/// All C API function pointers loaded at runtime via `libloading`.
///
/// Signatures are taken from `python/litert_lm/_ffi.py` (ctypes declarations)
/// and verified against the v0.17.0 DLL export table (390 symbols).
pub(crate) struct Symbols {
    // ── Engine settings ──
    pub engine_settings_create: unsafe extern "C" fn(
        model_path: *const c_char,
        backend: *const c_char,
        vision_backend: *const c_char,
        audio_backend: *const c_char,
    ) -> *mut LiteRtLmEngineSettings,
    pub engine_settings_delete: unsafe extern "C" fn(*mut LiteRtLmEngineSettings),
    pub engine_settings_set_max_num_tokens: unsafe extern "C" fn(*mut LiteRtLmEngineSettings, c_int),
    pub engine_settings_set_num_threads: unsafe extern "C" fn(*mut LiteRtLmEngineSettings, c_int),
    pub engine_settings_set_cache_dir: unsafe extern "C" fn(*mut LiteRtLmEngineSettings, *const c_char),

    // ── Engine ──
    pub engine_create: unsafe extern "C" fn(*const LiteRtLmEngineSettings) -> *mut LiteRtLmEngine,
    pub engine_delete: unsafe extern "C" fn(*mut LiteRtLmEngine),

    // ── Conversation config ──
    pub conversation_config_create: unsafe extern "C" fn() -> *mut LiteRtLmConversationConfig,
    pub conversation_config_delete: unsafe extern "C" fn(*mut LiteRtLmConversationConfig),
    pub conversation_config_set_tools: unsafe extern "C" fn(*mut LiteRtLmConversationConfig, *const c_char),
    pub conversation_config_set_system_message: unsafe extern "C" fn(*mut LiteRtLmConversationConfig, *const c_char),
    pub conversation_config_set_messages: unsafe extern "C" fn(*mut LiteRtLmConversationConfig, *const c_char),
    pub conversation_config_set_enable_constrained_decoding:
        unsafe extern "C" fn(*mut LiteRtLmConversationConfig, bool),
    pub conversation_config_set_stream_tool_calls:
        unsafe extern "C" fn(*mut LiteRtLmConversationConfig, bool, *const c_char),

    // ── Conversation ──
    pub conversation_create:
        unsafe extern "C" fn(*mut LiteRtLmEngine, *mut LiteRtLmConversationConfig) -> *mut LiteRtLmConversation,
    pub conversation_delete: unsafe extern "C" fn(*mut LiteRtLmConversation),
    pub conversation_clone: unsafe extern "C" fn(*mut LiteRtLmConversation) -> *mut LiteRtLmConversation,
    pub conversation_send_message_stream: unsafe extern "C" fn(
        *mut LiteRtLmConversation,
        *const c_char, // message_json
        *const c_char, // extra_context
        *const LiteRtLmConversationOptionalArgs,
        StreamCallback,
        *mut c_void, // callback_data
    ) -> c_int,
    pub conversation_cancel_process: unsafe extern "C" fn(*mut LiteRtLmConversation),
    pub conversation_get_token_count: unsafe extern "C" fn(*mut LiteRtLmConversation) -> c_int,

    // ── JSON response ──
    pub json_response_get_string: unsafe extern "C" fn(*const LiteRtLmJsonResponse) -> *const c_char,
    pub json_response_delete: unsafe extern "C" fn(*mut LiteRtLmJsonResponse),

    // ── Stream chunk ──
    pub stream_chunk_get_text: unsafe extern "C" fn(*const LiteRtLmStreamChunk) -> *const c_char,
    pub stream_chunk_is_final: unsafe extern "C" fn(*const LiteRtLmStreamChunk) -> bool,
    pub stream_chunk_get_error: unsafe extern "C" fn(*const LiteRtLmStreamChunk) -> *const c_char,

    // ── Optional args ──
    pub conversation_optional_args_create: unsafe extern "C" fn() -> *mut LiteRtLmConversationOptionalArgs,
    pub conversation_optional_args_delete: unsafe extern "C" fn(*mut LiteRtLmConversationOptionalArgs),

    // ── Log ──
    pub set_min_log_level: unsafe extern "C" fn(c_int),
}

macro_rules! load_sym {
    ($lib:expr, $name:literal) => {{
        let sym: libloading::Symbol<unsafe extern "C" fn()> = $lib
            .get(concat!("litert_lm_", $name).as_bytes())
            .map_err(|_| crate::LiteRtError::MissingSymbol(concat!("litert_lm_", $name).into()))?;
        let fp = *sym;
        std::mem::forget(sym);
        fp
    }};
}

impl Symbols {
    /// # Safety
    /// The library must be a valid LiteRT-LM shared library.
    pub(crate) unsafe fn load(lib: &libloading::Library) -> Result<Self, crate::LiteRtError> {
        Ok(unsafe {
            Self {
                engine_settings_create: std::mem::transmute(load_sym!(lib, "engine_settings_create")),
                engine_settings_delete: std::mem::transmute(load_sym!(lib, "engine_settings_delete")),
                engine_settings_set_max_num_tokens: std::mem::transmute(load_sym!(
                    lib,
                    "engine_settings_set_max_num_tokens"
                )),
                engine_settings_set_num_threads: std::mem::transmute(load_sym!(lib, "engine_settings_set_num_threads")),
                engine_settings_set_cache_dir: std::mem::transmute(load_sym!(lib, "engine_settings_set_cache_dir")),
                engine_create: std::mem::transmute(load_sym!(lib, "engine_create")),
                engine_delete: std::mem::transmute(load_sym!(lib, "engine_delete")),
                conversation_config_create: std::mem::transmute(load_sym!(lib, "conversation_config_create")),
                conversation_config_delete: std::mem::transmute(load_sym!(lib, "conversation_config_delete")),
                conversation_config_set_tools: std::mem::transmute(load_sym!(lib, "conversation_config_set_tools")),
                conversation_config_set_system_message: std::mem::transmute(load_sym!(
                    lib,
                    "conversation_config_set_system_message"
                )),
                conversation_config_set_messages: std::mem::transmute(load_sym!(
                    lib,
                    "conversation_config_set_messages"
                )),
                conversation_config_set_enable_constrained_decoding: std::mem::transmute(load_sym!(
                    lib,
                    "conversation_config_set_enable_constrained_decoding"
                )),
                conversation_config_set_stream_tool_calls: std::mem::transmute(load_sym!(
                    lib,
                    "conversation_config_set_stream_tool_calls"
                )),
                conversation_create: std::mem::transmute(load_sym!(lib, "conversation_create")),
                conversation_delete: std::mem::transmute(load_sym!(lib, "conversation_delete")),
                conversation_clone: std::mem::transmute(load_sym!(lib, "conversation_clone")),
                conversation_send_message_stream: std::mem::transmute(load_sym!(
                    lib,
                    "conversation_send_message_stream"
                )),
                conversation_cancel_process: std::mem::transmute(load_sym!(lib, "conversation_cancel_process")),
                conversation_get_token_count: std::mem::transmute(load_sym!(lib, "conversation_get_token_count")),
                json_response_get_string: std::mem::transmute(load_sym!(lib, "json_response_get_string")),
                json_response_delete: std::mem::transmute(load_sym!(lib, "json_response_delete")),
                stream_chunk_get_text: std::mem::transmute(load_sym!(lib, "stream_chunk_get_text")),
                stream_chunk_is_final: std::mem::transmute(load_sym!(lib, "stream_chunk_is_final")),
                stream_chunk_get_error: std::mem::transmute(load_sym!(lib, "stream_chunk_get_error")),
                conversation_optional_args_create: std::mem::transmute(load_sym!(
                    lib,
                    "conversation_optional_args_create"
                )),
                conversation_optional_args_delete: std::mem::transmute(load_sym!(
                    lib,
                    "conversation_optional_args_delete"
                )),
                set_min_log_level: std::mem::transmute(load_sym!(lib, "set_min_log_level")),
            }
        })
    }
}

/// Read a C string pointer into an owned `String`. Returns an empty string for
/// null pointers.
pub(crate) fn read_cstr(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        unsafe { std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned() }
    }
}

/// Create a `CString`, panicking on embedded NUL (which the C API cannot
/// handle anyway).
pub(crate) fn cstr(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).expect("string passed to C API contains NUL")
}

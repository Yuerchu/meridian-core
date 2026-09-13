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
    LiteRtLmStreamChunk,
}

pub type StreamCallback = unsafe extern "C" fn(user_data: *mut c_void, chunk: *const LiteRtLmStreamChunk);

type EngineSettingsCreate = unsafe extern "C" fn(
    model_path: *const c_char,
    backend: *const c_char,
    vision_backend: *const c_char,
    audio_backend: *const c_char,
) -> *mut LiteRtLmEngineSettings;
type EngineSettingsDelete = unsafe extern "C" fn(*mut LiteRtLmEngineSettings);
type EngineSettingsSetInt = unsafe extern "C" fn(*mut LiteRtLmEngineSettings, c_int);
type EngineSettingsSetStr = unsafe extern "C" fn(*mut LiteRtLmEngineSettings, *const c_char);
type EngineCreate = unsafe extern "C" fn(*const LiteRtLmEngineSettings) -> *mut LiteRtLmEngine;
type EngineDelete = unsafe extern "C" fn(*mut LiteRtLmEngine);
type ConfigCreate = unsafe extern "C" fn() -> *mut LiteRtLmConversationConfig;
type ConfigDelete = unsafe extern "C" fn(*mut LiteRtLmConversationConfig);
type ConfigSetStr = unsafe extern "C" fn(*mut LiteRtLmConversationConfig, *const c_char);
type ConfigSetBool = unsafe extern "C" fn(*mut LiteRtLmConversationConfig, bool);
type ConversationCreate =
    unsafe extern "C" fn(*mut LiteRtLmEngine, *mut LiteRtLmConversationConfig) -> *mut LiteRtLmConversation;
type ConversationDelete = unsafe extern "C" fn(*mut LiteRtLmConversation);
type ConversationClone = unsafe extern "C" fn(*mut LiteRtLmConversation) -> *mut LiteRtLmConversation;
type ConversationSendMessageStream = unsafe extern "C" fn(
    *mut LiteRtLmConversation,
    *const c_char, // message_json
    *const c_char, // extra_context
    *const LiteRtLmConversationOptionalArgs,
    StreamCallback,
    *mut c_void, // callback_data
) -> c_int;
type ConversationCancel = unsafe extern "C" fn(*mut LiteRtLmConversation);
type ConversationTokenCount = unsafe extern "C" fn(*mut LiteRtLmConversation) -> c_int;
type ChunkGetStr = unsafe extern "C" fn(*const LiteRtLmStreamChunk) -> *const c_char;
type ChunkIsFinal = unsafe extern "C" fn(*const LiteRtLmStreamChunk) -> bool;
type SetMinLogLevel = unsafe extern "C" fn(c_int);

/// All C API function pointers loaded at runtime via `libloading`.
///
/// Signatures are taken from `python/litert_lm/_ffi.py` (ctypes declarations)
/// and verified against the v0.17.0 DLL export table (390 symbols).
pub(crate) struct Symbols {
    // ── Engine settings ──
    pub engine_settings_create: EngineSettingsCreate,
    pub engine_settings_delete: EngineSettingsDelete,
    pub engine_settings_set_max_num_tokens: EngineSettingsSetInt,
    pub engine_settings_set_num_threads: EngineSettingsSetInt,
    pub engine_settings_set_cache_dir: EngineSettingsSetStr,

    // ── Engine ──
    pub engine_create: EngineCreate,
    pub engine_delete: EngineDelete,

    // ── Conversation config ──
    pub conversation_config_create: ConfigCreate,
    pub conversation_config_delete: ConfigDelete,
    pub conversation_config_set_tools: ConfigSetStr,
    pub conversation_config_set_system_message: ConfigSetStr,
    pub conversation_config_set_messages: ConfigSetStr,
    pub conversation_config_set_enable_constrained_decoding: ConfigSetBool,

    // ── Conversation ──
    pub conversation_create: ConversationCreate,
    pub conversation_delete: ConversationDelete,
    pub conversation_clone: ConversationClone,
    pub conversation_send_message_stream: ConversationSendMessageStream,
    pub conversation_cancel_process: ConversationCancel,
    pub conversation_get_token_count: ConversationTokenCount,

    // ── Stream chunk ──
    pub stream_chunk_get_text: ChunkGetStr,
    pub stream_chunk_is_final: ChunkIsFinal,
    pub stream_chunk_get_error: ChunkGetStr,

    // ── Log ──
    pub set_min_log_level: SetMinLogLevel,
}

macro_rules! load_sym {
    ($lib:expr, $name:literal, $ty:ty) => {{
        let sym: libloading::Symbol<$ty> = unsafe { $lib.get(concat!("litert_lm_", $name).as_bytes()) }
            .map_err(|_| crate::LiteRtError::MissingSymbol(concat!("litert_lm_", $name).into()))?;
        *sym
    }};
}

impl Symbols {
    /// # Safety
    /// The library must be a valid LiteRT-LM shared library.
    pub(crate) unsafe fn load(lib: &libloading::Library) -> Result<Self, crate::LiteRtError> {
        Ok(Self {
            engine_settings_create: load_sym!(lib, "engine_settings_create", EngineSettingsCreate),
            engine_settings_delete: load_sym!(lib, "engine_settings_delete", EngineSettingsDelete),
            engine_settings_set_max_num_tokens: load_sym!(
                lib,
                "engine_settings_set_max_num_tokens",
                EngineSettingsSetInt
            ),
            engine_settings_set_num_threads: load_sym!(lib, "engine_settings_set_num_threads", EngineSettingsSetInt),
            engine_settings_set_cache_dir: load_sym!(lib, "engine_settings_set_cache_dir", EngineSettingsSetStr),
            engine_create: load_sym!(lib, "engine_create", EngineCreate),
            engine_delete: load_sym!(lib, "engine_delete", EngineDelete),
            conversation_config_create: load_sym!(lib, "conversation_config_create", ConfigCreate),
            conversation_config_delete: load_sym!(lib, "conversation_config_delete", ConfigDelete),
            conversation_config_set_tools: load_sym!(lib, "conversation_config_set_tools", ConfigSetStr),
            conversation_config_set_system_message: load_sym!(
                lib,
                "conversation_config_set_system_message",
                ConfigSetStr
            ),
            conversation_config_set_messages: load_sym!(lib, "conversation_config_set_messages", ConfigSetStr),
            conversation_config_set_enable_constrained_decoding: load_sym!(
                lib,
                "conversation_config_set_enable_constrained_decoding",
                ConfigSetBool
            ),
            conversation_create: load_sym!(lib, "conversation_create", ConversationCreate),
            conversation_delete: load_sym!(lib, "conversation_delete", ConversationDelete),
            conversation_clone: load_sym!(lib, "conversation_clone", ConversationClone),
            conversation_send_message_stream: load_sym!(
                lib,
                "conversation_send_message_stream",
                ConversationSendMessageStream
            ),
            conversation_cancel_process: load_sym!(lib, "conversation_cancel_process", ConversationCancel),
            conversation_get_token_count: load_sym!(lib, "conversation_get_token_count", ConversationTokenCount),
            stream_chunk_get_text: load_sym!(lib, "stream_chunk_get_text", ChunkGetStr),
            stream_chunk_is_final: load_sym!(lib, "stream_chunk_is_final", ChunkIsFinal),
            stream_chunk_get_error: load_sym!(lib, "stream_chunk_get_error", ChunkGetStr),
            set_min_log_level: load_sym!(lib, "set_min_log_level", SetMinLogLevel),
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

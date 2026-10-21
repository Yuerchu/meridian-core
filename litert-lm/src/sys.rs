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
    LiteRtLmLoadedFile,
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
type OptionalArgsCreate = unsafe extern "C" fn() -> *mut LiteRtLmConversationOptionalArgs;
type OptionalArgsDelete = unsafe extern "C" fn(*mut LiteRtLmConversationOptionalArgs);
type OptionalArgsMaxOutput = unsafe extern "C" fn(*mut LiteRtLmConversationOptionalArgs, c_int);
type ConversationTokenCount = unsafe extern "C" fn(*mut LiteRtLmConversation) -> c_int;
type ChunkGetStr = unsafe extern "C" fn(*const LiteRtLmStreamChunk) -> *const c_char;
type ChunkIsFinal = unsafe extern "C" fn(*const LiteRtLmStreamChunk) -> bool;
type SetMinLogLevel = unsafe extern "C" fn(c_int);
type LoadedFileCreate = unsafe extern "C" fn(*const c_char) -> *mut LiteRtLmLoadedFile;
type LoadedFileDelete = unsafe extern "C" fn(*mut LiteRtLmLoadedFile);
type FileGetBool = unsafe extern "C" fn(*mut LiteRtLmLoadedFile) -> bool;
type FileGetInt = unsafe extern "C" fn(*mut LiteRtLmLoadedFile) -> c_int;
type FileGetUint = unsafe extern "C" fn(*mut LiteRtLmLoadedFile) -> u32;
type FileGetFloat = unsafe extern "C" fn(*mut LiteRtLmLoadedFile) -> f32;
type FileGetString = unsafe extern "C" fn(*mut LiteRtLmLoadedFile) -> *const c_char;
type FileGetVector = unsafe extern "C" fn(*mut LiteRtLmLoadedFile, *mut c_int, c_int) -> c_int;
type FileModalityBool = unsafe extern "C" fn(*mut LiteRtLmLoadedFile, c_int) -> bool;
type FileModalityInt = unsafe extern "C" fn(*mut LiteRtLmLoadedFile, c_int) -> c_int;
type FileModalityString = unsafe extern "C" fn(*mut LiteRtLmLoadedFile, c_int) -> *const c_char;
type FileModalityVector = unsafe extern "C" fn(*mut LiteRtLmLoadedFile, c_int, *mut c_int, c_int) -> c_int;
type LastErrorMessage = unsafe extern "C" fn() -> *const c_char;
type LastErrorCode = unsafe extern "C" fn() -> c_int;
type ClearLastError = unsafe extern "C" fn();

/// 动态加载的签名只对应 v0.18.0 / C ABI 0.2.0，不接受 main 的新 ABI。
#[derive(Clone)]
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
    pub conversation_optional_args_create: OptionalArgsCreate,
    pub conversation_optional_args_delete: OptionalArgsDelete,
    pub conversation_optional_args_set_max_output_tokens: OptionalArgsMaxOutput,
    pub conversation_get_token_count: ConversationTokenCount,

    // ── Stream chunk ──
    pub stream_chunk_get_text: ChunkGetStr,
    pub stream_chunk_is_final: ChunkIsFinal,
    pub stream_chunk_get_error: ChunkGetStr,

    // ── Log ──
    pub set_min_log_level: SetMinLogLevel,
    pub loaded_file_create: LoadedFileCreate,
    pub loaded_file_delete: LoadedFileDelete,
    pub loaded_file_model_type: FileGetInt,
    pub loaded_file_has_speculative_decoding_support: FileGetBool,
    pub loaded_file_supports_thinking: FileGetBool,
    pub loaded_file_supports_function_calling: FileGetBool,
    pub loaded_file_sampler_type: FileGetInt,
    pub loaded_file_sampler_temperature: FileGetFloat,
    pub loaded_file_sampler_top_k: FileGetInt,
    pub loaded_file_sampler_top_p: FileGetFloat,
    pub loaded_file_supports_input_modality: FileModalityBool,
    pub loaded_file_max_vision_token_budget: FileGetInt,
    pub loaded_file_max_context_tokens: FileGetUint,
    pub loaded_file_is_dynamic_context: FileGetBool,
    pub loaded_file_vision_signature_selection: FileGetVector,
    pub loaded_file_embedding_signature_selection: FileGetVector,
    pub loaded_file_embedding_dimension: FileGetInt,
    pub loaded_file_modality_supported_backends: FileModalityVector,
    pub loaded_file_modality_npu_brand: FileModalityInt,
    pub loaded_file_modality_soc_name: FileModalityString,
    pub loaded_file_min_runtime_version: FileGetString,
    pub get_last_error_message: LastErrorMessage,
    pub get_last_error_code: LastErrorCode,
    pub clear_last_error: ClearLastError,
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
            conversation_optional_args_create: load_sym!(lib, "conversation_optional_args_create", OptionalArgsCreate),
            conversation_optional_args_delete: load_sym!(lib, "conversation_optional_args_delete", OptionalArgsDelete),
            conversation_optional_args_set_max_output_tokens: load_sym!(
                lib,
                "conversation_optional_args_set_max_output_tokens",
                OptionalArgsMaxOutput
            ),
            conversation_get_token_count: load_sym!(lib, "conversation_get_token_count", ConversationTokenCount),
            stream_chunk_get_text: load_sym!(lib, "stream_chunk_get_text", ChunkGetStr),
            stream_chunk_is_final: load_sym!(lib, "stream_chunk_is_final", ChunkIsFinal),
            stream_chunk_get_error: load_sym!(lib, "stream_chunk_get_error", ChunkGetStr),
            set_min_log_level: load_sym!(lib, "set_min_log_level", SetMinLogLevel),
            loaded_file_create: load_sym!(lib, "loaded_file_create", LoadedFileCreate),
            loaded_file_delete: load_sym!(lib, "loaded_file_delete", LoadedFileDelete),
            loaded_file_model_type: load_sym!(lib, "loaded_file_model_type", FileGetInt),
            loaded_file_has_speculative_decoding_support: load_sym!(
                lib,
                "loaded_file_has_speculative_decoding_support",
                FileGetBool
            ),
            loaded_file_supports_thinking: load_sym!(lib, "loaded_file_supports_thinking", FileGetBool),
            loaded_file_supports_function_calling: load_sym!(lib, "loaded_file_supports_function_calling", FileGetBool),
            loaded_file_sampler_type: load_sym!(lib, "loaded_file_sampler_type", FileGetInt),
            loaded_file_sampler_temperature: load_sym!(lib, "loaded_file_sampler_temperature", FileGetFloat),
            loaded_file_sampler_top_k: load_sym!(lib, "loaded_file_sampler_top_k", FileGetInt),
            loaded_file_sampler_top_p: load_sym!(lib, "loaded_file_sampler_top_p", FileGetFloat),
            loaded_file_supports_input_modality: load_sym!(
                lib,
                "loaded_file_supports_input_modality",
                FileModalityBool
            ),
            loaded_file_max_vision_token_budget: load_sym!(lib, "loaded_file_max_vision_token_budget", FileGetInt),
            loaded_file_max_context_tokens: load_sym!(lib, "loaded_file_max_context_tokens", FileGetUint),
            loaded_file_is_dynamic_context: load_sym!(lib, "loaded_file_is_dynamic_context", FileGetBool),
            loaded_file_vision_signature_selection: load_sym!(
                lib,
                "loaded_file_vision_signature_selection",
                FileGetVector
            ),
            loaded_file_embedding_signature_selection: load_sym!(
                lib,
                "loaded_file_embedding_signature_selection",
                FileGetVector
            ),
            loaded_file_embedding_dimension: load_sym!(lib, "loaded_file_embedding_dimension", FileGetInt),
            loaded_file_modality_supported_backends: load_sym!(
                lib,
                "loaded_file_modality_supported_backends",
                FileModalityVector
            ),
            loaded_file_modality_npu_brand: load_sym!(lib, "loaded_file_modality_npu_brand", FileModalityInt),
            loaded_file_modality_soc_name: load_sym!(lib, "loaded_file_modality_soc_name", FileModalityString),
            loaded_file_min_runtime_version: load_sym!(lib, "loaded_file_min_runtime_version", FileGetString),
            get_last_error_message: load_sym!(lib, "get_last_error_message", LastErrorMessage),
            get_last_error_code: load_sym!(lib, "get_last_error_code", LastErrorCode),
            clear_last_error: load_sym!(lib, "clear_last_error", ClearLastError),
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
pub(crate) fn cstr(s: &str) -> Result<std::ffi::CString, crate::LiteRtError> {
    Ok(std::ffi::CString::new(s)?)
}

pub(crate) fn read_optional_cstr(p: *const c_char) -> Result<Option<String>, crate::LiteRtError> {
    if p.is_null() {
        return Ok(None);
    }
    let text = unsafe { std::ffi::CStr::from_ptr(p) }
        .to_str()
        .map_err(|error| crate::LiteRtError::InvalidMetadata(format!("invalid UTF-8 from runtime: {error}")))?;
    Ok(if text.is_empty() { None } else { Some(text.to_string()) })
}

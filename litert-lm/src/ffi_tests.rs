use crate::engine::{Backend, Engine, EngineConfig};
use crate::model_info::{Modality, ModelKind};
use crate::runtime::Runtime;
use crate::sys::*;
use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;

#[derive(Default)]
struct Probe {
    calls: Vec<(String, Option<String>, Option<String>)>,
    fail_engine: bool,
    error: Option<CString>,
    deleted_files: usize,
    backend_values: Vec<i32>,
    max_output: Option<i32>,
}

thread_local! {
    static PROBE: RefCell<Probe> = RefCell::new(Probe::default());
}

unsafe extern "C" fn settings_create(
    _: *const c_char,
    backend: *const c_char,
    vision: *const c_char,
    audio: *const c_char,
) -> *mut LiteRtLmEngineSettings {
    let text = |p: *const c_char| {
        if p.is_null() {
            None
        } else {
            Some(unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string())
        }
    };
    PROBE.with_borrow_mut(|probe| probe.calls.push((text(backend).unwrap(), text(vision), text(audio))));
    ptr::dangling_mut()
}
unsafe extern "C" fn settings_delete(_: *mut LiteRtLmEngineSettings) {
    PROBE.with_borrow_mut(|probe| probe.error = Some(c"cleanup must not replace the original error".to_owned()));
}
unsafe extern "C" fn set_int(_: *mut LiteRtLmEngineSettings, _: c_int) {}
unsafe extern "C" fn set_str(_: *mut LiteRtLmEngineSettings, _: *const c_char) {}
unsafe extern "C" fn engine_create(_: *const LiteRtLmEngineSettings) -> *mut LiteRtLmEngine {
    PROBE.with_borrow_mut(|probe| {
        if probe.fail_engine {
            probe.error = Some(c"GPU driver unavailable".to_owned());
            ptr::null_mut()
        } else {
            ptr::dangling_mut()
        }
    })
}
unsafe extern "C" fn engine_delete(_: *mut LiteRtLmEngine) {}
unsafe extern "C" fn config_create() -> *mut LiteRtLmConversationConfig {
    ptr::dangling_mut()
}
unsafe extern "C" fn config_delete(_: *mut LiteRtLmConversationConfig) {}
unsafe extern "C" fn config_str(_: *mut LiteRtLmConversationConfig, _: *const c_char) {}
unsafe extern "C" fn config_bool(_: *mut LiteRtLmConversationConfig, _: bool) {}
unsafe extern "C" fn conversation_create(
    _: *mut LiteRtLmEngine,
    _: *mut LiteRtLmConversationConfig,
) -> *mut LiteRtLmConversation {
    ptr::dangling_mut()
}
unsafe extern "C" fn conversation_delete(_: *mut LiteRtLmConversation) {}
unsafe extern "C" fn conversation_clone(_: *mut LiteRtLmConversation) -> *mut LiteRtLmConversation {
    ptr::dangling_mut()
}
unsafe extern "C" fn send(
    _: *mut LiteRtLmConversation,
    _: *const c_char,
    _: *const c_char,
    _: *const LiteRtLmConversationOptionalArgs,
    _: StreamCallback,
    _: *mut c_void,
) -> c_int {
    1
}
unsafe extern "C" fn cancel(_: *mut LiteRtLmConversation) {}
unsafe extern "C" fn token_count(_: *mut LiteRtLmConversation) -> c_int {
    0
}
unsafe extern "C" fn optional_create() -> *mut LiteRtLmConversationOptionalArgs {
    ptr::dangling_mut()
}
unsafe extern "C" fn optional_delete(_: *mut LiteRtLmConversationOptionalArgs) {}
unsafe extern "C" fn optional_output(_: *mut LiteRtLmConversationOptionalArgs, value: c_int) {
    PROBE.with_borrow_mut(|probe| probe.max_output = Some(value));
}
unsafe extern "C" fn chunk_str(_: *const LiteRtLmStreamChunk) -> *const c_char {
    ptr::null()
}
unsafe extern "C" fn chunk_final(_: *const LiteRtLmStreamChunk) -> bool {
    true
}
unsafe extern "C" fn log_level(_: c_int) {}
unsafe extern "C" fn file_create(_: *const c_char) -> *mut LiteRtLmLoadedFile {
    ptr::dangling_mut()
}
unsafe extern "C" fn file_delete(_: *mut LiteRtLmLoadedFile) {
    PROBE.with_borrow_mut(|p| p.deleted_files += 1);
}
unsafe extern "C" fn file_true(_: *mut LiteRtLmLoadedFile) -> bool {
    true
}
unsafe extern "C" fn file_kind(_: *mut LiteRtLmLoadedFile) -> c_int {
    1
}
unsafe extern "C" fn sampler_type(_: *mut LiteRtLmLoadedFile) -> c_int {
    2
}
unsafe extern "C" fn top_k(_: *mut LiteRtLmLoadedFile) -> c_int {
    40
}
unsafe extern "C" fn float_value(_: *mut LiteRtLmLoadedFile) -> f32 {
    1.0
}
unsafe extern "C" fn file_uint(_: *mut LiteRtLmLoadedFile) -> u32 {
    8192
}
unsafe extern "C" fn file_missing(_: *mut LiteRtLmLoadedFile) -> c_int {
    -1
}
unsafe extern "C" fn file_string(_: *mut LiteRtLmLoadedFile) -> *const c_char {
    c"0.18.0".as_ptr()
}
unsafe extern "C" fn vector(_: *mut LiteRtLmLoadedFile, _: *mut c_int, _: c_int) -> c_int {
    -1
}
unsafe extern "C" fn modality(_: *mut LiteRtLmLoadedFile, value: c_int) -> bool {
    value == 0
}
unsafe extern "C" fn brand(_: *mut LiteRtLmLoadedFile, _: c_int) -> c_int {
    0
}
unsafe extern "C" fn soc(_: *mut LiteRtLmLoadedFile, _: c_int) -> *const c_char {
    ptr::null()
}
unsafe extern "C" fn backends(_: *mut LiteRtLmLoadedFile, _: c_int, out: *mut c_int, capacity: c_int) -> c_int {
    PROBE.with_borrow(|probe| {
        if !out.is_null() {
            for (i, value) in probe.backend_values.iter().take(capacity as usize).enumerate() {
                unsafe {
                    *out.add(i) = *value;
                }
            }
        }
        probe.backend_values.len() as c_int
    })
}
unsafe extern "C" fn last_message() -> *const c_char {
    PROBE.with_borrow(|p| p.error.as_ref().map_or(ptr::null(), |e| e.as_ptr()))
}
unsafe extern "C" fn last_code() -> c_int {
    PROBE.with_borrow(|p| if p.error.is_some() { 14 } else { 0 })
}
unsafe extern "C" fn clear() {
    PROBE.with_borrow_mut(|p| p.error = None);
}

fn runtime() -> Runtime {
    PROBE.with_borrow_mut(|probe| {
        *probe = Probe {
            backend_values: vec![2, 1],
            ..Default::default()
        }
    });
    Runtime::from_test_symbols(Symbols {
        engine_settings_create: settings_create,
        engine_settings_delete: settings_delete,
        engine_settings_set_max_num_tokens: set_int,
        engine_settings_set_num_threads: set_int,
        engine_settings_set_cache_dir: set_str,
        engine_create,
        engine_delete,
        conversation_config_create: config_create,
        conversation_config_delete: config_delete,
        conversation_config_set_tools: config_str,
        conversation_config_set_system_message: config_str,
        conversation_config_set_messages: config_str,
        conversation_config_set_enable_constrained_decoding: config_bool,
        conversation_create,
        conversation_delete,
        conversation_clone,
        conversation_send_message_stream: send,
        conversation_cancel_process: cancel,
        conversation_optional_args_create: optional_create,
        conversation_optional_args_delete: optional_delete,
        conversation_optional_args_set_max_output_tokens: optional_output,
        conversation_get_token_count: token_count,
        stream_chunk_get_text: chunk_str,
        stream_chunk_is_final: chunk_final,
        stream_chunk_get_error: chunk_str,
        set_min_log_level: log_level,
        loaded_file_create: file_create,
        loaded_file_delete: file_delete,
        loaded_file_model_type: file_kind,
        loaded_file_has_speculative_decoding_support: file_true,
        loaded_file_supports_thinking: file_true,
        loaded_file_supports_function_calling: file_true,
        loaded_file_sampler_type: sampler_type,
        loaded_file_sampler_temperature: float_value,
        loaded_file_sampler_top_k: top_k,
        loaded_file_sampler_top_p: float_value,
        loaded_file_supports_input_modality: modality,
        loaded_file_max_vision_token_budget: file_missing,
        loaded_file_max_context_tokens: file_uint,
        loaded_file_is_dynamic_context: file_true,
        loaded_file_vision_signature_selection: vector,
        loaded_file_embedding_signature_selection: vector,
        loaded_file_embedding_dimension: file_missing,
        loaded_file_modality_supported_backends: backends,
        loaded_file_modality_npu_brand: brand,
        loaded_file_modality_soc_name: soc,
        loaded_file_min_runtime_version: file_string,
        get_last_error_message: last_message,
        get_last_error_code: last_code,
        clear_last_error: clear,
    })
}

#[test]
fn selected_backends_reach_the_c_api_without_fallback() {
    for backend in [Backend::Cpu, Backend::Gpu, Backend::Npu] {
        let rt = runtime();
        let mut config = EngineConfig::new("test.litertlm", backend, 1024);
        config.vision_backend = Some(Backend::Cpu);
        config.audio_backend = Some(Backend::Npu);
        let _engine = Engine::new(&rt, &config).unwrap();
        PROBE.with_borrow(|probe| {
            assert_eq!(
                probe.calls,
                [(backend.as_str().to_owned(), Some("cpu".into()), Some("npu".into()))]
            )
        });
    }
}

#[test]
fn native_error_is_copied_before_cleanup_and_never_retried_on_cpu() {
    let rt = runtime();
    PROBE.with_borrow_mut(|probe| {
        probe.fail_engine = true;
        probe.error = Some(c"old error".to_owned());
    });
    let error = Engine::new(&rt, &EngineConfig::new("test.litertlm", Backend::Gpu, 1024))
        .err()
        .unwrap();
    assert!(error.to_string().contains("GPU driver unavailable"));
    assert!(!error.to_string().contains("cleanup"));
    PROBE.with_borrow(|probe| assert_eq!(probe.calls.len(), 1));
}

#[test]
fn inspection_owns_its_values_and_releases_the_file() {
    let rt = runtime();
    let info = rt.inspect_model("test.litertlm").unwrap();
    assert_eq!(info.kind, ModelKind::Llm);
    assert_eq!(info.backends(Modality::Text), &[Backend::Gpu, Backend::Cpu]);
    assert_eq!(info.min_runtime_version.as_deref(), Some("0.18.0"));
    PROBE.with_borrow(|probe| assert_eq!(probe.deleted_files, 1));
}

#[test]
fn unknown_metadata_values_fail_and_release_the_handle() {
    let rt = runtime();
    PROBE.with_borrow_mut(|probe| probe.backend_values = vec![99]);
    assert!(
        rt.inspect_model("test.litertlm")
            .unwrap_err()
            .to_string()
            .contains("unknown backend")
    );
    PROBE.with_borrow(|probe| assert_eq!(probe.deleted_files, 1));
    assert!(rt.inspect_model("invalid\0path").is_err());
}

#[test]
fn output_budget_reaches_the_native_optional_args() {
    let rt = runtime();
    let engine = Engine::new(&rt, &EngineConfig::new("model", Backend::Gpu, 1024)).unwrap();
    let conversation = engine.create_conversation(Default::default()).unwrap();
    let result = conversation.send_message_stream(r#"{"role":"user","content":"test"}"#, None, 37, |_| true);
    assert!(result.is_err());
    PROBE.with_borrow(|probe| assert_eq!(probe.max_output, Some(37)));
}

#[test]
fn invalid_utf8_diagnostics_are_errors_not_lossy_strings() {
    let rt = runtime();
    PROBE.with_borrow_mut(|probe| probe.error = Some(CString::new(vec![0xff]).unwrap()));
    assert!(matches!(rt.failure("test"), crate::LiteRtError::InvalidMetadata(_)));
}

#[test]
fn native_diagnostics_are_thread_local() {
    let first = std::thread::spawn(|| {
        let rt = runtime();
        PROBE.with_borrow_mut(|p| p.error = Some(c"first thread".to_owned()));
        rt.failure("test").to_string()
    });
    let second = std::thread::spawn(|| {
        let rt = runtime();
        PROBE.with_borrow_mut(|p| p.error = Some(c"second thread".to_owned()));
        rt.failure("test").to_string()
    });
    assert!(first.join().unwrap().contains("first thread"));
    assert!(second.join().unwrap().contains("second thread"));
}

#[cfg(windows)]
#[test]
fn pinned_windows_runtime_really_exports_the_018_api_and_reports_errors() {
    let library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/litert-lm.dll");
    let rt = Runtime::load(&library).expect("run pnpm litert:fetch before Windows tests");
    let error = rt.inspect_model("definitely-not-a-model.litertlm").unwrap_err();
    assert!(
        matches!(error, crate::LiteRtError::Native { code, .. } if code != 0),
        "{error}"
    );
}

use super::*;
use litert_lm::model_info::ModalityBackends;

fn snapshot() -> LocalModelSnapshot {
    LocalModelSnapshot {
        file_name: "custom-file.litertlm".into(),
        file_size: 1024,
        revision: 1,
        path: PathBuf::from("custom-file.litertlm"),
        requested_backend: LocalInferenceBackend::Auto,
        selected_backend: Some(Backend::Gpu),
        info: ModelInfo {
            kind: ModelKind::Llm,
            modalities: vec![
                ModalityBackends {
                    modality: Modality::Text,
                    preferred_backends: vec![Backend::Gpu, Backend::Cpu],
                    npu_brand: None,
                    soc_name: None,
                },
                ModalityBackends {
                    modality: Modality::Vision,
                    preferred_backends: vec![Backend::Cpu],
                    npu_brand: None,
                    soc_name: None,
                },
                ModalityBackends {
                    modality: Modality::Audio,
                    preferred_backends: vec![Backend::Npu],
                    npu_brand: Some(NpuBrand::Qualcomm),
                    soc_name: Some("SM8750".into()),
                },
            ],
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
        },
    }
}

#[test]
fn main_and_media_backends_obey_their_own_constraints() {
    let model = snapshot();
    let config = engine_config(&model, LocalInferenceBackend::Auto, 1024).unwrap();
    assert_eq!(config.backend, Backend::Gpu);
    assert_eq!(config.vision_backend, Some(Backend::Cpu));
    assert_eq!(config.audio_backend, Some(Backend::Npu));
    assert_eq!(
        engine_config(&model, LocalInferenceBackend::Cpu, 1024).unwrap().backend,
        Backend::Cpu
    );
    assert!(engine_config(&model, LocalInferenceBackend::Npu, 1024).is_err());
    let mut gpu_only = model.clone();
    gpu_only.info.modalities[0].preferred_backends = vec![Backend::Gpu];
    assert!(engine_config(&gpu_only, LocalInferenceBackend::Cpu, 1024).is_err());
    assert!(engine_config(&model, LocalInferenceBackend::Auto, 8193).is_err());
}

#[test]
fn adapter_narrowing_never_invents_limits_or_input_support() {
    let mut caps = super::super::ProviderCapabilities {
        supports_images: true,
        supports_pdf: true,
        supports_temperature: true,
        supports_top_p: true,
        max_temperature: Some(2.0),
        max_context_tokens: Some(8192),
        max_output_tokens: Some(4096),
        ..Default::default()
    };
    narrow_to_adapter(&mut caps);
    assert!(!caps.supports_images && !caps.supports_pdf);
    assert!(!caps.supports_temperature && !caps.supports_top_p);
    assert_eq!(caps.max_context_tokens, None);
    assert_eq!(caps.max_output_tokens, None);
}

#[test]
fn replacing_a_file_at_the_same_path_changes_its_inspection_stamp() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("test-model.litertlm");
    std::fs::write(&file, b"first").unwrap();
    let before = stamp(&file).unwrap();
    std::fs::write(&file, b"a different model").unwrap();
    let after = stamp(&file).unwrap();
    assert_ne!(before, after);
    assert_eq!(before.path, after.path);
}

#[tokio::test]
async fn local_capability_resolution_requires_an_authoritative_provider() {
    let db = crate::db::sea::sea_test_db().await;
    let mut caps = super::super::ProviderCapabilities::default();
    assert!(apply_capabilities(&db, None, "litert_lm", &mut caps).await.is_err());
    assert!(for_provider(&db, "missing", false).await.is_err());
    assert!(
        apply_capabilities(&db, None, "openai", &mut caps)
            .await
            .unwrap()
            .is_none()
    );
}

#[test]
fn placeholder_relative_and_nul_paths_are_not_model_metadata() {
    for path in ["", "local://", "relative.litertlm", "invalid\0path"] {
        assert!(inspect(path, LocalInferenceBackend::Auto, false).is_err());
    }
}

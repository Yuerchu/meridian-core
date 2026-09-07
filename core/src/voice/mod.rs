//! Offline voice input: capture PCM from the microphone, transcribe it with a
//! local sherpa-onnx model, and clean up disfluencies before sending.
//!
//! The engine only consumes PCM samples and never touches an audio device. That
//! boundary is what lets Android share everything below: there the samples come
//! from the WebView over base64, and only the recording session is different.

/// Desktop only — cpal. Android records in the WebView and hands the samples to
/// `engine` directly, so it has no session to hold.
#[cfg(not(target_os = "android"))]
pub mod capture;
pub mod download;
pub mod engine;
pub mod filter;
pub mod model;
pub mod prompt;

use std::path::{Path, PathBuf};

/// Directory id of the one supported model. Mirrors the upstream archive name
/// so an imported archive lands in the same place a download would.
pub const MODEL_ID: &str = "sherpa-onnx-x-asr-1920ms-streaming-zipformer-transducer-zh-en-punct-int8-2026-06-05";

/// Default download source. GitHub is slow from some regions, so the settings
/// page lets the user substitute a mirror URL for the same archive.
pub const DEFAULT_MODEL_URL: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-x-asr-1920ms-streaming-zipformer-transducer-zh-en-punct-int8-2026-06-05.tar.bz2";

/// Files that must all be present (and non-empty) for the model to count as
/// installed. `voice_model_status` never reports a partial install.
pub const MODEL_FILES: [&str; 5] = [
    "encoder.int8.onnx",
    "decoder.onnx",
    "joiner.int8.onnx",
    "tokens.txt",
    "bpe.model",
];

pub fn models_dir(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("asr_models")
}

pub fn model_dir(app_data_dir: &Path) -> PathBuf {
    models_dir(app_data_dir).join(MODEL_ID)
}

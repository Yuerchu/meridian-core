//! sherpa-onnx wrapper. Creating a recognizer loads the 149MB encoder (~3.6s),
//! so one instance is cached in app state and rebuilt only when the model
//! files change. Transcription itself is fast (RTF ≈ 0.03).
//!
//! This module only ever sees PCM samples; where they come from (cpal today,
//! Android's AudioRecord over JNI later) is the caller's business.

use std::path::Path;

use sherpa_onnx::{OnlineRecognizer, OnlineRecognizerConfig};

pub struct Engine {
    recognizer: OnlineRecognizer,
}

impl Engine {
    /// Load the model from `dir`. Blocking and slow — call from
    /// `spawn_blocking`, never on the main thread.
    pub fn load(dir: &Path) -> Result<Self, String> {
        let file = |name: &str| dir.join(name).to_string_lossy().into_owned();

        let mut config = OnlineRecognizerConfig::default();
        config.model_config.transducer.encoder = Some(file("encoder.int8.onnx"));
        config.model_config.transducer.decoder = Some(file("decoder.onnx"));
        config.model_config.transducer.joiner = Some(file("joiner.int8.onnx"));
        config.model_config.tokens = Some(file("tokens.txt"));
        config.model_config.num_threads = 4;

        let recognizer =
            OnlineRecognizer::create(&config).ok_or("Failed to load the speech model; the files may be corrupt")?;
        Ok(Engine { recognizer })
    }

    /// Transcribe one utterance. Blocking (~0.03x realtime) — call from
    /// `spawn_blocking`. Any sample rate is fine; sherpa resamples internally.
    pub fn transcribe(&self, samples: &[f32], sample_rate: u32) -> String {
        let stream = self.recognizer.create_stream();
        stream.accept_waveform(sample_rate as i32, samples);
        // The model decodes in 1920ms chunks; without trailing silence the
        // last chunk never fills and the tail of the utterance is dropped.
        let tail = vec![0.0f32; (sample_rate * 3) as usize];
        stream.accept_waveform(sample_rate as i32, &tail);
        stream.input_finished();

        while self.recognizer.is_ready(&stream) {
            self.recognizer.decode(&stream);
        }

        let text = self.recognizer.get_result(&stream).map(|r| r.text).unwrap_or_default();
        normalize(&text)
    }
}

/// The model's bpe tokenizer leaves a stray space after each punctuation mark
/// ("Monday， today"); collapse those without touching spaces between words.
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut after_punct = false;
    for c in text.trim().chars() {
        if c == ' ' && after_punct {
            continue;
        }
        after_punct = matches!(c, '，' | '。' | '？' | '！' | '、' | '；' | '：');
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_space_after_cjk_punctuation() {
        assert_eq!(normalize("是麦， 听得到吗？ 好"), "是麦，听得到吗？好");
        assert_eq!(normalize("base url is fine"), "base url is fine");
        assert_eq!(normalize("  边缘  "), "边缘");
    }
}

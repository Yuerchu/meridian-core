//! Text-to-speech provider contract (stateless capability domain).

use async_trait::async_trait;

use crate::domain::SwitchImpact;

/// Maximum text length accepted for QQ voice messages (~60s spoken Chinese).
pub const MAX_TTS_CHARS: usize = 200;

/// Closed allowlist of expressive cue tags accepted in TTS scripts.
pub const CUES: &[&str] = &[
    "happy",
    "sad",
    "angry",
    "excited",
    "calm",
    "surprised",
    "curious",
    "sarcastic",
    "whispering",
    "shouting",
    "sighing",
    "laughing",
    "chuckling",
    "yawning",
    "break",
];

/// Reject any `[...]` tag outside [`CUES`] instead of silently stripping it.
pub fn check_cues(text: &str) -> Result<(), String> {
    let mut unknown = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let Some(close) = after.find(']') else { break };
        let cue = after[..close].trim();
        if !cue.is_empty() && !CUES.contains(&cue) {
            unknown.push(cue.to_string());
        }
        rest = &after[close + 1..];
    }
    if unknown.is_empty() {
        return Ok(());
    }
    Err(format!(
        "unknown cue(s): {}. Use only: {}",
        unknown.join(", "),
        CUES.join(", ")
    ))
}

#[derive(Debug, Clone)]
pub struct TtsRequest {
    pub text: String,
    pub model: String,
    pub voice_id: String,
}

#[derive(Debug, Clone)]
pub struct TtsResponse {
    pub bytes: Vec<u8>,
    pub format: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct TtsConstraints {
    pub max_chars: usize,
    pub supported_cues: &'static [&'static str],
}

#[async_trait]
pub trait TtsProvider: Send + Sync {
    fn provider_id(&self) -> &'static str;

    fn constraints(&self) -> TtsConstraints {
        TtsConstraints {
            max_chars: MAX_TTS_CHARS,
            supported_cues: CUES,
        }
    }

    fn switch_impact(&self) -> SwitchImpact {
        SwitchImpact::Stateless
    }

    async fn synthesize(&self, request: &TtsRequest) -> Result<TtsResponse, String>;
}

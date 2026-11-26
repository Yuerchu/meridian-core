//! Text-to-speech facade delegating synthesis to `meridian-providers::tts`.

pub mod limiter;

pub use meridian_provider_sdk::tts::{CUES, MAX_TTS_CHARS, check_cues};
use meridian_provider_sdk::tts::{TtsProvider, TtsRequest};
use meridian_providers::tts::FishAudioTtsProvider;

pub struct Speech {
    pub bytes: Vec<u8>,
    pub format: &'static str,
}

pub struct SpeechRequest<'a> {
    pub text: &'a str,
    pub model: &'a str,
    pub reference_id: &'a str,
}

pub async fn synthesize(api_key: &str, req: SpeechRequest<'_>) -> Result<Speech, String> {
    let response = FishAudioTtsProvider::new(api_key)
        .synthesize(&TtsRequest {
            text: req.text.to_owned(),
            model: req.model.to_owned(),
            voice_id: req.reference_id.to_owned(),
        })
        .await?;
    Ok(Speech {
        bytes: response.bytes,
        format: response.format,
    })
}

use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;

use meridian_provider_sdk::tts::{MAX_TTS_CHARS, TtsProvider, TtsRequest, TtsResponse, check_cues};
use meridian_provider_sdk::wire::read_body_capped;

const MAX_AUDIO_BYTES: usize = 4 * 1024 * 1024;
const ENDPOINT: &str = "https://api.fish.audio/v1/tts";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const FORMAT: &str = "mp3";

#[derive(Serialize)]
struct FishBody<'a> {
    text: &'a str,
    format: &'a str,
    reference_id: &'a str,
    latency: &'a str,
}

fn shared_client() -> Result<&'static reqwest::Client, String> {
    static HTTP: OnceLock<reqwest::Client> = OnceLock::new();
    match HTTP.get() {
        Some(c) => Ok(c),
        None => {
            let client = reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .build()
                .map_err(|e| e.to_string())?;
            Ok(HTTP.get_or_init(|| client))
        }
    }
}

pub struct FishAudioTtsProvider {
    api_key: String,
    endpoint: String,
}

impl FishAudioTtsProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            endpoint: ENDPOINT.to_owned(),
        }
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }
}

#[async_trait]
impl TtsProvider for FishAudioTtsProvider {
    fn provider_id(&self) -> &'static str {
        "fish_audio"
    }

    async fn synthesize(&self, req: &TtsRequest) -> Result<TtsResponse, String> {
        let chars = req.text.chars().count();
        if chars > MAX_TTS_CHARS {
            return Err(format!("too long: {chars} characters, the limit is {MAX_TTS_CHARS}"));
        }
        check_cues(&req.text)?;

        let response = shared_client()?
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .header("model", &req.model)
            .json(&FishBody {
                text: &req.text,
                format: FORMAT,
                reference_id: &req.voice_id,
                latency: "normal",
            })
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = read_body_capped(response, MAX_AUDIO_BYTES, "the speech").await?;

        if !status.is_success() {
            let detail: String = String::from_utf8_lossy(&body).chars().take(200).collect();
            return Err(format!("Fish Audio {status}: {detail}"));
        }
        if !content_type.starts_with("audio/") && !looks_like_mp3(&body) {
            return Err("Fish Audio answered with something that is not audio".into());
        }
        Ok(TtsResponse {
            bytes: body,
            format: FORMAT,
        })
    }
}

pub fn looks_like_mp3(bytes: &[u8]) -> bool {
    bytes.starts_with(b"ID3") || (bytes.len() >= 2 && bytes[0] == 0xFF && (bytes[1] & 0xE0) == 0xE0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_invented_cue_is_refused_rather_than_spoken() {
        assert!(check_cues("[happy] 你好").is_ok());
        assert!(check_cues("[whispering][sad] 嗯").is_ok());
        assert!(check_cues("没有标记").is_ok());

        let err = check_cues("[怒吼着说] 喂").unwrap_err();
        assert!(err.contains("怒吼着说"), "要说清是哪一个不认识：{err}");
        assert!(err.contains("happy"), "并且告诉它有哪些可以用");
    }

    #[test]
    fn an_unclosed_bracket_is_just_text() {
        assert!(check_cues("这里有个 [ 括号").is_ok());
    }

    #[test]
    fn mp3_is_recognised_by_its_bytes() {
        assert!(looks_like_mp3(b"ID3\x03\x00"));
        assert!(looks_like_mp3(b"\xff\xfb\x90\x00"));
        assert!(!looks_like_mp3(b"<html>"));
    }
}

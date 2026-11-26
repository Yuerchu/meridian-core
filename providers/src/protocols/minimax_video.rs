//! Official MiniMax / Hailuo V2 video generation protocol
//! (`POST /v2/video_generation` + `GET /v2/query/video_generation/{task_id}`).
//!
//! Explicitly sets `aigc_watermark: false` by default so `meridian-core`
//! always stores clean originals and owns compliance watermarking (D-001).

use async_trait::async_trait;
use meridian_provider_sdk::client::{HttpTransport, Request, RequestBody, ReqwestTransport};
use meridian_provider_sdk::media_gen::{
    GeneratedMedia, MediaGenError, MediaJobHandle, MediaJobStatus, MediaReferenceRole, MediaUsage, VideoGenProvider,
    VideoGenRequest,
};
use meridian_provider_sdk::wire::{ExtraIgnore, auth_header_value, warn_extra_fields};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::media_http::{MEDIA_API_TIMEOUT, download_media_bytes, map_transport_error, reference_to_data_uri};

pub const DEFAULT_MINIMAX_BASE_URL: &str = "https://api.minimax.cn";

pub struct MiniMaxVideoProvider {
    base_url: String,
    api_key: String,
}

impl MiniMaxVideoProvider {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        let trimmed = base_url.trim().trim_end_matches('/');
        Self {
            base_url: if trimmed.is_empty() {
                DEFAULT_MINIMAX_BASE_URL.to_string()
            } else {
                trimmed.to_string()
            },
            api_key: api_key.to_string(),
        }
    }

    pub(crate) fn build_submit_request(&self, request: &VideoGenRequest) -> Result<Request, MediaGenError> {
        if request.prompt.trim().is_empty() {
            return Err(MediaGenError::InvalidRequest(
                "MiniMax video prompt must not be empty".into(),
            ));
        }

        let mut content = vec![json!({
            "type": "text",
            "text": request.prompt,
        })];
        for reference in &request.reference_images {
            let role = match reference.role {
                MediaReferenceRole::FirstFrame => "first_frame",
                MediaReferenceRole::LastFrame => "last_frame",
                MediaReferenceRole::Reference => "reference_image",
            };
            content.push(json!({
                "type": "image_url",
                "image_url": { "url": reference_to_data_uri(reference) },
                "role": role,
            }));
        }

        let ratio = match request.aspect_ratio.as_deref() {
            None | Some("") | Some("auto") => "adaptive",
            Some(explicit) => explicit,
        };
        let resolution = normalize_minimax_resolution(request.resolution.as_deref());

        let mut body = Map::new();
        body.insert("model".into(), json!(request.model));
        body.insert("content".into(), Value::Array(content));
        body.insert("resolution".into(), json!(resolution));
        body.insert("duration".into(), json!(request.duration_seconds.unwrap_or(6)));
        body.insert("ratio".into(), json!(ratio));
        // D-001 compliance boundary: request clean original unless caller explicitly opts in.
        body.insert("aigc_watermark".into(), json!(request.watermark));

        let mut http_req = Request::new(http::Method::POST, format!("{}/v2/video_generation", self.base_url));
        http_req.headers.insert(
            http::header::AUTHORIZATION,
            auth_header_value(&format!("Bearer {}", self.api_key)),
        );
        http_req.body = Some(RequestBody::Json(Value::Object(body)));
        http_req.timeout = Some(MEDIA_API_TIMEOUT);
        http_req.retry = None;
        Ok(http_req)
    }
}

fn normalize_minimax_resolution(resolution: Option<&str>) -> String {
    match resolution.map(|r| r.trim().to_ascii_uppercase()).as_deref() {
        Some("480P") => "480P".into(),
        Some("1080P" | "2K") => "2K".into(),
        Some(other) if !other.is_empty() => other.to_string(),
        _ => "768P".into(),
    }
}

#[derive(Debug, Deserialize)]
struct MiniMaxCreateResponse {
    task_id: String,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct MiniMaxQueryEnvelope {
    task: MiniMaxTaskPayload,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct MiniMaxTaskPayload {
    status: String,
    error: Option<MiniMaxTaskError>,
    content: Option<MiniMaxTaskContent>,
    usage: Option<MiniMaxTaskUsage>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct MiniMaxTaskError {
    code: Option<String>,
    message: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct MiniMaxTaskContent {
    url: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct MiniMaxTaskUsage {
    output_seconds: Option<u32>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[async_trait]
impl VideoGenProvider for MiniMaxVideoProvider {
    fn provider_id(&self) -> &'static str {
        "minimax_video"
    }

    async fn submit_video_job(&self, request: &VideoGenRequest) -> Result<MediaJobHandle, MediaGenError> {
        let http_req = self.build_submit_request(request)?;
        let resp = ReqwestTransport::shared()
            .execute(http_req)
            .await
            .map_err(map_transport_error)?;

        let parsed: MiniMaxCreateResponse = serde_json::from_slice(&resp.body)
            .map_err(|err| MediaGenError::Terminal(format!("failed to parse MiniMax submit response: {err}")))?;
        warn_extra_fields("minimax_create_response", &parsed.extra);

        Ok(MediaJobHandle {
            job_id: parsed.task_id,
            model: request.model.clone(),
            metadata: Value::Null,
        })
    }

    async fn poll_video_job(&self, handle: &MediaJobHandle) -> Result<MediaJobStatus, MediaGenError> {
        let mut http_req = Request::new(
            http::Method::GET,
            format!("{}/v2/query/video_generation/{}", self.base_url, handle.job_id),
        );
        http_req.headers.insert(
            http::header::AUTHORIZATION,
            auth_header_value(&format!("Bearer {}", self.api_key)),
        );
        http_req.timeout = Some(MEDIA_API_TIMEOUT);
        http_req.retry = None;

        let resp = ReqwestTransport::shared()
            .execute(http_req)
            .await
            .map_err(map_transport_error)?;

        let parsed: MiniMaxQueryEnvelope = serde_json::from_slice(&resp.body)
            .map_err(|err| MediaGenError::Terminal(format!("failed to parse MiniMax poll response: {err}")))?;
        warn_extra_fields("minimax_query_envelope", &parsed.extra);
        warn_extra_fields("minimax_task_payload", &parsed.task.extra);

        match parsed.task.status.to_ascii_lowercase().as_str() {
            "queued" => Ok(MediaJobStatus::Queued),
            "running" => Ok(MediaJobStatus::Running { progress_pct: None }),
            "cancelled" => Ok(MediaJobStatus::Cancelled),
            "succeeded" => {
                let url = parsed
                    .task
                    .content
                    .and_then(|c| {
                        warn_extra_fields("minimax_task_content", &c.extra);
                        c.url
                    })
                    .ok_or_else(|| MediaGenError::Terminal("MiniMax task succeeded without content.url".into()))?;
                let (bytes, mime_type) = download_media_bytes(&url, &[], "video/mp4").await?;
                let output_seconds = parsed.task.usage.and_then(|u| {
                    warn_extra_fields("minimax_task_usage", &u.extra);
                    u.output_seconds
                });
                let usage = Some(MediaUsage {
                    input_tokens: None,
                    output_tokens: None,
                    total_tokens: None,
                    generated_count: Some(1),
                    duration_seconds: output_seconds,
                    cost_usd: None,
                });
                Ok(MediaJobStatus::Succeeded {
                    media: vec![GeneratedMedia {
                        mime_type,
                        bytes,
                        width: None,
                        height: None,
                        duration_seconds: output_seconds.map(f64::from),
                        revised_prompt: None,
                    }],
                    usage,
                })
            }
            "failed" => {
                let (code, message) = match parsed.task.error {
                    Some(err) => {
                        warn_extra_fields("minimax_task_error", &err.extra);
                        (
                            err.code,
                            err.message.unwrap_or_else(|| "MiniMax video generation failed".into()),
                        )
                    }
                    None => (None, "MiniMax video generation failed".into()),
                };
                if matches!(code.as_deref(), Some("1026" | "1027" | "1040")) {
                    return Err(MediaGenError::ContentFiltered(message));
                }
                Ok(MediaJobStatus::Failed {
                    message,
                    retryable: false,
                })
            }
            other => Ok(MediaJobStatus::Failed {
                message: format!("unexpected MiniMax task status `{other}`"),
                retryable: false,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use meridian_provider_sdk::conformance::assert_video_gen_provider_conformance;

    use super::*;

    #[test]
    fn conformance_and_aigc_watermark_false_by_default() {
        let provider = MiniMaxVideoProvider::new("", "mm-key");
        assert_video_gen_provider_conformance(&provider);

        let mut req = VideoGenRequest::new("MiniMax-Hailuo-2.3", "cinematic city night");
        req.aspect_ratio = Some("16:9".into());
        req.resolution = Some("768p".into());
        req.duration_seconds = Some(6);

        let http_req = provider.build_submit_request(&req).unwrap();
        assert_eq!(http_req.url, "https://api.minimax.cn/v2/video_generation");
        match http_req.body {
            Some(RequestBody::Json(body)) => {
                assert_eq!(body["aigc_watermark"], json!(false));
                assert_eq!(body["resolution"], "768P");
                assert_eq!(body["ratio"], "16:9");
                assert_eq!(body["duration"], 6);
            }
            other => panic!("expected JSON body, got {other:?}"),
        }
    }
}

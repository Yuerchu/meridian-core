//! Official Vidu enterprise video generation protocol
//! (`POST /ent/v2/{endpoint}` + `GET /ent/v2/tasks/{task_id}/creations`).
//!
//! Uses Vidu's `Authorization: Token {api_key}` header (not Bearer / JWT).

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

pub const DEFAULT_VIDU_BASE_URL: &str = "https://api.vidu.cn";

pub struct ViduVideoProvider {
    base_url: String,
    api_key: String,
}

impl ViduVideoProvider {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        let trimmed = base_url.trim().trim_end_matches('/');
        Self {
            base_url: if trimmed.is_empty() {
                DEFAULT_VIDU_BASE_URL.to_string()
            } else {
                trimmed.to_string()
            },
            api_key: api_key.to_string(),
        }
    }

    pub(crate) fn select_endpoint(request: &VideoGenRequest) -> &'static str {
        if request.reference_images.is_empty() {
            return "text2video";
        }
        let has_first = request
            .reference_images
            .iter()
            .any(|r| r.role == MediaReferenceRole::FirstFrame);
        let has_last = request
            .reference_images
            .iter()
            .any(|r| r.role == MediaReferenceRole::LastFrame);
        if has_first && has_last && request.reference_images.len() == 2 {
            "start-end2video"
        } else if request.reference_images.len() == 1 && has_first {
            "img2video"
        } else {
            "reference2video"
        }
    }

    pub(crate) fn build_submit_request(&self, request: &VideoGenRequest) -> Result<Request, MediaGenError> {
        if request.prompt.trim().is_empty() && request.reference_images.is_empty() {
            return Err(MediaGenError::InvalidRequest(
                "Vidu video prompt or reference images must not be empty".into(),
            ));
        }

        let endpoint = Self::select_endpoint(request);
        let mut body = Map::new();
        body.insert("model".into(), json!(request.model));
        body.insert("prompt".into(), json!(request.prompt));
        body.insert("duration".into(), json!(request.duration_seconds.unwrap_or(5)));
        body.insert(
            "resolution".into(),
            json!(
                request
                    .resolution
                    .as_deref()
                    .map(str::to_ascii_lowercase)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "720p".into())
            ),
        );

        // Vidu's `img2video` and `start-end2video` reject `aspect_ratio` (ratio follows input frame).
        if matches!(endpoint, "text2video" | "reference2video")
            && let Some(ratio) = request.aspect_ratio.as_deref().filter(|r| !r.trim().is_empty())
        {
            body.insert("aspect_ratio".into(), json!(ratio));
        }

        if !request.reference_images.is_empty() {
            let mut ordered = Vec::with_capacity(request.reference_images.len());
            for role in [
                MediaReferenceRole::FirstFrame,
                MediaReferenceRole::Reference,
                MediaReferenceRole::LastFrame,
            ] {
                for r in request.reference_images.iter().filter(|r| r.role == role) {
                    ordered.push(reference_to_data_uri(r));
                }
            }
            body.insert("images".into(), json!(ordered));
        }

        if let Some(seed) = request.seed.filter(|&s| s >= 0) {
            body.insert("seed".into(), json!(seed));
        }
        if let Some(audio) = request.generate_audio {
            body.insert("audio".into(), json!(audio));
        }

        let mut http_req = Request::new(http::Method::POST, format!("{}/ent/v2/{endpoint}", self.base_url));
        http_req.headers.insert(
            http::header::AUTHORIZATION,
            auth_header_value(&format!("Token {}", self.api_key)),
        );
        http_req.body = Some(RequestBody::Json(Value::Object(body)));
        http_req.timeout = Some(MEDIA_API_TIMEOUT);
        http_req.retry = None;
        Ok(http_req)
    }
}

#[derive(Debug, Deserialize)]
struct ViduSubmitResponse {
    task_id: String,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct ViduPollResponse {
    state: String,
    err_code: Option<String>,
    #[serde(default)]
    creations: Vec<ViduCreation>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct ViduCreation {
    url: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[async_trait]
impl VideoGenProvider for ViduVideoProvider {
    fn provider_id(&self) -> &'static str {
        "vidu_video"
    }

    async fn submit_video_job(&self, request: &VideoGenRequest) -> Result<MediaJobHandle, MediaGenError> {
        let http_req = self.build_submit_request(request)?;
        let resp = ReqwestTransport::shared()
            .execute(http_req)
            .await
            .map_err(map_transport_error)?;

        let parsed: ViduSubmitResponse = serde_json::from_slice(&resp.body)
            .map_err(|err| MediaGenError::Terminal(format!("failed to parse Vidu submit response: {err}")))?;
        warn_extra_fields("vidu_submit_response", &parsed.extra);

        Ok(MediaJobHandle {
            job_id: parsed.task_id,
            model: request.model.clone(),
            metadata: Value::Null,
        })
    }

    async fn poll_video_job(&self, handle: &MediaJobHandle) -> Result<MediaJobStatus, MediaGenError> {
        let mut http_req = Request::new(
            http::Method::GET,
            format!("{}/ent/v2/tasks/{}/creations", self.base_url, handle.job_id),
        );
        http_req.headers.insert(
            http::header::AUTHORIZATION,
            auth_header_value(&format!("Token {}", self.api_key)),
        );
        http_req.timeout = Some(MEDIA_API_TIMEOUT);
        http_req.retry = None;

        let resp = ReqwestTransport::shared()
            .execute(http_req)
            .await
            .map_err(map_transport_error)?;

        let parsed: ViduPollResponse = serde_json::from_slice(&resp.body)
            .map_err(|err| MediaGenError::Terminal(format!("failed to parse Vidu poll response: {err}")))?;
        warn_extra_fields("vidu_poll_response", &parsed.extra);

        match parsed.state.to_ascii_lowercase().as_str() {
            "created" | "queueing" => Ok(MediaJobStatus::Queued),
            "processing" => Ok(MediaJobStatus::Running { progress_pct: None }),
            "success" => {
                let mut media = Vec::new();
                for creation in parsed.creations {
                    warn_extra_fields("vidu_creation", &creation.extra);
                    if let Some(url) = creation.url {
                        let (bytes, mime_type) = download_media_bytes(&url, &[], "video/mp4").await?;
                        media.push(GeneratedMedia {
                            mime_type,
                            bytes,
                            width: None,
                            height: None,
                            duration_seconds: None,
                            revised_prompt: None,
                        });
                    }
                }
                if media.is_empty() {
                    return Err(MediaGenError::Terminal("Vidu task succeeded without creations".into()));
                }
                let usage = Some(MediaUsage {
                    input_tokens: None,
                    output_tokens: None,
                    total_tokens: None,
                    generated_count: Some(media.len() as u32),
                    duration_seconds: None,
                    cost_usd: None,
                });
                Ok(MediaJobStatus::Succeeded { media, usage })
            }
            "failed" => {
                let err_code = parsed.err_code.unwrap_or_else(|| "vidu_failed".into());
                let lower = err_code.to_ascii_lowercase();
                if lower.contains("audit") || lower.contains("moderation") || lower.contains("safety") {
                    return Err(MediaGenError::ContentFiltered(format!("Vidu rejected: {err_code}")));
                }
                Ok(MediaJobStatus::Failed {
                    message: format!("Vidu generation failed ({err_code})"),
                    retryable: false,
                })
            }
            other => Ok(MediaJobStatus::Failed {
                message: format!("unexpected Vidu state `{other}`"),
                retryable: false,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use meridian_provider_sdk::conformance::assert_video_gen_provider_conformance;
    use meridian_provider_sdk::media_gen::MediaReference;

    use super::*;

    #[test]
    fn conformance_token_auth_and_endpoint_selection() {
        let provider = ViduVideoProvider::new("", "vidu-secret");
        assert_video_gen_provider_conformance(&provider);

        let mut req = VideoGenRequest::new("viduq3-pro", "cherry blossoms falling");
        req.aspect_ratio = Some("16:9".into());
        let http_req = provider.build_submit_request(&req).unwrap();
        assert_eq!(http_req.url, "https://api.vidu.cn/ent/v2/text2video");
        assert_eq!(http_req.headers[http::header::AUTHORIZATION], "Token vidu-secret");

        req.reference_images.push(MediaReference {
            mime_type: "image/png".into(),
            bytes: vec![1, 2],
            role: MediaReferenceRole::FirstFrame,
        });
        let i2v_req = provider.build_submit_request(&req).unwrap();
        assert_eq!(i2v_req.url, "https://api.vidu.cn/ent/v2/img2video");
        if let Some(RequestBody::Json(body)) = i2v_req.body {
            assert!(body.get("aspect_ratio").is_none());
        }
    }
}

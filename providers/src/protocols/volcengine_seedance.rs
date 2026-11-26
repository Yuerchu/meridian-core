//! Official Volcengine Ark / BytePlus Seedance video generation protocol
//! (`POST /contents/generations/tasks` + `GET /contents/generations/tasks/{id}`).
//!
//! Explicitly sets `watermark: false` by default so `meridian-core` always
//! stores clean originals and owns compliance watermarking (D-001).

use async_trait::async_trait;
use meridian_provider_sdk::client::{HttpTransport, Request, RequestBody, ReqwestTransport};
use meridian_provider_sdk::media_gen::{
    GeneratedMedia, MediaGenError, MediaJobHandle, MediaJobStatus, MediaReferenceRole, MediaUsage, VideoGenProvider,
    VideoGenRequest,
};
use meridian_provider_sdk::wire::{ExtraIgnore, auth_header_value, warn_extra_fields};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::protocols::volcengine_seedream::DEFAULT_VOLCENGINE_ARK_BASE_URL;

use super::media_http::{MEDIA_API_TIMEOUT, download_media_bytes, map_transport_error, reference_to_data_uri};

pub struct VolcengineSeedanceProvider {
    base_url: String,
    api_key: String,
}

impl VolcengineSeedanceProvider {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        let trimmed = base_url.trim().trim_end_matches('/');
        Self {
            base_url: if trimmed.is_empty() {
                DEFAULT_VOLCENGINE_ARK_BASE_URL.to_string()
            } else {
                trimmed.to_string()
            },
            api_key: api_key.to_string(),
        }
    }

    pub(crate) fn build_submit_request(&self, request: &VideoGenRequest) -> Result<Request, MediaGenError> {
        if request.prompt.trim().is_empty() && request.reference_images.is_empty() {
            return Err(MediaGenError::InvalidRequest(
                "video prompt or reference image must not be empty".into(),
            ));
        }

        let mut content = Vec::new();
        if !request.prompt.trim().is_empty() {
            content.push(json!({
                "type": "text",
                "text": request.prompt,
            }));
        }
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

        let mut body = Map::new();
        body.insert("model".into(), json!(request.model));
        body.insert("content".into(), Value::Array(content));
        // D-001 compliance boundary: request clean original unless caller explicitly opts in.
        body.insert("watermark".into(), json!(request.watermark));

        if let Some(ratio) = request.aspect_ratio.as_deref().filter(|r| !r.trim().is_empty()) {
            body.insert("ratio".into(), json!(ratio));
        }
        if let Some(resolution) = request.resolution.as_deref().filter(|r| !r.trim().is_empty()) {
            body.insert("resolution".into(), json!(resolution));
        }
        if let Some(duration) = request.duration_seconds {
            body.insert("duration".into(), json!(duration));
        }
        if let Some(seed) = request.seed {
            body.insert("seed".into(), json!(seed));
        }
        if let Some(audio) = request.generate_audio {
            body.insert("generate_audio".into(), json!(audio));
        }

        let mut http_req = Request::new(
            http::Method::POST,
            format!("{}/contents/generations/tasks", self.base_url),
        );
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

#[derive(Debug, Deserialize)]
struct SeedanceCreateResponse {
    id: String,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct SeedanceTaskResponse {
    status: String,
    content: Option<SeedanceTaskContent>,
    error: Option<SeedanceTaskError>,
    duration: Option<f64>,
    usage: Option<SeedanceTaskUsage>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct SeedanceTaskContent {
    video_url: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct SeedanceTaskError {
    code: Option<String>,
    message: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct SeedanceTaskUsage {
    completion_tokens: Option<i64>,
    total_tokens: Option<i64>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[async_trait]
impl VideoGenProvider for VolcengineSeedanceProvider {
    fn provider_id(&self) -> &'static str {
        "volcengine_seedance"
    }

    async fn submit_video_job(&self, request: &VideoGenRequest) -> Result<MediaJobHandle, MediaGenError> {
        let http_req = self.build_submit_request(request)?;
        let resp = ReqwestTransport::shared()
            .execute(http_req)
            .await
            .map_err(map_transport_error)?;

        let parsed: SeedanceCreateResponse = serde_json::from_slice(&resp.body)
            .map_err(|err| MediaGenError::Terminal(format!("failed to parse Seedance submit response: {err}")))?;
        warn_extra_fields("volcengine_seedance_create_response", &parsed.extra);

        Ok(MediaJobHandle {
            job_id: parsed.id,
            model: request.model.clone(),
            metadata: Value::Null,
        })
    }

    async fn poll_video_job(&self, handle: &MediaJobHandle) -> Result<MediaJobStatus, MediaGenError> {
        let mut http_req = Request::new(
            http::Method::GET,
            format!("{}/contents/generations/tasks/{}", self.base_url, handle.job_id),
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

        let parsed: SeedanceTaskResponse = serde_json::from_slice(&resp.body)
            .map_err(|err| MediaGenError::Terminal(format!("failed to parse Seedance task response: {err}")))?;
        warn_extra_fields("volcengine_seedance_task_response", &parsed.extra);

        match parsed.status.to_ascii_lowercase().as_str() {
            "queued" => Ok(MediaJobStatus::Queued),
            "running" => Ok(MediaJobStatus::Running { progress_pct: None }),
            "cancelled" => Ok(MediaJobStatus::Cancelled),
            "succeeded" => {
                let video_url = parsed
                    .content
                    .and_then(|c| {
                        warn_extra_fields("volcengine_seedance_content", &c.extra);
                        c.video_url
                    })
                    .ok_or_else(|| MediaGenError::Terminal("Seedance succeeded without content.video_url".into()))?;
                let (bytes, mime_type) = download_media_bytes(&video_url, &[], "video/mp4").await?;
                let usage = parsed.usage.map(|u| {
                    warn_extra_fields("volcengine_seedance_usage", &u.extra);
                    MediaUsage {
                        input_tokens: None,
                        output_tokens: u.completion_tokens,
                        total_tokens: u.total_tokens,
                        generated_count: Some(1),
                        duration_seconds: parsed.duration.map(|d| d.round() as u32),
                        cost_usd: None,
                    }
                });
                Ok(MediaJobStatus::Succeeded {
                    media: vec![GeneratedMedia {
                        mime_type,
                        bytes,
                        width: None,
                        height: None,
                        duration_seconds: parsed.duration,
                        revised_prompt: None,
                    }],
                    usage,
                })
            }
            "failed" | "expired" => {
                let detail = parsed
                    .error
                    .map(|e| {
                        warn_extra_fields("volcengine_seedance_error", &e.extra);
                        e.message.or(e.code).unwrap_or_else(|| parsed.status.clone())
                    })
                    .unwrap_or_else(|| format!("Seedance task {}", parsed.status));
                if detail.to_ascii_lowercase().contains("sensitive") || detail.to_ascii_lowercase().contains("safety") {
                    return Err(MediaGenError::ContentFiltered(detail));
                }
                Ok(MediaJobStatus::Failed {
                    message: detail,
                    retryable: parsed.status.eq_ignore_ascii_case("expired"),
                })
            }
            other => Ok(MediaJobStatus::Failed {
                message: format!("unexpected Seedance status `{other}`"),
                retryable: false,
            }),
        }
    }

    async fn cancel_video_job(&self, handle: &MediaJobHandle) -> Result<(), MediaGenError> {
        let mut http_req = Request::new(
            http::Method::DELETE,
            format!("{}/contents/generations/tasks/{}", self.base_url, handle.job_id),
        );
        http_req.headers.insert(
            http::header::AUTHORIZATION,
            auth_header_value(&format!("Bearer {}", self.api_key)),
        );
        http_req.timeout = Some(MEDIA_API_TIMEOUT);
        http_req.retry = None;
        let _ = ReqwestTransport::shared().execute(http_req).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use meridian_provider_sdk::conformance::assert_video_gen_provider_conformance;
    use meridian_provider_sdk::media_gen::MediaReference;

    use super::*;

    #[test]
    fn conformance_and_watermark_false_by_default() {
        let provider = VolcengineSeedanceProvider::new("", "ark-key");
        assert_video_gen_provider_conformance(&provider);

        let mut req = VideoGenRequest::new("doubao-seedance-1-5-pro-251215", "drone flyover");
        req.aspect_ratio = Some("16:9".into());
        req.resolution = Some("1080p".into());
        req.duration_seconds = Some(5);
        req.reference_images.push(MediaReference {
            mime_type: "image/png".into(),
            bytes: vec![0x89, 0x50],
            role: MediaReferenceRole::FirstFrame,
        });

        let http_req = provider.build_submit_request(&req).unwrap();
        assert_eq!(
            http_req.url,
            "https://ark.cn-beijing.volces.com/api/v3/contents/generations/tasks"
        );
        match http_req.body {
            Some(RequestBody::Json(body)) => {
                assert_eq!(body["watermark"], json!(false));
                assert_eq!(body["ratio"], "16:9");
                assert_eq!(body["resolution"], "1080p");
                assert_eq!(body["duration"], 5);
                assert_eq!(body["content"][1]["role"], "first_frame");
            }
            other => panic!("expected JSON body, got {other:?}"),
        }
    }
}

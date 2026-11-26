//! Official Alibaba Bailian / DashScope Wan video generation protocol
//! (`POST {base_url}/services/aigc/video-generation/video-synthesis` with
//! `X-DashScope-Async: enable`, polling `GET {base_url}/tasks/{task_id}`).
//!
//! Explicitly sets `parameters.watermark: false` by default so `meridian-core`
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

pub const DEFAULT_BAILIAN_BASE_URL: &str = "https://dashscope.aliyuncs.com/api/v1";

pub struct BailianWanVideoProvider {
    base_url: String,
    api_key: String,
}

impl BailianWanVideoProvider {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        let trimmed = base_url.trim().trim_end_matches('/');
        Self {
            base_url: if trimmed.is_empty() {
                DEFAULT_BAILIAN_BASE_URL.to_string()
            } else {
                trimmed.to_string()
            },
            api_key: api_key.to_string(),
        }
    }

    pub(crate) fn build_submit_request(&self, request: &VideoGenRequest) -> Result<Request, MediaGenError> {
        if request.prompt.trim().is_empty() && request.reference_images.is_empty() {
            return Err(MediaGenError::InvalidRequest(
                "Bailian Wan video prompt or reference media must not be empty".into(),
            ));
        }

        let mut input = Map::new();
        if !request.prompt.trim().is_empty() {
            input.insert("prompt".into(), json!(request.prompt));
        }
        if !request.reference_images.is_empty() {
            let media: Vec<Value> = request
                .reference_images
                .iter()
                .map(|r| {
                    let kind = match r.role {
                        MediaReferenceRole::FirstFrame => "first_frame",
                        MediaReferenceRole::LastFrame => "last_frame",
                        MediaReferenceRole::Reference => "reference_image",
                    };
                    json!({
                        "type": kind,
                        "url": reference_to_data_uri(r),
                    })
                })
                .collect();
            input.insert("media".into(), Value::Array(media));
        }

        let ratio = match request.aspect_ratio.as_deref() {
            None | Some("") | Some("auto") => "adaptive",
            Some(explicit) => explicit,
        };
        let resolution = match request.resolution.as_deref().map(str::to_ascii_uppercase).as_deref() {
            Some("480P") => "480P",
            Some("1080P") => "1080P",
            _ => "720P",
        };

        let mut parameters = Map::new();
        parameters.insert("resolution".into(), json!(resolution));
        parameters.insert("ratio".into(), json!(ratio));
        if let Some(duration) = request.duration_seconds {
            parameters.insert("duration".into(), json!(duration));
        }
        if let Some(audio) = request.generate_audio {
            parameters.insert("audio".into(), json!(audio));
        }
        if let Some(seed) = request.seed {
            parameters.insert("seed".into(), json!(seed));
        }
        // D-001 compliance boundary: request clean original unless caller explicitly opts in.
        parameters.insert("watermark".into(), json!(request.watermark));

        let body = json!({
            "model": request.model,
            "input": Value::Object(input),
            "parameters": Value::Object(parameters),
        });

        let mut http_req = Request::new(
            http::Method::POST,
            format!("{}/services/aigc/video-generation/video-synthesis", self.base_url),
        );
        http_req.headers.insert(
            http::header::AUTHORIZATION,
            auth_header_value(&format!("Bearer {}", self.api_key)),
        );
        http_req.headers.insert(
            http::header::HeaderName::from_static("x-dashscope-async"),
            http::HeaderValue::from_static("enable"),
        );
        http_req.body = Some(RequestBody::Json(body));
        http_req.timeout = Some(MEDIA_API_TIMEOUT);
        http_req.retry = None;
        Ok(http_req)
    }
}

#[derive(Debug, Deserialize)]
struct BailianCreateResponse {
    output: Option<BailianTaskOutput>,
    code: Option<String>,
    message: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct BailianQueryResponse {
    output: BailianTaskOutput,
    usage: Option<BailianUsage>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct BailianTaskOutput {
    task_id: Option<String>,
    task_status: Option<String>,
    video_url: Option<String>,
    code: Option<String>,
    message: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct BailianUsage {
    video_count: Option<u32>,
    duration: Option<f64>,
    output_video_duration: Option<f64>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[async_trait]
impl VideoGenProvider for BailianWanVideoProvider {
    fn provider_id(&self) -> &'static str {
        "bailian_wan_video"
    }

    async fn submit_video_job(&self, request: &VideoGenRequest) -> Result<MediaJobHandle, MediaGenError> {
        let http_req = self.build_submit_request(request)?;
        let resp = ReqwestTransport::shared()
            .execute(http_req)
            .await
            .map_err(map_transport_error)?;

        let parsed: BailianCreateResponse = serde_json::from_slice(&resp.body)
            .map_err(|err| MediaGenError::Terminal(format!("failed to parse Bailian submit response: {err}")))?;
        warn_extra_fields("bailian_create_response", &parsed.extra);

        let task_id = parsed.output.and_then(|o| {
            warn_extra_fields("bailian_create_output", &o.extra);
            o.task_id
        });
        let Some(task_id) = task_id.filter(|id| !id.trim().is_empty()) else {
            let detail = parsed
                .message
                .or(parsed.code)
                .unwrap_or_else(|| "Bailian did not return task_id".into());
            return Err(MediaGenError::from_http_status(400, detail));
        };

        Ok(MediaJobHandle {
            job_id: task_id,
            model: request.model.clone(),
            metadata: Value::Null,
        })
    }

    async fn poll_video_job(&self, handle: &MediaJobHandle) -> Result<MediaJobStatus, MediaGenError> {
        let mut http_req = Request::new(http::Method::GET, format!("{}/tasks/{}", self.base_url, handle.job_id));
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

        let parsed: BailianQueryResponse = serde_json::from_slice(&resp.body)
            .map_err(|err| MediaGenError::Terminal(format!("failed to parse Bailian poll response: {err}")))?;
        warn_extra_fields("bailian_query_response", &parsed.extra);
        warn_extra_fields("bailian_query_output", &parsed.output.extra);

        match parsed.output.task_status.as_deref().unwrap_or("UNKNOWN") {
            "PENDING" => Ok(MediaJobStatus::Queued),
            "RUNNING" => Ok(MediaJobStatus::Running { progress_pct: None }),
            "CANCELED" => Ok(MediaJobStatus::Cancelled),
            "SUCCEEDED" => {
                let video_url = parsed
                    .output
                    .video_url
                    .ok_or_else(|| MediaGenError::Terminal("Bailian task succeeded without output.video_url".into()))?;
                let (bytes, mime_type) = download_media_bytes(&video_url, &[], "video/mp4").await?;
                let (duration, count) = parsed
                    .usage
                    .map(|u| {
                        warn_extra_fields("bailian_usage", &u.extra);
                        (u.output_video_duration.or(u.duration), u.video_count)
                    })
                    .unwrap_or((None, None));
                let usage = Some(MediaUsage {
                    input_tokens: None,
                    output_tokens: None,
                    total_tokens: None,
                    generated_count: count.or(Some(1)),
                    duration_seconds: duration.map(|d| d.round() as u32),
                    cost_usd: None,
                });
                Ok(MediaJobStatus::Succeeded {
                    media: vec![GeneratedMedia {
                        mime_type,
                        bytes,
                        width: None,
                        height: None,
                        duration_seconds: duration,
                        revised_prompt: None,
                    }],
                    usage,
                })
            }
            "FAILED" => {
                let detail = parsed
                    .output
                    .message
                    .or(parsed.output.code)
                    .unwrap_or_else(|| "Bailian Wan generation failed".into());
                let lower = detail.to_ascii_lowercase();
                if lower.contains("datainspection") || lower.contains("safety") || lower.contains("inappropriate") {
                    return Err(MediaGenError::ContentFiltered(detail));
                }
                Ok(MediaJobStatus::Failed {
                    message: detail,
                    retryable: false,
                })
            }
            other => Ok(MediaJobStatus::Failed {
                message: format!("Bailian task status `{other}`"),
                retryable: other == "UNKNOWN",
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use meridian_provider_sdk::conformance::assert_video_gen_provider_conformance;

    use super::*;

    #[test]
    fn conformance_async_header_and_watermark_false() {
        let provider = BailianWanVideoProvider::new("", "sk-dashscope-test");
        assert_video_gen_provider_conformance(&provider);

        let mut req = VideoGenRequest::new("wan2.6-t2v", "ink painting bamboo forest");
        req.resolution = Some("1080p".into());
        req.duration_seconds = Some(5);

        let http_req = provider.build_submit_request(&req).unwrap();
        assert_eq!(
            http_req.url,
            "https://dashscope.aliyuncs.com/api/v1/services/aigc/video-generation/video-synthesis"
        );
        assert_eq!(http_req.headers["x-dashscope-async"], "enable");
        match http_req.body {
            Some(RequestBody::Json(body)) => {
                assert_eq!(body["parameters"]["watermark"], json!(false));
                assert_eq!(body["parameters"]["resolution"], "1080P");
                assert_eq!(body["parameters"]["ratio"], "adaptive");
            }
            other => panic!("expected JSON body, got {other:?}"),
        }
    }
}

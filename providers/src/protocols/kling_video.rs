//! Official Kling New REST API video generation protocol.
//!
//! Uses static API Key authentication (`Authorization: Bearer {api_key}`) on
//! `https://api-beijing.klingai.com` (no `/v1` suffix), submitting to
//! `POST /{route}/{model}` and polling `GET /tasks?task_ids={id}`.

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

/// Official Kling new REST API Beijing root (explicitly without `/v1`).
pub const DEFAULT_KLING_BASE_URL: &str = "https://api-beijing.klingai.com";

pub struct KlingVideoProvider {
    base_url: String,
    api_key: String,
}

impl KlingVideoProvider {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        let trimmed = base_url.trim().trim_end_matches('/');
        Self {
            base_url: if trimmed.is_empty() {
                DEFAULT_KLING_BASE_URL.to_string()
            } else {
                trimmed.to_string()
            },
            api_key: api_key.to_string(),
        }
    }

    pub(crate) fn select_route(model: &str, has_references: bool) -> &'static str {
        let lower = model.to_ascii_lowercase();
        if lower.contains("omni") || lower.contains("4.0") {
            "omni-video"
        } else if has_references {
            "image-to-video"
        } else {
            "text-to-video"
        }
    }

    pub(crate) fn build_submit_request(&self, request: &VideoGenRequest) -> Result<Request, MediaGenError> {
        if request.prompt.trim().is_empty() && request.reference_images.is_empty() {
            return Err(MediaGenError::InvalidRequest(
                "video prompt or reference image must not be empty".into(),
            ));
        }

        let route = Self::select_route(&request.model, !request.reference_images.is_empty());
        let mut settings = Map::new();
        if let Some(res) = request.resolution.as_deref().filter(|r| !r.trim().is_empty()) {
            settings.insert("resolution".into(), json!(res.to_ascii_lowercase()));
        }
        if let Some(duration) = request.duration_seconds {
            settings.insert("duration".into(), json!(duration));
        }
        if route != "image-to-video"
            && let Some(ratio) = request.aspect_ratio.as_deref().filter(|r| !r.trim().is_empty())
        {
            settings.insert("aspect_ratio".into(), json!(ratio));
        }
        if let Some(audio) = request.generate_audio {
            settings.insert("audio".into(), json!(if audio { "native" } else { "off" }));
        }

        let mut body = Map::new();
        if route == "text-to-video" {
            body.insert("prompt".into(), json!(request.prompt));
        } else {
            let mut contents = Vec::new();
            if !request.prompt.trim().is_empty() {
                contents.push(json!({
                    "type": "prompt",
                    "text": request.prompt,
                }));
            }
            for reference in &request.reference_images {
                let kind = match reference.role {
                    MediaReferenceRole::FirstFrame => "first_frame",
                    MediaReferenceRole::LastFrame => "last_frame",
                    MediaReferenceRole::Reference => {
                        if route == "omni-video" {
                            "refer_image"
                        } else {
                            "first_frame"
                        }
                    }
                };
                contents.push(json!({
                    "type": kind,
                    "url": reference_to_data_uri(reference),
                }));
            }
            body.insert("contents".into(), Value::Array(contents));
        }

        body.insert("settings".into(), Value::Object(settings));
        // D-001 compliance boundary: request clean original unless caller explicitly opts in.
        body.insert(
            "options".into(),
            json!({ "watermark_info": { "enabled": request.watermark } }),
        );

        let mut http_req = Request::new(
            http::Method::POST,
            format!("{}/{route}/{}", self.base_url, request.model),
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

fn classify_kling_code(code: i64, message: &str) -> MediaGenError {
    match code {
        1301 => MediaGenError::ContentFiltered(format!("Kling code {code}: {message}")),
        1200..=1203 => MediaGenError::InvalidRequest(format!("Kling code {code}: {message}")),
        1000..=1004 | 1100..=1103 | 1302..=1304 | 5000..=5099 => {
            MediaGenError::Retryable(format!("Kling code {code}: {message}"))
        }
        _ => MediaGenError::Terminal(format!("Kling code {code}: {message}")),
    }
}

#[derive(Debug, Deserialize)]
struct KlingSubmitEnvelope {
    code: i64,
    message: Option<String>,
    data: Option<KlingSubmitData>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct KlingSubmitData {
    id: String,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct KlingTasksEnvelope {
    code: i64,
    message: Option<String>,
    #[serde(default)]
    data: Vec<KlingTaskItem>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct KlingTaskItem {
    id: String,
    status: String,
    status_message: Option<String>,
    #[serde(default)]
    outputs: Vec<KlingOutputItem>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct KlingOutputItem {
    #[serde(rename = "type")]
    kind: String,
    url: Option<String>,
    duration: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[async_trait]
impl VideoGenProvider for KlingVideoProvider {
    fn provider_id(&self) -> &'static str {
        "kling_video"
    }

    async fn submit_video_job(&self, request: &VideoGenRequest) -> Result<MediaJobHandle, MediaGenError> {
        let http_req = self.build_submit_request(request)?;
        let resp = ReqwestTransport::shared()
            .execute(http_req)
            .await
            .map_err(map_transport_error)?;

        let parsed: KlingSubmitEnvelope = serde_json::from_slice(&resp.body)
            .map_err(|err| MediaGenError::Terminal(format!("failed to parse Kling submit response: {err}")))?;
        warn_extra_fields("kling_submit_envelope", &parsed.extra);

        if parsed.code != 0 {
            return Err(classify_kling_code(
                parsed.code,
                parsed.message.as_deref().unwrap_or("Kling submission rejected"),
            ));
        }
        let data = parsed
            .data
            .ok_or_else(|| MediaGenError::Terminal("Kling submit response missing data.id".into()))?;
        warn_extra_fields("kling_submit_data", &data.extra);

        Ok(MediaJobHandle {
            job_id: data.id,
            model: request.model.clone(),
            metadata: Value::Null,
        })
    }

    async fn poll_video_job(&self, handle: &MediaJobHandle) -> Result<MediaJobStatus, MediaGenError> {
        let encoded_id = percent_encoding::utf8_percent_encode(&handle.job_id, percent_encoding::NON_ALPHANUMERIC);
        let mut http_req = Request::new(
            http::Method::GET,
            format!("{}/tasks?task_ids={encoded_id}", self.base_url),
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

        let parsed: KlingTasksEnvelope = serde_json::from_slice(&resp.body)
            .map_err(|err| MediaGenError::Terminal(format!("failed to parse Kling poll response: {err}")))?;
        warn_extra_fields("kling_tasks_envelope", &parsed.extra);

        if parsed.code != 0 {
            return Err(classify_kling_code(
                parsed.code,
                parsed.message.as_deref().unwrap_or("Kling poll rejected"),
            ));
        }

        let task = parsed
            .data
            .into_iter()
            .find(|t| t.id == handle.job_id)
            .ok_or_else(|| MediaGenError::Retryable(format!("Kling task `{}` not yet visible", handle.job_id)))?;
        warn_extra_fields("kling_task_item", &task.extra);

        match task.status.to_ascii_lowercase().as_str() {
            "submitted" => Ok(MediaJobStatus::Queued),
            "processing" => Ok(MediaJobStatus::Running { progress_pct: None }),
            "succeeded" => {
                let mut media = Vec::new();
                let mut total_duration = None;
                for output in task.outputs {
                    warn_extra_fields("kling_output_item", &output.extra);
                    if output.kind == "video"
                        && let Some(url) = output.url
                    {
                        let (bytes, mime_type) = download_media_bytes(&url, &[], "video/mp4").await?;
                        let dur = output.duration.as_deref().and_then(|d| d.parse::<f64>().ok());
                        if let Some(d) = dur {
                            total_duration = Some(d.round() as u32);
                        }
                        media.push(GeneratedMedia {
                            mime_type,
                            bytes,
                            width: None,
                            height: None,
                            duration_seconds: dur,
                            revised_prompt: None,
                        });
                    }
                }
                if media.is_empty() {
                    return Err(MediaGenError::Terminal(
                        "Kling task succeeded without video outputs".into(),
                    ));
                }
                let usage = Some(MediaUsage {
                    input_tokens: None,
                    output_tokens: None,
                    total_tokens: None,
                    generated_count: Some(media.len() as u32),
                    duration_seconds: total_duration,
                    cost_usd: None,
                });
                Ok(MediaJobStatus::Succeeded { media, usage })
            }
            "failed" => {
                let msg = task
                    .status_message
                    .unwrap_or_else(|| "Kling video generation failed".into());
                if msg.to_ascii_lowercase().contains("risk") || msg.to_ascii_lowercase().contains("policy") {
                    return Err(MediaGenError::ContentFiltered(msg));
                }
                Ok(MediaJobStatus::Failed {
                    message: msg,
                    retryable: false,
                })
            }
            other => Ok(MediaJobStatus::Failed {
                message: format!("unexpected Kling task status `{other}`"),
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
    fn conformance_bearer_auth_and_watermark_false() {
        let provider = KlingVideoProvider::new("", "kling-api-key");
        assert_video_gen_provider_conformance(&provider);

        let mut req = VideoGenRequest::new("kling-3.0", "a dragon flying over clouds");
        req.aspect_ratio = Some("16:9".into());
        req.resolution = Some("1080p".into());
        req.duration_seconds = Some(5);

        let http_req = provider.build_submit_request(&req).unwrap();
        assert_eq!(http_req.url, "https://api-beijing.klingai.com/text-to-video/kling-3.0");
        assert_eq!(http_req.headers[http::header::AUTHORIZATION], "Bearer kling-api-key");
        match http_req.body {
            Some(RequestBody::Json(body)) => {
                assert_eq!(body["options"]["watermark_info"]["enabled"], json!(false));
                assert_eq!(body["settings"]["resolution"], "1080p");
                assert_eq!(body["settings"]["duration"], 5);
            }
            other => panic!("expected JSON body, got {other:?}"),
        }
    }
}

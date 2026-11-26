//! Official Google Veo long-running video generation protocol
//! (`predictLongRunning` + operation polling), supporting both Gemini API Key
//! (`x-goog-api-key`) and Vertex AI Service Account authentication.

use async_trait::async_trait;
use base64::Engine;
use meridian_provider_sdk::client::{HttpTransport, Request, RequestBody, ReqwestTransport};
use meridian_provider_sdk::media_gen::{
    GeneratedMedia, MediaGenError, MediaJobHandle, MediaJobStatus, MediaReferenceRole, VideoGenProvider,
    VideoGenRequest,
};
use meridian_provider_sdk::wire::{ExtraIgnore, auth_header_value, warn_extra_fields};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::mixins::VertexServiceAccountAuthMixin;
use crate::protocols::google_generate_content::google_api_root;
use crate::protocols::google_image_gen::vertex_publisher_model_endpoint;

use super::media_http::{MEDIA_API_TIMEOUT, download_media_bytes, map_transport_error, reference_to_base64};

pub const DEFAULT_GOOGLE_VEO_BASE_URL: &str = "https://generativelanguage.googleapis.com";

enum GoogleVeoAuth {
    ApiKey(String),
    VertexServiceAccount(VertexServiceAccountAuthMixin),
}

pub struct GoogleVeoProvider {
    base_url: String,
    auth: GoogleVeoAuth,
}

impl GoogleVeoProvider {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        let root = if base_url.trim().is_empty() {
            DEFAULT_GOOGLE_VEO_BASE_URL.to_string()
        } else {
            google_api_root(base_url)
        };
        Self {
            base_url: root,
            auth: GoogleVeoAuth::ApiKey(api_key.to_string()),
        }
    }

    pub fn new_vertex(base_url: &str, service_account_json: &str) -> Result<Self, String> {
        let auth = VertexServiceAccountAuthMixin::from_json(service_account_json)?;
        let root = base_url.trim().trim_end_matches('/').to_string();
        Ok(Self {
            base_url: if root.is_empty() {
                "https://us-central1-aiplatform.googleapis.com".to_string()
            } else {
                root
            },
            auth: GoogleVeoAuth::VertexServiceAccount(auth),
        })
    }

    pub(crate) fn submit_url(&self, model: &str) -> String {
        let model = model.strip_prefix("models/").unwrap_or(model);
        match &self.auth {
            GoogleVeoAuth::ApiKey(_) => format!("{}/v1beta/models/{model}:predictLongRunning", self.base_url),
            GoogleVeoAuth::VertexServiceAccount(vertex) => {
                vertex_publisher_model_endpoint(&self.base_url, vertex.project_id(), model, "predictLongRunning")
            }
        }
    }

    pub(crate) fn build_submit_body(&self, request: &VideoGenRequest) -> Result<Value, MediaGenError> {
        if request.prompt.trim().is_empty() && request.reference_images.is_empty() {
            return Err(MediaGenError::InvalidRequest(
                "video prompt or reference image must not be empty".into(),
            ));
        }

        let is_vertex = matches!(self.auth, GoogleVeoAuth::VertexServiceAccount(_));
        let mut instance = Map::new();
        if !request.prompt.trim().is_empty() {
            instance.insert("prompt".into(), json!(request.prompt));
        }

        let mut asset_refs = Vec::new();
        for reference in &request.reference_images {
            let encoded_image = if is_vertex {
                json!({
                    "bytesBase64Encoded": reference_to_base64(reference),
                    "mimeType": reference.mime_type,
                })
            } else {
                json!({
                    "inlineData": {
                        "mimeType": reference.mime_type,
                        "data": reference_to_base64(reference),
                    }
                })
            };
            match reference.role {
                MediaReferenceRole::FirstFrame => {
                    instance.insert("image".into(), encoded_image);
                }
                MediaReferenceRole::LastFrame => {
                    instance.insert("lastFrame".into(), encoded_image);
                }
                MediaReferenceRole::Reference => {
                    asset_refs.push(json!({
                        "image": encoded_image,
                        "referenceType": "asset",
                    }));
                }
            }
        }
        if !asset_refs.is_empty() {
            instance.insert("referenceImages".into(), Value::Array(asset_refs));
        }

        let mut parameters = Map::new();
        if let Some(ratio) = request.aspect_ratio.as_deref().filter(|r| !r.trim().is_empty()) {
            parameters.insert("aspectRatio".into(), json!(ratio));
        }
        if let Some(resolution) = request.resolution.as_deref().filter(|r| !r.trim().is_empty()) {
            parameters.insert("resolution".into(), json!(resolution));
        }
        if let Some(duration) = request.duration_seconds {
            parameters.insert("durationSeconds".into(), json!(duration));
        }
        if let Some(seed) = request.seed {
            parameters.insert("seed".into(), json!(seed));
        }
        if let Some(audio) = request.generate_audio {
            parameters.insert("generateAudio".into(), json!(audio));
        }

        let mut payload = Map::new();
        payload.insert("instances".into(), Value::Array(vec![Value::Object(instance)]));
        if !parameters.is_empty() {
            payload.insert("parameters".into(), Value::Object(parameters));
        }
        Ok(Value::Object(payload))
    }

    async fn apply_auth(&self, req: &mut Request) -> Result<(), MediaGenError> {
        match &self.auth {
            GoogleVeoAuth::ApiKey(api_key) => {
                req.headers.insert("x-goog-api-key", auth_header_value(api_key));
                Ok(())
            }
            GoogleVeoAuth::VertexServiceAccount(vertex) => vertex
                .apply_bearer(req)
                .await
                .map_err(|err| MediaGenError::Retryable(format!("Vertex auth failed: {err}"))),
        }
    }
}

#[derive(Debug, Deserialize)]
struct VeoOperationResponse {
    name: Option<String>,
    #[serde(default)]
    done: bool,
    error: Option<VeoOperationError>,
    response: Option<VeoOperationPayload>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct VeoOperationError {
    code: Option<i64>,
    message: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VeoOperationPayload {
    generate_video_response: Option<VeoGenerateVideoResponse>,
    #[serde(default)]
    videos: Vec<VeoVertexVideo>,
    rai_media_filtered_count: Option<u32>,
    #[serde(default)]
    rai_media_filtered_reasons: Vec<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VeoGenerateVideoResponse {
    #[serde(default)]
    generated_samples: Vec<VeoGeneratedSample>,
    rai_media_filtered_count: Option<u32>,
    #[serde(default)]
    rai_media_filtered_reasons: Vec<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct VeoGeneratedSample {
    video: Option<VeoApiKeyVideo>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct VeoApiKeyVideo {
    uri: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VeoVertexVideo {
    bytes_base64_encoded: Option<String>,
    mime_type: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[async_trait]
impl VideoGenProvider for GoogleVeoProvider {
    fn provider_id(&self) -> &'static str {
        "google_veo"
    }

    async fn submit_video_job(&self, request: &VideoGenRequest) -> Result<MediaJobHandle, MediaGenError> {
        let body = self.build_submit_body(request)?;
        let mut http_req = Request::new(http::Method::POST, self.submit_url(&request.model));
        http_req.body = Some(RequestBody::Json(body));
        http_req.timeout = Some(MEDIA_API_TIMEOUT);
        http_req.retry = None;
        self.apply_auth(&mut http_req).await?;

        let resp = ReqwestTransport::shared()
            .execute(http_req)
            .await
            .map_err(map_transport_error)?;

        let parsed: VeoOperationResponse = serde_json::from_slice(&resp.body)
            .map_err(|err| MediaGenError::Terminal(format!("failed to parse Google Veo submit response: {err}")))?;
        warn_extra_fields("google_veo_submit_response", &parsed.extra);

        let operation_name = parsed
            .name
            .filter(|n| !n.trim().is_empty())
            .ok_or_else(|| MediaGenError::Terminal("Google Veo submit response missing operation name".into()))?;

        Ok(MediaJobHandle {
            job_id: operation_name,
            model: request.model.clone(),
            metadata: Value::Null,
        })
    }

    async fn poll_video_job(&self, handle: &MediaJobHandle) -> Result<MediaJobStatus, MediaGenError> {
        let mut http_req = match &self.auth {
            GoogleVeoAuth::ApiKey(_) => {
                let op = handle.job_id.trim_start_matches('/');
                Request::new(http::Method::GET, format!("{}/v1beta/{op}", self.base_url))
            }
            GoogleVeoAuth::VertexServiceAccount(vertex) => {
                let model = handle.model.strip_prefix("models/").unwrap_or(&handle.model);
                let url = vertex_publisher_model_endpoint(
                    &self.base_url,
                    vertex.project_id(),
                    model,
                    "fetchPredictOperation",
                );
                let mut req = Request::new(http::Method::POST, url);
                req.body = Some(RequestBody::Json(json!({ "operationName": handle.job_id })));
                req
            }
        };
        http_req.timeout = Some(MEDIA_API_TIMEOUT);
        http_req.retry = None;
        self.apply_auth(&mut http_req).await?;

        let resp = ReqwestTransport::shared()
            .execute(http_req)
            .await
            .map_err(map_transport_error)?;

        let parsed: VeoOperationResponse = serde_json::from_slice(&resp.body)
            .map_err(|err| MediaGenError::Terminal(format!("failed to parse Google Veo poll response: {err}")))?;
        warn_extra_fields("google_veo_poll_response", &parsed.extra);

        if let Some(error) = parsed.error {
            warn_extra_fields("google_veo_error", &error.extra);
            let msg = error.message.unwrap_or_else(|| "Google Veo operation failed".into());
            let retryable = matches!(error.code, Some(4 | 8 | 13 | 14));
            return Ok(MediaJobStatus::Failed {
                message: msg,
                retryable,
            });
        }

        if !parsed.done {
            return Ok(MediaJobStatus::Running { progress_pct: None });
        }

        let payload = parsed
            .response
            .ok_or_else(|| MediaGenError::Terminal("Google Veo finished without response payload".into()))?;
        warn_extra_fields("google_veo_payload", &payload.extra);

        let mut media = Vec::new();

        // API Key path: generateVideoResponse.generatedSamples[].video.uri
        if let Some(gvr) = payload.generate_video_response {
            warn_extra_fields("google_veo_generate_video_response", &gvr.extra);
            if gvr.rai_media_filtered_count.unwrap_or(0) > 0 && gvr.generated_samples.is_empty() {
                let reason = gvr
                    .rai_media_filtered_reasons
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "filtered by Google safety policy".into());
                return Err(MediaGenError::ContentFiltered(reason));
            }
            for sample in gvr.generated_samples {
                warn_extra_fields("google_veo_sample", &sample.extra);
                if let Some(video) = sample.video {
                    warn_extra_fields("google_veo_video", &video.extra);
                    if let Some(uri) = video.uri {
                        let headers: Vec<(&str, &str)> = match &self.auth {
                            GoogleVeoAuth::ApiKey(key) => vec![("x-goog-api-key", key.as_str())],
                            GoogleVeoAuth::VertexServiceAccount(_) => Vec::new(),
                        };
                        let (bytes, mime_type) = download_media_bytes(&uri, &headers, "video/mp4").await?;
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
            }
        }

        // Vertex AI path: videos[].bytesBase64Encoded
        if media.is_empty() {
            if payload.rai_media_filtered_count.unwrap_or(0) > 0 && payload.videos.is_empty() {
                let reason = payload
                    .rai_media_filtered_reasons
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "filtered by Vertex AI safety policy".into());
                return Err(MediaGenError::ContentFiltered(reason));
            }
            for video in payload.videos {
                warn_extra_fields("google_veo_vertex_video", &video.extra);
                if let Some(b64) = video.bytes_base64_encoded {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(b64.trim())
                        .map_err(|err| MediaGenError::Terminal(format!("invalid base64 video from Vertex: {err}")))?;
                    media.push(GeneratedMedia {
                        mime_type: video.mime_type.unwrap_or_else(|| "video/mp4".into()),
                        bytes,
                        width: None,
                        height: None,
                        duration_seconds: None,
                        revised_prompt: None,
                    });
                }
            }
        }

        if media.is_empty() {
            return Err(MediaGenError::Terminal(
                "Google Veo operation produced no videos".into(),
            ));
        }

        Ok(MediaJobStatus::Succeeded { media, usage: None })
    }
}

#[cfg(test)]
mod tests {
    use meridian_provider_sdk::conformance::assert_video_gen_provider_conformance;
    use meridian_provider_sdk::media_gen::MediaReference;

    use super::*;

    #[test]
    fn conformance_and_submit_payload() {
        let provider = GoogleVeoProvider::new("", "AIza-test");
        assert_video_gen_provider_conformance(&provider);
        assert_eq!(
            provider.submit_url("veo-3.1-generate-preview"),
            "https://generativelanguage.googleapis.com/v1beta/models/veo-3.1-generate-preview:predictLongRunning"
        );

        let mut req = VideoGenRequest::new("veo-3.1-generate-preview", "ocean waves at sunset");
        req.aspect_ratio = Some("16:9".into());
        req.resolution = Some("1080p".into());
        req.duration_seconds = Some(8);
        req.reference_images.push(MediaReference {
            mime_type: "image/png".into(),
            bytes: vec![1, 2, 3],
            role: MediaReferenceRole::FirstFrame,
        });

        let body = provider.build_submit_body(&req).unwrap();
        assert_eq!(body["instances"][0]["prompt"], "ocean waves at sunset");
        assert!(body["instances"][0]["image"]["inlineData"]["data"].is_string());
        assert_eq!(body["parameters"]["aspectRatio"], "16:9");
        assert_eq!(body["parameters"]["resolution"], "1080p");
        assert_eq!(body["parameters"]["durationSeconds"], 8);
    }
}

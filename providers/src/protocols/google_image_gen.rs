//! Official Google Gemini Native Image Generation protocol (`generateContent`
//! with `responseModalities: ["IMAGE"]`), supporting both Gemini API Key and
//! Google Cloud Vertex AI Service Account authentication.

use async_trait::async_trait;
use base64::Engine;
use meridian_provider_sdk::client::{HttpTransport, Request, RequestBody, ReqwestTransport};
use meridian_provider_sdk::media_gen::{
    GeneratedMedia, ImageGenProvider, ImageGenRequest, ImageGenResponse, MediaGenError, MediaUsage,
};
use meridian_provider_sdk::wire::{ExtraIgnore, auth_header_value, warn_extra_fields};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::mixins::VertexServiceAccountAuthMixin;
use crate::protocols::google_generate_content::google_api_root;

use super::media_http::{MEDIA_API_TIMEOUT, map_transport_error, reference_to_base64};

pub const DEFAULT_GOOGLE_IMAGE_GEN_BASE_URL: &str = "https://generativelanguage.googleapis.com";

enum GoogleImageAuth {
    ApiKey(String),
    VertexServiceAccount(VertexServiceAccountAuthMixin),
}

pub struct GoogleImageGenProvider {
    base_url: String,
    auth: GoogleImageAuth,
}

impl GoogleImageGenProvider {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        let root = if base_url.trim().is_empty() {
            DEFAULT_GOOGLE_IMAGE_GEN_BASE_URL.to_string()
        } else {
            google_api_root(base_url)
        };
        Self {
            base_url: root,
            auth: GoogleImageAuth::ApiKey(api_key.to_string()),
        }
    }

    pub fn new_vertex(base_url: &str, service_account_json: &str) -> Result<Self, String> {
        let auth = VertexServiceAccountAuthMixin::from_json(service_account_json)?;
        let root = base_url.trim().trim_end_matches('/').to_string();
        Ok(Self {
            base_url: if root.is_empty() {
                "https://aiplatform.googleapis.com/v1/locations/global".to_string()
            } else {
                root
            },
            auth: GoogleImageAuth::VertexServiceAccount(auth),
        })
    }

    pub(crate) fn endpoint_for_model(&self, model: &str) -> String {
        let model = model.strip_prefix("models/").unwrap_or(model);
        match &self.auth {
            GoogleImageAuth::ApiKey(_) => {
                format!("{}/v1beta/models/{model}:generateContent", self.base_url)
            }
            GoogleImageAuth::VertexServiceAccount(vertex) => {
                vertex_publisher_model_endpoint(&self.base_url, vertex.project_id(), model, "generateContent")
            }
        }
    }

    pub(crate) fn build_request_body(&self, request: &ImageGenRequest) -> Result<Value, MediaGenError> {
        if request.prompt.trim().is_empty() {
            return Err(MediaGenError::InvalidRequest("prompt must not be empty".into()));
        }

        let mut parts = Vec::with_capacity(request.reference_images.len() + 1);
        for reference in &request.reference_images {
            parts.push(json!({
                "inlineData": {
                    "mimeType": reference.mime_type,
                    "data": reference_to_base64(reference),
                }
            }));
        }
        parts.push(json!({ "text": request.prompt }));

        let mut generation_config = Map::new();
        generation_config.insert("responseModalities".into(), json!(["TEXT", "IMAGE"]));

        let mut image_config = Map::new();
        if let Some(ratio) = request.aspect_ratio.as_deref().filter(|r| !r.trim().is_empty()) {
            image_config.insert("aspectRatio".into(), json!(ratio));
        }
        if let Some(size) = request.size.as_deref().filter(|s| !s.trim().is_empty()) {
            image_config.insert("imageSize".into(), json!(size));
        }
        if !image_config.is_empty() {
            generation_config.insert("imageConfig".into(), Value::Object(image_config));
        }
        if let Some(n) = request.count.filter(|&n| n > 1) {
            generation_config.insert("candidateCount".into(), json!(n));
        }
        if let Some(seed) = request.seed {
            generation_config.insert("seed".into(), json!(seed));
        }

        Ok(json!({
            "contents": [{ "role": "user", "parts": parts }],
            "generationConfig": Value::Object(generation_config),
        }))
    }
}

/// Build a Vertex AI publisher model endpoint:
/// `{origin}/v1/projects/{project_id}/locations/{location}/publishers/google/models/{model}:{action}`.
pub(crate) fn vertex_publisher_model_endpoint(base_url: &str, project_id: &str, model: &str, action: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    // If caller already supplied `/projects/.../locations/...`, append directly.
    if trimmed.contains("/projects/") && trimmed.contains("/locations/") {
        return format!("{trimmed}/publishers/google/models/{model}:{action}");
    }
    // Extract location if base_url ends with `/locations/{loc}` (matching Meridian's Google manifest URL).
    let (origin, location) = if let Some(idx) = trimmed.find("/v1beta1/locations/") {
        (&trimmed[..idx], &trimmed[idx + "/v1beta1/locations/".len()..])
    } else if let Some(idx) = trimmed.find("/v1/locations/") {
        (&trimmed[..idx], &trimmed[idx + "/v1/locations/".len()..])
    } else {
        (trimmed, "us-central1")
    };
    let location = location.trim_matches('/');
    let location = if location.is_empty() { "us-central1" } else { location };
    format!("{origin}/v1/projects/{project_id}/locations/{location}/publishers/google/models/{model}:{action}")
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoogleImageGenResponse {
    #[serde(default)]
    candidates: Vec<GoogleImageCandidate>,
    prompt_feedback: Option<GooglePromptFeedback>,
    usage_metadata: Option<GoogleUsageMetadata>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GooglePromptFeedback {
    block_reason: Option<String>,
    block_reason_message: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoogleImageCandidate {
    content: Option<GoogleImageContent>,
    finish_reason: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct GoogleImageContent {
    #[serde(default)]
    parts: Vec<GoogleImagePart>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoogleImagePart {
    text: Option<String>,
    inline_data: Option<GoogleInlineData>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoogleInlineData {
    mime_type: String,
    data: String,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoogleUsageMetadata {
    prompt_token_count: Option<i64>,
    candidates_token_count: Option<i64>,
    total_token_count: Option<i64>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[async_trait]
impl ImageGenProvider for GoogleImageGenProvider {
    fn provider_id(&self) -> &'static str {
        "google_image_gen"
    }

    async fn generate_image(&self, request: &ImageGenRequest) -> Result<ImageGenResponse, MediaGenError> {
        let body = self.build_request_body(request)?;
        let mut http_req = Request::new(http::Method::POST, self.endpoint_for_model(&request.model));
        http_req.body = Some(RequestBody::Json(body));
        http_req.timeout = Some(MEDIA_API_TIMEOUT);
        http_req.retry = None;

        match &self.auth {
            GoogleImageAuth::ApiKey(api_key) => {
                http_req.headers.insert("x-goog-api-key", auth_header_value(api_key));
            }
            GoogleImageAuth::VertexServiceAccount(vertex) => {
                vertex
                    .apply_bearer(&mut http_req)
                    .await
                    .map_err(|err| MediaGenError::Retryable(format!("Vertex auth failed: {err}")))?;
            }
        }

        let resp = ReqwestTransport::shared()
            .execute(http_req)
            .await
            .map_err(map_transport_error)?;

        let parsed: GoogleImageGenResponse = serde_json::from_slice(&resp.body)
            .map_err(|err| MediaGenError::Terminal(format!("failed to parse Gemini image response: {err}")))?;
        warn_extra_fields("google_image_gen_response", &parsed.extra);

        if let Some(feedback) = parsed.prompt_feedback {
            warn_extra_fields("google_image_gen_feedback", &feedback.extra);
            if let Some(reason) = feedback.block_reason {
                let detail = feedback.block_reason_message.unwrap_or(reason);
                return Err(MediaGenError::ContentFiltered(detail));
            }
        }

        let mut images = Vec::new();
        let mut blocked_reason: Option<String> = None;

        for candidate in parsed.candidates {
            warn_extra_fields("google_image_gen_candidate", &candidate.extra);
            if let Some(reason) = candidate.finish_reason.as_deref()
                && matches!(reason, "SAFETY" | "IMAGE_SAFETY" | "PROHIBITED_CONTENT" | "RECITATION")
            {
                blocked_reason = Some(reason.to_string());
            }
            if let Some(content) = candidate.content {
                warn_extra_fields("google_image_gen_content", &content.extra);
                let mut text_notes = Vec::new();
                for part in content.parts {
                    warn_extra_fields("google_image_gen_part", &part.extra);
                    if let Some(text) = part.text.filter(|t| !t.trim().is_empty()) {
                        text_notes.push(text);
                    }
                    if let Some(inline) = part.inline_data {
                        warn_extra_fields("google_image_gen_inline_data", &inline.extra);
                        if inline.mime_type.starts_with("image/") {
                            let bytes = base64::engine::general_purpose::STANDARD
                                .decode(inline.data.trim())
                                .map_err(|err| {
                                    MediaGenError::Terminal(format!("invalid base64 image from Gemini: {err}"))
                                })?;
                            images.push(GeneratedMedia {
                                mime_type: inline.mime_type,
                                bytes,
                                width: None,
                                height: None,
                                duration_seconds: None,
                                revised_prompt: None,
                            });
                        }
                    }
                }
                if !text_notes.is_empty()
                    && let Some(first) = images.last_mut()
                {
                    first.revised_prompt = Some(text_notes.join("\n"));
                }
            }
        }

        if images.is_empty() {
            if let Some(reason) = blocked_reason {
                return Err(MediaGenError::ContentFiltered(format!(
                    "Gemini image generation blocked ({reason})"
                )));
            }
            return Err(MediaGenError::Terminal(
                "Gemini image generation returned no inline image data".into(),
            ));
        }

        let usage = parsed.usage_metadata.map(|u| {
            warn_extra_fields("google_image_gen_usage", &u.extra);
            MediaUsage {
                input_tokens: u.prompt_token_count,
                output_tokens: u.candidates_token_count,
                total_tokens: u.total_token_count,
                generated_count: Some(images.len() as u32),
                duration_seconds: None,
                cost_usd: None,
            }
        });

        Ok(ImageGenResponse { images, usage })
    }
}

#[cfg(test)]
mod tests {
    use meridian_provider_sdk::conformance::assert_image_gen_provider_conformance;

    use super::*;

    #[test]
    fn conformance_and_endpoint_building() {
        let api_provider = GoogleImageGenProvider::new("https://generativelanguage.googleapis.com", "AIza-test");
        assert_image_gen_provider_conformance(&api_provider);
        assert_eq!(
            api_provider.endpoint_for_model("gemini-2.5-flash-image"),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash-image:generateContent"
        );

        let vertex_json = r#"{
            "type": "service_account",
            "project_id": "demo-proj",
            "client_email": "bot@demo-proj.iam.gserviceaccount.com",
            "private_key": "not-a-real-key"
        }"#;
        let vertex_provider = GoogleImageGenProvider::new_vertex(
            "https://aiplatform.googleapis.com/v1beta1/locations/global",
            vertex_json,
        )
        .unwrap();
        assert_image_gen_provider_conformance(&vertex_provider);
        assert_eq!(
            vertex_provider.endpoint_for_model("gemini-3-pro-image-preview"),
            "https://aiplatform.googleapis.com/v1/projects/demo-proj/locations/global/publishers/google/models/gemini-3-pro-image-preview:generateContent"
        );
    }

    #[test]
    fn builds_image_config_and_response_modalities() {
        let provider = GoogleImageGenProvider::new("https://generativelanguage.googleapis.com", "k");
        let mut req = ImageGenRequest::new("gemini-3-pro-image-preview", "mountain sunrise");
        req.aspect_ratio = Some("16:9".into());
        req.size = Some("2K".into());
        let body = provider.build_request_body(&req).unwrap();
        assert_eq!(body["generationConfig"]["responseModalities"], json!(["TEXT", "IMAGE"]));
        assert_eq!(body["generationConfig"]["imageConfig"]["aspectRatio"], "16:9");
        assert_eq!(body["generationConfig"]["imageConfig"]["imageSize"], "2K");
    }
}

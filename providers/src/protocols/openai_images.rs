//! Official OpenAI Images API protocol (`POST /images/generations` and
//! `POST /images/edits`).

use async_trait::async_trait;
use base64::Engine;
use meridian_provider_sdk::client::{HttpTransport, Request, RequestBody, ReqwestTransport};
use meridian_provider_sdk::media_gen::{
    GeneratedMedia, ImageGenProvider, ImageGenRequest, ImageGenResponse, MediaGenError, MediaUsage,
};
use meridian_provider_sdk::wire::{ExtraIgnore, auth_header_value, warn_extra_fields};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::media_http::{
    MEDIA_API_TIMEOUT, MultipartFormBuilder, download_media_bytes, extension_for_mime, map_transport_error,
};

pub const DEFAULT_OPENAI_IMAGES_BASE_URL: &str = "https://api.openai.com/v1";

pub struct OpenAIImagesProvider {
    base_url: String,
    api_key: String,
}

impl OpenAIImagesProvider {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        let trimmed = base_url.trim().trim_end_matches('/');
        Self {
            base_url: if trimmed.is_empty() {
                DEFAULT_OPENAI_IMAGES_BASE_URL.to_string()
            } else {
                trimmed.to_string()
            },
            api_key: api_key.to_string(),
        }
    }

    pub(crate) fn build_request(&self, request: &ImageGenRequest) -> Result<Request, MediaGenError> {
        if request.prompt.trim().is_empty() {
            return Err(MediaGenError::InvalidRequest("prompt must not be empty".into()));
        }

        let size = resolve_openai_image_size(request.size.as_deref(), request.aspect_ratio.as_deref());

        let mut http_req = if request.reference_images.is_empty() {
            let mut body = Map::new();
            body.insert("model".into(), json!(request.model));
            body.insert("prompt".into(), json!(request.prompt));
            if let Some(n) = request.count {
                body.insert("n".into(), json!(n));
            }
            if let Some(size) = size {
                body.insert("size".into(), json!(size));
            }
            if let Some(quality) = request.quality.as_deref().filter(|q| !q.trim().is_empty()) {
                body.insert("quality".into(), json!(quality));
            }

            let mut req = Request::new(http::Method::POST, format!("{}/images/generations", self.base_url));
            req.body = Some(RequestBody::Json(Value::Object(body)));
            req
        } else {
            let mut form = MultipartFormBuilder::new()
                .text("model", &request.model)
                .text("prompt", &request.prompt);
            if let Some(n) = request.count {
                form = form.text("n", &n.to_string());
            }
            if let Some(size) = size {
                form = form.text("size", size);
            }
            if let Some(quality) = request.quality.as_deref().filter(|q| !q.trim().is_empty()) {
                form = form.text("quality", quality);
            }

            let field_name = if request.reference_images.len() > 1 {
                "image[]"
            } else {
                "image"
            };
            for (idx, reference) in request.reference_images.iter().enumerate() {
                let ext = extension_for_mime(&reference.mime_type);
                let filename = format!("reference_{idx}.{ext}");
                form = form.file(field_name, &filename, &reference.mime_type, &reference.bytes);
            }

            let (content_type, raw_bytes) = form.finish();
            let mut req = Request::new(http::Method::POST, format!("{}/images/edits", self.base_url));
            req.headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_str(&content_type)
                    .map_err(|err| MediaGenError::InvalidRequest(format!("invalid multipart header: {err}")))?,
            );
            req.body = Some(RequestBody::Raw(raw_bytes));
            req
        };

        http_req.headers.insert(
            http::header::AUTHORIZATION,
            auth_header_value(&format!("Bearer {}", self.api_key)),
        );
        http_req.timeout = Some(MEDIA_API_TIMEOUT);
        http_req.retry = None;
        Ok(http_req)
    }
}

fn resolve_openai_image_size<'a>(size: Option<&'a str>, aspect_ratio: Option<&'a str>) -> Option<&'a str> {
    if let Some(s) = size.filter(|s| !s.trim().is_empty()) {
        return Some(s);
    }
    match aspect_ratio? {
        "1:1" => Some("1024x1024"),
        "3:2" | "16:9" | "4:3" => Some("1536x1024"),
        "2:3" | "9:16" | "3:4" => Some("1024x1536"),
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
struct OpenAIImagesResponse {
    #[serde(default)]
    data: Vec<OpenAIImageItem>,
    output_format: Option<String>,
    usage: Option<OpenAIImagesUsage>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct OpenAIImageItem {
    b64_json: Option<String>,
    url: Option<String>,
    revised_prompt: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct OpenAIImagesUsage {
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    total_tokens: Option<i64>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

fn mime_for_output_format(fmt: Option<&str>) -> &'static str {
    match fmt.unwrap_or("png").to_ascii_lowercase().as_str() {
        "jpeg" | "jpg" => "image/jpeg",
        "webp" => "image/webp",
        _ => "image/png",
    }
}

#[async_trait]
impl ImageGenProvider for OpenAIImagesProvider {
    fn provider_id(&self) -> &'static str {
        "openai_images"
    }

    async fn generate_image(&self, request: &ImageGenRequest) -> Result<ImageGenResponse, MediaGenError> {
        let http_req = self.build_request(request)?;
        let resp = ReqwestTransport::shared()
            .execute(http_req)
            .await
            .map_err(map_transport_error)?;

        let parsed: OpenAIImagesResponse = serde_json::from_slice(&resp.body)
            .map_err(|err| MediaGenError::Terminal(format!("failed to parse OpenAI Images response: {err}")))?;
        warn_extra_fields("openai_images_response", &parsed.extra);

        let default_mime = mime_for_output_format(parsed.output_format.as_deref());
        let mut images = Vec::with_capacity(parsed.data.len());
        for item in parsed.data {
            warn_extra_fields("openai_images_item", &item.extra);
            if let Some(b64) = item.b64_json {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64.trim())
                    .map_err(|err| MediaGenError::Terminal(format!("invalid base64 image from OpenAI: {err}")))?;
                images.push(GeneratedMedia {
                    mime_type: default_mime.to_string(),
                    bytes,
                    width: None,
                    height: None,
                    duration_seconds: None,
                    revised_prompt: item.revised_prompt,
                });
            } else if let Some(url) = item.url {
                let (bytes, mime_type) = download_media_bytes(&url, &[], default_mime).await?;
                images.push(GeneratedMedia {
                    mime_type,
                    bytes,
                    width: None,
                    height: None,
                    duration_seconds: None,
                    revised_prompt: item.revised_prompt,
                });
            }
        }

        if images.is_empty() {
            return Err(MediaGenError::Terminal(
                "OpenAI Images API returned no generated images".into(),
            ));
        }

        let usage = parsed.usage.map(|u| {
            warn_extra_fields("openai_images_usage", &u.extra);
            MediaUsage {
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
                total_tokens: u.total_tokens,
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
    use meridian_provider_sdk::media_gen::{MediaReference, MediaReferenceRole};

    use super::*;

    #[test]
    fn conformance_passes() {
        let provider = OpenAIImagesProvider::new("https://api.openai.com/v1", "sk-test");
        assert_image_gen_provider_conformance(&provider);
    }

    #[test]
    fn text_to_image_builds_json_generations_request() {
        let provider = OpenAIImagesProvider::new("https://api.openai.com/v1/", "sk-test");
        let mut req = ImageGenRequest::new("gpt-image-1", "a red fox in snow");
        req.aspect_ratio = Some("16:9".into());
        req.quality = Some("high".into());
        req.count = Some(2);

        let http_req = provider.build_request(&req).unwrap();
        assert_eq!(http_req.url, "https://api.openai.com/v1/images/generations");
        assert_eq!(http_req.headers[http::header::AUTHORIZATION], "Bearer sk-test");
        match http_req.body {
            Some(RequestBody::Json(body)) => {
                assert_eq!(body["model"], "gpt-image-1");
                assert_eq!(body["size"], "1536x1024");
                assert_eq!(body["quality"], "high");
                assert_eq!(body["n"], 2);
            }
            other => panic!("expected JSON body, got {other:?}"),
        }
    }

    #[test]
    fn image_to_image_builds_multipart_edits_request() {
        let provider = OpenAIImagesProvider::new("https://api.openai.com/v1", "sk-test");
        let mut req = ImageGenRequest::new("gpt-image-1", "make it watercolor");
        req.reference_images.push(MediaReference {
            mime_type: "image/png".into(),
            bytes: vec![0x89, 0x50, 0x4e, 0x47],
            role: MediaReferenceRole::Reference,
        });

        let http_req = provider.build_request(&req).unwrap();
        assert_eq!(http_req.url, "https://api.openai.com/v1/images/edits");
        let ct = http_req.headers[http::header::CONTENT_TYPE].to_str().unwrap();
        assert!(ct.starts_with("multipart/form-data; boundary="));
        match http_req.body {
            Some(RequestBody::Raw(bytes)) => {
                let text = String::from_utf8_lossy(&bytes);
                assert!(text.contains("name=\"model\""));
                assert!(text.contains("name=\"image\"; filename=\"reference_0.png\""));
            }
            other => panic!("expected Raw multipart body, got {other:?}"),
        }
    }
}

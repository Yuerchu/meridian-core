//! Official Volcengine Ark / BytePlus Seedream image generation protocol
//! (`POST {base_url}/images/generations`).
//!
//! Explicitly sends `watermark: false` by default so `meridian-core` always
//! stores clean originals and owns compliance watermarking (D-001).

use async_trait::async_trait;
use base64::Engine;
use meridian_provider_sdk::client::{HttpTransport, Request, RequestBody, ReqwestTransport};
use meridian_provider_sdk::media_gen::{
    GeneratedMedia, ImageGenProvider, ImageGenRequest, ImageGenResponse, MediaGenError, MediaUsage,
};
use meridian_provider_sdk::wire::{ExtraIgnore, auth_header_value, warn_extra_fields};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::media_http::{MEDIA_API_TIMEOUT, download_media_bytes, map_transport_error, reference_to_data_uri};

pub const DEFAULT_VOLCENGINE_ARK_BASE_URL: &str = "https://ark.cn-beijing.volces.com/api/v3";

pub struct VolcengineSeedreamProvider {
    base_url: String,
    api_key: String,
}

impl VolcengineSeedreamProvider {
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

    pub(crate) fn build_request(&self, request: &ImageGenRequest) -> Result<Request, MediaGenError> {
        if request.prompt.trim().is_empty() {
            return Err(MediaGenError::InvalidRequest("prompt must not be empty".into()));
        }

        let mut body = Map::new();
        body.insert("model".into(), json!(request.model));
        body.insert("prompt".into(), json!(request.prompt));
        body.insert("response_format".into(), json!("b64_json"));
        // D-001 compliance boundary: request clean original unless caller explicitly opts in.
        body.insert("watermark".into(), json!(request.watermark));

        if let Some(size) = resolve_seedream_size(request.size.as_deref(), request.aspect_ratio.as_deref()) {
            body.insert("size".into(), json!(size));
        }
        if let Some(seed) = request.seed {
            body.insert("seed".into(), json!(seed));
        }
        if request.reference_images.len() == 1 {
            body.insert(
                "image".into(),
                json!(reference_to_data_uri(&request.reference_images[0])),
            );
        } else if request.reference_images.len() > 1 {
            let uris: Vec<String> = request.reference_images.iter().map(reference_to_data_uri).collect();
            body.insert("image".into(), json!(uris));
        }

        let mut http_req = Request::new(http::Method::POST, format!("{}/images/generations", self.base_url));
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

fn resolve_seedream_size<'a>(size: Option<&'a str>, aspect_ratio: Option<&'a str>) -> Option<&'a str> {
    if let Some(s) = size.filter(|s| !s.trim().is_empty()) {
        return Some(s);
    }
    match aspect_ratio? {
        "1:1" => Some("2048x2048"),
        "16:9" => Some("2560x1440"),
        "9:16" => Some("1440x2560"),
        "4:3" => Some("2304x1728"),
        "3:4" => Some("1728x2304"),
        "3:2" => Some("2496x1664"),
        "2:3" => Some("1664x2496"),
        "21:9" => Some("3024x1296"),
        _ => None,
    }
}

fn parse_dimensions(size: Option<&str>) -> (Option<u32>, Option<u32>) {
    let Some(raw) = size else { return (None, None) };
    let Some((w, h)) = raw.split_once('x').or_else(|| raw.split_once('X')) else {
        return (None, None);
    };
    (w.trim().parse().ok(), h.trim().parse().ok())
}

#[derive(Debug, Deserialize)]
struct SeedreamResponse {
    #[serde(default)]
    data: Vec<SeedreamImageItem>,
    usage: Option<SeedreamUsage>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct SeedreamImageItem {
    url: Option<String>,
    b64_json: Option<String>,
    size: Option<String>,
    error: Option<SeedreamItemError>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct SeedreamItemError {
    code: Option<String>,
    message: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Deserialize)]
struct SeedreamUsage {
    generated_images: Option<u32>,
    output_tokens: Option<i64>,
    total_tokens: Option<i64>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[async_trait]
impl ImageGenProvider for VolcengineSeedreamProvider {
    fn provider_id(&self) -> &'static str {
        "volcengine_seedream"
    }

    async fn generate_image(&self, request: &ImageGenRequest) -> Result<ImageGenResponse, MediaGenError> {
        let http_req = self.build_request(request)?;
        let resp = ReqwestTransport::shared()
            .execute(http_req)
            .await
            .map_err(map_transport_error)?;

        let parsed: SeedreamResponse = serde_json::from_slice(&resp.body)
            .map_err(|err| MediaGenError::Terminal(format!("failed to parse Seedream response: {err}")))?;
        warn_extra_fields("volcengine_seedream_response", &parsed.extra);

        let mut images = Vec::with_capacity(parsed.data.len());
        let mut last_item_error: Option<String> = None;

        for item in parsed.data {
            warn_extra_fields("volcengine_seedream_item", &item.extra);
            if let Some(err) = item.error {
                warn_extra_fields("volcengine_seedream_item_error", &err.extra);
                let msg = err.message.or(err.code).unwrap_or_else(|| "item failed".into());
                last_item_error = Some(msg);
                continue;
            }
            let (width, height) = parse_dimensions(item.size.as_deref());
            if let Some(b64) = item.b64_json {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64.trim())
                    .map_err(|err| MediaGenError::Terminal(format!("invalid base64 image from Seedream: {err}")))?;
                images.push(GeneratedMedia {
                    mime_type: "image/png".into(),
                    bytes,
                    width,
                    height,
                    duration_seconds: None,
                    revised_prompt: None,
                });
            } else if let Some(url) = item.url {
                let (bytes, mime_type) = download_media_bytes(&url, &[], "image/jpeg").await?;
                images.push(GeneratedMedia {
                    mime_type,
                    bytes,
                    width,
                    height,
                    duration_seconds: None,
                    revised_prompt: None,
                });
            }
        }

        if images.is_empty() {
            if let Some(detail) = last_item_error {
                return Err(MediaGenError::from_http_status(400, detail));
            }
            return Err(MediaGenError::Terminal(
                "Volcengine Seedream returned no generated images".into(),
            ));
        }

        let usage = parsed.usage.map(|u| {
            warn_extra_fields("volcengine_seedream_usage", &u.extra);
            MediaUsage {
                input_tokens: None,
                output_tokens: u.output_tokens,
                total_tokens: u.total_tokens,
                generated_count: u.generated_images.or(Some(images.len() as u32)),
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
    fn conformance_and_watermark_false_by_default() {
        let provider = VolcengineSeedreamProvider::new("", "ark-key");
        assert_image_gen_provider_conformance(&provider);

        let mut req = ImageGenRequest::new("doubao-seedream-4-5-251128", "cyberpunk street");
        req.aspect_ratio = Some("16:9".into());
        let http_req = provider.build_request(&req).unwrap();
        assert_eq!(
            http_req.url,
            "https://ark.cn-beijing.volces.com/api/v3/images/generations"
        );
        match http_req.body {
            Some(RequestBody::Json(body)) => {
                assert_eq!(body["watermark"], json!(false));
                assert_eq!(body["size"], "2560x1440");
                assert_eq!(body["response_format"], "b64_json");
            }
            other => panic!("expected JSON body, got {other:?}"),
        }
    }
}

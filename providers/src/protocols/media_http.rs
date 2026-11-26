//! Shared HTTP, binary download, data-URI, and multipart helpers for official
//! image and video generation protocols.

use std::time::Duration;

use base64::Engine;
use bytes::Bytes;
use meridian_provider_sdk::client::TransportError;
use meridian_provider_sdk::media_gen::{MediaGenError, MediaReference};
use meridian_provider_sdk::wire::{embedded_upstream_error, read_body_capped};

/// Maximum binary payload downloaded from an upstream media URL (matches
/// `meridian-core`'s 200 MiB `MAX_TOOL_MEDIA_BYTES` ceiling).
pub(crate) const MAX_MEDIA_DOWNLOAD_BYTES: usize = 200 * 1024 * 1024;

/// Default per-request HTTP timeout for media generation API calls.
pub(crate) const MEDIA_API_TIMEOUT: Duration = Duration::from_secs(180);

/// Default timeout for downloading a finished generated image or video binary.
pub(crate) const MEDIA_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// Map a transport-layer error into a structured [`MediaGenError`] so the
/// stateless failover router knows whether retrying another deployment is safe.
pub(crate) fn map_transport_error(err: TransportError) -> MediaGenError {
    match err {
        TransportError::Http { status, body, .. } => {
            let detail = body
                .as_deref()
                .and_then(|raw| embedded_upstream_error(raw.as_bytes()))
                .or(body)
                .unwrap_or_else(|| status.canonical_reason().unwrap_or("upstream error").to_string());
            MediaGenError::from_http_status(status.as_u16(), detail)
        }
        TransportError::Timeout => MediaGenError::Retryable("upstream media request timed out".into()),
        TransportError::RetryLimit => MediaGenError::Retryable("upstream media request exhausted retries".into()),
        TransportError::Network(kind) => MediaGenError::Retryable(format!("network error ({kind})")),
        TransportError::Limit(what) => MediaGenError::Terminal(format!("response exceeded limit ({what})")),
        TransportError::Build(msg) => MediaGenError::InvalidRequest(format!("invalid request: {msg}")),
    }
}

/// Encode a [`MediaReference`] as a `data:<mime>;base64,<data>` URI.
pub(crate) fn reference_to_data_uri(reference: &MediaReference) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(&reference.bytes);
    format!("data:{};base64,{encoded}", reference.mime_type)
}

/// Encode a [`MediaReference`] as raw base64 (without the `data:` prefix).
pub(crate) fn reference_to_base64(reference: &MediaReference) -> String {
    base64::engine::general_purpose::STANDARD.encode(&reference.bytes)
}

/// Download a generated binary artifact from `url` with an optional list of
/// headers (e.g. `x-goog-api-key` for Gemini Files/Veo downloads).
pub(crate) async fn download_media_bytes(
    url: &str,
    headers: &[(&str, &str)],
    fallback_mime: &str,
) -> Result<(Vec<u8>, String), MediaGenError> {
    let client = reqwest::Client::builder()
        .timeout(MEDIA_DOWNLOAD_TIMEOUT)
        .build()
        .map_err(|err| MediaGenError::Terminal(format!("failed to initialize media download client: {err}")))?;

    let mut req = client.get(url);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }

    let resp = req
        .send()
        .await
        .map_err(|err| MediaGenError::Retryable(format!("failed to download generated media: {err}")))?;
    let status = resp.status();
    if !status.is_success() {
        let detail = resp
            .text()
            .await
            .unwrap_or_else(|_| status.canonical_reason().unwrap_or("download failed").to_string());
        return Err(MediaGenError::from_http_status(status.as_u16(), detail));
    }

    let mime = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != "application/octet-stream")
        .unwrap_or(fallback_mime)
        .to_string();

    let bytes = read_body_capped(resp, MAX_MEDIA_DOWNLOAD_BYTES, "generated media")
        .await
        .map_err(MediaGenError::Terminal)?;
    Ok((bytes, mime))
}

/// Build an RFC 7578 `multipart/form-data` body without pulling in extra
/// `reqwest` Cargo features. Returns `(content_type_header, body_bytes)`.
pub(crate) struct MultipartFormBuilder {
    boundary: String,
    buf: Vec<u8>,
}

impl MultipartFormBuilder {
    pub(crate) fn new() -> Self {
        Self {
            boundary: format!("----MeridianBoundary{}", uuid::Uuid::new_v4().simple()),
            buf: Vec::new(),
        }
    }

    pub(crate) fn text(mut self, name: &str, value: &str) -> Self {
        self.buf
            .extend_from_slice(format!("--{}\r\n", self.boundary).as_bytes());
        self.buf
            .extend_from_slice(format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes());
        self.buf.extend_from_slice(value.as_bytes());
        self.buf.extend_from_slice(b"\r\n");
        self
    }

    pub(crate) fn file(mut self, name: &str, filename: &str, mime_type: &str, bytes: &[u8]) -> Self {
        self.buf
            .extend_from_slice(format!("--{}\r\n", self.boundary).as_bytes());
        self.buf.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\n").as_bytes(),
        );
        self.buf
            .extend_from_slice(format!("Content-Type: {mime_type}\r\n\r\n").as_bytes());
        self.buf.extend_from_slice(bytes);
        self.buf.extend_from_slice(b"\r\n");
        self
    }

    pub(crate) fn finish(mut self) -> (String, Bytes) {
        self.buf
            .extend_from_slice(format!("--{}--\r\n", self.boundary).as_bytes());
        let content_type = format!("multipart/form-data; boundary={}", self.boundary);
        (content_type, Bytes::from(self.buf))
    }
}

/// Pick a canonical file extension for a media MIME type.
pub(crate) fn extension_for_mime(mime: &str) -> &'static str {
    match mime.to_ascii_lowercase().as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        _ => "bin",
    }
}

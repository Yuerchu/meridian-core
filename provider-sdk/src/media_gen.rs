//! Image and video generation provider contracts (stateless domains).
//!
//! Both `image_gen` and `video_gen` declare [`SwitchImpact::Stateless`] by
//! default so Meridian's multi-provider router may automatically fail over on
//! [`MediaGenError::Retryable`] errors without silently corrupting stateful
//! conversation caches or reasoning signatures.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::decimal::Decimal;
use crate::domain::SwitchImpact;

/// Semantic role of a reference image supplied to an image or video generator.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaReferenceRole {
    /// Generic style/subject reference image (image-to-image, multi-ref video).
    #[default]
    Reference,
    /// First-frame anchor for image-to-video generation.
    FirstFrame,
    /// Last-frame anchor for first+last-frame video interpolation.
    LastFrame,
}

/// Binary reference media attached to an image or video generation request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaReference {
    pub mime_type: String,
    pub bytes: Vec<u8>,
    pub role: MediaReferenceRole,
}

/// Optional normalized usage & billing telemetry returned by an upstream media API.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaUsage {
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub generated_count: Option<u32>,
    pub duration_seconds: Option<u32>,
    pub cost_usd: Option<Decimal>,
}

/// One generated binary media artifact returned from an upstream provider.
#[derive(Debug, Clone, PartialEq)]
pub struct GeneratedMedia {
    pub mime_type: String,
    pub bytes: Vec<u8>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_seconds: Option<f64>,
    pub revised_prompt: Option<String>,
}

/// Request payload for synchronous image generation (`image_gen` domain).
#[derive(Debug, Clone)]
pub struct ImageGenRequest {
    pub model: String,
    pub prompt: String,
    pub reference_images: Vec<MediaReference>,
    pub aspect_ratio: Option<String>,
    pub size: Option<String>,
    pub quality: Option<String>,
    pub count: Option<u32>,
    pub seed: Option<i64>,
    /// Upstream watermark request flag. Defaults to `false` so `meridian-core`
    /// always receives clean originals and owns compliance watermarking.
    pub watermark: bool,
}

impl ImageGenRequest {
    pub fn new(model: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            prompt: prompt.into(),
            reference_images: Vec::new(),
            aspect_ratio: None,
            size: None,
            quality: None,
            count: None,
            seed: None,
            watermark: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImageGenResponse {
    pub images: Vec<GeneratedMedia>,
    pub usage: Option<MediaUsage>,
}

/// Request payload for asynchronous video generation (`video_gen` domain).
#[derive(Debug, Clone)]
pub struct VideoGenRequest {
    pub model: String,
    pub prompt: String,
    pub reference_images: Vec<MediaReference>,
    pub aspect_ratio: Option<String>,
    pub resolution: Option<String>,
    pub duration_seconds: Option<u32>,
    pub seed: Option<i64>,
    pub generate_audio: Option<bool>,
    /// Upstream watermark request flag. Defaults to `false` so `meridian-core`
    /// always receives clean originals and owns compliance watermarking.
    pub watermark: bool,
}

impl VideoGenRequest {
    pub fn new(model: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            prompt: prompt.into(),
            reference_images: Vec::new(),
            aspect_ratio: None,
            resolution: None,
            duration_seconds: None,
            seed: None,
            generate_audio: None,
            watermark: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VideoGenResponse {
    pub videos: Vec<GeneratedMedia>,
    pub usage: Option<MediaUsage>,
}

/// Opaque upstream job handle returned by [`VideoGenProvider::submit_video_job`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaJobHandle {
    pub job_id: String,
    pub model: String,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// Poll result returned by [`VideoGenProvider::poll_video_job`].
#[derive(Debug, Clone)]
pub enum MediaJobStatus {
    Queued,
    Running {
        progress_pct: Option<u8>,
    },
    Succeeded {
        media: Vec<GeneratedMedia>,
        usage: Option<MediaUsage>,
    },
    Failed {
        message: String,
        retryable: bool,
    },
    Cancelled,
}

/// Structured error surface for image & video generation providers.
///
/// The router uses [`MediaGenError::allows_failover`] to decide whether a
/// failure may trigger automatic priority failover across stateless
/// deployments.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MediaGenError {
    #[error("retryable media generation error: {0}")]
    Retryable(String),
    #[error("content filtered by upstream safety policy: {0}")]
    ContentFiltered(String),
    #[error("invalid media generation request: {0}")]
    InvalidRequest(String),
    #[error("media generation cancelled")]
    Cancelled,
    #[error("media generation failed: {0}")]
    Terminal(String),
}

impl MediaGenError {
    /// Only transient / upstream availability failures may trigger automatic
    /// failover. Parameter errors, safety rejections, and cancellations never
    /// silently jump to another vendor.
    pub const fn allows_failover(&self) -> bool {
        matches!(self, Self::Retryable(_))
    }

    /// Classify an HTTP status + upstream message into a [`MediaGenError`].
    pub fn from_http_status(status: u16, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        let lower = detail.to_ascii_lowercase();
        if lower.contains("content_policy")
            || lower.contains("safety")
            || lower.contains("moderation")
            || lower.contains("blocked")
            || lower.contains("sensitive")
            || lower.contains("inappropriate")
        {
            return Self::ContentFiltered(detail);
        }
        match status {
            408 | 429 | 500..=599 => Self::Retryable(format!("HTTP {status}: {detail}")),
            400 | 422 => Self::InvalidRequest(format!("HTTP {status}: {detail}")),
            401..=403 => Self::Retryable(format!("HTTP {status}: {detail}")),
            _ => Self::Terminal(format!("HTTP {status}: {detail}")),
        }
    }
}

#[async_trait]
pub trait ImageGenProvider: Send + Sync {
    fn provider_id(&self) -> &'static str;

    fn switch_impact(&self) -> SwitchImpact {
        SwitchImpact::Stateless
    }

    async fn generate_image(&self, request: &ImageGenRequest) -> Result<ImageGenResponse, MediaGenError>;
}

#[async_trait]
pub trait VideoGenProvider: Send + Sync {
    fn provider_id(&self) -> &'static str;

    fn switch_impact(&self) -> SwitchImpact {
        SwitchImpact::Stateless
    }

    async fn submit_video_job(&self, request: &VideoGenRequest) -> Result<MediaJobHandle, MediaGenError>;

    async fn poll_video_job(&self, handle: &MediaJobHandle) -> Result<MediaJobStatus, MediaGenError>;

    async fn cancel_video_job(&self, _handle: &MediaJobHandle) -> Result<(), MediaGenError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failover_eligibility_only_permits_retryable_errors() {
        assert!(MediaGenError::Retryable("503".into()).allows_failover());
        assert!(!MediaGenError::ContentFiltered("safety".into()).allows_failover());
        assert!(!MediaGenError::InvalidRequest("bad ratio".into()).allows_failover());
        assert!(!MediaGenError::Cancelled.allows_failover());
        assert!(!MediaGenError::Terminal("fatal".into()).allows_failover());
    }

    #[test]
    fn http_status_classification_separates_safety_invalid_and_retryable() {
        assert!(matches!(
            MediaGenError::from_http_status(400, "rejected by safety policy"),
            MediaGenError::ContentFiltered(_)
        ));
        assert!(matches!(
            MediaGenError::from_http_status(400, "unsupported aspect ratio"),
            MediaGenError::InvalidRequest(_)
        ));
        assert!(matches!(
            MediaGenError::from_http_status(429, "rate limit exceeded"),
            MediaGenError::Retryable(_)
        ));
        assert!(matches!(
            MediaGenError::from_http_status(503, "service unavailable"),
            MediaGenError::Retryable(_)
        ));
    }

    #[test]
    fn requests_default_watermark_to_false_for_clean_originals() {
        assert!(!ImageGenRequest::new("gpt-image-1", "a cat").watermark);
        assert!(!VideoGenRequest::new("veo-3.1-generate-preview", "ocean waves").watermark);
    }
}

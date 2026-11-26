//! AIGC Media Compliance Port (`meridian-core` boundary).
//!
//! Aligned with standing architecture decision `D-001`:
//! - Upstream providers are always requested with `watermark: false` so Meridian
//!   persists clean original generated assets in managed storage, strictly
//!   separated from user uploads.
//! - All compliance watermarking (visible marks, invisible/metadata watermarks,
//!   and export packaging) belongs behind [`MediaCompliancePort`] in
//!   `meridian-core` — NEVER in frontend CSS overlays or browser `<canvas>` /
//!   `ffmpeg.wasm` code.
//! - Phase 1 ships [`CleanOriginalCompliancePort`], recording provenance
//!   (`produce_id`, `is_ai_generated: true`, `watermark_snapshot`) while
//!   reserving the exact hook points for pixel-level watermarking in Phase 2.

use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Provenance and compliance metadata attached to every stored AI-generated
/// image or video original.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratedAssetMeta {
    /// Stable per-asset provenance identifier (for future blind-watermark /
    /// C2PA / regulatory tracing).
    pub produce_id: String,
    /// Always `true` for assets produced by `MediaRunner`.
    pub is_ai_generated: bool,
    /// Snapshot of compliance watermark applied at generation time, if any
    /// (`None` for Phase 1 clean originals).
    pub watermark_snapshot: Option<String>,
    pub provider_id: String,
    pub model_id: String,
    pub created_at: i64,
}

/// Policy governing how an asset is packaged when exported outside Meridian.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportWatermarkMode {
    /// Deliver the clean original bytes as stored (Phase 1 default).
    #[default]
    CleanOriginal,
    /// Reserved for future core-side visible + blind watermarking.
    VisibleAndBlind,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportWatermarkPolicy {
    pub mode: ExportWatermarkMode,
}

/// Core compliance boundary for every AI-generated image or video asset.
#[async_trait]
pub trait MediaCompliancePort: Send + Sync {
    /// Invoked immediately after an upstream generated binary is written as a
    /// clean original on disk. May enrich or persist provenance metadata.
    async fn on_generated_original(
        &self,
        asset_path: &Path,
        meta: &GeneratedAssetMeta,
    ) -> Result<GeneratedAssetMeta, String>;

    /// Invoked whenever a user exports or downloads a generated asset out of
    /// Meridian's managed storage.
    async fn prepare_export(
        &self,
        original_path: &Path,
        meta: &GeneratedAssetMeta,
        policy: ExportWatermarkPolicy,
    ) -> Result<Vec<u8>, String>;
}

/// Phase 1 compliance port: preserves clean originals on disk, stamps
/// provenance metadata (`produce_id`, `is_ai_generated: true`), and routes all
/// exports through the core Port boundary.
#[derive(Debug, Default)]
pub struct CleanOriginalCompliancePort;

#[async_trait]
impl MediaCompliancePort for CleanOriginalCompliancePort {
    async fn on_generated_original(
        &self,
        asset_path: &Path,
        meta: &GeneratedAssetMeta,
    ) -> Result<GeneratedAssetMeta, String> {
        if !asset_path.is_file() {
            return Err(format!(
                "generated original missing at compliance hook: {}",
                asset_path.display()
            ));
        }
        let mut stamped = meta.clone();
        stamped.is_ai_generated = true;
        Ok(stamped)
    }

    async fn prepare_export(
        &self,
        original_path: &Path,
        _meta: &GeneratedAssetMeta,
        _policy: ExportWatermarkPolicy,
    ) -> Result<Vec<u8>, String> {
        tokio::fs::read(original_path)
            .await
            .map_err(|err| format!("failed to read generated asset for export: {err}"))
    }
}

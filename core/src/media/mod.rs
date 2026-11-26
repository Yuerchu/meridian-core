//! Multi-domain media generation (`image_gen` & `video_gen`) core services:
//! clean-original managed storage, [`MediaCompliancePort`] (D-001 boundary),
//! and [`MediaRunner`] with `SwitchImpact::Stateless` multi-provider failover.

pub mod compliance;
pub mod runner;
pub mod storage;

pub use compliance::{
    CleanOriginalCompliancePort, ExportWatermarkMode, ExportWatermarkPolicy, GeneratedAssetMeta, MediaCompliancePort,
};
pub use meridian_provider_sdk::media_gen::{MediaReference, MediaReferenceRole};
pub use runner::{
    DEFAULT_IMAGE_MODEL_PREF, DEFAULT_VIDEO_MODEL_PREF, MediaGenerationJobSpec, MediaGenerationParams,
    MediaGenerationRecord, MediaRunner, StoredMediaAsset,
};
pub use storage::MediaAssetStore;

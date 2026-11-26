//! Reserved image and video generation provider contracts (stateless domains).

use async_trait::async_trait;

use crate::domain::SwitchImpact;

#[derive(Debug, Clone)]
pub struct ImageGenRequest {
    pub model: String,
    pub prompt: String,
    pub reference_images: Vec<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct ImageGenResponse {
    pub images: Vec<GeneratedMedia>,
}

#[derive(Debug, Clone)]
pub struct VideoGenRequest {
    pub model: String,
    pub prompt: String,
    pub reference_images: Vec<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct VideoGenResponse {
    pub videos: Vec<GeneratedMedia>,
}

#[derive(Debug, Clone)]
pub struct GeneratedMedia {
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

#[async_trait]
pub trait ImageGenProvider: Send + Sync {
    fn provider_id(&self) -> &'static str;

    fn switch_impact(&self) -> SwitchImpact {
        SwitchImpact::Stateless
    }

    async fn generate_image(&self, request: &ImageGenRequest) -> Result<ImageGenResponse, String>;
}

#[async_trait]
pub trait VideoGenProvider: Send + Sync {
    fn provider_id(&self) -> &'static str;

    fn switch_impact(&self) -> SwitchImpact {
        SwitchImpact::Stateless
    }

    async fn generate_video(&self, request: &VideoGenRequest) -> Result<VideoGenResponse, String>;
}

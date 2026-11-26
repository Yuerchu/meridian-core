//! Vector embedding provider contract (stateless capability domain).

use async_trait::async_trait;

use crate::domain::SwitchImpact;

#[derive(Debug, Clone)]
pub enum EmbeddingInput {
    Text(String),
    TextAndImage { text: String, image_bytes: Vec<u8> },
}

#[derive(Debug, Clone)]
pub struct EmbeddingRequest {
    pub model: String,
    pub inputs: Vec<EmbeddingInput>,
}

#[derive(Debug, Clone)]
pub struct EmbeddingResponse {
    pub vectors: Vec<Vec<f32>>,
}

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn provider_id(&self) -> &'static str;

    fn switch_impact(&self) -> SwitchImpact {
        SwitchImpact::Stateless
    }

    async fn embed(&self, request: &EmbeddingRequest) -> Result<EmbeddingResponse, String>;
}

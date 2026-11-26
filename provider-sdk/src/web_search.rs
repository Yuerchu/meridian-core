//! Web search provider contract (stateless capability domain).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::domain::SwitchImpact;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchSource {
    pub title: String,
    pub url: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub favicon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchRequest {
    pub query: String,
    pub max_results: usize,
    pub time_range: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchResponse {
    pub sources: Vec<SearchSource>,
}

pub fn extract_domain(url: &str) -> Option<String> {
    url.split("//")
        .nth(1)
        .and_then(|s| s.split('/').next())
        .map(|s| s.strip_prefix("www.").unwrap_or(s).to_string())
}

#[async_trait]
pub trait WebSearchProvider: Send + Sync {
    fn provider_id(&self) -> &'static str;

    fn switch_impact(&self) -> SwitchImpact {
        SwitchImpact::Stateless
    }

    async fn search(&self, request: &WebSearchRequest) -> Result<WebSearchResponse, String>;
}

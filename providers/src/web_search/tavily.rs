use async_trait::async_trait;
use serde::Deserialize;

use meridian_provider_sdk::web_search::{
    SearchSource, WebSearchProvider, WebSearchRequest, WebSearchResponse, extract_domain,
};
use meridian_provider_sdk::wire::read_body_capped;

const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const ENDPOINT: &str = "https://api.tavily.com/search";

#[derive(Deserialize)]
struct TavilyResponse {
    results: Vec<TavilyResult>,
}

#[derive(Deserialize)]
struct TavilyResult {
    title: String,
    url: String,
    content: String,
    #[serde(default)]
    favicon: Option<String>,
}

pub struct TavilyWebSearchProvider {
    client: reqwest::Client,
    api_key: String,
    endpoint: String,
}

impl TavilyWebSearchProvider {
    pub fn new(client: reqwest::Client, api_key: impl Into<String>) -> Self {
        Self {
            client,
            api_key: api_key.into(),
            endpoint: ENDPOINT.to_owned(),
        }
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }
}

#[async_trait]
impl WebSearchProvider for TavilyWebSearchProvider {
    fn provider_id(&self) -> &'static str {
        "tavily"
    }

    async fn search(&self, request: &WebSearchRequest) -> Result<WebSearchResponse, String> {
        let mut body = serde_json::json!({
            "query": request.query,
            "max_results": request.max_results,
            "include_answer": false,
            "include_raw_content": false,
            "include_favicon": true,
        });
        if let Some(tr) = request.time_range.as_deref() {
            body["time_range"] = serde_json::Value::String(tr.to_string());
        }

        let resp = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Tavily request failed: {e}"))?;

        let status = resp.status();
        let body = read_body_capped(resp, MAX_RESPONSE_BYTES, "the search response").await?;
        if !status.is_success() {
            let text = String::from_utf8_lossy(&body);
            return Err(format!("Tavily API error {status}: {text}"));
        }

        let data: TavilyResponse =
            serde_json::from_slice(&body).map_err(|e| format!("failed to parse Tavily response: {e}"))?;

        let sources = data
            .results
            .into_iter()
            .map(|r| {
                let site_name = extract_domain(&r.url);
                SearchSource {
                    title: r.title,
                    url: r.url,
                    content: r.content,
                    favicon: r.favicon,
                    site_name,
                }
            })
            .collect();

        Ok(WebSearchResponse { sources })
    }
}

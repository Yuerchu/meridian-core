use async_trait::async_trait;
use serde::Deserialize;

use meridian_provider_sdk::web_search::{
    SearchSource, WebSearchProvider, WebSearchRequest, WebSearchResponse, extract_domain,
};
use meridian_provider_sdk::wire::read_body_capped;

const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const ENDPOINT: &str = "https://open.bigmodel.cn/api/paas/v4/web_search";

#[derive(Deserialize)]
struct ZhipuResponse {
    #[serde(default)]
    search_result: Vec<ZhipuResult>,
}

#[derive(Deserialize)]
struct ZhipuResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    link: String,
    #[serde(default)]
    media: Option<String>,
    #[serde(default)]
    icon: Option<String>,
}

fn map_time_range_zhipu(time_range: Option<&str>) -> &'static str {
    match time_range {
        Some("day") => "oneDay",
        Some("week") => "oneWeek",
        Some("month") => "oneMonth",
        Some("year") => "oneYear",
        _ => "noLimit",
    }
}

pub struct ZhipuWebSearchProvider {
    client: reqwest::Client,
    api_key: String,
    endpoint: String,
}

impl ZhipuWebSearchProvider {
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
impl WebSearchProvider for ZhipuWebSearchProvider {
    fn provider_id(&self) -> &'static str {
        "zhipu"
    }

    async fn search(&self, request: &WebSearchRequest) -> Result<WebSearchResponse, String> {
        let count = request.max_results.min(50);
        let body = serde_json::json!({
            "search_query": request.query,
            "search_engine": "search_std",
            "search_intent": false,
            "count": count,
            "search_recency_filter": map_time_range_zhipu(request.time_range.as_deref()),
        });

        let resp = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Zhipu request failed: {e}"))?;

        let status = resp.status();
        let body = read_body_capped(resp, MAX_RESPONSE_BYTES, "the search response").await?;
        if !status.is_success() {
            let text = String::from_utf8_lossy(&body);
            return Err(format!("Zhipu API error {status}: {text}"));
        }

        let data: ZhipuResponse =
            serde_json::from_slice(&body).map_err(|e| format!("failed to parse Zhipu response: {e}"))?;

        let sources = data
            .search_result
            .into_iter()
            .map(|r| {
                let site_name = r.media.or_else(|| extract_domain(&r.link));
                SearchSource {
                    title: r.title,
                    url: r.link,
                    content: r.content,
                    favicon: r.icon,
                    site_name,
                }
            })
            .collect();

        Ok(WebSearchResponse { sources })
    }
}

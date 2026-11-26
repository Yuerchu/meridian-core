//! Brave Search API (`GET https://api.search.brave.com/res/v1/web/search`)
//! web-search provider implementation.

use async_trait::async_trait;
use serde::Deserialize;

use meridian_provider_sdk::client::{HttpTransport, Request, ReqwestTransport};
use meridian_provider_sdk::web_search::{
    SearchSource, WebSearchProvider, WebSearchRequest, WebSearchResponse, extract_domain,
};
use meridian_provider_sdk::wire::auth_header_value;

const BRAVE_SEARCH_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";

pub struct BraveWebSearchProvider {
    endpoint: String,
    api_key: String,
}

impl BraveWebSearchProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_endpoint(BRAVE_SEARCH_ENDPOINT, api_key)
    }

    pub fn with_endpoint(endpoint: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            api_key: api_key.into(),
        }
    }

    pub fn build_http_request(&self, request: &WebSearchRequest) -> Request {
        let count = request.max_results.clamp(1, 20);
        let encoded_query = percent_encoding::utf8_percent_encode(&request.query, percent_encoding::NON_ALPHANUMERIC);
        let mut url = format!("{}?q={encoded_query}&count={count}", self.endpoint);
        if let Some(ref freshness) = request.time_range {
            let mapped = match freshness.as_str() {
                "day" => Some("pd"),
                "week" => Some("pw"),
                "month" => Some("pm"),
                "year" => Some("py"),
                _ => None,
            };
            if let Some(f) = mapped {
                url.push_str(&format!("&freshness={f}"));
            }
        }
        let mut req = Request::new(http::Method::GET, url);
        req.headers
            .insert("X-Subscription-Token", auth_header_value(&self.api_key));
        req.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json"),
        );
        req
    }

    pub fn parse_response(raw: &[u8]) -> Result<WebSearchResponse, String> {
        let parsed: BraveSearchApiResponse =
            serde_json::from_slice(raw).map_err(|e| format!("failed to parse Brave Search response: {e}"))?;
        let sources = parsed
            .web
            .map(|web| {
                web.results
                    .into_iter()
                    .map(|item| {
                        let site_name = extract_domain(&item.url);
                        SearchSource {
                            title: item.title,
                            url: item.url,
                            content: item.description.unwrap_or_default(),
                            favicon: None,
                            site_name,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(WebSearchResponse { sources })
    }
}

#[derive(Deserialize)]
struct BraveSearchApiResponse {
    web: Option<BraveWebResults>,
}

#[derive(Deserialize)]
struct BraveWebResults {
    #[serde(default)]
    results: Vec<BraveWebResultItem>,
}

#[derive(Deserialize)]
struct BraveWebResultItem {
    title: String,
    url: String,
    description: Option<String>,
}

#[async_trait]
impl WebSearchProvider for BraveWebSearchProvider {
    fn provider_id(&self) -> &'static str {
        "brave"
    }

    async fn search(&self, request: &WebSearchRequest) -> Result<WebSearchResponse, String> {
        let req = self.build_http_request(request);
        let resp = ReqwestTransport::shared()
            .execute(req)
            .await
            .map_err(|e| e.to_string())?;
        Self::parse_response(&resp.body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_brave_get_request_with_subscription_token_header() {
        let provider = BraveWebSearchProvider::new("BSA-test-token");
        let req = provider.build_http_request(&WebSearchRequest {
            query: "rust sea-orm".into(),
            max_results: 5,
            time_range: Some("week".into()),
        });
        assert_eq!(req.method, http::Method::GET);
        assert!(req.url.contains("q=rust%20sea%2Dorm&count=5&freshness=pw"), "url={}", req.url);
        assert_eq!(req.headers.get("X-Subscription-Token").unwrap(), "BSA-test-token");
    }

    #[test]
    fn parses_brave_web_results_into_unified_response() {
        let raw = br#"{
            "web": {
                "results": [
                    {
                        "title": "Meridian",
                        "url": "https://example.com/meridian",
                        "description": "Local-first AI agent workspace"
                    }
                ]
            }
        }"#;
        let response = BraveWebSearchProvider::parse_response(raw).unwrap();
        assert_eq!(response.sources.len(), 1);
        assert_eq!(response.sources[0].title, "Meridian");
        assert_eq!(response.sources[0].content, "Local-first AI agent workspace");
        assert_eq!(response.sources[0].site_name.as_deref(), Some("example.com"));
    }
}

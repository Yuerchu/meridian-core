use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{Permission, Tool, ToolContext};

const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

pub struct WebSearchTool {
    client: reqwest::Client,
}

impl WebSearchTool {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
        }
    }
}

/// Read a response body with a hard size cap so a misbehaving endpoint can't
/// buffer unbounded data into memory.
async fn read_body_capped(resp: reqwest::Response) -> Result<Vec<u8>, String> {
    crate::util::read_body_capped(resp, MAX_RESPONSE_BYTES, "the search response").await
}

#[derive(Debug, Serialize, Deserialize)]
struct SearchSource {
    title: String,
    url: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    favicon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    site_name: Option<String>,
}

#[derive(Debug, Serialize)]
struct WebSearchOutput {
    llm_text: String,
    sources: Vec<SearchSource>,
}

fn extract_domain(url: &str) -> Option<String> {
    url.split("//")
        .nth(1)
        .and_then(|s| s.split('/').next())
        .map(|s| s.strip_prefix("www.").unwrap_or(s).to_string())
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web for current information. Returns relevant web pages with titles, URLs, and content snippets. Use this when you need up-to-date information that may not be in your training data."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query"
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum number of results to return (default: 5)"
                },
                "time_range": {
                    "type": "string",
                    "description": "Filter results by time range",
                    "enum": ["day", "week", "month", "year"]
                }
            },
            "required": ["query"]
        })
    }

    // Queries are model-generated and leave the machine — require confirmation.
    fn default_permission(&self) -> Permission {
        Permission::Ask
    }

    async fn execute(&self, args: serde_json::Value, context: &ToolContext) -> Result<String, String> {
        let query = args["query"].as_str().ok_or("missing 'query' argument")?.to_string();
        let max_results = args["max_results"].as_u64().unwrap_or(5).min(20) as usize;
        let time_range = args["time_range"].as_str().map(|s| s.to_string());

        let provider = context
            .tool_secrets
            .get("SEARCH_PROVIDER")
            .map(|s| s.as_str())
            .unwrap_or("tavily");

        let sources = match provider {
            "zhipu" => {
                let api_key = context
                    .tool_secrets
                    .get("SERVICE_ZHIPU_SEARCH_KEY")
                    .ok_or("Zhipu Web Search API key not configured. Please set it in Settings > General.")?;
                search_zhipu(&self.client, api_key, &query, max_results, time_range.as_deref()).await?
            }
            _ => {
                let api_key = context
                    .tool_secrets
                    .get("SERVICE_TAVILY_KEY")
                    .ok_or("Tavily API key not configured. Please set it in Settings > General.")?;
                search_tavily(&self.client, api_key, &query, max_results, time_range.as_deref()).await?
            }
        };

        let mut llm_text = String::new();
        for (i, src) in sources.iter().enumerate() {
            llm_text.push_str(&format!(
                "[{}] {}\nURL: {}\n{}\n\n",
                i + 1,
                src.title,
                src.url,
                src.content
            ));
        }
        if sources.is_empty() {
            llm_text.push_str("No results found.");
        }

        let output = WebSearchOutput { llm_text, sources };
        serde_json::to_string(&output).map_err(|e| format!("failed to serialize results: {e}"))
    }
}

// --- Tavily ---

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

async fn search_tavily(
    client: &reqwest::Client,
    api_key: &str,
    query: &str,
    max_results: usize,
    time_range: Option<&str>,
) -> Result<Vec<SearchSource>, String> {
    let mut body = serde_json::json!({
        "query": query,
        "max_results": max_results,
        "include_answer": false,
        "include_raw_content": false,
        "include_favicon": true,
    });
    if let Some(tr) = time_range {
        body["time_range"] = serde_json::Value::String(tr.to_string());
    }

    let resp = client
        .post("https://api.tavily.com/search")
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Tavily request failed: {e}"))?;

    let status = resp.status();
    let body = read_body_capped(resp).await?;
    if !status.is_success() {
        let text = String::from_utf8_lossy(&body);
        return Err(format!("Tavily API error {status}: {text}"));
    }

    let data: TavilyResponse =
        serde_json::from_slice(&body).map_err(|e| format!("failed to parse Tavily response: {e}"))?;

    Ok(data
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
        .collect())
}

// --- Zhipu Web Search ---

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

fn map_time_range_zhipu(time_range: Option<&str>) -> &str {
    match time_range {
        Some("day") => "oneDay",
        Some("week") => "oneWeek",
        Some("month") => "oneMonth",
        Some("year") => "oneYear",
        _ => "noLimit",
    }
}

async fn search_zhipu(
    client: &reqwest::Client,
    api_key: &str,
    query: &str,
    max_results: usize,
    time_range: Option<&str>,
) -> Result<Vec<SearchSource>, String> {
    let count = max_results.min(50);
    let body = serde_json::json!({
        "search_query": query,
        "search_engine": "search_std",
        "search_intent": false,
        "count": count,
        "search_recency_filter": map_time_range_zhipu(time_range),
    });

    let resp = client
        .post("https://open.bigmodel.cn/api/paas/v4/web_search")
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Zhipu request failed: {e}"))?;

    let status = resp.status();
    let body = read_body_capped(resp).await?;
    if !status.is_success() {
        let text = String::from_utf8_lossy(&body);
        return Err(format!("Zhipu API error {status}: {text}"));
    }

    let data: ZhipuResponse =
        serde_json::from_slice(&body).map_err(|e| format!("failed to parse Zhipu response: {e}"))?;

    Ok(data
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
        .collect())
}

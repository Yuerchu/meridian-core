use async_trait::async_trait;
use meridian_provider_sdk::web_search::{SearchSource, WebSearchProvider, WebSearchRequest};
use meridian_providers::web_search::{TavilyWebSearchProvider, ZhipuWebSearchProvider};
use serde::Serialize;

use super::{Permission, Tool, ToolContext};

const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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

#[derive(Debug, Serialize)]
struct WebSearchOutput {
    llm_text: String,
    sources: Vec<SearchSource>,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn spec(&self) -> crate::tools::spec::ToolSpec {
        crate::tools::spec::ToolSpec {
            effect: crate::tools::spec::Effect::Network,
            loop_handled: false,
            plan_mode: true,
            explore: true,
            reviewer: false,
            parallel: true,
        }
    }

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
        // domain-default: how many results a search returns unless the model asks is this app's own page size, not a fact about the provider
        let max_results = args["max_results"].as_u64().unwrap_or(5).min(20) as usize;
        let time_range = args["time_range"].as_str().map(|s| s.to_string());

        let provider = context
            .tool_secrets
            .get("SEARCH_PROVIDER")
            .map(|s| s.as_str())
            .unwrap_or("tavily");

        let req = WebSearchRequest {
            query,
            max_results,
            time_range,
        };

        let response = match provider {
            "zhipu" => {
                let api_key = context
                    .tool_secrets
                    .get("SERVICE_ZHIPU_SEARCH_KEY")
                    .ok_or("Zhipu Web Search API key not configured. Please set it in Settings > General.")?;
                ZhipuWebSearchProvider::new(self.client.clone(), api_key)
                    .search(&req)
                    .await?
            }
            _ => {
                let api_key = context
                    .tool_secrets
                    .get("SERVICE_TAVILY_KEY")
                    .ok_or("Tavily API key not configured. Please set it in Settings > General.")?;
                TavilyWebSearchProvider::new(self.client.clone(), api_key)
                    .search(&req)
                    .await?
            }
        };
        let sources = response.sources;

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

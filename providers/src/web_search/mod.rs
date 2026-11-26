//! Built-in web search provider implementations & registry.

pub mod brave;
pub mod tavily;
pub mod zhipu;

pub use brave::BraveWebSearchProvider;
pub use tavily::TavilyWebSearchProvider;
pub use zhipu::ZhipuWebSearchProvider;

use meridian_provider_sdk::web_search::WebSearchProvider;

/// Instantiate a built-in web search provider by vendor id.
pub fn create_web_search_provider(
    vendor_id: &str,
    client: reqwest::Client,
    api_key: &str,
) -> Result<Box<dyn WebSearchProvider>, String> {
    match vendor_id {
        "tavily" => Ok(Box::new(TavilyWebSearchProvider::new(client, api_key))),
        "zhipu" => Ok(Box::new(ZhipuWebSearchProvider::new(client, api_key))),
        "brave" => Ok(Box::new(BraveWebSearchProvider::new(api_key))),
        other => Err(format!("unknown web search vendor `{other}`")),
    }
}

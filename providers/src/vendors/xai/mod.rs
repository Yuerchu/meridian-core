//! xAI Chat Completions extension over [`OpenAIChatProvider`].
//!
//! Adds xAI's `x-grok-conv-id` prompt-cache routing header without duplicating
//! any OpenAI Chat Completions request/streaming code.

use std::sync::Arc;

use serde_json::{Map, Value};

use crate::protocols::openai_chat::{OpenAIChatExtension, OpenAIChatProvider, apply_standard_openai_chat_params};
use crate::{ChatParams, ProviderError};

#[derive(Debug, Default)]
pub struct XaiOpenAIChatExtension;

impl OpenAIChatExtension for XaiOpenAIChatExtension {
    fn adapter_name(&self) -> &'static str {
        "XaiOpenAIChatProvider"
    }

    fn customize_request(
        &self,
        base_url: &str,
        params: &ChatParams,
        body: &mut Map<String, Value>,
        headers: &mut http::HeaderMap,
    ) -> Result<(), ProviderError> {
        apply_standard_openai_chat_params(base_url, params, body, false);
        if let Some(key) = params.cache_key.as_deref() {
            match http::HeaderValue::from_str(key) {
                Ok(value) => {
                    headers.insert("x-grok-conv-id", value);
                }
                Err(_) => tracing::warn!(
                    key_chars = key.chars().count(),
                    "the cache key is not representable as a header; this turn will not route to a warm cache"
                ),
            }
        }
        Ok(())
    }
}

/// Concrete xAI Chat Completions provider composed over [`OpenAIChatProvider`].
pub struct XaiOpenAIChatProvider;

impl XaiOpenAIChatProvider {
    pub fn new(base_url: &str, api_key: &str) -> OpenAIChatProvider {
        OpenAIChatProvider::with_extension(base_url, api_key, Arc::new(XaiOpenAIChatExtension))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChatMessage;
    use crate::protocols::openai_chat::{ChatChunk, ChunkUsage, normalise_openai_usage, parse_openai_sse_events};
    use meridian_provider_sdk::client::RequestBody;

    fn params() -> ChatParams {
        ChatParams {
            model: "grok-4.6".into(),
            thinking_enabled: true,
            thinking_effort: Some("xhigh".into()),
            temperature: Some(0.7),
            cache_key: Some("conv-42".into()),
            ..Default::default()
        }
    }

    #[test]
    fn the_cache_key_becomes_the_routing_header() {
        let provider = XaiOpenAIChatProvider::new("https://api.x.ai/v1", "key");
        let req = provider
            .build_request(&[ChatMessage::user("hello")], None, &params(), true)
            .unwrap();
        assert_eq!(req.headers.get("x-grok-conv-id").unwrap(), "conv-42");

        let Some(RequestBody::Json(body)) = req.body else {
            panic!("JSON body")
        };
        assert_eq!(body["reasoning_effort"], "xhigh");
        assert_eq!(body["temperature"], 0.7);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn no_cache_key_means_no_header_rather_than_an_empty_one() {
        let provider = XaiOpenAIChatProvider::new("https://api.x.ai/v1", "key");
        let mut p = params();
        p.cache_key = None;
        let req = provider
            .build_request(&[ChatMessage::user("hello")], None, &p, true)
            .unwrap();
        assert!(req.headers.get("x-grok-conv-id").is_none());
    }

    #[test]
    fn a_generic_endpoint_is_not_sent_the_vendor_header() {
        let provider = OpenAIChatProvider::new("https://api.example.test/v1", "key");
        let req = provider
            .build_request(&[ChatMessage::user("hello")], None, &params(), true)
            .unwrap();
        assert!(req.headers.get("x-grok-conv-id").is_none());
    }

    fn usage(json: &str) -> ChunkUsage {
        serde_json::from_str(json).expect("a chat-completions usage body")
    }

    #[test]
    fn grok_reasoning_tokens_are_counted_as_output() {
        let u = normalise_openai_usage(&usage(
            r#"{"prompt_tokens":214,"completion_tokens":1,"total_tokens":274,
                "prompt_tokens_details":{"cached_tokens":128},
                "completion_tokens_details":{"reasoning_tokens":59}}"#,
        ));
        assert_eq!(u.completion_tokens, Some(60));
        assert_eq!(u.prompt_tokens, Some(214), "the prompt is untouched");
        assert_eq!(u.cache_read_tokens, Some(128));
        assert_eq!(u.uncached_prompt_tokens(), Some(86));
    }

    #[test]
    fn an_inclusive_dialect_is_left_alone() {
        let u = normalise_openai_usage(&usage(
            r#"{"prompt_tokens":100,"completion_tokens":80,"total_tokens":180,
                "completion_tokens_details":{"reasoning_tokens":64}}"#,
        ));
        assert_eq!(u.completion_tokens, Some(80));
    }

    #[test]
    fn an_ambiguous_usage_block_is_reported_as_given() {
        let no_total = normalise_openai_usage(&usage(
            r#"{"prompt_tokens":100,"completion_tokens":10,
                "completion_tokens_details":{"reasoning_tokens":50}}"#,
        ));
        assert_eq!(no_total.completion_tokens, Some(10));

        let odd = normalise_openai_usage(&usage(
            r#"{"prompt_tokens":100,"completion_tokens":10,"total_tokens":200,
                "completion_tokens_details":{"reasoning_tokens":50}}"#,
        ));
        assert_eq!(odd.completion_tokens, Some(10));
    }

    #[test]
    fn the_trailing_usage_chunk_parses_with_empty_choices() {
        let chunk: ChatChunk = serde_json::from_str(
            r#"{"choices":[],"usage":{"prompt_tokens":292,"completion_tokens":13,"total_tokens":366,
                "prompt_tokens_details":{"cached_tokens":256},
                "completion_tokens_details":{"reasoning_tokens":61}}}"#,
        )
        .expect("the closing chunk of a real xAI stream");
        let (events, finish, usage) = parse_openai_sse_events(&chunk);
        assert!(events.is_empty());
        assert_eq!(finish, None);
        assert_eq!(usage.expect("usage").completion_tokens, Some(74));
    }
}

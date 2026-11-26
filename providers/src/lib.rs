//! Built-in protocol implementations, vendor presets, model catalogs, and
//! web-search / TTS providers for Meridian.

pub mod balance;
pub mod capabilities;
pub mod catalog;
pub mod local_model;
pub mod mixins;
pub mod models;
pub mod protocols;
pub mod registry;
pub mod tts;
pub mod vendors;
pub mod web_search;

// Backwards-compatible module aliases for callers migrating from the flat
// Phase 0 layout into `protocols/` and `vendors/`.
pub use protocols::anthropic_messages as anthropic;
pub use protocols::gemma_tool;
pub use protocols::google_generate_content;
pub use protocols::google_interactions;
pub use protocols::litert_lm as litert_lm_provider;
pub use protocols::openai_chat as openai_compat;
pub use protocols::openai_responses;
pub use vendors::deepseek;
pub use vendors::openai::codex;
pub use vendors::openai::codex_identity;

// Re-export SDK chat/wire/state primitives at crate root so internal adapters
// and `meridian-core` share one vocabulary.
pub use meridian_provider_sdk::chat::*;
pub use meridian_provider_sdk::chat_state as state;
pub use meridian_provider_sdk::codex_metadata;
pub use meridian_provider_sdk::wire as dto;

#[cfg(test)]
mod usage_tests {
    use super::*;

    fn openai_style(json: &str) -> TokenUsage {
        openai_compat::normalise_openai_usage(&serde_json::from_str(json).expect("a chat-completions usage body"))
    }

    fn responses_style(json: &str) -> TokenUsage {
        openai_responses::normalise_responses_usage(&serde_json::from_str(json).expect("a Responses usage body"))
    }

    fn anthropic_style(json: &str) -> TokenUsage {
        anthropic::normalise_anthropic_usage(&serde_json::from_str(json).expect("a Messages usage body"))
    }

    #[test]
    fn uncached_is_the_prompt_minus_both_cache_legs() {
        let u = TokenUsage {
            prompt_tokens: Some(1000),
            cache_read_tokens: Some(700),
            cache_write_tokens: Some(100),
            ..Default::default()
        };
        assert_eq!(u.uncached_prompt_tokens(), Some(200));
    }

    #[test]
    fn an_over_reported_cache_count_cannot_go_negative() {
        let u = TokenUsage {
            prompt_tokens: Some(100),
            cache_read_tokens: Some(9_999),
            ..Default::default()
        };
        assert_eq!(u.uncached_prompt_tokens(), Some(0));
    }

    #[test]
    fn deepseek_hit_and_miss_become_read_and_uncached() {
        let u = openai_style(
            r#"{"prompt_tokens":1000,"completion_tokens":50,"total_tokens":1050,
                "prompt_cache_hit_tokens":896,"prompt_cache_miss_tokens":104}"#,
        );
        assert_eq!(u.prompt_tokens, Some(1000));
        assert_eq!(u.cache_read_tokens, Some(896));
        assert_eq!(u.cache_write_tokens, None);
        assert_eq!(u.uncached_prompt_tokens(), Some(104));
    }

    #[test]
    fn openai_nested_cached_tokens_become_cache_read() {
        let u = openai_style(
            r#"{"prompt_tokens":2000,"completion_tokens":10,"total_tokens":2010,
                "prompt_tokens_details":{"cached_tokens":1792,"audio_tokens":0}}"#,
        );
        assert_eq!(u.cache_read_tokens, Some(1792));
        assert_eq!(u.uncached_prompt_tokens(), Some(208));
    }

    #[test]
    fn a_response_without_cache_fields_reports_no_cache_rather_than_zero() {
        let u = openai_style(r#"{"prompt_tokens":300,"completion_tokens":40,"total_tokens":340}"#);
        assert_eq!(u.cache_read_tokens, None);
        assert_eq!(u.cache_write_tokens, None);
        assert_eq!(u.uncached_prompt_tokens(), Some(300));
    }

    #[test]
    fn the_native_field_wins_when_a_gateway_emits_both() {
        let u = openai_style(
            r#"{"prompt_tokens":1000,"prompt_cache_hit_tokens":700,
                "prompt_tokens_details":{"cached_tokens":123}}"#,
        );
        assert_eq!(u.cache_read_tokens, Some(700));
    }

    #[test]
    fn every_adapter_normalises_to_the_same_invariants() {
        let cases: Vec<(&str, TokenUsage, i32)> = vec![
            (
                "deepseek",
                openai_style(
                    r#"{"prompt_tokens":1000,"prompt_cache_hit_tokens":896,
                        "prompt_cache_miss_tokens":104}"#,
                ),
                896 + 104,
            ),
            (
                "openai_compat",
                openai_style(r#"{"prompt_tokens":2000,"prompt_tokens_details":{"cached_tokens":1792}}"#),
                1792 + 208,
            ),
            (
                "openai_responses",
                responses_style(
                    r#"{"input_tokens":500,"output_tokens":30,"total_tokens":530,
                        "input_tokens_details":{"cached_tokens":384}}"#,
                ),
                384 + 116,
            ),
            (
                "anthropic",
                anthropic_style(
                    r#"{"input_tokens":150,"output_tokens":80,
                        "cache_read_input_tokens":1000,"cache_creation_input_tokens":250}"#,
                ),
                150 + 1000 + 250,
            ),
        ];

        for (name, u, expected_prompt) in cases {
            assert_eq!(u.prompt_tokens, Some(expected_prompt), "{name}: prompt_tokens");
            let sum = u.uncached_prompt_tokens().unwrap()
                + u.cache_read_tokens.unwrap_or(0)
                + u.cache_write_tokens.unwrap_or(0);
            assert_eq!(sum, expected_prompt, "{name}: buckets must partition prompt_tokens");
        }
    }

    #[test]
    fn anthropic_without_cache_fields_keeps_uncached_equal_to_prompt() {
        let u = anthropic_style(r#"{"input_tokens":400,"output_tokens":20}"#);
        assert_eq!(u.prompt_tokens, Some(400));
        assert_eq!(u.cache_read_tokens, None);
        assert_eq!(u.cache_write_tokens, None);
        assert_eq!(u.uncached_prompt_tokens(), Some(400));
    }

    #[test]
    fn responses_cache_read_only_when_details_present() {
        let cold = responses_style(r#"{"input_tokens":200,"output_tokens":10}"#);
        assert_eq!(cold.cache_read_tokens, None);
        assert_eq!(cold.uncached_prompt_tokens(), Some(200));

        let warm = responses_style(
            r#"{"input_tokens":200,"output_tokens":10,"input_tokens_details":{"cached_tokens":0}}"#,
        );
        assert_eq!(warm.cache_read_tokens, Some(0));
        assert_eq!(warm.uncached_prompt_tokens(), Some(200));
    }
}

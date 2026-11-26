//! Vendor & protocol conformance integration test suite.
//!
//! Exercises the reusable `meridian-provider-sdk::conformance` harness across
//! every built-in vendor manifest, chat protocol provider, web-search provider,
//! TTS provider, and the Official Standard Protocol Policy.

use meridian_provider_sdk::conformance::{
    assert_chat_provider_conformance, assert_manifest_conformance, assert_official_protocol_compliance,
    assert_tts_provider_conformance, assert_web_search_provider_conformance,
};
use meridian_provider_sdk::domain::ProtocolCompliance;
use meridian_providers::models::evaluate_protocol_compliance;
use meridian_providers::protocols::anthropic_messages::AnthropicMessagesProvider;
use meridian_providers::protocols::google_generate_content::GoogleGenerateContentProvider;
use meridian_providers::protocols::google_interactions::GoogleInteractionsProvider;
use meridian_providers::protocols::openai_chat::OpenAIChatProvider;
use meridian_providers::protocols::openai_responses::{CodexShape, OpenAIResponsesProvider};
use meridian_providers::tts::FishAudioTtsProvider;
use meridian_providers::vendors::deepseek::DeepSeekChatProvider;
use meridian_providers::vendors::google::{GoogleCompatThinkingMode, GoogleOpenAIChatProvider};
use meridian_providers::vendors::xai::XaiOpenAIChatProvider;
use meridian_providers::web_search::{BraveWebSearchProvider, TavilyWebSearchProvider, ZhipuWebSearchProvider};

#[test]
fn all_shipped_vendor_manifests_pass_conformance() {
    let manifests = meridian_providers::vendors::all_manifests();
    assert!(manifests.len() >= 13);
    for manifest in manifests {
        assert_manifest_conformance(manifest);
    }
}

#[test]
fn all_shipped_protocol_and_capability_providers_pass_conformance() {
    assert_chat_provider_conformance(&OpenAIChatProvider::new("https://api.openai.com/v1", "k"));
    assert_chat_provider_conformance(&OpenAIResponsesProvider::new(
        "https://api.openai.com/v1",
        "k",
        CodexShape::off(),
    ));
    assert_chat_provider_conformance(&AnthropicMessagesProvider::new("https://api.anthropic.com", "k"));
    assert_chat_provider_conformance(&GoogleGenerateContentProvider::new(
        "https://generativelanguage.googleapis.com",
        "k",
    ));
    assert_chat_provider_conformance(&GoogleInteractionsProvider::new(
        "https://aiplatform.googleapis.com/v1beta1/locations/global",
        "k",
    ));
    assert_chat_provider_conformance(&DeepSeekChatProvider::build("https://api.deepseek.com", "k"));
    assert_chat_provider_conformance(&XaiOpenAIChatProvider::build("https://api.x.ai/v1", "k"));
    assert_chat_provider_conformance(&GoogleOpenAIChatProvider::build(
        "https://generativelanguage.googleapis.com/v1beta/openai",
        "k",
        GoogleCompatThinkingMode::Auto,
    ));

    let client = reqwest::Client::new();
    assert_web_search_provider_conformance(&TavilyWebSearchProvider::new(client.clone(), "k"));
    assert_web_search_provider_conformance(&ZhipuWebSearchProvider::new(client, "k"));
    assert_web_search_provider_conformance(&BraveWebSearchProvider::new("k"));
    assert_tts_provider_conformance(&FishAudioTtsProvider::new("k"));
}

#[test]
fn official_standard_protocol_policy_enforces_canonical_protocols_and_detects_degradation() {
    // Official standard pairs pass strict (allow_degraded = false)
    for (model, protocol) in [
        ("claude-sonnet-4-6", "anthropic_messages"),
        ("anthropic/claude-opus-4-7", "anthropic_messages"),
        ("gemini-2.5-pro", "gemini_generate_content"),
        ("gemini-3.7-flash", "gemini_interactions"),
        ("gpt-5.6", "openai_responses"),
        ("gpt-4o", "openai_chat"),
        ("deepseek-reasoner", "openai_chat"),
        ("grok-4.6", "openai_responses"),
    ] {
        let compliance = evaluate_protocol_compliance(model, protocol);
        assert_eq!(compliance, ProtocolCompliance::Standard, "{model} on {protocol}");
        assert_official_protocol_compliance(model, protocol, &compliance, false);
    }

    // Relay running Claude or Gemini over openai_chat MUST be flagged Degraded
    for (model, protocol) in [
        ("anthropic/claude-sonnet-4-6", "openai_chat"),
        ("gemini-2.5-pro", "openai_chat"),
    ] {
        let compliance = evaluate_protocol_compliance(model, protocol);
        assert!(
            matches!(compliance, ProtocolCompliance::Degraded { .. }),
            "{model} on {protocol} must be Degraded, got {compliance:?}"
        );
        assert_official_protocol_compliance(model, protocol, &compliance, true);
    }
}

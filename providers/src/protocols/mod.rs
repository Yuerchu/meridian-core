//! Standard wire protocol implementations, strictly separated from vendor
//! identities.
//!
//! Naming convention (aligned with Meridian & foxline):
//! - Protocol bodies use bare protocol names (`OpenAIChatProvider`,
//!   `OpenAIResponsesProvider`, `AnthropicMessagesProvider`,
//!   `GoogleGenerateContentProvider`, `GoogleInteractionsProvider`,
//!   `OpenAIImagesProvider`, `GoogleImageGenProvider`,
//!   `VolcengineSeedreamProvider`, `GoogleVeoProvider`,
//!   `VolcengineSeedanceProvider`, `KlingVideoProvider`,
//!   `MiniMaxVideoProvider`, `ViduVideoProvider`, `BailianWanVideoProvider`).
//! - Shared greatest-common-divisor wire shapes use `*Base`
//!   (`OpenAIChatRequestBase`, `OpenAIChatResponseBase`, ...).
//! - Cross-protocol and non-GCD capabilities live in `crate::mixins`.
//! - Vendor-specific presets and protocol extensions live in `crate::vendors`.

pub mod anthropic_messages;
pub mod bailian_wan_video;
pub mod gemma_tool;
pub mod google_generate_content;
pub mod google_image_gen;
pub mod google_interactions;
pub mod google_veo;
pub mod kling_video;
pub mod litert_lm;
pub(crate) mod media_http;
pub mod minimax_video;
pub mod openai_chat;
pub mod openai_images;
pub mod openai_responses;
pub mod vidu_video;
pub mod volcengine_seedance;
pub mod volcengine_seedream;

use meridian_provider_sdk::domain::CapabilityDomain;

/// Static descriptor of one built-in wire protocol registered in `meridian-providers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolDescriptor {
    /// Canonical protocol identifier (e.g. `openai_chat`, `openai_responses`,
    /// `anthropic_messages`, `gemini_generate_content`, `gemini_interactions`,
    /// `openai_images`, `google_veo`, ...).
    pub id: &'static str,
    /// Legacy `ApiFormat` wire spelling during the Phase 1-3 migration window.
    pub legacy_api_format: Option<&'static str>,
    pub name: &'static str,
    pub domain: CapabilityDomain,
    /// Whether this protocol is an official standard protocol (as opposed to an
    /// adapter-only local or fallback format).
    pub is_official_standard: bool,
}

/// Authoritative list of wire protocols registered by this crate.
pub const BUILTIN_PROTOCOLS: &[ProtocolDescriptor] = &[
    ProtocolDescriptor {
        id: "openai_chat",
        legacy_api_format: Some("chat_completions"),
        name: "OpenAI Chat Completions",
        domain: CapabilityDomain::Chat,
        is_official_standard: true,
    },
    ProtocolDescriptor {
        id: "openai_responses",
        legacy_api_format: Some("responses"),
        name: "OpenAI Responses",
        domain: CapabilityDomain::Chat,
        is_official_standard: true,
    },
    ProtocolDescriptor {
        id: "anthropic_messages",
        legacy_api_format: Some("chat_completions"),
        name: "Anthropic Messages",
        domain: CapabilityDomain::Chat,
        is_official_standard: true,
    },
    ProtocolDescriptor {
        id: "gemini_generate_content",
        legacy_api_format: Some("gemini_generate_content"),
        name: "Google Gemini GenerateContent",
        domain: CapabilityDomain::Chat,
        is_official_standard: true,
    },
    ProtocolDescriptor {
        id: "gemini_interactions",
        legacy_api_format: Some("gemini_interactions"),
        name: "Google Gemini Interactions",
        domain: CapabilityDomain::Chat,
        is_official_standard: true,
    },
    ProtocolDescriptor {
        id: "gemma_tool",
        legacy_api_format: Some("gemma_tool"),
        name: "Gemma Text Tool Protocol",
        domain: CapabilityDomain::Chat,
        is_official_standard: false,
    },
    ProtocolDescriptor {
        id: "litert_lm",
        legacy_api_format: Some("litert_lm"),
        name: "LiteRT-LM On-Device",
        domain: CapabilityDomain::Chat,
        is_official_standard: true,
    },
    ProtocolDescriptor {
        id: "tavily_search",
        legacy_api_format: None,
        name: "Tavily Search API",
        domain: CapabilityDomain::WebSearch,
        is_official_standard: true,
    },
    ProtocolDescriptor {
        id: "zhipu_search",
        legacy_api_format: None,
        name: "Zhipu Web Search API",
        domain: CapabilityDomain::WebSearch,
        is_official_standard: true,
    },
    ProtocolDescriptor {
        id: "fish_audio_tts",
        legacy_api_format: None,
        name: "Fish Audio TTS API",
        domain: CapabilityDomain::Tts,
        is_official_standard: true,
    },
    // Official image generation protocols
    ProtocolDescriptor {
        id: "openai_images",
        legacy_api_format: None,
        name: "OpenAI Images API",
        domain: CapabilityDomain::ImageGen,
        is_official_standard: true,
    },
    ProtocolDescriptor {
        id: "google_image_gen",
        legacy_api_format: None,
        name: "Google Gemini Native Image Generation",
        domain: CapabilityDomain::ImageGen,
        is_official_standard: true,
    },
    ProtocolDescriptor {
        id: "volcengine_seedream",
        legacy_api_format: None,
        name: "Volcengine Ark Seedream Images API",
        domain: CapabilityDomain::ImageGen,
        is_official_standard: true,
    },
    // Official video generation protocols
    ProtocolDescriptor {
        id: "google_veo",
        legacy_api_format: None,
        name: "Google Veo Long-Running Video API",
        domain: CapabilityDomain::VideoGen,
        is_official_standard: true,
    },
    ProtocolDescriptor {
        id: "volcengine_seedance",
        legacy_api_format: None,
        name: "Volcengine Ark Seedance Video API",
        domain: CapabilityDomain::VideoGen,
        is_official_standard: true,
    },
    ProtocolDescriptor {
        id: "kling_video",
        legacy_api_format: None,
        name: "Kling New REST Video API",
        domain: CapabilityDomain::VideoGen,
        is_official_standard: true,
    },
    ProtocolDescriptor {
        id: "minimax_video",
        legacy_api_format: None,
        name: "MiniMax Hailuo V2 Video API",
        domain: CapabilityDomain::VideoGen,
        is_official_standard: true,
    },
    ProtocolDescriptor {
        id: "vidu_video",
        legacy_api_format: None,
        name: "Vidu Enterprise V2 Video API",
        domain: CapabilityDomain::VideoGen,
        is_official_standard: true,
    },
    ProtocolDescriptor {
        id: "bailian_wan_video",
        legacy_api_format: None,
        name: "Alibaba Bailian Wan Video API",
        domain: CapabilityDomain::VideoGen,
        is_official_standard: true,
    },
];

pub fn find_protocol(id: &str) -> Option<&'static ProtocolDescriptor> {
    BUILTIN_PROTOCOLS.iter().find(|p| p.id == id)
}

/// Resolve canonical protocol ID from the legacy `(provider_type, api_format)` pair.
pub fn canonical_protocol_id(provider_type: &str, api_format: &str) -> Option<&'static str> {
    match (provider_type, api_format) {
        ("anthropic", "chat_completions") => Some("anthropic_messages"),
        (_, "chat_completions") => Some("openai_chat"),
        (_, "responses") => Some("openai_responses"),
        (_, "gemini_generate_content") => Some("gemini_generate_content"),
        (_, "gemini_interactions") => Some("gemini_interactions"),
        (_, "gemma_tool") => Some("gemma_tool"),
        (_, "litert_lm") => Some("litert_lm"),
        _ => None,
    }
}

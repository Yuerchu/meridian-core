//! Provider-independent canonical model capability library, model-ID
//! canonicalizer, protocol compliance evaluator, and 3-layer orthogonal
//! capability intersection.
//!
//! Model families live in `src/models/<family>.json` and are auto-discovered by
//! `build.rs`. Each family declares its official `canonical_protocols` so
//! Meridian can detect degraded compatibility relays (e.g. Claude over
//! `openai_chat`) and surface exact lost capabilities.

use std::sync::LazyLock;

use serde::Deserialize;

use crate::capabilities::{CapabilityEffort, CapabilityVerbosity};
use crate::protocols::find_protocol;
use crate::{ProviderCapabilities, ServerToolKind, ThinkingStyle};
use meridian_provider_sdk::domain::ProtocolCompliance;
use meridian_provider_sdk::media::{MediaCategory, ReadableMime};

include!(concat!(env!("OUT_DIR"), "/model_families.rs"));

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ModelComment {
    Text(String),
    Lines(Vec<String>),
}

/// One model family file (`src/models/<family>.json`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelFamilyCatalog {
    pub version: u32,
    pub family: String,
    pub origin_vendor: String,
    pub canonical_protocols: Vec<String>,
    pub models: Vec<CanonicalModelEntry>,
}

/// Inherent model capabilities (independent of which provider hosts it).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalModelEntry {
    pub prefix: String,
    #[serde(rename = "_comment")]
    pub _comment: Option<ModelComment>,
    pub canonical_protocols: Option<Vec<String>>,
    pub supports_tools: Option<bool>,
    pub supports_streaming_tools: Option<bool>,
    pub supports_thinking: Option<bool>,
    pub supports_thinking_off: Option<bool>,
    pub readable_mime_types: Option<Vec<ReadableMime>>,
    pub supports_temperature: Option<bool>,
    pub supports_top_p: Option<bool>,
    pub max_context_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub max_temperature: Option<f32>,
    pub thinking_style: Option<ThinkingStyle>,
    pub supported_efforts: Option<Vec<CapabilityEffort>>,
    pub default_effort: Option<CapabilityEffort>,
    pub supports_fast: Option<bool>,
    pub supports_verbosity: Option<bool>,
    pub default_verbosity: Option<CapabilityVerbosity>,
    pub server_tools: Option<Vec<ServerToolKind>>,
    pub supports_remote_compaction: Option<bool>,
    pub responses_lite: Option<bool>,
}

static FAMILIES: LazyLock<Vec<ModelFamilyCatalog>> = LazyLock::new(|| {
    let mut families = Vec::with_capacity(MODEL_FAMILY_SOURCES.len());
    for &(stem, raw) in MODEL_FAMILY_SOURCES {
        let catalog: ModelFamilyCatalog = serde_json::from_str(raw)
            .unwrap_or_else(|err| panic!("models/{stem}.json is malformed: {err}"));
        assert_eq!(
            catalog.version, 1,
            "models/{stem}.json has unsupported version {}",
            catalog.version
        );
        assert_eq!(
            catalog.family, stem,
            "models/{stem}.json family `{}` must match filename `{stem}`",
            catalog.family
        );
        assert!(
            !catalog.canonical_protocols.is_empty(),
            "models/{stem}.json must declare at least one canonical_protocol"
        );
        for proto in &catalog.canonical_protocols {
            assert!(
                find_protocol(proto).is_some(),
                "models/{stem}.json references unknown protocol `{proto}`"
            );
        }
        families.push(catalog);
    }
    families
});

pub fn all_families() -> &'static [ModelFamilyCatalog] {
    &FAMILIES
}

/// Lookup used by `capabilities::resolve` when scoped to an origin vendor namespace.
pub fn find_in_vendor(origin_vendor: &str, model: &str) -> Option<&'static CanonicalModelEntry> {
    let lower = model.to_ascii_lowercase();
    let lower = lower.strip_prefix("models/").unwrap_or(&lower);
    FAMILIES
        .iter()
        .filter(|family| family.origin_vendor == origin_vendor)
        .flat_map(|family| family.models.iter())
        .filter(|entry| lower.starts_with(&entry.prefix))
        .max_by_key(|entry| entry.prefix.len())
}

/// Result of canonicalizing any vendor-specific `remote_model_id` (e.g.
/// `anthropic/claude-sonnet-4-6-20260501:thinking` → `anthropic/claude-sonnet-4-6`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalModelMatch {
    /// Canonical logical model ID (`<origin_vendor>/<normalized_name>`).
    pub canonical_id: String,
    pub family: String,
    pub origin_vendor: String,
    pub canonical_protocols: Vec<String>,
    pub matched_prefix: String,
}

/// Strip vendor routing namespaces (`anthropic/`, `openai/`, `google/`,
/// `deepseek/`, `x-ai/`, `models/`, etc.), `:variant` tags (`:free`, `:beta`,
/// `:thinking`), and trailing date stamps (`-20250514`, `-2025-05-14`).
pub fn strip_remote_model_decorations(remote_model_id: &str) -> String {
    let trimmed = remote_model_id.trim().to_ascii_lowercase();
    // Strip tag suffix like `:free`, `:beta`, `:thinking`
    let without_tag = trimmed.split(':').next().unwrap_or(&trimmed);
    // Strip leading provider path segments (`anthropic/claude-...`, `accounts/fireworks/models/...`)
    let base = without_tag.rsplit('/').next().unwrap_or(without_tag);
    strip_trailing_date_stamp(base).to_string()
}

fn strip_trailing_date_stamp(name: &str) -> &str {
    // Match `-YYYY-MM-DD` (11 chars)
    if name.len() > 11 {
        let tail = &name[name.len() - 11..];
        let b = tail.as_bytes();
        if b[0] == b'-'
            && b[1..5].iter().all(u8::is_ascii_digit)
            && b[5] == b'-'
            && b[6..8].iter().all(u8::is_ascii_digit)
            && b[8] == b'-'
            && b[9..11].iter().all(u8::is_ascii_digit)
        {
            return &name[..name.len() - 11];
        }
    }
    // Match `-YYYYMMDD` (9 chars)
    if name.len() > 9 {
        let tail = &name[name.len() - 9..];
        let b = tail.as_bytes();
        if b[0] == b'-' && b[1..9].iter().all(u8::is_ascii_digit) {
            return &name[..name.len() - 9];
        }
    }
    name
}

/// Identify a remote model ID against the provider-independent canonical model
/// library (longest prefix wins across all families).
pub fn canonicalize_model_id(remote_model_id: &str) -> Option<CanonicalModelMatch> {
    let normalized = strip_remote_model_decorations(remote_model_id);
    if normalized.is_empty() {
        return None;
    }

    let mut best: Option<(&ModelFamilyCatalog, &CanonicalModelEntry)> = None;
    for family in all_families() {
        for entry in &family.models {
            if normalized.starts_with(&entry.prefix) {
                let replace = match best {
                    None => true,
                    Some((_, prev)) => entry.prefix.len() > prev.prefix.len(),
                };
                if replace {
                    best = Some((family, entry));
                }
            }
        }
    }

    let (family, entry) = best?;
    let canonical_protocols = entry
        .canonical_protocols
        .clone()
        .unwrap_or_else(|| family.canonical_protocols.clone());

    Some(CanonicalModelMatch {
        canonical_id: format!("{}/{normalized}", family.origin_vendor),
        family: family.family.clone(),
        origin_vendor: family.origin_vendor.clone(),
        canonical_protocols,
        matched_prefix: entry.prefix.clone(),
    })
}

/// Evaluate whether reaching `remote_model_id` over `actual_protocol` conforms
/// to the model family's official standard protocol policy.
pub fn evaluate_protocol_compliance(remote_model_id: &str, actual_protocol: &str) -> ProtocolCompliance {
    let Some(matched) = canonicalize_model_id(remote_model_id) else {
        return ProtocolCompliance::Unknown;
    };

    if matched.canonical_protocols.iter().any(|p| p == actual_protocol) {
        return ProtocolCompliance::Standard;
    }

    let lost_capabilities = compute_lost_capabilities(&matched.origin_vendor, actual_protocol);
    ProtocolCompliance::Degraded {
        expected: matched.canonical_protocols,
        lost_capabilities,
    }
}

fn compute_lost_capabilities(origin_vendor: &str, actual_protocol: &str) -> Vec<String> {
    let mut lost = Vec::new();
    match (origin_vendor, actual_protocol) {
        ("anthropic", "openai_chat" | "openai_responses") => {
            lost.push("prompt_cache_breakpoints".into());
            lost.push("thinking_signature_continuity".into());
            if actual_protocol == "openai_chat" {
                lost.push("native_pdf".into());
                lost.push("server_tools".into());
            }
        }
        ("google", "openai_chat") => {
            lost.push("native_pdf".into());
            lost.push("native_audio_video".into());
            lost.push("google_search_grounding".into());
            lost.push("server_tools".into());
        }
        (_, "gemma_tool") => {
            lost.push("native_tool_calling".into());
            lost.push("native_thinking".into());
            lost.push("server_tools".into());
        }
        _ => {
            lost.push(format!("official_{origin_vendor}_protocol_features"));
        }
    }
    lost
}

/// Result of 3-layer orthogonal capability intersection:
/// `Layer 1 (Canonical Model)` ∩ `Layer 2 (Wire Protocol)` ∩ `Layer 3 (Overrides)`.
#[derive(Debug, Clone)]
pub struct LayeredCapabilityResolution {
    pub capabilities: ProviderCapabilities,
    pub compliance: ProtocolCompliance,
    pub canonical_id: Option<String>,
    pub family: Option<String>,
    pub suggested_protocol: Option<String>,
}

/// Resolve effective capabilities via the 3-layer orthogonal model:
/// 1. Canonical model inherent capabilities (matched by normalized model ID,
///    independent of which provider serves it).
/// 2. Wire protocol ceiling (e.g. `openai_chat` drops PDF/audio/video and
///    server tools; `chatgpt_codex` strips sampling params and fast tier).
/// 3. Optional model-level & deployment-level JSON overrides.
pub fn resolve_layered_capabilities(
    remote_model_id: &str,
    actual_protocol: &str,
    transport_profile: Option<&str>,
    model_overrides_json: Option<&str>,
    deployment_overrides_json: Option<&str>,
) -> Result<LayeredCapabilityResolution, String> {
    let matched = canonicalize_model_id(remote_model_id);
    let compliance = evaluate_protocol_compliance(remote_model_id, actual_protocol);

    // Layer 1: Inherent capabilities from canonical origin family (or generic fallback).
    let (canonical_id, family, suggested_protocol, mut caps) = if let Some(ref m) = matched {
        let preferred_api_format = match m.origin_vendor.as_str() {
            "anthropic" => Some("chat_completions"),
            "google" => Some("gemini_generate_content"),
            "openai" | "xai" => Some("responses"),
            "deepseek" => Some("chat_completions"),
            _ => None,
        };
        let inherent = crate::capabilities::resolve(&m.origin_vendor, preferred_api_format, &m.matched_prefix);
        (
            Some(m.canonical_id.clone()),
            Some(m.family.clone()),
            m.canonical_protocols.first().cloned(),
            inherent,
        )
    } else {
        let fallback_api = match actual_protocol {
            "openai_responses" => Some("responses"),
            "gemini_generate_content" => Some("gemini_generate_content"),
            "gemini_interactions" => Some("gemini_interactions"),
            "gemma_tool" => Some("gemma_tool"),
            "litert_lm" => Some("litert_lm"),
            _ => Some("chat_completions"),
        };
        let fallback_provider = match actual_protocol {
            "anthropic_messages" => "anthropic",
            "gemini_generate_content" | "gemini_interactions" => "google",
            "litert_lm" => "litert_lm",
            _ => "openai",
        };
        (
            None,
            None,
            None,
            crate::capabilities::resolve(fallback_provider, fallback_api, remote_model_id),
        )
    };

    // Layer 2: Intersect with actual wire protocol ceiling.
    narrow_to_protocol_ceiling(actual_protocol, transport_profile, &mut caps);

    // Layer 3: Apply model-level then deployment-level overrides.
    if let Some(raw) = model_overrides_json {
        crate::capabilities::apply_overrides(&mut caps, Some(raw))?;
    }
    if let Some(raw) = deployment_overrides_json {
        crate::capabilities::apply_overrides(&mut caps, Some(raw))?;
    }

    Ok(LayeredCapabilityResolution {
        capabilities: caps,
        compliance,
        canonical_id,
        family,
        suggested_protocol,
    })
}

fn narrow_to_protocol_ceiling(
    actual_protocol: &str,
    transport_profile: Option<&str>,
    caps: &mut ProviderCapabilities,
) {
    match actual_protocol {
        "openai_chat" => {
            // OpenAI Chat Completions cannot transport PDF/audio/video natively
            // and has no provider-side server tools.
            caps.readable_mime_types
                .retain(|mime| mime.category() == MediaCategory::Image);
            caps.server_tools.clear();
            caps.supports_remote_compaction = false;
            if caps.thinking_style == ThinkingStyle::Budget {
                caps.thinking_style = ThinkingStyle::EffortOnly;
            }
        }
        "openai_responses" => {
            caps.readable_mime_types
                .retain(|mime| matches!(mime.category(), MediaCategory::Image | MediaCategory::Document));
        }
        "anthropic_messages" => {
            caps.readable_mime_types
                .retain(|mime| matches!(mime.category(), MediaCategory::Image | MediaCategory::Document));
            caps.server_tools
                .retain(|tool| matches!(tool, ServerToolKind::WebSearch));
            caps.supports_remote_compaction = false;
        }
        "gemini_generate_content" | "gemini_interactions" => {
            caps.server_tools
                .retain(|tool| matches!(tool, ServerToolKind::GoogleSearch | ServerToolKind::UrlContext));
            caps.supports_remote_compaction = false;
        }
        "gemma_tool" => {
            caps.supports_thinking = false;
            caps.thinking_style = ThinkingStyle::None;
            caps.supported_efforts.clear();
            caps.default_effort = None;
            caps.server_tools.clear();
            caps.supports_remote_compaction = false;
        }
        _ => {}
    }

    if transport_profile == Some("chatgpt_codex") {
        caps.supports_temperature = false;
        caps.supports_top_p = false;
        caps.max_temperature = None;
        caps.supports_fast = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizer_normalizes_vendor_prefixes_tags_and_dates() {
        let claude = canonicalize_model_id("anthropic/claude-sonnet-4-6-20260501:thinking").unwrap();
        assert_eq!(claude.canonical_id, "anthropic/claude-sonnet-4-6");
        assert_eq!(claude.family, "anthropic-claude");
        assert_eq!(claude.canonical_protocols, vec!["anthropic_messages"]);

        let gemini = canonicalize_model_id("models/gemini-2.5-pro").unwrap();
        assert_eq!(gemini.canonical_id, "google/gemini-2.5-pro");
        assert_eq!(gemini.canonical_protocols, vec!["gemini_generate_content", "gemini_interactions"]);

        let grok = canonicalize_model_id("x-ai/grok-4.6:free").unwrap();
        assert_eq!(grok.canonical_id, "xai/grok-4.6");
        assert_eq!(grok.origin_vendor, "xai");

        assert!(canonicalize_model_id("private/my-fine-tune-v1").is_none());
    }

    #[test]
    fn protocol_compliance_flags_claude_over_openai_chat_as_degraded_and_intersects_pdf() {
        let standard = evaluate_protocol_compliance("claude-sonnet-4-6", "anthropic_messages");
        assert_eq!(standard, ProtocolCompliance::Standard);

        let degraded = evaluate_protocol_compliance("anthropic/claude-sonnet-4-6", "openai_chat");
        match degraded {
            ProtocolCompliance::Degraded {
                expected,
                lost_capabilities,
            } => {
                assert_eq!(expected, vec!["anthropic_messages"]);
                assert!(lost_capabilities.contains(&"native_pdf".to_string()));
                assert!(lost_capabilities.contains(&"prompt_cache_breakpoints".to_string()));
                assert!(lost_capabilities.contains(&"thinking_signature_continuity".to_string()));
            }
            other => panic!("expected Degraded, got {other:?}"),
        }

        // Layered intersection: Claude over openai_chat loses PDF & server_tools
        let layered = resolve_layered_capabilities("anthropic/claude-sonnet-4-6", "openai_chat", None, None, None)
            .unwrap();
        assert!(matches!(layered.compliance, ProtocolCompliance::Degraded { .. }));
        assert!(!layered.capabilities.readable_mime_types.contains(&ReadableMime::ApplicationPdf));
        assert!(layered.capabilities.server_tools.is_empty());

        // Same model over official anthropic_messages keeps PDF!
        let official = resolve_layered_capabilities("anthropic/claude-sonnet-4-6", "anthropic_messages", None, None, None)
            .unwrap();
        assert_eq!(official.compliance, ProtocolCompliance::Standard);
        assert!(official.capabilities.readable_mime_types.contains(&ReadableMime::ApplicationPdf));
    }

    #[test]
    fn unknown_custom_model_returns_unknown_compliance() {
        assert_eq!(
            evaluate_protocol_compliance("my-company/custom-70b", "openai_chat"),
            ProtocolCompliance::Unknown
        );
    }
}

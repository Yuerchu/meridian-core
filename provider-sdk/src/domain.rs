//! Capability domains, cross-provider switch impact, and protocol compliance.

use serde::{Deserialize, Serialize};

/// The functional domain a provider or model serves.
///
/// One vendor manifest can declare multiple capability domains (for example,
/// Zhipu offering both `chat` and `web_search`, or LiteRT-LM offering `chat`
/// and `embedding`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::EnumIter,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum CapabilityDomain {
    Chat,
    WebSearch,
    Tts,
    Embedding,
    ImageGen,
    VideoGen,
}

impl CapabilityDomain {
    pub fn parse(value: &str) -> Result<Self, String> {
        value
            .parse()
            .map_err(|_| format!("unknown capability domain `{value}`"))
    }

    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

#[cfg(feature = "sea-orm")]
crate::impl_sea_orm_text_enum!(CapabilityDomain);

/// Multi-provider routing policy for a logical model.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::EnumIter,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum RoutingPolicy {
    /// Session-sticky for stateful chats; on failure, never silently switch —
    /// surface `SwitchImpact` and let the user decide.
    #[default]
    StickyManual,
    /// Automatic priority failover allowed ONLY for `SwitchImpact::Stateless`
    /// capabilities (image/video gen, web search, TTS, embeddings, stateless calls).
    AutoFailoverStateless,
    /// User explicitly opted into automatic failover even for stateful turns
    /// (emits an explicit system notice when switching).
    UserOptInFailover,
}

impl RoutingPolicy {
    pub fn parse(value: &str) -> Result<Self, String> {
        value.parse().map_err(|_| format!("unknown routing policy `{value}`"))
    }

    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

#[cfg(feature = "sea-orm")]
crate::impl_sea_orm_text_enum!(RoutingPolicy);

/// Whether switching a request from one provider deployment to another causes
/// observable negative side effects.
///
/// Routing uses this as the sole criterion for automatic failover:
/// - `Stateless`: safe to fail over automatically by deployment priority
///   (image/video generation, web search, TTS, embeddings, stateless calls).
/// - `Stateful`: never silently fail over; surface the exact impact and let
///   the user decide (or opt in explicitly).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SwitchImpact {
    Stateless,
    Stateful {
        breaks_prompt_cache: bool,
        breaks_reasoning_signature: bool,
        breaks_upstream_session: bool,
    },
}

impl SwitchImpact {
    pub const fn allows_automatic_failover(self) -> bool {
        matches!(self, Self::Stateless)
    }
}

/// Whether a deployment reaches a canonical model over one of that model's
/// official standard protocols or over a compatibility translation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProtocolCompliance {
    Standard,
    Degraded {
        expected: Vec<String>,
        lost_capabilities: Vec<String>,
    },
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stateless_allows_auto_failover_and_stateful_forbids_it() {
        assert!(SwitchImpact::Stateless.allows_automatic_failover());
        assert!(
            !SwitchImpact::Stateful {
                breaks_prompt_cache: true,
                breaks_reasoning_signature: false,
                breaks_upstream_session: false,
            }
            .allows_automatic_failover()
        );
    }

    #[test]
    fn capability_domain_round_trips() {
        for raw in ["chat", "web_search", "tts", "embedding", "image_gen", "video_gen"] {
            let parsed = CapabilityDomain::parse(raw).unwrap();
            assert_eq!(parsed.as_str(), raw);
        }
        assert!(CapabilityDomain::parse("unknown_domain").is_err());
    }
}

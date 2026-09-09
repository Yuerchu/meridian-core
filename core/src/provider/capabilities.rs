use super::{ChatParams, ProviderCapabilities, ServerToolKind, ThinkingStyle};
use serde::Deserialize;
use std::sync::LazyLock;

/// Every effort tier we know how to talk about, ascending. The frontend mirrors
/// this list; per-model subsets live in the catalog.
pub const EFFORT_LADDER: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// A concrete per-conversation/per-turn reasoning choice. Inheritance is
/// represented by `None` at the request and persistence boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoredThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl StoredThinkingLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "off" => Ok(Self::Off),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            _ => Err(format!("unknown thinking level {value:?}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    #[serde(rename = "version")]
    _version: u32,
    #[serde(rename = "_comment")]
    _comment: Vec<String>,
    models: Vec<CatalogEntry>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CatalogComment {
    Text(String),
    Lines(Vec<String>),
}

/// A patch over the provider default. Every capability is `Option` so an entry
/// only states what differs; omitted fields inherit.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogEntry {
    provider: String,
    prefix: String,
    #[serde(rename = "_comment")]
    _comment: Option<CatalogComment>,
    supports_tools: Option<bool>,
    supports_streaming_tools: Option<bool>,
    supports_thinking: Option<bool>,
    supports_thinking_off: Option<bool>,
    supports_images: Option<bool>,
    supports_pdf: Option<bool>,
    supports_temperature: Option<bool>,
    supports_top_p: Option<bool>,
    max_context_tokens: Option<u32>,
    max_output_tokens: Option<u32>,
    max_temperature: Option<f32>,
    thinking_style: Option<ThinkingStyle>,
    supported_efforts: Option<Vec<CapabilityEffort>>,
    default_effort: Option<CapabilityEffort>,
    supports_fast: Option<bool>,
    supports_verbosity: Option<bool>,
    default_verbosity: Option<CapabilityVerbosity>,
    server_tools: Option<Vec<ServerToolKind>>,
    supports_remote_compaction: Option<bool>,
}

static CATALOG: LazyLock<Catalog> = LazyLock::new(|| {
    // Parsed once at first use. A malformed catalog is a build-time authoring
    // error, not something to degrade around at runtime.
    let catalog: Catalog =
        serde_json::from_str(include_str!("model_catalog.json")).expect("model_catalog.json is malformed");
    assert_eq!(catalog._version, 1, "unsupported model_catalog.json version");
    catalog
});

fn apply(base: &mut ProviderCapabilities, entry: &CatalogEntry) {
    if let Some(v) = entry.supports_tools {
        base.supports_tools = v;
    }
    if let Some(v) = entry.supports_streaming_tools {
        base.supports_streaming_tools = v;
    }
    if let Some(v) = entry.supports_thinking {
        base.supports_thinking = v;
    }
    if let Some(v) = entry.supports_thinking_off {
        base.supports_thinking_off = v;
    }
    if let Some(v) = entry.supports_images {
        base.supports_images = v;
    }
    if let Some(v) = entry.supports_pdf {
        base.supports_pdf = v;
    }
    if let Some(v) = entry.supports_temperature {
        base.supports_temperature = v;
    }
    if let Some(v) = entry.supports_top_p {
        base.supports_top_p = v;
    }
    if let Some(v) = entry.max_context_tokens {
        base.max_context_tokens = Some(v);
    }
    if let Some(v) = entry.max_output_tokens {
        base.max_output_tokens = Some(v);
    }
    if let Some(v) = entry.max_temperature {
        base.max_temperature = Some(v);
    }
    if let Some(v) = entry.thinking_style {
        base.thinking_style = v;
    }
    if let Some(ref v) = entry.supported_efforts {
        base.supported_efforts = v.iter().map(|effort| effort.as_str().to_string()).collect();
    }
    if let Some(v) = entry.default_effort {
        base.default_effort = Some(v.as_str().to_string());
    }
    if let Some(v) = entry.supports_fast {
        base.supports_fast = v;
    }
    if let Some(v) = entry.supports_verbosity {
        base.supports_verbosity = v;
    }
    if let Some(v) = entry.default_verbosity {
        base.default_verbosity = Some(v.as_str().to_string());
    }
    if let Some(ref v) = entry.server_tools {
        base.server_tools = v.clone();
    }
    if let Some(v) = entry.supports_remote_compaction {
        base.supports_remote_compaction = v;
    }
}

fn find_longest_prefix_match<'a>(provider: &str, model: &str) -> Option<&'a CatalogEntry> {
    let lower = model.to_ascii_lowercase();
    let lower = lower.strip_prefix("models/").unwrap_or(&lower);
    CATALOG
        .models
        .iter()
        .filter(|e| e.provider == provider && lower.starts_with(&e.prefix))
        .max_by_key(|e| e.prefix.len())
}

// ---------------------------------------------------------------------------
// Provider defaults
// ---------------------------------------------------------------------------

fn anthropic_default() -> ProviderCapabilities {
    ProviderCapabilities {
        supports_tools: true,
        supports_streaming_tools: true,
        supports_thinking: true,
        supports_thinking_off: true,
        supports_images: true,
        supports_pdf: true,
        supports_temperature: true,
        supports_top_p: true,
        max_context_tokens: Some(200_000),
        max_output_tokens: Some(64_000),
        max_temperature: Some(1.0),
        thinking_style: ThinkingStyle::Budget,
        ..Default::default()
    }
}

fn openai_responses_default() -> ProviderCapabilities {
    ProviderCapabilities {
        supports_tools: true,
        supports_streaming_tools: true,
        supports_thinking: true,
        supports_thinking_off: true,
        supports_images: true,
        supports_temperature: true,
        supports_top_p: true,
        max_context_tokens: Some(200_000),
        max_output_tokens: Some(100_000),
        max_temperature: Some(2.0),
        thinking_style: ThinkingStyle::EffortOnly,
        supported_efforts: vec!["low".into(), "medium".into(), "high".into()],
        ..Default::default()
    }
}

fn deepseek_default() -> ProviderCapabilities {
    ProviderCapabilities {
        supports_tools: true,
        supports_streaming_tools: true,
        supports_thinking: true,
        supports_thinking_off: true,
        max_context_tokens: Some(128_000),
        max_output_tokens: Some(16_000),
        max_temperature: Some(2.0),
        thinking_style: ThinkingStyle::ToggleOff,
        supported_efforts: vec!["low".into(), "medium".into(), "high".into()],
        ..Default::default()
    }
}

/// Every Grok in the catalog reasons and none of them can be told not to —
/// xAI's own wording is "reasoning cannot be disabled" — so the default is
/// thinking on with effort as the only knob.
///
/// `grok-4.20-non-reasoning` is the exception and this cannot express it: the
/// variant is a *suffix* of the model id, which longest-prefix matching cannot
/// reach, so it inherits `supports_thinking` and needs a `capability_overrides`
/// entry to correct. The catalog entry for `grok-4.20` says so.
///
/// The context window is deliberately the smallest of the family (256k, which
/// is `grok-code-fast`'s) rather than 4.6's 500k: an unknown model inheriting
/// this gets a limit that is too small at worst, and a limit that is too large
/// is a request the provider refuses after the whole prompt has been assembled.
fn xai_default() -> ProviderCapabilities {
    ProviderCapabilities {
        supports_tools: true,
        supports_streaming_tools: true,
        supports_thinking: true,
        supports_thinking_off: false,
        supports_images: true,
        supports_temperature: true,
        supports_top_p: true,
        max_context_tokens: Some(256_000),
        max_output_tokens: Some(64_000),
        max_temperature: Some(2.0),
        thinking_style: ThinkingStyle::EffortOnly,
        supported_efforts: vec!["low".into(), "medium".into(), "high".into()],
        default_effort: Some("high".into()),
        ..Default::default()
    }
}

fn generic_default() -> ProviderCapabilities {
    ProviderCapabilities {
        supports_tools: true,
        supports_streaming_tools: true,
        supports_thinking_off: true,
        supports_temperature: true,
        supports_top_p: true,
        max_temperature: Some(2.0),
        ..Default::default()
    }
}

/// The gemma_tool format simulates function calling through prompt injection on
/// a plain chat/completions endpoint, so it has no reasoning surface at all and
/// must not inherit OpenAI model rules -- a gemma_tool provider serving a model
/// named `gpt-4o` is not GPT-4o.
fn gemma_default() -> ProviderCapabilities {
    ProviderCapabilities {
        supports_tools: true,
        supports_streaming_tools: true,
        supports_thinking_off: true,
        supports_temperature: true,
        supports_top_p: true,
        max_temperature: Some(2.0),
        ..Default::default()
    }
}

fn google_default() -> ProviderCapabilities {
    ProviderCapabilities {
        supports_tools: true,
        supports_streaming_tools: true,
        supports_thinking: true,
        supports_thinking_off: false,
        supports_images: true,
        supports_pdf: false,
        supports_temperature: false,
        supports_top_p: false,
        max_context_tokens: Some(1_048_576),
        max_output_tokens: Some(65_536),
        thinking_style: ThinkingStyle::EffortOnly,
        supported_efforts: vec!["low".into(), "medium".into(), "high".into()],
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn resolve(provider_type: &str, api_format: Option<&str>, model: &str) -> ProviderCapabilities {
    resolve_on(provider_type, api_format, None, model)
}

/// The same, told which wire the request will actually go out on.
///
/// A separate entry point rather than a fourth argument everywhere, because
/// almost every caller is asking about a model in the abstract — what a picker
/// should offer, whether images are possible — and only the turn needs to know
/// what *this* row will really send.
pub fn resolve_on(
    provider_type: &str,
    api_format: Option<&str>,
    transport_profile: Option<&str>,
    model: &str,
) -> ProviderCapabilities {
    super::registry::ProviderType::parse(provider_type)
        .unwrap_or_else(|error| panic!("capability resolver received an invalid provider contract: {error}"));
    if let Some(api_format) = api_format {
        super::registry::ApiFormat::parse(api_format)
            .unwrap_or_else(|error| panic!("capability resolver received an invalid API contract: {error}"));
    }
    if let Some(transport_profile) = transport_profile {
        super::registry::TransportProfile::parse(transport_profile)
            .unwrap_or_else(|error| panic!("capability resolver received an invalid transport contract: {error}"));
    }
    let mut caps = resolve_inner(provider_type, api_format, model);

    // The Codex backend ignores sampling parameters and has no priority tier to
    // sell — a subscription is not an API account. Saying so here rather than
    // only in the adapter is what keeps the UI honest: a temperature slider that
    // renders and then changes nothing is worse than one that is absent, and
    // `filter_params` reads these to decide what may be sent at all.
    if transport_profile == Some("chatgpt_codex") {
        caps.supports_temperature = false;
        caps.supports_top_p = false;
        caps.supports_fast = false;
    }
    caps
}

fn resolve_inner(provider_type: &str, api_format: Option<&str>, model: &str) -> ProviderCapabilities {
    // `catalog_provider` scopes the prefix search. gemma_tool deliberately maps
    // to a namespace with no entries so it only ever gets its default.
    let (mut caps, catalog_provider) = match provider_type {
        "anthropic" => (anthropic_default(), "anthropic"),
        "deepseek" => {
            let mut caps = deepseek_default();
            if api_format == Some("responses") {
                // `ToggleOff` exists only in the chat adapter, which sends
                // `thinking: {"type": "disabled"}`. The Responses adapter has
                // never heard of it, so turning thinking off there sent nothing
                // at all and the model reasoned anyway — a switch that reported
                // itself off while having no effect. Effort is what this dialect
                // does support, per DeepSeek's own compatibility table.
                caps.supports_thinking_off = false;
                caps.thinking_style = ThinkingStyle::EffortOnly;
            }
            (caps, "deepseek")
        }
        "xai" => (xai_default(), "xai"),
        "google" => (google_default(), "google"),
        _ => match api_format {
            Some("responses") => (openai_responses_default(), "openai"),
            Some("gemma_tool") => (gemma_default(), "gemma"),
            _ => (generic_default(), "openai"),
        },
    };
    caps.server_tools = server_tools_for(provider_type, api_format);
    caps.supports_remote_compaction = remote_compaction_for(provider_type, api_format);
    if let Some(entry) = find_longest_prefix_match(catalog_provider, model) {
        apply(&mut caps, entry);
    }
    caps
}

/// Which provider-side tools this dialect offers at all.
///
/// Gated on the Responses API because that is the only place they exist. xAI's
/// chat-completions endpoint answers `{"type":"web_search"}` with a 422 —
/// "expected `function` or `live_search`" — so offering the switch there would
/// be offering a setting that turns every request into an error.
///
/// Deliberately short. Only what has been measured against a live endpoint
/// (xAI) or spelled out in a compatibility table (DeepSeek) is listed; OpenAI's
/// own Responses tools are absent because their wire names have moved around
/// (`web_search_preview`) and a wrong name here is a 400 on every request. A
/// model that needs one it does not inherit can be given it in
/// `capability_overrides`.
fn server_tools_for(provider_type: &str, api_format: Option<&str>) -> Vec<ServerToolKind> {
    let tools: &[ServerToolKind] = match (provider_type, api_format) {
        // The Messages API's own search, `web_search_<date>`. The adapter
        // picks the dated name the model's generation accepts.
        ("anthropic", None | Some("messages")) => &[ServerToolKind::WebSearch],
        ("xai", Some("responses")) => &[
            ServerToolKind::WebSearch,
            ServerToolKind::XSearch,
            ServerToolKind::CodeExecution,
        ],
        // Its compatibility table lists `function` and `web_search` as the
        // supported tool types and says everything else is ignored.
        ("deepseek", Some("responses")) => &[ServerToolKind::WebSearch],
        // Chat-completions has no such thing anywhere — measured: xAI answers
        // "expected `function` or `live_search`" — and nobody else is listed
        // without having been measured.
        _ => &[],
    };
    tools.to_vec()
}

fn remote_compaction_for(provider_type: &str, api_format: Option<&str>) -> bool {
    matches!(
        (provider_type, api_format),
        ("openai" | "xai" | "deepseek", Some("responses"))
    )
}

#[derive(Debug, Default)]
enum OverrideField<T> {
    #[default]
    Unset,
    Set(T),
}

impl<'de, T> Deserialize<'de> for OverrideField<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        T::deserialize(deserializer).map(Self::Set)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CapabilityEffort {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl CapabilityEffort {
    fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CapabilityVerbosity {
    Low,
    Medium,
    High,
}

impl CapabilityVerbosity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// The complete user-owned contract stored in
/// `model_configs.capability_overrides`.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderCapabilityOverrides {
    supports_tools: OverrideField<bool>,
    supports_streaming_tools: OverrideField<bool>,
    supports_thinking: OverrideField<bool>,
    supports_thinking_off: OverrideField<bool>,
    supports_images: OverrideField<bool>,
    supports_pdf: OverrideField<bool>,
    supports_temperature: OverrideField<bool>,
    supports_top_p: OverrideField<bool>,
    supports_fast: OverrideField<bool>,
    supports_verbosity: OverrideField<bool>,
    thinking_style: OverrideField<ThinkingStyle>,
    supported_efforts: OverrideField<Vec<CapabilityEffort>>,
    server_tools: OverrideField<Vec<ServerToolKind>>,
    supports_remote_compaction: OverrideField<bool>,
    default_effort: OverrideField<Option<CapabilityEffort>>,
    default_verbosity: OverrideField<Option<CapabilityVerbosity>>,
    max_context_tokens: OverrideField<Option<u32>>,
    max_output_tokens: OverrideField<Option<u32>>,
    max_temperature: OverrideField<Option<f32>>,
}

impl ProviderCapabilityOverrides {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let parsed: Self =
            serde_json::from_str(raw).map_err(|error| format!("invalid capability_overrides: {error}"))?;
        parsed.validate()?;
        Ok(parsed)
    }

    fn validate(&self) -> Result<(), String> {
        if let OverrideField::Set(efforts) = &self.supported_efforts {
            let unique: std::collections::BTreeSet<CapabilityEffort> = efforts.iter().copied().collect();
            if unique.len() != efforts.len() {
                return Err("capability_overrides.supported_efforts cannot contain duplicates".into());
            }
        }
        if let OverrideField::Set(tools) = &self.server_tools {
            let unique: std::collections::BTreeSet<ServerToolKind> = tools.iter().copied().collect();
            if unique.len() != tools.len() {
                return Err("capability_overrides.server_tools cannot contain duplicates".into());
            }
        }
        for (field, value) in [
            ("max_context_tokens", &self.max_context_tokens),
            ("max_output_tokens", &self.max_output_tokens),
        ] {
            if matches!(value, OverrideField::Set(Some(0))) {
                return Err(format!("capability_overrides.{field} must be positive or null"));
            }
        }
        if let OverrideField::Set(Some(value)) = self.max_temperature
            && value < 0.0
        {
            return Err("capability_overrides.max_temperature must be non-negative or null".into());
        }
        Ok(())
    }
}

pub fn validate_overrides(overrides: Option<&str>) -> Result<(), String> {
    overrides
        .map(ProviderCapabilityOverrides::parse)
        .transpose()
        .map(|_| ())
}

/// Parse and atomically apply a user-authored capability override. Invalid
/// content leaves the catalog-derived capability set untouched.
pub fn apply_overrides(caps: &mut ProviderCapabilities, overrides: Option<&str>) -> Result<(), String> {
    let Some(raw) = overrides else { return Ok(()) };
    let patch = ProviderCapabilityOverrides::parse(raw)?;
    let mut updated = caps.clone();

    macro_rules! apply_value {
        ($field:ident) => {
            if let OverrideField::Set(value) = patch.$field {
                updated.$field = value;
            }
        };
    }
    apply_value!(supports_tools);
    apply_value!(supports_streaming_tools);
    apply_value!(supports_thinking);
    apply_value!(supports_thinking_off);
    apply_value!(supports_images);
    apply_value!(supports_pdf);
    apply_value!(supports_temperature);
    apply_value!(supports_top_p);
    apply_value!(supports_fast);
    apply_value!(supports_verbosity);
    apply_value!(thinking_style);
    apply_value!(server_tools);
    apply_value!(supports_remote_compaction);
    apply_value!(max_context_tokens);
    apply_value!(max_output_tokens);
    apply_value!(max_temperature);

    if let OverrideField::Set(mut efforts) = patch.supported_efforts {
        efforts.sort_unstable();
        updated.supported_efforts = efforts.into_iter().map(|effort| effort.as_str().to_string()).collect();
    }
    if let OverrideField::Set(effort) = patch.default_effort {
        updated.default_effort = effort.map(|value| value.as_str().to_string());
    }
    if let OverrideField::Set(verbosity) = patch.default_verbosity {
        updated.default_verbosity = verbosity.map(|value| value.as_str().to_string());
    }
    if let Some(default) = updated.default_effort.as_deref()
        && !updated.supported_efforts.iter().any(|effort| effort == default)
    {
        return Err(format!(
            "capability_overrides.default_effort '{default}' is not present in supported_efforts"
        ));
    }
    *caps = updated;
    Ok(())
}

/// Resolve the effective thinking triple from an assistant's stored defaults
/// plus an optional per-request tier. Shared by the chat command and the OneBot
/// agent so the two entry points cannot drift apart.
///
/// A request may omit the level to use the assistant default. Any present
/// value is a closed first-party contract and must be recognised exactly.
pub fn resolve_thinking(
    assistant_enabled: bool,
    assistant_budget: Option<i32>,
    requested_level: Option<&str>,
) -> Result<(bool, Option<i32>, Option<String>), String> {
    Ok(match requested_level {
        None => (assistant_enabled, assistant_budget, None),
        Some(level) => match StoredThinkingLevel::parse(level)? {
            StoredThinkingLevel::Off => (false, None, None),
            effort => (true, assistant_budget, Some(effort.as_str().to_string())),
        },
    })
}

pub fn filter_params(params: &mut ChatParams, caps: &ProviderCapabilities) -> Result<(), String> {
    if let Some(effort) = params.thinking_effort.as_deref() {
        if !EFFORT_LADDER[1..].contains(&effort) {
            return Err(format!("unknown thinking effort '{effort}'"));
        }
        if !caps.supports_thinking || !caps.supported_efforts.iter().any(|item| item == effort) {
            return Err(format!(
                "thinking effort '{effort}' is not supported by model '{}'",
                params.model
            ));
        }
    }
    params.thinking_style = caps.thinking_style;
    if !caps.supports_temperature {
        params.temperature = None;
    }
    if !caps.supports_top_p {
        params.top_p = None;
    }
    if !caps.supports_thinking {
        // The UI still shows a thinking switch. Turning it on and getting no
        // reasoning at all reads as a broken feature rather than as a model
        // that cannot do it.
        if params.thinking_enabled {
            tracing::info!(
                model = %params.model,
                "thinking was requested but this model has no reasoning support; dropped"
            );
        }
        params.thinking_enabled = false;
        params.thinking_budget = None;
        params.thinking_effort = None;
    } else if !caps.supports_thinking_off && !params.thinking_enabled {
        // Gemini 3.x and similar always-thinking models interpret omission as
        // the model default; there is no wire value that disables reasoning.
        params.thinking_enabled = true;
        params.thinking_budget = None;
        params.thinking_effort = None;
    }
    // budget_tokens is rejected outright by adaptive/always-on models, and is
    // meaningless where effort is the only knob.
    if matches!(
        caps.thinking_style,
        ThinkingStyle::Adaptive | ThinkingStyle::AlwaysOn | ThinkingStyle::EffortOnly
    ) {
        params.thinking_budget = None;
    }
    if !caps.supports_fast {
        if params.fast {
            tracing::info!(
                model = %params.model,
                "fast mode is not available on this model; dropped"
            );
        }
        params.fast = false;
    }
    if caps.supports_verbosity {
        if params.verbosity.is_none() {
            params.verbosity = caps.default_verbosity.clone();
        }
    } else {
        params.verbosity = None;
    }
    if let (Some(max_temp), Some(temp)) = (caps.max_temperature, params.temperature)
        && temp > max_temp as f64
    {
        params.temperature = Some(max_temp as f64);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_parses() {
        assert!(!CATALOG.models.is_empty());
    }

    #[test]
    fn catalog_effort_and_verbosity_values_are_closed() {
        for raw in [
            r#"{"provider":"openai","prefix":"strict-test","supported_efforts":["future"]}"#,
            r#"{"provider":"openai","prefix":"strict-test","supported_efforts":["none"]}"#,
            r#"{"provider":"openai","prefix":"strict-test","default_effort":"future"}"#,
            r#"{"provider":"openai","prefix":"strict-test","default_effort":"High"}"#,
            r#"{"provider":"openai","prefix":"strict-test","default_verbosity":"verbose"}"#,
        ] {
            assert!(
                serde_json::from_str::<CatalogEntry>(raw).is_err(),
                "accepted unknown catalog value in {raw}"
            );
        }
    }

    #[test]
    fn catalog_enums_project_to_the_public_string_contract_without_loss() {
        let entry: CatalogEntry = serde_json::from_str(
            r#"{
                "provider":"openai",
                "prefix":"strict-test",
                "supported_efforts":["minimal","low","medium","high","xhigh","max"],
                "default_effort":"xhigh",
                "default_verbosity":"high"
            }"#,
        )
        .unwrap();
        let mut caps = ProviderCapabilities::default();

        apply(&mut caps, &entry);

        assert_eq!(
            caps.supported_efforts,
            vec!["minimal", "low", "medium", "high", "xhigh", "max"]
        );
        assert_eq!(caps.default_effort.as_deref(), Some("xhigh"));
        assert_eq!(caps.default_verbosity.as_deref(), Some("high"));
    }

    /// The Codex backend ignores sampling parameters and sells no priority
    /// tier. Saying so here is what keeps the UI honest — a temperature slider
    /// that renders and changes nothing is worse than one that is absent.
    #[test]
    fn the_codex_transport_drops_what_a_subscription_cannot_use() {
        let api = resolve_on("openai", Some("responses"), Some("standard"), "gpt-5.6");
        let codex = resolve_on("openai", Some("responses"), Some("chatgpt_codex"), "gpt-5.6");

        assert!(api.supports_temperature && !codex.supports_temperature);
        assert!(api.supports_top_p && !codex.supports_top_p);
        assert!(api.supports_fast && !codex.supports_fast);
    }

    /// Everything else is the model's own, inherited from the same catalog
    /// entry: the transport changes how a request is sent, not what the model
    /// can do.
    #[test]
    fn the_codex_transport_keeps_the_models_own_abilities() {
        let api = resolve_on("openai", Some("responses"), Some("standard"), "gpt-5.6");
        let codex = resolve_on("openai", Some("responses"), Some("chatgpt_codex"), "gpt-5.6");

        assert_eq!(api.supported_efforts, codex.supported_efforts);
        assert_eq!(api.supports_tools, codex.supports_tools);
        assert_eq!(api.supports_images, codex.supports_images);
        assert_eq!(api.max_context_tokens, codex.max_context_tokens);
    }

    /// The three-argument form is the whole-family answer, and it must not
    /// change for anybody.
    #[test]
    fn asking_without_a_transport_answers_as_before() {
        for (provider_type, api_format, model) in [
            ("openai", Some("responses"), "gpt-5.6"),
            ("anthropic", None, "claude-opus-4"),
            ("xai", Some("responses"), "grok-4.6"),
        ] {
            let plain = resolve(provider_type, api_format, model);
            let standard = resolve_on(provider_type, api_format, Some("standard"), model);
            assert_eq!(plain.supports_temperature, standard.supports_temperature);
            assert_eq!(plain.supports_fast, standard.supports_fast);
        }
    }

    #[test]
    fn anthropic_claude_3_haiku_no_thinking() {
        let caps = resolve("anthropic", None, "claude-3-haiku-20240307");
        assert!(!caps.supports_thinking);
        assert!(caps.supports_images);
        assert!(!caps.supports_pdf);
        assert_eq!(caps.max_output_tokens, Some(4_096));
    }

    #[test]
    fn anthropic_claude_sonnet_4_has_thinking() {
        let caps = resolve("anthropic", None, "claude-sonnet-4-20250514");
        assert!(caps.supports_thinking);
        assert!(caps.supports_images);
        assert!(caps.supports_pdf);
        assert_eq!(caps.max_output_tokens, Some(64_000));
    }

    #[test]
    fn openai_o3_no_temperature() {
        let caps = resolve("openai", None, "o3-2025-04-16");
        assert!(caps.supports_thinking);
        assert!(!caps.supported_efforts.is_empty());
        assert!(!caps.supports_temperature);
        assert!(!caps.supports_top_p);
    }

    #[test]
    fn gemini_effort_matrix_and_always_on_thinking() {
        let flash_37 = resolve("google", Some("chat_completions"), "gemini-3.7-flash");
        assert_eq!(flash_37.supported_efforts, vec!["low", "medium", "high"]);
        assert_eq!(flash_37.default_effort.as_deref(), Some("medium"));
        assert!(!flash_37.supports_thinking_off);
        assert!(!flash_37.supports_temperature);
        assert!(!flash_37.supports_top_p);
        assert_eq!(flash_37.max_context_tokens, Some(1_048_576));
        assert_eq!(flash_37.max_output_tokens, Some(65_536));

        let lite = resolve("google", None, "gemini-3.5-flash-lite-preview");
        assert_eq!(lite.supported_efforts, vec!["minimal", "low", "medium", "high"]);
        assert_eq!(lite.default_effort.as_deref(), Some("minimal"));

        let pro = resolve("google", None, "gemini-3.1-pro-preview-customtools");
        assert_eq!(pro.supported_efforts, vec!["low", "medium", "high"]);
        assert_eq!(pro.default_effort.as_deref(), Some("high"));

        let original_pro = resolve("google", None, "gemini-3-pro-preview");
        assert_eq!(original_pro.supported_efforts, vec!["low", "high"]);
        assert_eq!(original_pro.default_effort.as_deref(), Some("high"));
    }

    #[test]
    fn gemini_off_becomes_provider_default_and_sampling_is_removed() {
        let caps = resolve("google", None, "gemini-3.7-flash");
        let mut params = ChatParams {
            model: "gemini-3.7-flash".into(),
            thinking_enabled: false,
            thinking_effort: None,
            temperature: Some(0.7),
            top_p: Some(0.9),
            ..Default::default()
        };
        filter_params(&mut params, &caps).unwrap();
        assert!(params.thinking_enabled);
        assert_eq!(params.thinking_effort, None);
        assert_eq!(params.temperature, None);
        assert_eq!(params.top_p, None);
    }

    #[test]
    fn openai_gpt4o_has_vision() {
        let caps = resolve("openai", None, "gpt-4o-2024-11-20");
        assert!(caps.supports_images);
        assert!(!caps.supports_thinking);
        assert!(caps.supports_temperature);
    }

    #[test]
    fn deepseek_v4_pro_capabilities() {
        let caps = resolve("deepseek", None, "deepseek-v4-pro");
        assert!(caps.supports_thinking);
        assert!(!caps.supported_efforts.is_empty());
        assert!(!caps.supports_temperature);
        assert!(!caps.supports_top_p);
        assert_eq!(caps.max_context_tokens, Some(128_000));
    }

    #[test]
    fn deepseek_v4_flash_capabilities() {
        let caps = resolve("deepseek", None, "deepseek-v4-flash");
        assert!(caps.supports_thinking);
        assert!(!caps.supported_efforts.is_empty());
        assert!(!caps.supports_temperature);
    }

    #[test]
    fn deepseek_unknown_model_gets_default() {
        let caps = resolve("deepseek", None, "deepseek-future-model");
        assert!(caps.supports_thinking);
        assert!(!caps.supported_efforts.is_empty());
        assert!(!caps.supports_temperature);
    }

    #[test]
    fn grok_4_6_reasons_and_cannot_be_told_not_to() {
        let caps = resolve("xai", Some("chat_completions"), "grok-4.6");
        assert!(caps.supports_thinking);
        assert!(!caps.supports_thinking_off, "xAI: reasoning cannot be disabled");
        assert_eq!(caps.thinking_style, ThinkingStyle::EffortOnly);
        assert_eq!(caps.supported_efforts, vec!["low", "medium", "high", "xhigh"]);
        assert_eq!(caps.default_effort.as_deref(), Some("high"));
        assert_eq!(caps.max_context_tokens, Some(500_000));
        assert!(caps.supports_images);
        assert!(caps.supports_temperature, "measured: 0.7 is accepted");
        assert!(!caps.supports_fast, "xAI has no priority tier");
    }

    /// Turning thinking off has no wire value here, so the request must not
    /// simply omit the effort — `filter_params` turns it back on and lets the
    /// model default apply, the same as Gemini.
    #[test]
    fn asking_grok_not_to_think_yields_the_model_default() {
        let caps = resolve("xai", None, "grok-4.6");
        let mut params = ChatParams {
            model: "grok-4.6".into(),
            thinking_enabled: false,
            temperature: Some(0.7),
            ..Default::default()
        };
        filter_params(&mut params, &caps).unwrap();
        assert!(params.thinking_enabled);
        assert_eq!(params.thinking_effort, None);
        assert_eq!(params.temperature, Some(0.7), "sampling is fine on Grok");
    }

    /// 4.5 does not expose `xhigh`; passing it is a contract error instead of
    /// silently selecting a different effort.
    #[test]
    fn grok_4_5_rejects_xhigh() {
        let caps = resolve("xai", None, "grok-4.5");
        assert_eq!(caps.supported_efforts, vec!["low", "medium", "high"]);
        let mut params = ChatParams {
            model: "grok-4.5".into(),
            thinking_enabled: true,
            thinking_effort: Some("xhigh".into()),
            ..Default::default()
        };
        assert!(filter_params(&mut params, &caps).is_err());
    }

    /// The aliases and the real id are the same model, and both are things a
    /// user can pick out of the model list.
    #[test]
    fn both_names_for_grok_code_fast_resolve_alike() {
        let by_alias = resolve("xai", None, "grok-code-fast-1");
        let by_id = resolve("xai", None, "grok-build-0.1");
        assert_eq!(by_alias.max_context_tokens, Some(256_000));
        assert_eq!(by_id.max_context_tokens, by_alias.max_context_tokens);
        assert_eq!(by_id.supported_efforts, by_alias.supported_efforts);
    }

    /// A Grok nobody has catalogued yet still reasons, and still gets a context
    /// limit small enough that the provider will accept the request.
    #[test]
    fn an_uncatalogued_grok_keeps_reasoning_and_the_smallest_window() {
        let caps = resolve("xai", None, "grok-5-something");
        assert!(caps.supports_thinking);
        assert!(!caps.supported_efforts.is_empty());
        assert_eq!(caps.max_context_tokens, Some(256_000));
    }

    /// Server-side tools exist only on the Responses API. Offering the switch on
    /// chat-completions would be offering a setting that turns every request
    /// into a 422 — measured: xAI answers "expected `function` or `live_search`".
    #[test]
    fn server_tools_are_a_responses_api_thing_only() {
        let responses = resolve("xai", Some("responses"), "grok-4.6");
        assert_eq!(
            responses.server_tools,
            vec![
                ServerToolKind::WebSearch,
                ServerToolKind::XSearch,
                ServerToolKind::CodeExecution,
            ]
        );

        for format in [Some("chat_completions"), None] {
            assert!(
                resolve("xai", format, "grok-4.6").server_tools.is_empty(),
                "chat-completions has no such thing",
            );
        }
    }

    /// DeepSeek's compatibility table lists `function` and `web_search` as the
    /// supported tool types and says the rest are ignored.
    #[test]
    fn deepseek_offers_only_the_one_it_documents() {
        let caps = resolve("deepseek", Some("responses"), "deepseek-v4-flash");
        assert_eq!(caps.server_tools, vec![ServerToolKind::WebSearch]);
    }

    /// A provider nobody has measured gets none, rather than a guess. A wrong
    /// tool name is a 400 on every request the setting is on for.
    #[test]
    fn an_unmeasured_provider_is_offered_none() {
        assert!(resolve("openai", Some("responses"), "gpt-5.6").server_tools.is_empty());
        assert!(
            resolve("anthropic", Some("responses"), "claude-opus-4-8")
                .server_tools
                .is_empty()
        );
    }

    /// The Messages API documents its own search tool, on every generation
    /// under one dated name or another.
    #[test]
    fn anthropic_offers_web_search_on_its_own_api() {
        assert_eq!(
            resolve("anthropic", None, "claude-opus-4-8").server_tools,
            vec![ServerToolKind::WebSearch]
        );
        assert_eq!(
            resolve("anthropic", None, "claude-sonnet-4-20250514").server_tools,
            vec![ServerToolKind::WebSearch]
        );
    }

    /// The escape hatch for a model whose support differs from its provider's
    /// default — including taking them all away.
    #[test]
    fn an_override_can_reshape_the_server_tool_list() {
        let mut caps = resolve("xai", Some("responses"), "grok-4.6");
        apply_overrides(&mut caps, Some(r#"{"server_tools":["web_search"]}"#)).unwrap();
        assert_eq!(caps.server_tools, vec![ServerToolKind::WebSearch]);

        apply_overrides(&mut caps, Some(r#"{"server_tools":[]}"#)).unwrap();
        assert!(caps.server_tools.is_empty());
    }

    #[test]
    fn unknown_model_gets_generic_defaults() {
        let caps = resolve("openai", None, "some-custom-model-v2");
        assert!(caps.supports_temperature);
        assert!(caps.supports_top_p);
        assert!(!caps.supports_images);
        assert!(!caps.supports_thinking);
    }

    #[test]
    fn filter_params_strips_temperature_for_reasoning() {
        let caps = resolve("openai", None, "o3-2025-04-16");
        let mut params = ChatParams {
            model: "o3-2025-04-16".into(),
            temperature: Some(0.7),
            top_p: Some(0.9),
            thinking_enabled: true,
            thinking_budget: Some(10000),
            thinking_effort: Some("high".into()),
            ..Default::default()
        };
        filter_params(&mut params, &caps).unwrap();
        assert!(params.temperature.is_none());
        assert!(params.top_p.is_none());
        assert!(params.thinking_enabled);
        assert_eq!(params.thinking_effort, Some("high".into()));
    }

    #[test]
    fn filter_params_strips_thinking_for_non_thinking_model() {
        let caps = resolve("openai", None, "gpt-4o");
        let mut params = ChatParams {
            model: "gpt-4o".into(),
            temperature: Some(0.7),
            thinking_enabled: true,
            thinking_budget: Some(10000),
            thinking_effort: Some("high".into()),
            ..Default::default()
        };
        let error = filter_params(&mut params, &caps).unwrap_err();
        assert!(error.contains("not supported by model 'gpt-4o'"));
    }

    #[test]
    fn filter_params_clamps_temperature() {
        let caps = resolve("anthropic", None, "claude-sonnet-4-20250514");
        let mut params = ChatParams {
            model: "claude-sonnet-4-20250514".into(),
            temperature: Some(1.5),
            ..Default::default()
        };
        filter_params(&mut params, &caps).unwrap();
        assert_eq!(params.temperature, Some(1.0));
    }

    #[test]
    fn longest_prefix_wins() {
        let caps_35_sonnet = resolve("anthropic", None, "claude-3-5-sonnet-20241022");
        assert!(!caps_35_sonnet.supports_thinking);
        assert!(caps_35_sonnet.supports_pdf);
        assert_eq!(caps_35_sonnet.max_output_tokens, Some(8_192));

        let caps_3_sonnet = resolve("anthropic", None, "claude-3-sonnet-20240229");
        assert!(!caps_3_sonnet.supports_thinking);
        assert!(!caps_3_sonnet.supports_pdf);
        assert_eq!(caps_3_sonnet.max_output_tokens, Some(4_096));
    }

    // --- new coverage ---

    #[test]
    fn gpt_5_6_sol_full_effort_ladder() {
        let caps = resolve("openai", Some("responses"), "gpt-5.6-sol");
        assert!(caps.supports_thinking);
        assert_eq!(caps.thinking_style, ThinkingStyle::EffortOnly);
        assert_eq!(caps.supported_efforts, vec!["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(caps.default_effort, Some("low".into()));
        assert!(caps.supports_fast);
        assert!(caps.supports_verbosity);
    }

    #[test]
    fn bare_gpt_5_6_alias_resolves_like_sol() {
        let caps = resolve("openai", Some("responses"), "gpt-5.6");
        assert_eq!(caps.supported_efforts, vec!["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(caps.default_effort, Some("low".into()));
    }

    #[test]
    fn gpt_5_2_has_no_max_tier_and_no_fast() {
        let caps = resolve("openai", Some("responses"), "gpt-5.2");
        assert_eq!(caps.supported_efforts, vec!["low", "medium", "high", "xhigh"]);
        assert!(!caps.supports_fast);
    }

    #[test]
    fn claude_opus_4_8_is_adaptive_and_rejects_sampling() {
        let caps = resolve("anthropic", None, "claude-opus-4-8");
        assert_eq!(caps.thinking_style, ThinkingStyle::Adaptive);
        assert!(!caps.supports_temperature);
        assert!(!caps.supports_top_p);
        assert!(caps.supports_fast);
        assert_eq!(caps.supported_efforts, vec!["low", "medium", "high", "xhigh", "max"]);
    }

    #[test]
    fn claude_opus_4_6_has_no_xhigh() {
        let caps = resolve("anthropic", None, "claude-opus-4-6");
        assert_eq!(caps.supported_efforts, vec!["low", "medium", "high", "max"]);
        assert!(caps.supports_temperature);
        assert!(!caps.supports_fast);
    }

    #[test]
    fn claude_fable_5_is_always_on() {
        let caps = resolve("anthropic", None, "claude-fable-5");
        assert_eq!(caps.thinking_style, ThinkingStyle::AlwaysOn);
        assert!(!caps.supports_temperature);
    }

    #[test]
    fn claude_haiku_4_5_has_no_effort() {
        let caps = resolve("anthropic", None, "claude-haiku-4-5");
        assert!(caps.supports_thinking);
        assert!(caps.supported_efforts.is_empty());
    }

    #[test]
    fn adaptive_model_drops_thinking_budget() {
        let caps = resolve("anthropic", None, "claude-opus-4-8");
        let mut params = ChatParams {
            model: "claude-opus-4-8".into(),
            temperature: Some(0.7),
            thinking_enabled: true,
            thinking_budget: Some(10_000),
            thinking_effort: Some("xhigh".into()),
            ..Default::default()
        };
        filter_params(&mut params, &caps).unwrap();
        assert!(params.temperature.is_none(), "sampling params are a 400 on Opus 4.7+");
        assert!(
            params.thinking_budget.is_none(),
            "budget_tokens is a 400 on adaptive models"
        );
        assert_eq!(params.thinking_effort, Some("xhigh".into()));
    }

    #[test]
    fn budget_model_keeps_budget_tokens() {
        let caps = resolve("anthropic", None, "claude-sonnet-4-20250514");
        let mut params = ChatParams {
            model: "claude-sonnet-4-20250514".into(),
            thinking_enabled: true,
            thinking_budget: Some(10_000),
            ..Default::default()
        };
        filter_params(&mut params, &caps).unwrap();
        assert_eq!(params.thinking_budget, Some(10_000));
    }

    #[test]
    fn unsupported_effort_is_rejected() {
        // gpt-5.2 tops out at xhigh; "max" must not be changed behind the
        // caller's back or passed through to a provider 400.
        let caps = resolve("openai", Some("responses"), "gpt-5.2");
        let mut params = ChatParams {
            model: "gpt-5.2".into(),
            thinking_enabled: true,
            thinking_effort: Some("max".into()),
            ..Default::default()
        };
        assert!(filter_params(&mut params, &caps).is_err());
    }

    #[test]
    fn fast_is_stripped_when_unsupported() {
        let caps = resolve("openai", Some("responses"), "gpt-5.2");
        let mut params = ChatParams {
            model: "gpt-5.2".into(),
            fast: true,
            ..Default::default()
        };
        filter_params(&mut params, &caps).unwrap();
        assert!(!params.fast);
    }

    #[test]
    fn verbosity_defaults_from_catalog_and_is_stripped_when_unsupported() {
        let caps = resolve("openai", Some("responses"), "gpt-5.6-sol");
        let mut params = ChatParams {
            model: "gpt-5.6-sol".into(),
            ..Default::default()
        };
        filter_params(&mut params, &caps).unwrap();
        assert_eq!(params.verbosity, Some("low".into()));

        let caps = resolve("anthropic", None, "claude-opus-4-8");
        let mut params = ChatParams {
            model: "claude-opus-4-8".into(),
            verbosity: Some("high".into()),
            ..Default::default()
        };
        filter_params(&mut params, &caps).unwrap();
        assert!(params.verbosity.is_none(), "Anthropic has no verbosity parameter");
    }

    #[test]
    fn gemma_tool_does_not_inherit_openai_rules() {
        // A gemma_tool provider serving a model called "gpt-4o" is not GPT-4o.
        let caps = resolve("openai", Some("gemma_tool"), "gpt-4o");
        assert!(!caps.supports_images);
        assert!(!caps.supports_thinking);
        assert!(caps.supported_efforts.is_empty());
    }

    #[test]
    fn responses_format_gets_reasoning_defaults() {
        let caps = resolve("openai", Some("responses"), "some-unknown-reasoning-model");
        assert!(caps.supports_thinking);
        assert!(!caps.supported_efforts.is_empty());
        assert_eq!(caps.thinking_style, ThinkingStyle::EffortOnly);
    }

    #[test]
    fn overrides_patch_catalog() {
        let mut caps = resolve("anthropic", None, "claude-haiku-4-5");
        assert!(caps.supported_efforts.is_empty());
        apply_overrides(
            &mut caps,
            Some(r#"{"supported_efforts":["high","low"],"supports_fast":true}"#),
        )
        .unwrap();
        // Rebuilt through the ladder, so ascending order regardless of input order.
        assert_eq!(caps.supported_efforts, vec!["low", "high"]);
        assert!(caps.supports_fast);
    }

    #[test]
    fn thinking_request_rejects_unknown_and_non_request_levels() {
        assert_eq!(
            resolve_thinking(true, Some(1024), None).unwrap(),
            (true, Some(1024), None)
        );
        assert_eq!(
            resolve_thinking(true, Some(1024), Some("off")).unwrap(),
            (false, None, None)
        );
        assert_eq!(
            resolve_thinking(false, Some(1024), Some("high")).unwrap(),
            (true, Some(1024), Some("high".into()))
        );
        for level in ["default", "none", "future", " high"] {
            assert!(
                resolve_thinking(true, Some(1024), Some(level)).is_err(),
                "accepted {level}"
            );
        }
    }

    #[test]
    fn malformed_and_unknown_overrides_are_rejected_atomically() {
        let mut caps = resolve("anthropic", None, "claude-opus-4-8");
        let before = caps.clone();
        for raw in [
            "not json at all",
            "[1,2,3]",
            r#"{"future_capability":true}"#,
            r#"{"supports_tools":"yes"}"#,
            r#"{"supports_tools":null}"#,
            r#"{"supported_efforts":["low","future"]}"#,
            r#"{"supported_efforts":["low",7]}"#,
            r#"{"supported_efforts":["low","low"]}"#,
            r#"{"server_tools":["web_search",7]}"#,
            r#"{"server_tools":["future_search"]}"#,
        ] {
            assert!(apply_overrides(&mut caps, Some(raw)).is_err(), "accepted {raw}");
            assert_eq!(caps.supports_tools, before.supports_tools);
            assert_eq!(caps.supported_efforts, before.supported_efforts);
            assert_eq!(caps.server_tools, before.server_tools);
        }
        apply_overrides(&mut caps, None).unwrap();
    }
}

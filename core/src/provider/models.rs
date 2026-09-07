use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use super::ProviderError;
use super::dto::{ExtraIgnore, warn_extra_fields};
use crate::client::{HttpTransport, Request, ReqwestTransport};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
}

pub async fn fetch_models(
    provider_type: &str,
    api_format: Option<&str>,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<ModelInfo>, ProviderError> {
    fetch_models_on(provider_type, api_format, None, base_url, api_key).await
}

/// The same, told which wire the provider is configured for.
///
/// Split out because the ChatGPT backend has no `/models` at all: asking it
/// yields a 404, and the set of models a subscription may reach is both
/// narrower than the API's and different per account. Measured — `gpt-5.4`
/// answers *"not supported when using Codex with a ChatGPT account"* while the
/// model in the CLI's own config succeeds.
pub async fn fetch_models_on(
    provider_type: &str,
    api_format: Option<&str>,
    transport_profile: Option<&str>,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<ModelInfo>, ProviderError> {
    let provider_type = super::registry::ProviderType::parse(provider_type).map_err(ProviderError::Parse)?;
    let api_format = api_format
        .map(super::registry::ApiFormat::parse)
        .transpose()
        .map_err(ProviderError::Parse)?;
    let transport_profile = transport_profile
        .map(super::registry::TransportProfile::parse)
        .transpose()
        .map_err(ProviderError::Parse)?;
    if transport_profile == Some(super::registry::TransportProfile::ChatgptCodex) {
        if provider_type != super::registry::ProviderType::Openai {
            return Err(ProviderError::Parse(
                "the ChatGPT Codex transport requires provider type `openai`".into(),
            ));
        }
        return Ok(codex_models());
    }
    match provider_type {
        super::registry::ProviderType::Anthropic => fetch_anthropic_models(base_url, api_key).await,
        super::registry::ProviderType::Xai => {
            let models = fetch_openai_models(base_url, api_key).await?;
            Ok(models
                .into_iter()
                .filter(|model| is_xai_text_model(&model.id))
                .collect())
        }
        super::registry::ProviderType::Google => {
            let models = if api_format == Some(super::registry::ApiFormat::GeminiGenerateContent) {
                fetch_google_models(base_url, api_key).await?
            } else {
                fetch_openai_models(base_url, api_key).await?
            };
            Ok(models
                .into_iter()
                .filter(|model| is_google_agent_model(&model.id))
                .collect())
        }
        super::registry::ProviderType::Openai | super::registry::ProviderType::Deepseek => {
            fetch_openai_models(base_url, api_key).await
        }
    }
}

/// What a ChatGPT subscription can reach, best-effort.
///
/// **The CLI's own configured model comes first**, and that is the important
/// part rather than a nicety: the accepted set moves, differs by plan, and is
/// published nowhere. Whatever `codex` is set to is a model this account has
/// been able to use, which is a better guess than anything shipped in a binary.
///
/// The rest is a short list of families that have been available on this
/// backend. It is a starting point for the picker, not an authority — a model
/// typed by hand works just as well, and one listed here may still be refused.
fn codex_models() -> Vec<ModelInfo> {
    let mut ids: Vec<String> = Vec::new();
    if let Some(configured) = crate::codex_auth::storage::find_codex_home().and_then(|home| configured_model(&home)) {
        ids.push(configured);
    }
    for fallback in ["gpt-5.6", "gpt-5.6-terra", "gpt-5.5", "gpt-5.1-codex"] {
        if !ids.iter().any(|id| id == fallback) {
            ids.push(fallback.to_string());
        }
    }
    ids.into_iter().map(|id| ModelInfo { name: id.clone(), id }).collect()
}

/// The `model` line from the CLI's `config.toml`.
///
/// Parsed by hand rather than with a TOML crate: one scalar at the top level is
/// not worth a dependency, and a file this cannot understand simply yields
/// nothing — the fallback list still applies.
fn configured_model(codex_home: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(codex_home.join("config.toml")).ok()?;
    text.lines()
        .map(str::trim)
        // Only before the first table header: `model` under `[profiles.x]` is
        // that profile's, not the active one.
        .take_while(|line| !line.starts_with('['))
        .filter_map(|line| line.strip_prefix("model")?.trim().strip_prefix('='))
        .map(|value| value.trim().trim_matches('"').to_string())
        .find(|value| !value.is_empty())
}

fn is_google_agent_model(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    let id = lower.strip_prefix("models/").unwrap_or(&lower);
    if !id.starts_with("gemini-3") {
        return false;
    }
    if ["image", "live", "tts", "audio", "embedding", "embed"]
        .iter()
        .any(|part| id.contains(part))
    {
        return false;
    }
    id.contains("-flash-lite") || id.contains("-flash") || id.contains("-pro")
}

/// xAI answers `/v1/models` with its image and video models mixed in among the
/// Grok ones, and nothing in the OpenAI-compatible shape distinguishes them —
/// the modality fields only exist on their own `/v1/language-models`. Offering
/// `grok-imagine-video` in a chat model picker is a turn that fails at the
/// first request, so the family names are matched instead.
fn is_xai_text_model(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    !["imagine", "image", "video", "embed"]
        .iter()
        .any(|part| id.contains(part))
}

#[cfg(test)]
mod tests {
    use super::{OpenAIModelsResponse, configured_model, is_google_agent_model, is_xai_text_model};

    /// The documented `/v1/models` shape must not feed the ignored-fields
    /// warning: fired on every fetch, that warning stops meaning anything. A
    /// field the docs do not name still lands in `extra`.
    #[test]
    fn a_standard_openai_model_list_reports_no_ignored_fields() {
        let standard: OpenAIModelsResponse = serde_json::from_str(
            r#"{"object":"list","data":[{"id":"gpt-4.1","object":"model","created":1,"owned_by":"openai"}]}"#,
        )
        .unwrap();
        assert!(standard.extra.is_empty());
        assert!(standard.data[0].extra.is_empty());

        let novel: OpenAIModelsResponse =
            serde_json::from_str(r#"{"data":[{"id":"m","context_length":8192}]}"#).unwrap();
        assert_eq!(novel.data[0].extra.keys().collect::<Vec<_>>(), ["context_length"]);
    }

    fn config_with(body: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), body).unwrap();
        dir
    }

    /// The CLI's own model is the best evidence available of what this account
    /// can reach — the accepted set moves, differs by plan and is published
    /// nowhere.
    #[test]
    fn the_configured_model_is_read_from_the_cli_config() {
        let dir = config_with("model = \"gpt-5.6-terra\"\nmodel_reasoning_effort = \"xhigh\"\n");
        assert_eq!(configured_model(dir.path()).as_deref(), Some("gpt-5.6-terra"));
    }

    /// A `model` under a profile table belongs to that profile, not to the
    /// active configuration — reading it would offer a model the user is not
    /// actually set up to use.
    #[test]
    fn a_profiles_model_is_not_mistaken_for_the_active_one() {
        let dir = config_with("model = \"top-level\"\n\n[profiles.other]\nmodel = \"not-this-one\"\n");
        assert_eq!(configured_model(dir.path()).as_deref(), Some("top-level"));

        let only_profile = config_with("[profiles.other]\nmodel = \"not-this-one\"\n");
        assert_eq!(configured_model(only_profile.path()), None);
    }

    /// A file we cannot understand costs the hint, not the feature: the
    /// fallback list still applies.
    #[test]
    fn an_unreadable_config_yields_nothing_rather_than_failing() {
        assert_eq!(configured_model(std::path::Path::new("/nonexistent")), None);
        let empty = config_with("# just a comment\n");
        assert_eq!(configured_model(empty.path()), None);
        let blank = config_with("model = \"\"\n");
        assert_eq!(configured_model(blank.path()), None);
    }

    #[test]
    fn xai_filter_keeps_grok_and_drops_the_other_modalities() {
        assert!(is_xai_text_model("grok-4.6"));
        assert!(is_xai_text_model("grok-4.20-0309-non-reasoning"));
        assert!(is_xai_text_model("grok-build-0.1"));
        assert!(!is_xai_text_model("grok-imagine-image-2.0"));
        assert!(!is_xai_text_model("grok-imagine-video-1.5"));
    }

    #[test]
    fn google_filter_keeps_general_gemini_3_models() {
        assert!(is_google_agent_model("gemini-3.7-flash"));
        assert!(is_google_agent_model("gemini-3.5-flash-lite"));
        assert!(is_google_agent_model("gemini-3.1-pro-preview-customtools"));
        assert!(is_google_agent_model("models/gemini-3-flash-preview"));
    }

    #[test]
    fn google_filter_drops_other_families_and_modalities() {
        assert!(!is_google_agent_model("gemini-2.5-flash"));
        assert!(!is_google_agent_model("gemini-3-pro-image-preview"));
        assert!(!is_google_agent_model("gemini-3-live-preview"));
        assert!(!is_google_agent_model("gemini-embedding-001"));
    }
}

// The fields OpenAI documents on `/v1/models` and reads nothing from are named
// here as `IgnoredAny` rather than left to `extra`: the warning `extra` feeds
// is for shapes this code has not seen, and a standard reply tripping it on
// every fetch is a warning nobody reads. `default`, because relays omit them.
#[derive(Deserialize)]
struct OpenAIModelsResponse {
    data: Vec<OpenAIModel>,
    #[serde(default, rename = "object")]
    _object: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Deserialize)]
struct OpenAIModel {
    id: String,
    #[serde(default, rename = "object")]
    _object: IgnoredAny,
    #[serde(default, rename = "created")]
    _created: IgnoredAny,
    #[serde(default, rename = "owned_by")]
    _owned_by: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoogleModelsResponse {
    models: Vec<GoogleModel>,
    next_page_token: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoogleModel {
    name: String,
    display_name: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

async fn fetch_google_models(base_url: &str, api_key: &str) -> Result<Vec<ModelInfo>, ProviderError> {
    let root = super::google_generate_content::google_api_root(base_url);
    let transport = ReqwestTransport::shared();
    let mut page_token = None::<String>;
    let mut seen_tokens = HashSet::new();
    let mut all_models = Vec::new();
    loop {
        let url = match page_token.as_deref() {
            Some(token) => format!(
                "{root}/v1beta/models?pageToken={}",
                percent_encoding::utf8_percent_encode(token, percent_encoding::NON_ALPHANUMERIC)
            ),
            None => format!("{root}/v1beta/models"),
        };
        let mut req = Request::new(http::Method::GET, url);
        req.headers.insert("x-goog-api-key", super::auth_header_value(api_key));
        let resp = transport.execute(req).await.inspect_err(|error| {
            tracing::error!(api = "gemini", error = %error, "could not fetch the model list");
        })?;
        let parsed: GoogleModelsResponse = serde_json::from_slice(&resp.body).map_err(|error| {
            tracing::warn!(
                api = "gemini",
                body_len = resp.body.len(),
                error = %error,
                "the model list response was not in the expected shape"
            );
            ProviderError::Parse(error.to_string())
        })?;
        warn_extra_fields("gemini_models_response", &parsed.extra);
        for model in &parsed.models {
            warn_extra_fields("gemini_model", &model.extra);
        }
        all_models.extend(parsed.models);
        let Some(next) = parsed.next_page_token.filter(|token| !token.is_empty()) else {
            break;
        };
        if !seen_tokens.insert(next.clone()) {
            return Err(ProviderError::Parse("Gemini model list repeated a page token".into()));
        }
        page_token = Some(next);
    }
    let mut models = all_models
        .into_iter()
        .map(|model| ModelInfo {
            id: model.name.clone(),
            name: model.display_name.unwrap_or(model.name),
        })
        .collect::<Vec<_>>();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(models)
}

async fn fetch_openai_models(base_url: &str, api_key: &str) -> Result<Vec<ModelInfo>, ProviderError> {
    let base_url = base_url.trim_end_matches('/');
    let transport = ReqwestTransport::shared();

    let mut req = Request::new(http::Method::GET, format!("{base_url}/models"));
    req.headers.insert(
        http::header::AUTHORIZATION,
        super::auth_header_value(&format!("Bearer {api_key}")),
    );

    // The HTTP failure itself is recorded by the transport; what that cannot say
    // is that this was the model list — the first button pressed after adding a
    // provider, and where a wrong base URL or key usually shows up.
    let resp = transport.execute(req).await.inspect_err(|e| {
        tracing::error!(api = "openai", error = %e, "could not fetch the model list");
    })?;
    let parsed: OpenAIModelsResponse = serde_json::from_slice(&resp.body).map_err(|e| {
        // Relays often answer /models with a non-standard shape, and the user
        // just sees an empty dropdown with no error at all.
        tracing::warn!(
            api = "openai",
            body_len = resp.body.len(),
            error = %e,
            "the model list response was not in the expected shape"
        );
        ProviderError::Parse(e.to_string())
    })?;
    warn_extra_fields("openai_models_response", &parsed.extra);
    for model in &parsed.data {
        warn_extra_fields("openai_model", &model.extra);
    }

    let mut models: Vec<ModelInfo> = parsed
        .data
        .into_iter()
        .map(|m| ModelInfo {
            name: m.id.clone(),
            id: m.id,
        })
        .collect();

    models.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(models)
}

#[derive(Deserialize)]
struct AnthropicModelsResponse {
    data: Vec<AnthropicModel>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Deserialize)]
struct AnthropicModel {
    id: String,
    display_name: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

async fn fetch_anthropic_models(base_url: &str, api_key: &str) -> Result<Vec<ModelInfo>, ProviderError> {
    let base_url = base_url.trim_end_matches('/');
    let transport = ReqwestTransport::shared();

    let mut req = Request::new(http::Method::GET, format!("{base_url}/v1/models"));
    req.headers.insert("x-api-key", super::auth_header_value(api_key));
    req.headers.insert("anthropic-version", "2023-06-01".parse().unwrap());

    let resp = transport.execute(req).await.inspect_err(|e| {
        tracing::error!(api = "anthropic", error = %e, "could not fetch the model list");
    })?;
    let parsed: AnthropicModelsResponse = serde_json::from_slice(&resp.body).map_err(|e| {
        tracing::warn!(
            api = "anthropic",
            body_len = resp.body.len(),
            error = %e,
            "the model list response was not in the expected shape"
        );
        ProviderError::Parse(e.to_string())
    })?;
    warn_extra_fields("anthropic_models_response", &parsed.extra);
    for model in &parsed.data {
        warn_extra_fields("anthropic_model", &model.extra);
    }

    let mut models: Vec<ModelInfo> = parsed
        .data
        .into_iter()
        .map(|m| ModelInfo {
            name: m.display_name.unwrap_or_else(|| m.id.clone()),
            id: m.id,
        })
        .collect();

    models.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(models)
}

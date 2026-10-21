use super::ChatProvider;
use super::ProviderCapabilities;
use super::anthropic::AnthropicProvider;
use super::deepseek::DeepSeekProvider;
use super::gemma_tool::GemmaToolProvider;
use super::google_generate_content::GoogleGenerateContentProvider;
use super::google_interactions::GoogleInteractionsProvider;
use super::openai_compat::OpenAICompatProvider;
use super::openai_responses::OpenAIResponsesProvider;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::EnumIter,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ApiFormat {
    ChatCompletions,
    Responses,
    GeminiGenerateContent,
    GeminiInteractions,
    GemmaTool,
    LitertLm,
}

impl ApiFormat {
    pub fn parse(value: &str) -> Result<Self, String> {
        value
            .parse()
            .map_err(|_| format!("unknown provider API format `{value}`"))
    }

    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::EnumIter,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ProviderType {
    Openai,
    Anthropic,
    Deepseek,
    Xai,
    Google,
    LitertLm,
}

impl ProviderType {
    pub fn parse(value: &str) -> Result<Self, String> {
        value.parse().map_err(|_| format!("unknown provider type `{value}`"))
    }

    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::EnumIter,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TransportProfile {
    Standard,
    ChatgptCodex,
    LocalNative,
}

impl TransportProfile {
    pub fn parse(value: &str) -> Result<Self, String> {
        value
            .parse()
            .map_err(|_| format!("unknown provider transport profile `{value}`"))
    }

    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::EnumIter,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum CredentialKind {
    ApiKey,
    GoogleVertexServiceAccount,
    CodexCli,
    ChatgptOauth,
    None,
}

impl CredentialKind {
    pub fn parse(value: &str) -> Result<Self, String> {
        value
            .parse()
            .map_err(|_| format!("unknown provider credential kind `{value}`"))
    }

    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::EnumIter,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum LocalInferenceBackend {
    Auto,
    Cpu,
    Gpu,
    Npu,
}

impl LocalInferenceBackend {
    pub fn parse(value: &str) -> Result<Self, String> {
        value
            .parse()
            .map_err(|_| format!("unknown local inference backend `{value}`"))
    }

    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn native(self) -> Option<litert_lm::engine::Backend> {
        use litert_lm::engine::Backend;
        match self {
            Self::Auto => None,
            Self::Cpu => Some(Backend::Cpu),
            Self::Gpu => Some(Backend::Gpu),
            Self::Npu => Some(Backend::Npu),
        }
    }
}

pub fn validate_local_backend(
    provider_type: ProviderType,
    backend: Option<LocalInferenceBackend>,
) -> Result<(), String> {
    match (provider_type, backend) {
        (ProviderType::LitertLm, Some(_)) => Ok(()),
        (ProviderType::LitertLm, None) => Err("LiteRT-LM requires an explicit local_backend setting".into()),
        (_, None) => Ok(()),
        (_, Some(_)) => Err("local_backend must be null for a non-LiteRT provider".into()),
    }
}

fn validate_transport_profile(value: &str) -> Result<(), String> {
    TransportProfile::parse(value).map(|_| ())
}

fn validate_wire_pair(api_format: ApiFormat, transport_profile: &str) -> Result<(), String> {
    match (api_format, transport_profile) {
        (ApiFormat::Responses, "chatgpt_codex") | (_, "standard") => Ok(()),
        (ApiFormat::LitertLm, "local_native") => Ok(()),
        (_, "local_native") => Err("the local_native transport requires the `litert_lm` API format".into()),
        (_, "chatgpt_codex") => Err("the ChatGPT Codex transport requires the `responses` API format".into()),
        (_, other) => Err(format!("unknown provider transport profile `{other}`")),
    }
}

/// Validate the two closed wire selectors stored on every provider row.
pub fn validate_contract(api_format: &str, transport_profile: &str) -> Result<(), String> {
    let api_format = ApiFormat::parse(api_format)?;
    validate_transport_profile(transport_profile)?;
    validate_wire_pair(api_format, transport_profile)
}

/// Validate the complete first-party contract stored on a provider row.
///
/// Login kind and transport are one choice split across two columns. Accepting
/// either independently permits rows that can never authenticate: an API key
/// sent to ChatGPT's Codex backend, or a ChatGPT session sent to an API-key
/// endpoint. Keep the valid triples closed here so every writer and reader can
/// ask the same question.
pub fn validate_stored_contract(
    provider_type: &str,
    api_format: &str,
    transport_profile: &str,
    credential_kind: &str,
) -> Result<(), String> {
    let provider_type = ProviderType::parse(provider_type)?;
    let api_format = ApiFormat::parse(api_format)?;
    let transport_profile = TransportProfile::parse(transport_profile)?;
    let credential_kind = CredentialKind::parse(credential_kind)?;
    validate_wire_pair(api_format, transport_profile.as_str())?;

    match (transport_profile, credential_kind) {
        (TransportProfile::Standard, CredentialKind::ApiKey)
        | (TransportProfile::Standard, CredentialKind::GoogleVertexServiceAccount)
        | (TransportProfile::ChatgptCodex, CredentialKind::CodexCli | CredentialKind::ChatgptOauth)
        | (TransportProfile::LocalNative, CredentialKind::None) => {}
        _ => {
            return Err(format!(
                "credential kind `{}` is not valid with transport profile `{}`",
                credential_kind.as_str(),
                transport_profile.as_str()
            ));
        }
    }

    if transport_profile == TransportProfile::ChatgptCodex && provider_type != ProviderType::Openai {
        return Err("the ChatGPT Codex transport requires provider type `openai`".into());
    }

    if credential_kind == CredentialKind::GoogleVertexServiceAccount
        && (provider_type != ProviderType::Google || api_format != ApiFormat::GeminiInteractions)
    {
        return Err(
            "the Google Vertex Service Account credential requires provider type `google` and the `gemini_interactions` API format"
                .into(),
        );
    }

    let supported = match provider_type {
        ProviderType::Anthropic => api_format == ApiFormat::ChatCompletions,
        ProviderType::Deepseek | ProviderType::Xai => {
            matches!(api_format, ApiFormat::ChatCompletions | ApiFormat::Responses)
        }
        ProviderType::Google => {
            matches!(
                api_format,
                ApiFormat::ChatCompletions | ApiFormat::GeminiGenerateContent | ApiFormat::GeminiInteractions
            )
        }
        ProviderType::Openai => {
            matches!(
                api_format,
                ApiFormat::ChatCompletions | ApiFormat::Responses | ApiFormat::GemmaTool
            )
        }
        ProviderType::LitertLm => api_format == ApiFormat::LitertLm,
    };
    if supported {
        Ok(())
    } else {
        Err(format!(
            "API format `{}` is not supported by provider type `{}`",
            api_format.as_str(),
            provider_type.as_str()
        ))
    }
}

fn validate_runtime_credential(
    provider_type: ProviderType,
    api_format: ApiFormat,
    transport_profile: &str,
    credential: &super::Credential,
) -> Result<(), String> {
    match (transport_profile, credential) {
        ("standard", super::Credential::ApiKey(_))
        | ("chatgpt_codex", super::Credential::ChatGpt(_))
        | ("local_native", super::Credential::None) => Ok(()),
        ("standard", super::Credential::GoogleVertexServiceAccount(_))
            if provider_type == ProviderType::Google && api_format == ApiFormat::GeminiInteractions =>
        {
            Ok(())
        }
        ("standard", super::Credential::ChatGpt(_)) => {
            Err("the standard provider transport requires an API-key credential".into())
        }
        ("standard", super::Credential::GoogleVertexServiceAccount(_)) => {
            Err("the Google Vertex Service Account credential requires the Google Interactions API".into())
        }
        ("chatgpt_codex", super::Credential::ApiKey(_)) => {
            Err("the ChatGPT Codex transport requires a ChatGPT login credential".into())
        }
        ("local_native", _) => Err("the local_native transport does not use credentials".into()),
        (other, _) => Err(format!("unknown provider transport profile `{other}`")),
    }
}

/// Everything about a provider row that decides what goes on the wire.
///
/// A struct rather than five positional arguments plus a bare `bool`, and the
/// bool is the reason. `codex_request_shape` is off for almost every row, so a
/// caller that forgot to pass it would be indistinguishable from one that meant
/// `false` — and the sites that build a provider are scattered across the
/// summariser, the reviewer, OneBot, the sub-agent runner and three commands.
/// Named fields make the omission a compile error instead.
pub struct ProviderWire<'a> {
    pub provider_type: &'a str,
    pub base_url: &'a str,
    pub credential: &'a super::Credential,
    pub api_format: &'a str,
    pub transport_profile: &'a str,
    /// Whether to send exactly what Codex sends. See
    /// [`super::openai_responses::CodexShape`] and migration 63.
    pub codex_request_shape: bool,
    /// The user's `codex.client_version` override, read beside the row.
    ///
    /// Global rather than per-row: it answers "which Codex release does this
    /// app claim to be", and a second answer per provider would be two
    /// identities for one install. Meaningless unless the shape is on.
    pub codex_client_version: Option<&'a str>,
    pub local_backend: Option<LocalInferenceBackend>,
}

/// Pick the adapter for a provider row.
///
/// Keyed on `provider_type`, `api_format` and `transport_profile`. Deliberately
/// *not* on the credential: how we authenticated says nothing about how the
/// request is shaped, and two ChatGPT logins reaching one endpoint must not
/// produce two adapters. The credential arrives as a value that already knows
/// which of those it is.
///
/// `codex_request_shape` is not a selector either — it never changes *which*
/// adapter is built, only what the Responses one puts on the wire. A row that
/// is not `responses` carries it harmlessly and nothing reads it.
pub fn create_provider(wire: ProviderWire<'_>) -> Result<Box<dyn ChatProvider>, String> {
    let ProviderWire {
        provider_type,
        base_url,
        credential,
        api_format,
        transport_profile,
        codex_request_shape,
        codex_client_version,
        local_backend,
    } = wire;
    let codex_shape = super::openai_responses::CodexShape {
        enabled: codex_request_shape,
        client_version: codex_client_version.map(str::to_string),
    };
    let provider_type = ProviderType::parse(provider_type)?;
    validate_local_backend(provider_type, local_backend)?;
    let api_format = ApiFormat::parse(api_format)?;
    validate_transport_profile(transport_profile)?;
    validate_wire_pair(api_format, transport_profile)?;
    validate_runtime_credential(provider_type, api_format, transport_profile, credential)?;
    let api_key = credential.api_key();

    // Checked before `provider_type`, because it is the stronger statement: a
    // row reaching ChatGPT's Codex backend is that transport whatever family it
    // is filed under. It is also the only arm that needs a live session rather
    // than a key, so a mismatched credential is refused here instead of
    // producing an adapter that cannot authenticate.
    if transport_profile == "chatgpt_codex" {
        if provider_type != ProviderType::Openai {
            return Err("the ChatGPT Codex transport requires provider type `openai`".into());
        }
        match credential {
            super::Credential::ChatGpt(auth) => {
                return Ok(Box::new(super::codex::CodexProvider::new(base_url, auth.clone())));
            }
            // `validate_runtime_credential` already made this impossible.
            super::Credential::ApiKey(_)
            | super::Credential::GoogleVertexServiceAccount(_)
            | super::Credential::None => unreachable!(),
        }
    }

    let provider: Box<dyn ChatProvider> = match provider_type {
        ProviderType::Anthropic if api_format == ApiFormat::ChatCompletions => {
            Box::new(AnthropicProvider::new(base_url, api_key))
        }
        ProviderType::Anthropic => {
            return Err("provider type `anthropic` requires the `chat_completions` API format".into());
        }
        // Both of these speak two dialects, and the choice is not cosmetic: the
        // server-side tools (Grok's own web search, DeepSeek's) exist only on
        // the Responses API. xAI's chat-completions endpoint rejects
        // `{"type":"web_search"}` outright — measured, it answers 422 with
        // "expected `function` or `live_search`".
        ProviderType::Deepseek => match api_format {
            ApiFormat::Responses => Box::new(OpenAIResponsesProvider::new(base_url, api_key, codex_shape.clone())),
            ApiFormat::ChatCompletions => Box::new(DeepSeekProvider::new(base_url, api_key)),
            other => {
                return Err(format!(
                    "API format `{other:?}` is not supported by provider type `deepseek`"
                ));
            }
        },
        // Chat-completions here is ordinary chat-completions plus one header;
        // see `OpenAICompatFlavor` for why that is a flavor rather than an
        // adapter of its own.
        ProviderType::Xai => match api_format {
            ApiFormat::Responses => Box::new(OpenAIResponsesProvider::new(base_url, api_key, codex_shape.clone())),
            ApiFormat::ChatCompletions => Box::new(OpenAICompatProvider::new_xai(base_url, api_key)),
            other => {
                return Err(format!(
                    "API format `{other:?}` is not supported by provider type `xai`"
                ));
            }
        },
        ProviderType::Google => match api_format {
            ApiFormat::GeminiGenerateContent => Box::new(GoogleGenerateContentProvider::new(base_url, api_key)),
            ApiFormat::GeminiInteractions => match credential {
                super::Credential::ApiKey(api_key) => Box::new(GoogleInteractionsProvider::new(base_url, api_key)),
                super::Credential::GoogleVertexServiceAccount(json) => {
                    Box::new(GoogleInteractionsProvider::new_vertex(base_url, json)?)
                }
                super::Credential::ChatGpt(_) | super::Credential::None => unreachable!(),
            },
            ApiFormat::ChatCompletions => Box::new(OpenAICompatProvider::new_google(base_url, api_key)),
            other => {
                return Err(format!(
                    "API format `{other:?}` is not supported by provider type `google`"
                ));
            }
        },
        ProviderType::Openai => match api_format {
            ApiFormat::ChatCompletions => Box::new(OpenAICompatProvider::new(base_url, api_key)),
            ApiFormat::Responses => Box::new(OpenAIResponsesProvider::new(base_url, api_key, codex_shape.clone())),
            ApiFormat::GemmaTool => Box::new(GemmaToolProvider::new(base_url, api_key)),
            ApiFormat::GeminiGenerateContent | ApiFormat::GeminiInteractions => {
                return Err(format!(
                    "API format `{}` is not supported by provider type `{}`",
                    api_format.as_str(),
                    provider_type.as_str()
                ));
            }
            ApiFormat::LitertLm => {
                return Err("the litert_lm API format is not supported by provider type `openai`".into());
            }
        },
        ProviderType::LitertLm => {
            let backend = local_backend.ok_or("LiteRT-LM requires local_backend")?;
            let provider = super::litert_lm_provider::LiteRtLmProvider::from_model_path(base_url, backend)?;
            return Ok(Box::new(provider));
        }
    };
    Ok(provider)
}

/// The transport is a parameter rather than an overload, because leaving it
/// off is how the settings panel came to offer a temperature slider on a
/// `chatgpt_codex` row — `resolve_on` disables it, the adapter ignores it, and
/// a three-argument shortcut here answered the UI's question from a wire the
/// row does not speak.
pub fn get_capabilities(
    provider_type: &str,
    api_format: &str,
    transport_profile: &str,
    model: &str,
) -> Result<ProviderCapabilities, String> {
    let provider_type = ProviderType::parse(provider_type)?;
    validate_contract(api_format, transport_profile)?;
    Ok(super::capabilities::resolve_on(
        provider_type.as_str(),
        Some(api_format),
        Some(transport_profile),
        model,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Credential;
    use std::sync::Arc;

    fn chatgpt_credential() -> Credential {
        Credential::ChatGpt(crate::codex_auth::registry().get(crate::codex_auth::StoreId::CodexCli {
            home: std::path::PathBuf::from("/nonexistent"),
        }))
    }

    /// A row as it comes out of the database for every test but the one below:
    /// the Codex shape off, which is what every existing row holds.
    fn wire<'a>(
        provider_type: &'a str,
        base_url: &'a str,
        credential: &'a Credential,
        api_format: &'a str,
        transport_profile: &'a str,
    ) -> ProviderWire<'a> {
        ProviderWire {
            provider_type,
            base_url,
            credential,
            api_format,
            transport_profile,
            codex_request_shape: false,
            codex_client_version: None,
            local_backend: None,
        }
    }

    /// The Codex transport is an OpenAI contract, not a generic escape hatch
    /// for provider types this build does not know.
    #[test]
    fn the_transport_profile_selects_the_codex_adapter() {
        let provider = create_provider(wire(
            "openai",
            "https://example.invalid",
            &chatgpt_credential(),
            "responses",
            "chatgpt_codex",
        ))
        .unwrap();
        assert_eq!(provider.adapter_name(), "CodexProvider");
        assert!(
            create_provider(wire(
                "anything-else",
                "https://example.invalid",
                &chatgpt_credential(),
                "responses",
                "chatgpt_codex",
            ))
            .is_err()
        );
    }

    /// **Both ChatGPT logins produce one adapter.** They yield the same token
    /// against the same endpoint, so the credential must not be an input to this
    /// choice — that is the whole reason `Credential` is shaped by how the
    /// secret is obtained rather than by which login the user picked.
    #[test]
    fn either_chatgpt_login_reaches_the_same_adapter() {
        let from_cli = crate::codex_auth::registry().get(crate::codex_auth::StoreId::CodexCli {
            home: std::path::PathBuf::from("/nonexistent"),
        });
        let app_owned = crate::codex_auth::registry().get(crate::codex_auth::StoreId::MeridianOwned {
            provider_id: "p1".into(),
            slot: "default".into(),
        });
        assert!(!Arc::ptr_eq(&from_cli, &app_owned), "two distinct stores");

        for manager in [from_cli, app_owned] {
            let provider = create_provider(wire(
                "openai",
                "https://example.invalid",
                &Credential::ChatGpt(manager),
                "responses",
                "chatgpt_codex",
            ))
            .unwrap();
            assert_eq!(provider.adapter_name(), "CodexProvider");
        }
    }

    /// An API key against this transport cannot work, and must be rejected as a
    /// broken first-party contract before an adapter is constructed.
    #[test]
    fn an_api_key_against_the_codex_transport_is_rejected() {
        let error = create_provider(wire(
            "openai",
            "https://example.invalid",
            &Credential::ApiKey("sk-test".into()),
            "responses",
            "chatgpt_codex",
        ))
        .err()
        .expect("an API key cannot authenticate the ChatGPT Codex transport");
        assert!(error.contains("ChatGPT login"), "{error}");
    }

    #[test]
    fn a_chatgpt_login_against_the_standard_transport_is_rejected() {
        let error = create_provider(wire(
            "openai",
            "https://example.invalid",
            &chatgpt_credential(),
            "responses",
            "standard",
        ))
        .err()
        .expect("a ChatGPT session cannot authenticate a standard API endpoint");
        assert!(error.contains("API-key"), "{error}");
    }

    /// Everything that existed before still resolves the way it did — the new
    /// argument is additive, and `standard` is what every row holds.
    #[test]
    fn the_standard_transport_leaves_every_existing_choice_alone() {
        let key = Credential::ApiKey("k".into());
        let cases = [
            ("anthropic", "chat_completions", "AnthropicProvider"),
            ("deepseek", "responses", "OpenAIResponsesProvider"),
            ("deepseek", "chat_completions", "DeepSeekProvider"),
            ("xai", "responses", "OpenAIResponsesProvider"),
            ("xai", "chat_completions", "OpenAICompatProvider"),
            ("google", "gemini_generate_content", "GoogleGenerateContentProvider"),
            ("google", "gemini_interactions", "GoogleInteractionsProvider"),
            ("openai", "responses", "OpenAIResponsesProvider"),
            ("openai", "chat_completions", "OpenAICompatProvider"),
            ("openai", "gemma_tool", "GemmaToolProvider"),
        ];
        for (provider_type, api_format, expected) in cases {
            let provider =
                create_provider(wire(provider_type, "https://e.invalid", &key, api_format, "standard")).unwrap();
            assert_eq!(provider.adapter_name(), expected, "{provider_type}/{api_format}");
        }
    }

    #[test]
    fn a_vertex_service_account_uses_the_interactions_adapter() {
        let credentials = Credential::GoogleVertexServiceAccount(
            r#"{
                "type": "service_account",
                "project_id": "billing-project",
                "client_email": "vertex@example.iam.gserviceaccount.com",
                "private_key": "not-a-key"
            }"#
            .into(),
        );
        let provider = create_provider(wire(
            "google",
            "https://aiplatform.googleapis.com/v1beta1/locations/global",
            &credentials,
            "gemini_interactions",
            "standard",
        ))
        .unwrap();
        assert_eq!(provider.adapter_name(), "GoogleInteractionsProvider");
    }

    /// **The Codex shape picks no adapter.** It changes what the Responses one
    /// puts on the wire and nothing else — so turning it on must not quietly
    /// route a row somewhere new, and a row that is not `responses` has to
    /// carry it harmlessly rather than being refused.
    #[test]
    fn the_codex_request_shape_changes_the_wire_and_not_the_adapter() {
        let key = Credential::ApiKey("k".into());
        for (provider_type, api_format, expected) in [
            ("openai", "responses", "OpenAIResponsesProvider"),
            ("xai", "responses", "OpenAIResponsesProvider"),
            ("openai", "chat_completions", "OpenAICompatProvider"),
            ("anthropic", "chat_completions", "AnthropicProvider"),
        ] {
            let provider = create_provider(ProviderWire {
                codex_request_shape: true,
                ..wire(provider_type, "https://e.invalid", &key, api_format, "standard")
            })
            .unwrap();
            assert_eq!(provider.adapter_name(), expected, "{provider_type}/{api_format}");
        }
    }

    #[test]
    fn unknown_wire_selectors_are_rejected_instead_of_using_openai_compat() {
        let key = Credential::ApiKey("k".into());
        assert!(create_provider(wire("custom", "https://e.invalid", &key, "future_api", "standard")).is_err());
        assert!(
            create_provider(wire(
                "custom",
                "https://e.invalid",
                &key,
                "chat_completions",
                "future_transport"
            ))
            .is_err()
        );
    }

    #[test]
    fn stored_wire_and_credential_combinations_are_closed() {
        for valid in [
            ("openai", "chat_completions", "standard", "api_key"),
            ("xai", "responses", "standard", "api_key"),
            (
                "google",
                "gemini_interactions",
                "standard",
                "google_vertex_service_account",
            ),
            ("openai", "responses", "chatgpt_codex", "codex_cli"),
            ("openai", "responses", "chatgpt_codex", "chatgpt_oauth"),
        ] {
            assert!(
                validate_stored_contract(valid.0, valid.1, valid.2, valid.3).is_ok(),
                "{valid:?}"
            );
        }

        for invalid in [
            ("openai", "chat_completions", "chatgpt_codex", "codex_cli"),
            ("openai", "responses", "chatgpt_codex", "api_key"),
            ("openai", "responses", "standard", "codex_cli"),
            ("openai", "responses", "standard", "future_login"),
            ("openai", "responses", "standard", "google_vertex_service_account"),
            (
                "google",
                "gemini_generate_content",
                "standard",
                "google_vertex_service_account",
            ),
            ("future", "responses", "standard", "api_key"),
            ("anthropic", "responses", "standard", "api_key"),
            ("xai", "responses", "chatgpt_codex", "codex_cli"),
        ] {
            assert!(
                validate_stored_contract(invalid.0, invalid.1, invalid.2, invalid.3).is_err(),
                "{invalid:?}"
            );
        }
    }
}

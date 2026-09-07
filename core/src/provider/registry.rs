use super::ChatProvider;
use super::ProviderCapabilities;
use super::anthropic::AnthropicProvider;
use super::deepseek::DeepSeekProvider;
use super::gemma_tool::GemmaToolProvider;
use super::google_generate_content::GoogleGenerateContentProvider;
use super::openai_compat::OpenAICompatProvider;
use super::openai_responses::OpenAIResponsesProvider;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, strum::EnumString, strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ApiFormat {
    ChatCompletions,
    Responses,
    GeminiGenerateContent,
    GemmaTool,
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
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, strum::EnumString, strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ProviderType {
    Openai,
    Anthropic,
    Deepseek,
    Xai,
    Google,
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
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, strum::EnumString, strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TransportProfile {
    Standard,
    ChatgptCodex,
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
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, strum::EnumString, strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum CredentialKind {
    ApiKey,
    CodexCli,
    ChatgptOauth,
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

fn validate_transport_profile(value: &str) -> Result<(), String> {
    TransportProfile::parse(value).map(|_| ())
}

fn validate_wire_pair(api_format: ApiFormat, transport_profile: &str) -> Result<(), String> {
    match (api_format, transport_profile) {
        (ApiFormat::Responses, "chatgpt_codex") | (_, "standard") => Ok(()),
        (_, "chatgpt_codex") => Err("the ChatGPT Codex transport requires the `responses` API format".into()),
        // `validate_transport_profile` owns this error. Keeping this arm total
        // prevents a later caller from accidentally treating a new profile as
        // valid before its combinations have been decided.
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
        | (TransportProfile::ChatgptCodex, CredentialKind::CodexCli | CredentialKind::ChatgptOauth) => {}
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

    let supported = match provider_type {
        ProviderType::Anthropic => api_format == ApiFormat::ChatCompletions,
        ProviderType::Deepseek | ProviderType::Xai => {
            matches!(api_format, ApiFormat::ChatCompletions | ApiFormat::Responses)
        }
        ProviderType::Google => {
            matches!(
                api_format,
                ApiFormat::ChatCompletions | ApiFormat::GeminiGenerateContent
            )
        }
        ProviderType::Openai => {
            matches!(
                api_format,
                ApiFormat::ChatCompletions | ApiFormat::Responses | ApiFormat::GemmaTool
            )
        }
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

fn validate_runtime_credential(transport_profile: &str, credential: &super::Credential) -> Result<(), String> {
    match (transport_profile, credential) {
        ("standard", super::Credential::ApiKey(_)) | ("chatgpt_codex", super::Credential::ChatGpt(_)) => Ok(()),
        ("standard", super::Credential::ChatGpt(_)) => {
            Err("the standard provider transport requires an API-key credential".into())
        }
        ("chatgpt_codex", super::Credential::ApiKey(_)) => {
            Err("the ChatGPT Codex transport requires a ChatGPT login credential".into())
        }
        (other, _) => Err(format!("unknown provider transport profile `{other}`")),
    }
}

/// Pick the adapter for a provider row.
///
/// Keyed on `provider_type`, `api_format` and `transport_profile`. Deliberately
/// *not* on the credential: how we authenticated says nothing about how the
/// request is shaped, and two ChatGPT logins reaching one endpoint must not
/// produce two adapters. The credential arrives as a value that already knows
/// which of those it is.
pub fn create_provider(
    provider_type: &str,
    base_url: &str,
    credential: &super::Credential,
    api_format: &str,
    transport_profile: &str,
) -> Result<Box<dyn ChatProvider>, String> {
    let provider_type = ProviderType::parse(provider_type)?;
    let api_format = ApiFormat::parse(api_format)?;
    validate_transport_profile(transport_profile)?;
    validate_wire_pair(api_format, transport_profile)?;
    validate_runtime_credential(transport_profile, credential)?;
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
            super::Credential::ApiKey(_) => unreachable!(),
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
            ApiFormat::Responses => Box::new(OpenAIResponsesProvider::new(base_url, api_key)),
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
            ApiFormat::Responses => Box::new(OpenAIResponsesProvider::new(base_url, api_key)),
            ApiFormat::ChatCompletions => Box::new(OpenAICompatProvider::new_xai(base_url, api_key)),
            other => {
                return Err(format!(
                    "API format `{other:?}` is not supported by provider type `xai`"
                ));
            }
        },
        ProviderType::Google => match api_format {
            ApiFormat::GeminiGenerateContent => Box::new(GoogleGenerateContentProvider::new(base_url, api_key)),
            ApiFormat::ChatCompletions => Box::new(OpenAICompatProvider::new_google(base_url, api_key)),
            other => {
                return Err(format!(
                    "API format `{other:?}` is not supported by provider type `google`"
                ));
            }
        },
        ProviderType::Openai => match api_format {
            ApiFormat::ChatCompletions => Box::new(OpenAICompatProvider::new(base_url, api_key)),
            ApiFormat::Responses => Box::new(OpenAIResponsesProvider::new(base_url, api_key)),
            ApiFormat::GemmaTool => Box::new(GemmaToolProvider::new(base_url, api_key)),
            ApiFormat::GeminiGenerateContent => {
                return Err(format!(
                    "API format `gemini_generate_content` is not supported by provider type `{}`",
                    provider_type.as_str()
                ));
            }
        },
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

    /// The Codex transport is an OpenAI contract, not a generic escape hatch
    /// for provider types this build does not know.
    #[test]
    fn the_transport_profile_selects_the_codex_adapter() {
        let provider = create_provider(
            "openai",
            "https://example.invalid",
            &chatgpt_credential(),
            "responses",
            "chatgpt_codex",
        )
        .unwrap();
        assert_eq!(provider.adapter_name(), "CodexProvider");
        assert!(
            create_provider(
                "anything-else",
                "https://example.invalid",
                &chatgpt_credential(),
                "responses",
                "chatgpt_codex",
            )
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
            let provider = create_provider(
                "openai",
                "https://example.invalid",
                &Credential::ChatGpt(manager),
                "responses",
                "chatgpt_codex",
            )
            .unwrap();
            assert_eq!(provider.adapter_name(), "CodexProvider");
        }
    }

    /// An API key against this transport cannot work, and must be rejected as a
    /// broken first-party contract before an adapter is constructed.
    #[test]
    fn an_api_key_against_the_codex_transport_is_rejected() {
        let error = create_provider(
            "openai",
            "https://example.invalid",
            &Credential::ApiKey("sk-test".into()),
            "responses",
            "chatgpt_codex",
        )
        .err()
        .expect("an API key cannot authenticate the ChatGPT Codex transport");
        assert!(error.contains("ChatGPT login"), "{error}");
    }

    #[test]
    fn a_chatgpt_login_against_the_standard_transport_is_rejected() {
        let error = create_provider(
            "openai",
            "https://example.invalid",
            &chatgpt_credential(),
            "responses",
            "standard",
        )
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
            ("openai", "responses", "OpenAIResponsesProvider"),
            ("openai", "chat_completions", "OpenAICompatProvider"),
            ("openai", "gemma_tool", "GemmaToolProvider"),
        ];
        for (provider_type, api_format, expected) in cases {
            let provider = create_provider(provider_type, "https://e.invalid", &key, api_format, "standard").unwrap();
            assert_eq!(provider.adapter_name(), expected, "{provider_type}/{api_format}");
        }
    }

    #[test]
    fn unknown_wire_selectors_are_rejected_instead_of_using_openai_compat() {
        let key = Credential::ApiKey("k".into());
        assert!(create_provider("custom", "https://e.invalid", &key, "future_api", "standard").is_err());
        assert!(
            create_provider(
                "custom",
                "https://e.invalid",
                &key,
                "chat_completions",
                "future_transport"
            )
            .is_err()
        );
    }

    #[test]
    fn stored_wire_and_credential_combinations_are_closed() {
        for valid in [
            ("openai", "chat_completions", "standard", "api_key"),
            ("xai", "responses", "standard", "api_key"),
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

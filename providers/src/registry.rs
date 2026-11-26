use super::ChatProvider;
use super::ProviderCapabilities;
use crate::protocols::anthropic_messages::AnthropicMessagesProvider;
use crate::protocols::gemma_tool::GemmaToolProvider;
use crate::protocols::google_generate_content::GoogleGenerateContentProvider;
use crate::protocols::google_interactions::GoogleInteractionsProvider;
use crate::protocols::openai_chat::OpenAIChatProvider;
use crate::protocols::openai_responses::OpenAIResponsesProvider;
use crate::vendors::deepseek::DeepSeekChatProvider;
use crate::vendors::google::{GoogleCompatThinkingMode, GoogleOpenAIChatProvider};
use crate::vendors::xai::XaiOpenAIChatProvider;

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

pub use meridian_provider_sdk::chat::LocalInferenceBackend;

#[cfg(feature = "sea-orm")]
meridian_provider_sdk::impl_sea_orm_text_enum!(ProviderType);
#[cfg(feature = "sea-orm")]
meridian_provider_sdk::impl_sea_orm_text_enum!(ApiFormat);
#[cfg(feature = "sea-orm")]
meridian_provider_sdk::impl_sea_orm_text_enum!(TransportProfile);
#[cfg(feature = "sea-orm")]
meridian_provider_sdk::impl_sea_orm_text_enum!(CredentialKind);

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
    /// How Google's OpenAI-compatible adapter encodes thinking parameters.
    /// Meaningful only for `provider_type == "google"` with `chat_completions`.
    pub google_compat_thinking_mode: GoogleCompatThinkingMode,
    /// Whether `gemini_generate_content` keeps `includeThoughts: true` when
    /// `thinking_enabled` is false (for relays that require it).
    pub google_include_thoughts_when_off: bool,
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
        google_compat_thinking_mode,
        google_include_thoughts_when_off,
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
            Box::new(AnthropicMessagesProvider::new(base_url, api_key))
        }
        ProviderType::Anthropic => {
            return Err("provider type `anthropic` requires the `chat_completions` API format".into());
        }
        ProviderType::Deepseek => match api_format {
            ApiFormat::Responses => Box::new(OpenAIResponsesProvider::new(base_url, api_key, codex_shape.clone())),
            ApiFormat::ChatCompletions => Box::new(DeepSeekChatProvider::build(base_url, api_key)),
            other => {
                return Err(format!(
                    "API format `{other:?}` is not supported by provider type `deepseek`"
                ));
            }
        },
        ProviderType::Xai => match api_format {
            ApiFormat::Responses => Box::new(OpenAIResponsesProvider::new(base_url, api_key, codex_shape.clone())),
            ApiFormat::ChatCompletions => Box::new(XaiOpenAIChatProvider::build(base_url, api_key)),
            other => {
                return Err(format!(
                    "API format `{other:?}` is not supported by provider type `xai`"
                ));
            }
        },
        ProviderType::Google => match api_format {
            ApiFormat::GeminiGenerateContent => Box::new(
                GoogleGenerateContentProvider::new(base_url, api_key)
                    .with_include_thoughts_when_off(google_include_thoughts_when_off),
            ),
            ApiFormat::GeminiInteractions => match credential {
                super::Credential::ApiKey(api_key) => Box::new(GoogleInteractionsProvider::new(base_url, api_key)),
                super::Credential::GoogleVertexServiceAccount(json) => {
                    Box::new(GoogleInteractionsProvider::new_vertex(base_url, json)?)
                }
                super::Credential::ChatGpt(_) | super::Credential::None => unreachable!(),
            },
            ApiFormat::ChatCompletions => Box::new(GoogleOpenAIChatProvider::build(
                base_url,
                api_key,
                google_compat_thinking_mode,
            )),
            other => {
                return Err(format!(
                    "API format `{other:?}` is not supported by provider type `google`"
                ));
            }
        },
        ProviderType::Openai => match api_format {
            ApiFormat::ChatCompletions => Box::new(OpenAIChatProvider::new(base_url, api_key)),
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

/// Which content-part shapes this door actually has, independent of the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialectMedia {
    pub image: bool,
    pub document: bool,
    pub video: bool,
    pub audio: bool,
}

impl DialectMedia {
    pub fn has(self, category: meridian_provider_sdk::media::MediaCategory) -> bool {
        match category {
            meridian_provider_sdk::media::MediaCategory::Image => self.image,
            meridian_provider_sdk::media::MediaCategory::Document => self.document,
            meridian_provider_sdk::media::MediaCategory::Video => self.video,
            meridian_provider_sdk::media::MediaCategory::Audio => self.audio,
        }
    }

    pub fn from_wire(provider_type: ProviderType, api_format: ApiFormat) -> Self {
        match (provider_type, api_format) {
            (ProviderType::Anthropic, ApiFormat::ChatCompletions) => Self {
                image: true,
                document: true,
                video: false,
                audio: false,
            },
            (_, ApiFormat::Responses) => Self {
                image: true,
                document: true,
                video: false,
                audio: false,
            },
            (ProviderType::Google, ApiFormat::GeminiGenerateContent) => Self {
                image: true,
                document: true,
                video: true,
                audio: true,
            },
            (ProviderType::Google, ApiFormat::GeminiInteractions) => Self {
                image: true,
                document: true,
                video: false,
                audio: false,
            },
            (_, ApiFormat::ChatCompletions | ApiFormat::GemmaTool) => Self {
                image: true,
                document: false,
                video: false,
                audio: false,
            },
            (_, ApiFormat::LitertLm | ApiFormat::GeminiGenerateContent | ApiFormat::GeminiInteractions) => Self {
                image: false,
                document: false,
                video: false,
                audio: false,
            },
        }
    }
}

/// Send-time policy: the model's readable MIME set intersected with this door's
/// part shapes. Native send is membership on both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireMediaPolicy {
    pub readable: Vec<meridian_provider_sdk::media::ReadableMime>,
    pub dialect: DialectMedia,
}

impl WireMediaPolicy {
    pub fn from_wire(provider_type: ProviderType, api_format: ApiFormat, caps: &ProviderCapabilities) -> Self {
        Self {
            readable: caps.readable_mime_types.clone(),
            dialect: DialectMedia::from_wire(provider_type, api_format),
        }
    }

    pub fn from_resolved(provider_type: &str, api_format: &str, caps: &ProviderCapabilities) -> Result<Self, String> {
        Ok(Self::from_wire(
            ProviderType::parse(provider_type)?,
            ApiFormat::parse(api_format)?,
            caps,
        ))
    }

    pub fn native(&self, mime: meridian_provider_sdk::media::ReadableMime) -> bool {
        self.readable.contains(&mime) && self.dialect.has(mime.category())
    }

    pub fn can_read_mime(&self, mime: &str) -> bool {
        meridian_provider_sdk::media::ReadableMime::parse(mime).is_some_and(|parsed| self.readable.contains(&parsed))
    }

    pub fn reads_any_image(&self) -> bool {
        self.readable
            .iter()
            .any(|mime| mime.category() == meridian_provider_sdk::media::MediaCategory::Image)
    }

    /// Tool-row / sticker tests: the model accepts the stored image types over a
    /// door that has an image part.
    pub fn native_images() -> Self {
        Self {
            readable: meridian_provider_sdk::media::image_mimes(),
            dialect: DialectMedia {
                image: true,
                document: false,
                video: false,
                audio: false,
            },
        }
    }

    pub fn none() -> Self {
        Self {
            readable: Vec::new(),
            dialect: DialectMedia {
                image: false,
                document: false,
                video: false,
                audio: false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Credential;
    use std::sync::Arc;

    struct StubBearerSession(&'static str);

    #[async_trait::async_trait]
    impl meridian_provider_sdk::auth::BearerSession for StubBearerSession {
        async fn bearer(
            &self,
        ) -> Result<meridian_provider_sdk::auth::BearerToken, meridian_provider_sdk::auth::BearerSessionError> {
            Ok(meridian_provider_sdk::auth::BearerToken {
                access_token: self.0.into(),
                account_id: "acct".into(),
                is_fedramp: false,
            })
        }

        async fn refresh_after_rejection(
            &self,
            _failed_access_token: &str,
        ) -> Result<(), meridian_provider_sdk::auth::BearerSessionError> {
            Ok(())
        }
    }

    fn chatgpt_credential() -> Credential {
        Credential::ChatGpt(Arc::new(StubBearerSession("cli")))
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
            google_compat_thinking_mode: crate::openai_compat::GoogleCompatThinkingMode::Auto,
            google_include_thoughts_when_off: false,
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
        let from_cli: Arc<dyn meridian_provider_sdk::auth::BearerSession> = Arc::new(StubBearerSession("cli"));
        let app_owned: Arc<dyn meridian_provider_sdk::auth::BearerSession> = Arc::new(StubBearerSession("owned"));
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
            ("anthropic", "chat_completions", "AnthropicMessagesProvider"),
            ("deepseek", "responses", "OpenAIResponsesProvider"),
            ("deepseek", "chat_completions", "DeepSeekChatProvider"),
            ("xai", "responses", "OpenAIResponsesProvider"),
            ("xai", "chat_completions", "XaiOpenAIChatProvider"),
            ("google", "gemini_generate_content", "GoogleGenerateContentProvider"),
            ("google", "gemini_interactions", "GoogleInteractionsProvider"),
            ("google", "chat_completions", "GoogleOpenAIChatProvider"),
            ("openai", "responses", "OpenAIResponsesProvider"),
            ("openai", "chat_completions", "OpenAIChatProvider"),
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
            ("openai", "chat_completions", "OpenAIChatProvider"),
            ("anthropic", "chat_completions", "AnthropicMessagesProvider"),
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

    fn caps_with(mimes: Vec<meridian_provider_sdk::media::ReadableMime>) -> ProviderCapabilities {
        ProviderCapabilities {
            readable_mime_types: mimes,
            ..Default::default()
        }
    }

    #[test]
    fn dialect_shapes_are_a_fact_about_the_door() {
        let anthropic = DialectMedia::from_wire(ProviderType::Anthropic, ApiFormat::ChatCompletions);
        assert!(anthropic.document && anthropic.image);
        let openai_chat = DialectMedia::from_wire(ProviderType::Openai, ApiFormat::ChatCompletions);
        assert!(openai_chat.image && !openai_chat.document);
        let openai_responses = DialectMedia::from_wire(ProviderType::Openai, ApiFormat::Responses);
        assert!(openai_responses.document);
        let gemini = DialectMedia::from_wire(ProviderType::Google, ApiFormat::GeminiGenerateContent);
        assert!(gemini.video && gemini.audio && gemini.document);
        let litert = DialectMedia::from_wire(ProviderType::LitertLm, ApiFormat::LitertLm);
        assert!(!litert.image);
    }

    #[test]
    fn native_send_is_the_intersection() {
        let pdf = vec![meridian_provider_sdk::media::ReadableMime::ApplicationPdf];
        let jpeg = meridian_provider_sdk::media::image_mimes();
        let anthropic = WireMediaPolicy::from_wire(
            ProviderType::Anthropic,
            ApiFormat::ChatCompletions,
            &caps_with(pdf.clone()),
        );
        assert!(anthropic.native(meridian_provider_sdk::media::ReadableMime::ApplicationPdf));

        let openai_chat = WireMediaPolicy::from_wire(
            ProviderType::Openai,
            ApiFormat::ChatCompletions,
            &caps_with(pdf.clone()),
        );
        assert!(!openai_chat.native(meridian_provider_sdk::media::ReadableMime::ApplicationPdf));

        let openai_responses = WireMediaPolicy::from_wire(ProviderType::Openai, ApiFormat::Responses, &caps_with(pdf));
        assert!(openai_responses.native(meridian_provider_sdk::media::ReadableMime::ApplicationPdf));

        let no_images = WireMediaPolicy::from_wire(
            ProviderType::Anthropic,
            ApiFormat::ChatCompletions,
            &caps_with(Vec::new()),
        );
        assert!(!no_images.native(meridian_provider_sdk::media::ReadableMime::ImageJpeg));

        let litert = WireMediaPolicy::from_wire(ProviderType::LitertLm, ApiFormat::LitertLm, &caps_with(jpeg));
        assert!(!litert.native(meridian_provider_sdk::media::ReadableMime::ImageJpeg));
    }
}

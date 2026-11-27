//! OpenAI Chat Completions wire protocol (`POST /chat/completions`).
//!
//! This module owns the greatest-common-divisor request/response shapes
//! (`OpenAIChatRequestBase`, `OpenAIChatResponseBase`, `OpenAIChatChunkResponse`)
//! and the concrete [`OpenAIChatProvider`]. Vendor-specific standard-protocol
//! extensions (DeepSeek thinking/reasoning replay, xAI cache routing header,
//! Google thought signatures) compose [`OpenAIChatProvider`] via the
//! [`OpenAIChatExtension`] trait instead of adding vendor branches here.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::StreamExt;
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::dto::{ExtraIgnore, embedded_upstream_error, warn_extra_fields};
use crate::mixins::{OpenAISamplingParamsMixin, SseStreamMixin, ToolImageCarryMixin};
use crate::state::ProviderState;
use crate::{
    AgentResponse, ChatMessage, ChatParams, ChatProvider, ChatStream, ProviderError, StreamEvent, TokenUsage, ToolCall,
    ToolDefinition,
};
use meridian_provider_sdk::client::{HttpTransport, Request, RequestBody, ReqwestTransport};

// ── Extension point ──────────────────────────────────────────────────────────

/// Standard-protocol extension point for vendors speaking OpenAI Chat Completions.
///
/// Strictly scoped to standard-protocol extension points (message rendering,
/// optional sampling mixin, vendor headers/extra request fields, and typed
/// extraction of vendor-owned keys from `extra` on responses/chunks).
pub trait OpenAIChatExtension: Send + Sync {
    fn adapter_name(&self) -> &'static str {
        "OpenAIChatProvider"
    }

    fn serialize_messages(&self, messages: &[ChatMessage], _model: &str) -> Result<Vec<Value>, ProviderError> {
        serialize_openai_messages(messages)
    }

    /// Whether `OpenAISamplingParamsMixin` (`temperature`, `top_p`) should be
    /// applied. DeepSeek chat omits them; standard OpenAI / xAI / Google apply them.
    fn supports_sampling_params(&self) -> bool {
        true
    }

    /// Apply vendor-specific request fields or headers on top of
    /// [`OpenAIChatRequestBase`].
    fn customize_request(
        &self,
        base_url: &str,
        params: &ChatParams,
        body: &mut Map<String, Value>,
        headers: &mut http::HeaderMap,
    ) -> Result<(), ProviderError>;

    /// Extract vendor-owned fields from a streaming chunk's `extra` maps
    /// (removing claimed keys so `warn_ignored_fields` only warns on truly
    /// unknown keys) and return any resulting [`StreamEvent`]s.
    fn extract_chunk_state_events(&self, _chunk: &mut OpenAIChatChunkResponse, _model: &str) -> Vec<StreamEvent> {
        Vec::new()
    }

    /// Extract vendor-owned fields from a non-streaming response message's
    /// `extra` maps (removing claimed keys before `warn_ignored_fields`).
    fn extract_response_state(
        &self,
        _message: &mut OpenAIChatMessageResponse,
        _model: &str,
    ) -> Result<Option<ProviderState>, ProviderError> {
        Ok(None)
    }
}

/// Default OpenAI Chat Completions extension (official OpenAI & generic relays).
#[derive(Debug, Default)]
pub struct StandardOpenAIChatExtension;

impl OpenAIChatExtension for StandardOpenAIChatExtension {
    fn adapter_name(&self) -> &'static str {
        "OpenAIChatProvider"
    }

    fn customize_request(
        &self,
        base_url: &str,
        params: &ChatParams,
        body: &mut Map<String, Value>,
        _headers: &mut http::HeaderMap,
    ) -> Result<(), ProviderError> {
        apply_standard_openai_chat_params(base_url, params, body, true);
        Ok(())
    }
}

/// Apply standard OpenAI Chat Completions non-GCD fields (`reasoning_effort`,
/// `verbosity`, `service_tier`, and `prompt_cache_key` on official OpenAI host).
pub fn apply_standard_openai_chat_params(
    base_url: &str,
    params: &ChatParams,
    body: &mut Map<String, Value>,
    allow_openai_prompt_cache_key: bool,
) {
    if let Some(ref effort) = params.thinking_effort {
        body.insert("reasoning_effort".into(), serde_json::json!(effort));
    }
    if let Some(ref verbosity) = params.verbosity {
        body.insert("verbosity".into(), serde_json::json!(verbosity));
    }
    if params.fast {
        body.insert("service_tier".into(), serde_json::json!("priority"));
    }
    if allow_openai_prompt_cache_key && is_openai_host(base_url) {
        if let Some(key) = params.cache_key.as_deref() {
            body.insert("prompt_cache_key".into(), serde_json::json!(key));
        }
        if let Some(retention) = params.cache_ttl.and_then(|ttl| match ttl {
            crate::PromptCacheTtl::InMemory => Some("in_memory"),
            crate::PromptCacheTtl::TwentyFourHours => Some("24h"),
            _ => None,
        }) {
            body.insert("prompt_cache_retention".into(), serde_json::json!(retention));
        }
    }
}

// ── Typed Request Base ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct OpenAIChatStreamOptions {
    pub include_usage: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenAIChatToolRequest {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: OpenAIChatFunctionRequest,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenAIChatFunctionRequest {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Greatest-common-divisor wire payload for `POST /chat/completions`.
///
/// Sampling params (`temperature`, `top_p`) are intentionally NOT here — they
/// belong to [`OpenAISamplingParamsMixin`] because variants like DeepSeek omit
/// them.
#[derive(Debug, Clone, Serialize)]
pub struct OpenAIChatRequestBase {
    pub model: String,
    pub messages: Vec<Value>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<OpenAIChatStreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<OpenAIChatToolRequest>>,
}

impl OpenAIChatRequestBase {
    pub fn new(
        model: String,
        messages: Vec<Value>,
        tools: Option<&[ToolDefinition]>,
        max_tokens: Option<i32>,
        stream: bool,
    ) -> Self {
        let tools = tools.filter(|t| !t.is_empty()).map(|items| {
            items
                .iter()
                .map(|t| OpenAIChatToolRequest {
                    kind: "function",
                    function: OpenAIChatFunctionRequest {
                        name: t.name.clone(),
                        description: t.description.clone(),
                        parameters: t.parameters.clone(),
                    },
                })
                .collect()
        });
        Self {
            model,
            messages,
            stream,
            stream_options: stream.then_some(OpenAIChatStreamOptions { include_usage: true }),
            max_tokens,
            tools,
        }
    }

    pub fn into_json_object(self) -> Result<Map<String, Value>, ProviderError> {
        match serde_json::to_value(self).map_err(|e| ProviderError::Parse(e.to_string()))? {
            Value::Object(map) => Ok(map),
            _ => Err(ProviderError::Parse(
                "OpenAIChatRequestBase must serialize to an object".into(),
            )),
        }
    }
}

// ── Provider ─────────────────────────────────────────────────────────────────

pub struct OpenAIChatProvider {
    base_url: String,
    api_key: String,
    extension: Arc<dyn OpenAIChatExtension>,
}

/// Compatibility alias for call sites migrating from `OpenAICompatProvider`.
pub type OpenAICompatProvider = OpenAIChatProvider;
pub use crate::vendors::google::{GOOGLE_COMPAT_THINKING_MODE_PREF, GoogleCompatThinkingMode};

impl OpenAIChatProvider {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        Self::with_extension(base_url, api_key, Arc::new(StandardOpenAIChatExtension))
    }

    pub fn with_extension(base_url: &str, api_key: &str, extension: Arc<dyn OpenAIChatExtension>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            extension,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn build_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
        params: &ChatParams,
        stream: bool,
    ) -> Result<Request, ProviderError> {
        if !params.server_tools.is_empty() {
            return Err(ProviderError::InvalidRequest(
                "provider-side server tools are not supported by chat-completions".into(),
            ));
        }

        let serialized_messages = self.extension.serialize_messages(messages, &params.model)?;
        let mut body = OpenAIChatRequestBase::new(
            params.model.clone(),
            serialized_messages,
            tools,
            params.max_tokens,
            stream,
        )
        .into_json_object()?;

        if self.extension.supports_sampling_params() {
            OpenAISamplingParamsMixin::from_chat_params(params).apply_to_json(&mut body);
        }

        let mut req = Request::new(http::Method::POST, format!("{}/chat/completions", self.base_url));
        req.headers.insert(
            http::header::AUTHORIZATION,
            crate::auth_header_value(&format!("Bearer {}", self.api_key)),
        );

        self.extension
            .customize_request(&self.base_url, params, &mut body, &mut req.headers)?;

        req.body = Some(RequestBody::Json(Value::Object(body)));
        Ok(req)
    }
}

/// Whether this base URL is OpenAI's own API rather than a relay.
pub fn is_openai_host(base_url: &str) -> bool {
    base_url
        .parse::<http::Uri>()
        .ok()
        .and_then(|uri| uri.host().map(|host| host.eq_ignore_ascii_case("api.openai.com")))
        .unwrap_or(false)
}

/// Only a user message can be the attachment envelope. Everything else is text
/// whatever it starts with.
pub fn content_value(role: &str, content: &str) -> Result<Value, ProviderError> {
    if role == "user"
        && let Some(parts) = crate::decode_message_parts(content).map_err(ProviderError::Parse)?
    {
        for part in &parts {
            match part {
                crate::MessageContentPart::File { .. } => {
                    return Err(ProviderError::Parse("chat-completions cannot take a file part".into()));
                }
                crate::MessageContentPart::ImageUrl { image_url } => {
                    crate::require_wire_attachment_url(&image_url.url, "openai-compat")?;
                }
                _ => {}
            }
        }
        return serde_json::to_value(parts).map_err(|error| ProviderError::Parse(error.to_string()));
    }
    Ok(Value::String(content.to_string()))
}

pub fn serialize_openai_messages(messages: &[ChatMessage]) -> Result<Vec<Value>, ProviderError> {
    let mut carry = ToolImageCarryMixin::default();
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        if m.role != "tool" {
            carry.flush(&mut out);
        }
        out.push(serialize_openai_message(m, &mut carry)?);
    }
    carry.flush(&mut out);
    Ok(out)
}

pub fn serialize_openai_message(m: &ChatMessage, carry: &mut ToolImageCarryMixin) -> Result<Value, ProviderError> {
    let rendered = crate::render_message(m, crate::SenderRendering::NameField).map_err(ProviderError::Parse)?;
    let content = match carry.take(m).map_err(ProviderError::Parse)? {
        Some(text) => Value::String(text),
        None => content_value(&m.role, &rendered.content)?,
    };
    let mut msg = serde_json::json!({ "role": m.role, "content": content });
    if let Some(ref name) = rendered.name {
        msg["name"] = serde_json::json!(name);
    }
    if let Some(ref tool_calls) = m.tool_calls {
        msg["tool_calls"] = serde_json::json!(
            tool_calls
                .iter()
                .map(|tc| {
                    serde_json::json!({
                        "id": tc.id,
                        "type": "function",
                        "function": { "name": tc.name, "arguments": tc.arguments }
                    })
                })
                .collect::<Vec<_>>()
        );
    }
    if let Some(ref tool_call_id) = m.tool_call_id {
        msg["tool_call_id"] = serde_json::json!(tool_call_id);
    }
    Ok(msg)
}

// ── Typed Streaming & Non-Streaming Responses ────────────────────────────────

#[derive(Deserialize)]
pub struct OpenAIChatChunkResponse {
    pub choices: Vec<OpenAIChatChunkChoiceResponse>,
    pub usage: Option<OpenAIChatUsageResponse>,
    #[serde(default, rename = "id")]
    _id: IgnoredAny,
    #[serde(default, rename = "object")]
    _object: IgnoredAny,
    #[serde(default, rename = "created")]
    _created: IgnoredAny,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default, rename = "service_tier")]
    _service_tier: IgnoredAny,
    #[serde(default, rename = "system_fingerprint")]
    _system_fingerprint: IgnoredAny,
    #[serde(default, rename = "obfuscation")]
    _obfuscation: IgnoredAny,
    #[serde(default, rename = "moderation")]
    _moderation: IgnoredAny,
    #[serde(default, flatten)]
    pub extra: ExtraIgnore,
}

impl OpenAIChatChunkResponse {
    pub fn warn_ignored_fields(&self) {
        warn_extra_fields("chat_chunk", &self.extra);
        for choice in &self.choices {
            choice.warn_ignored_fields();
        }
        if let Some(usage) = &self.usage {
            usage.warn_ignored_fields();
        }
    }
}

pub fn parse_chat_chunk(raw: &str) -> Result<OpenAIChatChunkResponse, ProviderError> {
    serde_json::from_str(raw).map_err(|error| {
        embedded_upstream_error(raw.as_bytes())
            .map(ProviderError::Upstream)
            .unwrap_or_else(|| ProviderError::Parse(error.to_string()))
    })
}

#[derive(Deserialize)]
pub struct OpenAIChatChunkChoiceResponse {
    pub delta: Option<OpenAIChatDeltaResponse>,
    pub finish_reason: Option<String>,
    #[serde(default, rename = "index")]
    _index: IgnoredAny,
    #[serde(default, rename = "logprobs")]
    _logprobs: IgnoredAny,
    #[serde(default, flatten)]
    pub extra: ExtraIgnore,
}

impl OpenAIChatChunkChoiceResponse {
    pub fn warn_ignored_fields(&self) {
        warn_extra_fields("chunk_choice", &self.extra);
        if let Some(delta) = &self.delta {
            delta.warn_ignored_fields();
        }
    }
}

#[derive(Deserialize)]
pub struct OpenAIChatDeltaResponse {
    pub content: Option<String>,
    pub refusal: Option<String>,
    pub reasoning_content: Option<String>,
    pub tool_calls: Option<Vec<OpenAIChatDeltaToolCallResponse>>,
    #[serde(default, rename = "role")]
    _role: IgnoredAny,
    #[serde(default, rename = "function_call")]
    _function_call: IgnoredAny,
    #[serde(default, flatten)]
    pub extra: ExtraIgnore,
}

impl OpenAIChatDeltaResponse {
    pub fn warn_ignored_fields(&self) {
        warn_extra_fields("delta", &self.extra);
        if let Some(tool_calls) = &self.tool_calls {
            for tool_call in tool_calls {
                tool_call.warn_ignored_fields();
            }
        }
    }
}

#[derive(Deserialize)]
pub struct OpenAIChatDeltaToolCallResponse {
    pub index: usize,
    pub id: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    pub function: Option<OpenAIChatDeltaFunctionResponse>,
    #[serde(default, rename = "custom")]
    _custom: IgnoredAny,
    #[serde(default, flatten)]
    pub extra: ExtraIgnore,
}

pub fn is_unrequested_custom_call(kind: Option<&str>, id: Option<&str>) -> bool {
    if kind != Some("custom") {
        return false;
    }
    tracing::warn!(
        call_id = id.unwrap_or(""),
        "upstream announced a custom tool call this app never requested; ignoring it"
    );
    true
}

impl OpenAIChatDeltaToolCallResponse {
    pub fn warn_ignored_fields(&self) {
        warn_extra_fields("delta_tool_call", &self.extra);
        if let Some(function) = &self.function {
            function.warn_ignored_fields();
        }
    }
}

#[derive(Deserialize)]
pub struct OpenAIChatDeltaFunctionResponse {
    pub name: Option<String>,
    pub arguments: Option<String>,
    #[serde(default, flatten)]
    pub extra: ExtraIgnore,
}

impl OpenAIChatDeltaFunctionResponse {
    pub fn warn_ignored_fields(&self) {
        warn_extra_fields("delta_function", &self.extra);
    }
}

#[derive(Deserialize)]
pub struct OpenAIChatUsageResponse {
    pub prompt_tokens: Option<i32>,
    pub completion_tokens: Option<i32>,
    pub total_tokens: Option<i32>,
    pub prompt_cache_hit_tokens: Option<i32>,
    pub prompt_tokens_details: Option<PromptTokensDetailsResponse>,
    pub completion_tokens_details: Option<CompletionTokensDetailsResponse>,
    #[serde(default, flatten)]
    pub extra: ExtraIgnore,
}

impl OpenAIChatUsageResponse {
    pub fn warn_ignored_fields(&self) {
        warn_extra_fields("chunk_usage", &self.extra);
        if let Some(details) = &self.prompt_tokens_details {
            details.warn_ignored_fields();
        }
        if let Some(details) = &self.completion_tokens_details {
            details.warn_ignored_fields();
        }
    }
}

#[derive(Deserialize)]
pub struct PromptTokensDetailsResponse {
    pub cached_tokens: Option<i32>,
    #[serde(default, rename = "audio_tokens")]
    _audio_tokens: IgnoredAny,
    #[serde(default, flatten)]
    pub extra: ExtraIgnore,
}

impl PromptTokensDetailsResponse {
    pub fn warn_ignored_fields(&self) {
        warn_extra_fields("prompt_tokens_details", &self.extra);
    }
}

#[derive(Deserialize)]
pub struct CompletionTokensDetailsResponse {
    pub reasoning_tokens: Option<i32>,
    #[serde(default, rename = "audio_tokens")]
    _audio_tokens: IgnoredAny,
    #[serde(default, rename = "accepted_prediction_tokens")]
    _accepted_prediction_tokens: IgnoredAny,
    #[serde(default, rename = "rejected_prediction_tokens")]
    _rejected_prediction_tokens: IgnoredAny,
    #[serde(default, flatten)]
    pub extra: ExtraIgnore,
}

impl CompletionTokensDetailsResponse {
    pub fn warn_ignored_fields(&self) {
        warn_extra_fields("completion_tokens_details", &self.extra);
    }
}

#[derive(Deserialize)]
pub struct OpenAIChatResponseBase {
    pub choices: Vec<OpenAIChatChoiceResponse>,
    pub usage: Option<OpenAIChatUsageResponse>,
    #[serde(default, rename = "id")]
    _id: IgnoredAny,
    #[serde(default, rename = "object")]
    _object: IgnoredAny,
    #[serde(default, rename = "created")]
    _created: IgnoredAny,
    #[serde(default)]
    #[allow(dead_code)]
    pub model: Option<String>,
    #[serde(default, rename = "service_tier")]
    _service_tier: IgnoredAny,
    #[serde(default, rename = "system_fingerprint")]
    _system_fingerprint: IgnoredAny,
    #[serde(default, rename = "metadata")]
    _metadata: IgnoredAny,
    #[serde(default, rename = "moderation")]
    _moderation: IgnoredAny,
    #[serde(default, flatten)]
    pub extra: ExtraIgnore,
}

impl OpenAIChatResponseBase {
    pub fn warn_ignored_fields(&self) {
        warn_extra_fields("chat_response", &self.extra);
        for choice in &self.choices {
            choice.warn_ignored_fields();
        }
        if let Some(usage) = &self.usage {
            usage.warn_ignored_fields();
        }
    }
}

pub fn parse_chat_response(raw: &[u8]) -> Result<OpenAIChatResponseBase, ProviderError> {
    serde_json::from_slice(raw).map_err(|error| {
        embedded_upstream_error(raw)
            .map(ProviderError::Upstream)
            .unwrap_or_else(|| ProviderError::Parse(error.to_string()))
    })
}

#[derive(Deserialize)]
pub struct OpenAIChatChoiceResponse {
    pub message: OpenAIChatMessageResponse,
    pub finish_reason: Option<String>,
    #[serde(default, rename = "index")]
    _index: IgnoredAny,
    #[serde(default, rename = "logprobs")]
    _logprobs: IgnoredAny,
    #[serde(default, flatten)]
    pub extra: ExtraIgnore,
}

impl OpenAIChatChoiceResponse {
    pub fn warn_ignored_fields(&self) {
        warn_extra_fields("response_choice", &self.extra);
        self.message.warn_ignored_fields();
    }
}

#[derive(Deserialize)]
pub struct OpenAIChatMessageResponse {
    pub content: Option<String>,
    pub refusal: Option<String>,
    pub reasoning_content: Option<String>,
    pub tool_calls: Option<Vec<OpenAIChatToolCallResponse>>,
    #[serde(default, rename = "role")]
    _role: IgnoredAny,
    #[serde(default, rename = "function_call")]
    _function_call: IgnoredAny,
    #[serde(default, rename = "annotations")]
    _annotations: IgnoredAny,
    #[serde(default, rename = "audio")]
    _audio: IgnoredAny,
    #[serde(default, flatten)]
    pub extra: ExtraIgnore,
}

pub fn reply_text(content: Option<String>, refusal: Option<String>, finish_reason: Option<&str>) -> Option<String> {
    let refusal = refusal.filter(|r| !r.is_empty());
    let Some(refusal) = refusal else {
        return content;
    };
    tracing::info!(
        finish_reason = finish_reason.unwrap_or(""),
        chars = refusal.chars().count(),
        "the model refused; showing the refusal as the reply"
    );
    let mut text = content.unwrap_or_default();
    text.push_str(&refusal);
    Some(text)
}

impl OpenAIChatMessageResponse {
    pub fn warn_ignored_fields(&self) {
        warn_extra_fields("response_message", &self.extra);
        if let Some(tool_calls) = &self.tool_calls {
            for tool_call in tool_calls {
                tool_call.warn_ignored_fields();
            }
        }
    }
}

#[derive(Deserialize)]
pub struct OpenAIChatToolCallResponse {
    pub id: String,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    pub function: Option<OpenAIChatFunctionResponse>,
    #[serde(default, rename = "custom")]
    _custom: IgnoredAny,
    #[serde(default, flatten)]
    pub extra: ExtraIgnore,
}

impl OpenAIChatToolCallResponse {
    pub fn warn_ignored_fields(&self) {
        warn_extra_fields("response_tool_call", &self.extra);
        if let Some(function) = &self.function {
            function.warn_ignored_fields();
        }
    }
}

#[derive(Deserialize)]
pub struct OpenAIChatFunctionResponse {
    pub name: String,
    pub arguments: String,
    #[serde(default, flatten)]
    pub extra: ExtraIgnore,
}

impl OpenAIChatFunctionResponse {
    pub fn warn_ignored_fields(&self) {
        warn_extra_fields("response_function", &self.extra);
    }
}

// Backwards-compatible type aliases during migration
pub type ChatChunk = OpenAIChatChunkResponse;
pub type ChunkChoice = OpenAIChatChunkChoiceResponse;
pub type Delta = OpenAIChatDeltaResponse;
pub type DeltaToolCall = OpenAIChatDeltaToolCallResponse;
pub type DeltaFunction = OpenAIChatDeltaFunctionResponse;
pub type ChunkUsage = OpenAIChatUsageResponse;
pub type PromptTokensDetails = PromptTokensDetailsResponse;
pub type CompletionTokensDetails = CompletionTokensDetailsResponse;

// ── Usage & SSE Normalization ────────────────────────────────────────────────

pub fn normalise_openai_usage(u: &OpenAIChatUsageResponse) -> TokenUsage {
    let cache_read = u
        .prompt_cache_hit_tokens
        .or_else(|| u.prompt_tokens_details.as_ref().and_then(|d| d.cached_tokens));
    TokenUsage {
        prompt_tokens: u.prompt_tokens,
        completion_tokens: billable_completion_tokens(u),
        total_tokens: u.total_tokens,
        cache_read_tokens: cache_read,
        cache_write_tokens: None,
        cache_write_1h_tokens: None,
        billable_tool_calls: None,
    }
}

fn billable_completion_tokens(u: &OpenAIChatUsageResponse) -> Option<i32> {
    let completion = u.completion_tokens?;
    let Some(reasoning) = u
        .completion_tokens_details
        .as_ref()
        .and_then(|d| d.reasoning_tokens)
        .filter(|reasoning| *reasoning > 0)
    else {
        return Some(completion);
    };
    let unaccounted = match (u.total_tokens, u.prompt_tokens) {
        (Some(total), Some(prompt)) => total.saturating_sub(prompt).saturating_sub(completion),
        _ => return Some(completion),
    };
    if unaccounted == reasoning {
        Some(completion.saturating_add(reasoning))
    } else {
        Some(completion)
    }
}

pub fn parse_openai_sse_events(
    chunk: &OpenAIChatChunkResponse,
) -> (Vec<StreamEvent>, Option<String>, Option<TokenUsage>) {
    let mut events = Vec::new();
    let mut finish_reason = None;
    let mut usage = None;

    if let Some(ref u) = chunk.usage {
        usage = Some(normalise_openai_usage(u));
    }

    if let Some(choice) = chunk.choices.first() {
        if let Some(ref fr) = choice.finish_reason {
            finish_reason = Some(fr.clone());
        }
        if let Some(ref delta) = choice.delta {
            if let Some(ref r) = delta.reasoning_content
                && !r.is_empty()
            {
                events.push(StreamEvent::Reasoning { content: r.clone() });
            }
            if let Some(ref c) = delta.content
                && !c.is_empty()
            {
                events.push(StreamEvent::Text { content: c.clone() });
            }
            if let Some(ref r) = delta.refusal
                && !r.is_empty()
            {
                tracing::info!(
                    finish_reason = choice.finish_reason.as_deref().unwrap_or(""),
                    chars = r.chars().count(),
                    "the model refused; showing the refusal as the reply"
                );
                events.push(StreamEvent::Text { content: r.clone() });
            }
            if let Some(ref tcs) = delta.tool_calls {
                for tc in tcs {
                    if is_unrequested_custom_call(tc.kind.as_deref(), tc.id.as_deref()) {
                        continue;
                    }
                    if let Some(ref id) = tc.id {
                        let name = tc.function.as_ref().and_then(|f| f.name.clone()).unwrap_or_default();
                        events.push(StreamEvent::ToolCallStart {
                            index: tc.index,
                            id: id.clone(),
                            name,
                        });
                    }
                    if let Some(ref f) = tc.function
                        && let Some(ref args) = f.arguments
                        && !args.is_empty()
                    {
                        events.push(StreamEvent::ToolCallDelta {
                            index: tc.index,
                            arguments: args.clone(),
                        });
                    }
                }
            }
        }
    }

    (events, finish_reason, usage)
}

#[async_trait]
impl ChatProvider for OpenAIChatProvider {
    fn adapter_name(&self) -> &'static str {
        self.extension.adapter_name()
    }

    async fn stream_chat_with_tools(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        params: ChatParams,
    ) -> Result<ChatStream, ProviderError> {
        let tools_opt = if tools.is_empty() { None } else { Some(tools.as_slice()) };
        let transport = ReqwestTransport::shared();
        let req = self.build_request(&messages, tools_opt, &params, true)?;
        let resp = transport.stream(req).await?;
        let extension = self.extension.clone();
        let model_name = params.model.clone();

        let mut response_model_emitted = false;
        let stream = SseStreamMixin::from_bytes(resp.bytes).flat_map(move |event| {
            let events: Vec<Result<StreamEvent, ProviderError>> = match event {
                Ok(ev) => {
                    if ev.data == "[DONE]" {
                        return futures::stream::iter(vec![]);
                    }
                    match parse_chat_chunk(&ev.data) {
                        Ok(mut chunk) => {
                            let state_events = extension.extract_chunk_state_events(&mut chunk, &model_name);
                            chunk.warn_ignored_fields();
                            let (mut stream_events, finish_reason, usage) = parse_openai_sse_events(&chunk);
                            if !state_events.is_empty() {
                                let mut combined = state_events;
                                combined.append(&mut stream_events);
                                stream_events = combined;
                            }
                            if !response_model_emitted && let Some(ref model) = chunk.model {
                                stream_events.push(StreamEvent::ResponseModel { model: model.clone() });
                                response_model_emitted = true;
                            }
                            if let Some(u) = usage {
                                stream_events.push(StreamEvent::UsageUpdate { usage: u });
                            }
                            if let Some(fr) = finish_reason {
                                stream_events.push(StreamEvent::Stop {
                                    reason: fr,
                                    usage: None,
                                });
                            }
                            stream_events.into_iter().map(Ok).collect()
                        }
                        Err(error) => vec![Err(error)],
                    }
                }
                Err(e) => vec![Err(ProviderError::Parse(e.to_string()))],
            };
            futures::stream::iter(events)
        });

        Ok(Box::pin(stream))
    }

    async fn chat(&self, messages: Vec<ChatMessage>, params: ChatParams) -> Result<String, ProviderError> {
        let transport = ReqwestTransport::shared();
        let req = self.build_request(&messages, None, &params, false)?;
        let resp = transport.execute(req).await?;

        let mut parsed = parse_chat_response(&resp.body)?;
        if let Some(choice) = parsed.choices.first_mut() {
            let _ = self
                .extension
                .extract_response_state(&mut choice.message, &params.model)?;
        }
        parsed.warn_ignored_fields();

        parsed
            .choices
            .into_iter()
            .next()
            .and_then(|choice| {
                reply_text(
                    choice.message.content,
                    choice.message.refusal,
                    choice.finish_reason.as_deref(),
                )
            })
            .ok_or_else(|| ProviderError::Parse("no content in response".into()))
    }

    async fn chat_with_tools(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        params: ChatParams,
    ) -> Result<AgentResponse, ProviderError> {
        let transport = ReqwestTransport::shared();
        let req = self.build_request(&messages, Some(&tools), &params, false)?;
        let resp = transport.execute(req).await?;

        let mut parsed = parse_chat_response(&resp.body)?;
        let provider_state = match parsed.choices.first_mut() {
            Some(choice) => self
                .extension
                .extract_response_state(&mut choice.message, &params.model)?,
            None => None,
        };
        parsed.warn_ignored_fields();

        let usage = parsed.usage.as_ref().map(normalise_openai_usage);
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError::Parse("response choices is empty".into()))?;
        let finish_reason = choice.finish_reason;
        let message = choice.message;
        let text = reply_text(message.content, message.refusal, finish_reason.as_deref()).unwrap_or_default();
        let reasoning_content = message.reasoning_content;
        let mut tool_calls = Vec::new();
        for tool_call in message.tool_calls.unwrap_or_default() {
            if is_unrequested_custom_call(tool_call.kind.as_deref(), Some(&tool_call.id)) {
                continue;
            }
            let function = tool_call.function.ok_or_else(|| {
                ProviderError::Parse(format!("tool call {} carries no `function` body", tool_call.id))
            })?;
            tool_calls.push(ToolCall {
                id: tool_call.id,
                name: function.name,
                arguments: function.arguments,
            });
        }

        Ok(AgentResponse {
            text,
            reasoning_content,
            tool_calls,
            usage,
            provider_state,
        })
    }
}

#[cfg(test)]
mod openai_tests {
    use super::*;
    use meridian_provider_sdk::client::RequestBody;

    fn standard_chunk() -> serde_json::Value {
        serde_json::json!({
            "id": "chatcmpl-abc123",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000,
            "model": "gpt-4.1-mini",
            "service_tier": "default",
            "system_fingerprint": "fp_44709d6fcb",
            "obfuscation": "x9Kq",
            "moderation": null,
            "choices": [{
                "index": 0,
                "delta": {
                    "role": "assistant",
                    "content": "hello",
                    "refusal": null,
                    "function_call": null,
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{\"path\":" }
                    }]
                },
                "finish_reason": null,
                "logprobs": null
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15,
                "prompt_tokens_details": { "cached_tokens": 4, "audio_tokens": 0 },
                "completion_tokens_details": {
                    "reasoning_tokens": 0,
                    "audio_tokens": 0,
                    "accepted_prediction_tokens": 0,
                    "rejected_prediction_tokens": 0
                }
            }
        })
    }

    #[test]
    fn a_standard_chunk_leaves_every_extra_empty_and_an_invented_field_does_not() {
        let chunk: ChatChunk = serde_json::from_value(standard_chunk()).unwrap();
        assert!(chunk.extra.is_empty(), "chunk: {:?}", chunk.extra.keys());
        let choice = &chunk.choices[0];
        assert!(choice.extra.is_empty(), "choice: {:?}", choice.extra.keys());
        let delta = choice.delta.as_ref().unwrap();
        assert!(delta.extra.is_empty(), "delta: {:?}", delta.extra.keys());
        let call = &delta.tool_calls.as_ref().unwrap()[0];
        assert!(call.extra.is_empty(), "tool call: {:?}", call.extra.keys());
        assert!(call.function.as_ref().unwrap().extra.is_empty());
        let usage = chunk.usage.as_ref().unwrap();
        assert!(usage.extra.is_empty(), "usage: {:?}", usage.extra.keys());

        let mut invented = standard_chunk();
        invented["relay_envelope"] = serde_json::json!("not logged");
        invented["choices"][0]["delta"]["relay_trace"] = serde_json::json!("not logged");
        let chunk: ChatChunk = serde_json::from_value(invented).unwrap();
        assert_eq!(chunk.extra.keys().collect::<Vec<_>>(), ["relay_envelope"]);
        let delta = chunk.choices[0].delta.as_ref().unwrap();
        assert_eq!(delta.extra.keys().collect::<Vec<_>>(), ["relay_trace"]);
    }

    #[test]
    fn documented_usage_details_are_not_extra() {
        let usage: ChunkUsage = serde_json::from_value(standard_chunk()["usage"].clone()).unwrap();
        let prompt = usage.prompt_tokens_details.as_ref().unwrap();
        assert!(prompt.extra.is_empty(), "{:?}", prompt.extra.keys());
        assert_eq!(prompt.cached_tokens, Some(4), "the priced field is still read");
        let completion = usage.completion_tokens_details.as_ref().unwrap();
        assert!(completion.extra.is_empty(), "{:?}", completion.extra.keys());
        assert_eq!(completion.reasoning_tokens, Some(0));
    }

    #[test]
    fn a_streamed_refusal_is_shown_as_text() {
        let chunk: ChatChunk = serde_json::from_value(serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": { "content": null, "refusal": "I cannot help with that." },
                "finish_reason": "stop"
            }]
        }))
        .unwrap();
        let (events, finish, _) = parse_openai_sse_events(&chunk);
        assert!(
            matches!(&events[..], [StreamEvent::Text { content }] if content == "I cannot help with that."),
            "{events:?}"
        );
        assert_eq!(finish.as_deref(), Some("stop"));
    }

    #[test]
    fn a_non_streamed_refusal_is_appended_to_the_text() {
        let parsed = parse_chat_response(
            br#"{"id":"chatcmpl-1","object":"chat.completion","created":1,"model":"gpt-4.1-mini",
                "choices":[{"index":0,"finish_reason":"stop","logprobs":null,
                    "message":{"role":"assistant","content":null,"refusal":"No.","annotations":[]}}],
                "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
        )
        .unwrap();
        assert!(parsed.extra.is_empty(), "{:?}", parsed.extra.keys());
        let choice = parsed.choices.into_iter().next().unwrap();
        assert!(choice.extra.is_empty());
        assert!(choice.message.extra.is_empty(), "{:?}", choice.message.extra.keys());
        let text = reply_text(
            choice.message.content,
            choice.message.refusal,
            choice.finish_reason.as_deref(),
        );
        assert_eq!(text.as_deref(), Some("No."));
        assert_eq!(
            reply_text(Some("a".into()), Some("b".into()), None).as_deref(),
            Some("ab")
        );
        assert_eq!(reply_text(None, Some(String::new()), None), None);
    }

    #[test]
    fn an_unrequested_custom_call_is_ignored() {
        let chunk: ChatChunk = serde_json::from_value(serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": { "tool_calls": [
                    { "index": 0, "id": "call_c", "type": "custom", "custom": { "name": "grammar", "input": "x" } },
                    { "index": 1, "id": "call_f", "type": "function", "function": { "name": "read_file", "arguments": "" } }
                ] },
                "finish_reason": null
            }]
        }))
        .unwrap();
        let call = &chunk.choices[0].delta.as_ref().unwrap().tool_calls.as_ref().unwrap()[0];
        assert!(
            call.extra.is_empty(),
            "`custom` is a documented field: {:?}",
            call.extra.keys()
        );
        let (events, _, _) = parse_openai_sse_events(&chunk);
        assert!(
            matches!(&events[..], [StreamEvent::ToolCallStart { id, .. }] if id == "call_f"),
            "{events:?}"
        );
    }

    #[test]
    fn verbosity_is_sent_at_the_top_level_when_set() {
        let provider = OpenAIChatProvider::new("https://api.openai.com/v1", "key");
        let mut params = ChatParams {
            model: "gpt-5".into(),
            verbosity: Some("low".into()),
            ..Default::default()
        };
        let req = provider
            .build_request(&[ChatMessage::user("hello")], None, &params, false)
            .unwrap();
        let Some(RequestBody::Json(body)) = req.body else {
            panic!("JSON body")
        };
        assert_eq!(body["verbosity"], "low");

        params.verbosity = None;
        let req = provider
            .build_request(&[ChatMessage::user("hello")], None, &params, false)
            .unwrap();
        let Some(RequestBody::Json(body)) = req.body else {
            panic!("JSON body")
        };
        assert!(body.get("verbosity").is_none());
    }

    #[test]
    fn tool_and_assistant_text_that_looks_like_parts_is_sent_as_text() {
        let wire = serialize_openai_messages(&[
            ChatMessage::assistant_with_tools(
                "",
                None,
                vec![crate::ToolCall {
                    id: "c1".into(),
                    name: "read_file".into(),
                    arguments: "{}".into(),
                }],
            ),
            ChatMessage::tool_result("c1", r#"[{"a":1}]"#),
            ChatMessage::assistant(r#"[{"b":2}]"#),
        ])
        .unwrap();
        assert_eq!(wire[1]["content"], r#"[{"a":1}]"#);
        assert_eq!(wire[2]["content"], r#"[{"b":2}]"#);
    }

    #[test]
    fn tool_pictures_follow_the_last_result_of_the_batch() {
        let wire = serialize_openai_messages(&[
            ChatMessage::assistant_with_tools(
                "",
                None,
                vec![
                    crate::ToolCall {
                        id: "c1".into(),
                        name: "shot".into(),
                        arguments: "{}".into(),
                    },
                    crate::ToolCall {
                        id: "c2".into(),
                        name: "read".into(),
                        arguments: "{}".into(),
                    },
                ],
            ),
            ChatMessage::tool_result_parts(
                "c1",
                r#"[{"type":"text","text":"shot"},{"type":"image_url","image_url":{"url":"data:image/png;base64,QUJD"}}]"#,
            ),
            ChatMessage::tool_result("c2", "plain"),
            ChatMessage::assistant("seen"),
        ])
        .unwrap();
        assert_eq!(wire.len(), 5);
        assert_eq!(wire[1]["role"], "tool");
        assert_eq!(wire[1]["content"], "shot\n[1 image(s) follow the tool results]");
        assert_eq!(wire[2]["role"], "tool");
        assert_eq!(wire[3]["role"], "user");
        assert_eq!(
            wire[3]["content"],
            serde_json::json!([
                {"type": "text", "text": "[Images returned by tool call c1]"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}},
            ])
        );
        assert_eq!(wire[4]["role"], "assistant");
    }

    #[test]
    fn a_user_file_part_is_refused_on_chat_completions() {
        let content = r#"[{"type":"file","file":{"url":"data:application/pdf;base64,AAAA","mime_type":"application/pdf","name":"a.pdf"}}]"#;
        let error = serialize_openai_messages(&[ChatMessage::user(content)]).unwrap_err();
        assert!(error.to_string().contains("cannot take a file part"), "{error}");
    }

    #[test]
    fn a_leftover_file_uri_on_an_image_is_refused() {
        let content = r#"[{"type":"image_url","image_url":{"url":"file:///C:/data/files/a.png"}}]"#;
        let error = serialize_openai_messages(&[ChatMessage::user(content)]).unwrap_err();
        assert!(error.to_string().contains("unresolved attachment"), "{error}");
    }

    fn openai_body(base_url: &str, cache_key: Option<&str>) -> serde_json::Value {
        let provider = OpenAIChatProvider::new(base_url, "key");
        let p = ChatParams {
            model: "gpt-5.5".into(),
            cache_key: cache_key.map(str::to_string),
            ..Default::default()
        };
        let req = provider
            .build_request(&[ChatMessage::user("hello")], None, &p, true)
            .unwrap();
        let Some(RequestBody::Json(body)) = req.body else {
            panic!("JSON body")
        };
        body
    }

    #[test]
    fn the_official_openai_host_gets_a_prompt_cache_key() {
        let body = openai_body("https://api.openai.com/v1", Some("conv-42"));
        assert_eq!(body["prompt_cache_key"], "conv-42");
    }

    #[test]
    fn a_relay_is_not_sent_an_unknown_body_field() {
        let body = openai_body("https://relay.example.test/v1", Some("conv-42"));
        assert!(body.get("prompt_cache_key").is_none());
    }

    #[test]
    fn a_lookalike_host_is_not_the_official_api() {
        let body = openai_body("https://api.openai.com.evil.test/v1", Some("conv-42"));
        assert!(body.get("prompt_cache_key").is_none());
    }

    #[test]
    fn without_a_cache_key_no_field_is_sent() {
        let body = openai_body("https://api.openai.com/v1", None);
        assert!(body.get("prompt_cache_key").is_none());
    }

    #[test]
    fn embedded_relay_error_is_not_reported_as_missing_choices() {
        let error = parse_chat_chunk(
            r#"{"error":{"message":"request parameters are invalid","type":"invalid_request_error"}}"#,
        )
        .err()
        .expect("error envelope must fail");
        assert!(matches!(error, ProviderError::Upstream(ref message) if message.contains("request parameters")));
    }

    #[test]
    fn malformed_success_still_reports_the_required_choices_field() {
        let error = parse_chat_chunk(r#"{"usage":{}}"#).err().expect("choices is required");
        assert!(matches!(error, ProviderError::Parse(ref message) if message.contains("missing field `choices`")));
    }

    #[test]
    fn missing_required_choices_is_rejected() {
        let error = serde_json::from_value::<ChatChunk>(serde_json::json!({
            "usage": {
                "prompt_tokens": 12,
                "completion_tokens": 3,
                "total_tokens": 15
            }
        }))
        .err()
        .expect("choices must be required");
        assert!(error.to_string().contains("missing field `choices`"));
    }

    #[test]
    fn null_required_choices_is_rejected() {
        let error = serde_json::from_value::<ChatChunk>(serde_json::json!({
            "choices": null,
            "usage": { "total_tokens": 15 }
        }))
        .err()
        .expect("choices must be an array");
        assert!(error.to_string().contains("invalid type: null"));
    }

    #[test]
    fn extra_fields_are_captured_without_entering_the_domain_model() {
        let chunk: ChatChunk = serde_json::from_value(serde_json::json!({
            "id": "relay-chunk-id",
            "relay_envelope": "not logged",
            "choices": [{
                "index": 0,
                "relay_choice": "not logged",
                "delta": { "content": "hello", "relay_trace": "not logged" },
                "finish_reason": null
            }],
            "usage": null
        }))
        .unwrap();
        assert!(chunk.extra.contains_key("relay_envelope"));
        assert!(!chunk.extra.contains_key("id"), "a documented field is not extra");
        assert!(chunk.choices[0].extra.contains_key("relay_choice"));
        assert!(!chunk.choices[0].extra.contains_key("index"));
        assert!(
            chunk.choices[0]
                .delta
                .as_ref()
                .unwrap()
                .extra
                .contains_key("relay_trace")
        );
    }

    #[test]
    fn relay_error_envelope_is_rejected_as_missing_the_required_shape() {
        let error = serde_json::from_value::<ChatChunk>(serde_json::json!({
            "error": { "code": 400, "type": "invalid_request", "message": "not logged" }
        }))
        .err()
        .expect("an error envelope is not a chat chunk");
        assert!(error.to_string().contains("missing field `choices`"));
    }
}

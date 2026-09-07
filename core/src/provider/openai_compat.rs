use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::StreamExt;
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

use super::{
    AgentResponse, ChatMessage, ChatParams, ChatProvider, ChatStream, ProviderError, StreamEvent, TokenUsage, ToolCall,
    ToolDefinition,
};
use crate::client::{HttpTransport, Request, RequestBody, ReqwestTransport};
use crate::provider::dto::{ExtraIgnore, embedded_upstream_error, warn_extra_fields};
use crate::provider::state::GOOGLE_OPENAI_CHAT_PROTOCOL;

pub struct OpenAICompatProvider {
    base_url: String,
    api_key: String,
    flavor: OpenAICompatFlavor,
}

/// What a chat-completions endpoint needs *beyond* the dialect itself.
///
/// A flavor exists only where the wire format is the same but the endpoint
/// wants something extra: Google's thought signatures, xAI's cache routing
/// header. Anything that needs a different request or response shape belongs in
/// its own adapter instead — which is what `deepseek.rs` and `gemma_tool.rs`
/// are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenAICompatFlavor {
    Generic,
    Google,
    Xai,
}

impl OpenAICompatProvider {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            flavor: OpenAICompatFlavor::Generic,
        }
    }

    pub fn new_google(base_url: &str, api_key: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            flavor: OpenAICompatFlavor::Google,
        }
    }

    pub fn new_xai(base_url: &str, api_key: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            flavor: OpenAICompatFlavor::Xai,
        }
    }

    fn build_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
        params: &ChatParams,
        stream: bool,
    ) -> Result<Request, ProviderError> {
        let serialized_messages = match self.flavor {
            // Spelled out rather than wildcarded: the next flavor should have
            // to say which serialisation it wants.
            OpenAICompatFlavor::Generic | OpenAICompatFlavor::Xai => serialize_openai_messages(messages),
            OpenAICompatFlavor::Google => serialize_google_messages(messages, &params.model),
        }?;
        let mut body = serde_json::json!({
            "model": params.model,
            "messages": serialized_messages,
            "stream": stream,
        });
        if stream {
            body["stream_options"] = serde_json::json!({"include_usage": true});
        }
        if self.flavor != OpenAICompatFlavor::Google
            && let Some(t) = params.temperature
        {
            body["temperature"] = serde_json::json!(t);
        }
        if self.flavor != OpenAICompatFlavor::Google
            && let Some(p) = params.top_p
        {
            body["top_p"] = serde_json::json!(p);
        }
        if let Some(m) = params.max_tokens {
            body["max_tokens"] = serde_json::json!(m);
        }
        if let Some(ref effort) = params.thinking_effort {
            body["reasoning_effort"] = serde_json::json!(effort);
        }
        // A top-level field on chat-completions, unlike the Responses API's
        // `text.verbosity`. `filter_params` has already cleared it for models
        // whose capabilities do not list it, so this is a pure passthrough.
        if let Some(ref verbosity) = params.verbosity {
            body["verbosity"] = serde_json::json!(verbosity);
        }
        if self.flavor == OpenAICompatFlavor::Google {
            body["extra_body"] = serde_json::json!({
                "google": {
                    "thinking_config": { "include_thoughts": true }
                }
            });
        }
        if self.flavor != OpenAICompatFlavor::Google && params.fast {
            // The config-facing name is "fast"; the wire value is the priority tier.
            body["service_tier"] = serde_json::json!("priority");
        }
        if let Some(tools) = tools
            && !tools.is_empty()
        {
            body["tools"] = serde_json::json!(
                tools
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "type": "function",
                            "function": {
                                "name": t.name,
                                "description": t.description,
                                "parameters": t.parameters,
                            }
                        })
                    })
                    .collect::<Vec<_>>()
            );
        }

        let mut req = Request::new(http::Method::POST, format!("{}/chat/completions", self.base_url));
        req.headers.insert(
            http::header::AUTHORIZATION,
            super::auth_header_value(&format!("Bearer {}", self.api_key)),
        );
        // xAI's prompt cache lives on whichever server answered, so this header
        // is what sends a conversation back to the one already holding its
        // prefix. Their own docs put it plainly: without it you often pay full
        // input price on a cache-cold server.
        //
        // A key that cannot be a header value is dropped rather than replaced
        // with a placeholder: unlike the API key, nothing here fails visibly, so
        // a stand-in would silently pin every such conversation to one server.
        if self.flavor == OpenAICompatFlavor::Xai
            && let Some(key) = params.cache_key.as_deref()
        {
            match http::HeaderValue::from_str(key) {
                Ok(value) => {
                    req.headers.insert("x-grok-conv-id", value);
                }
                Err(_) => tracing::warn!(
                    key_chars = key.chars().count(),
                    "the cache key is not representable as a header; this turn will not route to a warm cache"
                ),
            }
        }
        req.body = Some(RequestBody::Json(body));
        Ok(req)
    }
}

fn content_value(content: &str) -> Result<serde_json::Value, ProviderError> {
    if let Some(parts) = super::decode_message_parts(content).map_err(ProviderError::Parse)? {
        return serde_json::to_value(parts).map_err(|error| ProviderError::Parse(error.to_string()));
    }
    Ok(serde_json::Value::String(content.to_string()))
}

pub fn serialize_openai_messages(messages: &[ChatMessage]) -> Result<Vec<serde_json::Value>, ProviderError> {
    messages
        .iter()
        .map(|m| {
            // chat-completions has a native `name`, so identity never touches the body.
            let rendered = super::render_message(m, super::SenderRendering::NameField).map_err(ProviderError::Parse)?;
            let mut msg = serde_json::json!({ "role": m.role, "content": content_value(&rendered.content)? });
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
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleExtraContent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    google: Option<GoogleThoughtSignatureDto>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GoogleThoughtSignatureDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thought_signature: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl GoogleExtraContent {
    fn thought_signature(&self) -> Option<&str> {
        self.google
            .as_ref()?
            .thought_signature
            .as_deref()
            .filter(|s| !s.is_empty())
    }

    fn warn_ignored_fields(&self) {
        warn_extra_fields("google_extra_content", &self.extra);
        if let Some(google) = &self.google {
            warn_extra_fields("google_thought_signature", &google.extra);
        }
    }
}

fn google_extra(signature: &str) -> serde_json::Value {
    serde_json::to_value(GoogleExtraContent {
        google: Some(GoogleThoughtSignatureDto {
            thought_signature: Some(signature.to_string()),
            extra: ExtraIgnore::default(),
        }),
        extra: ExtraIgnore::default(),
    })
    .expect("Google signature DTO is serializable")
}

/// Google validates historical Gemini 3 function calls. A call produced by a
/// different vendor (or by Meridian before signatures were durable) cannot be
/// sent as a native tool call, so it becomes ordinary transcript text instead.
fn serialize_google_messages(messages: &[ChatMessage], model: &str) -> Result<Vec<serde_json::Value>, ProviderError> {
    use super::state::GoogleSignatureLocation;
    use std::collections::{HashMap, HashSet};

    let mut flattened_calls = HashSet::<String>::new();
    let mut signed_tool_names = HashMap::<String, String>::new();
    let mut interrupted_results = Vec::<(String, String)>::new();
    let mut out = Vec::new();

    for (message_index, m) in messages.iter().enumerate() {
        if m.role != "tool" {
            append_interrupted_results(&mut out, std::mem::take(&mut interrupted_results));
            flattened_calls.clear();
            signed_tool_names.clear();
        }
        if m.role == "assistant"
            && let Some(tool_calls) = m.tool_calls.as_ref()
        {
            for call in tool_calls {
                super::decode_tool_arguments(&call.arguments, &call.id).map_err(ProviderError::Parse)?;
            }
            // Call ids are only unique within one provider round. A later
            // assistant message may legally reuse one, so flattening state is
            // scoped to the immediately following result group.
            let signatures = m
                .provider_state
                .as_ref()
                .and_then(|s| s.google_signatures_for(GOOGLE_OPENAI_CHAT_PROTOCOL, model));
            let has_tool_signature = signatures.is_some_and(|items| {
                items
                    .iter()
                    .any(|item| matches!(item.location, GoogleSignatureLocation::ToolCall { .. }))
            });
            if !has_tool_signature {
                let mut content = m.content.clone();
                for tc in tool_calls {
                    if !content.is_empty() {
                        content.push('\n');
                    }
                    content.push_str(&format!("[Historical tool call: {}({})]", tc.name, tc.arguments));
                    flattened_calls.insert(tc.id.clone());
                }
                out.push(serde_json::json!({ "role": "assistant", "content": content }));
                continue;
            }
            signed_tool_names.extend(tool_calls.iter().map(|tc| (tc.id.clone(), tc.name.clone())));
        }

        if m.role == "tool" && m.tool_call_id.as_ref().is_some_and(|id| flattened_calls.contains(id)) {
            out.push(serde_json::json!({
                "role": "user",
                "content": format!(
                    "[Historical tool result for {}]\n{}",
                    m.tool_call_id.as_deref().unwrap_or("unknown"),
                    m.content
                )
            }));
            continue;
        }

        let rendered = super::render_message(m, super::SenderRendering::NameField).map_err(ProviderError::Parse)?;
        let mut msg = serde_json::json!({ "role": m.role, "content": content_value(&rendered.content)? });
        if let Some(name) = rendered.name {
            msg["name"] = serde_json::json!(name);
        }
        let signatures = m
            .provider_state
            .as_ref()
            .and_then(|s| s.google_signatures_for(GOOGLE_OPENAI_CHAT_PROTOCOL, model));
        if let Some(message_signature) = signatures.and_then(|items| {
            items
                .iter()
                .find(|item| matches!(item.location, GoogleSignatureLocation::Message))
        }) {
            msg["extra_content"] = google_extra(&message_signature.signature);
        }
        if let Some(tool_calls) = m.tool_calls.as_ref() {
            let mut wire_calls = Vec::with_capacity(tool_calls.len());
            for (index, tc) in tool_calls.iter().enumerate() {
                let mut wire = serde_json::json!({
                    "id": tc.id,
                    "type": "function",
                    "function": { "name": tc.name, "arguments": tc.arguments }
                });
                if let Some(signature) = signatures.and_then(|items| {
                    items.iter().find(|item| match &item.location {
                        GoogleSignatureLocation::ToolCall { index: stored, call_id } => {
                            *stored == index || call_id.as_deref() == Some(tc.id.as_str())
                        }
                        _ => false,
                    })
                }) {
                    wire["extra_content"] = google_extra(&signature.signature);
                }
                wire_calls.push(wire);
            }
            msg["tool_calls"] = serde_json::Value::Array(wire_calls);
            // A killed process may leave the signed call durable but no result.
            // Close that protocol edge deterministically without re-running it.
            out.push(msg);
            for tc in tool_calls {
                let has_result = messages[message_index + 1..]
                    .iter()
                    .take_while(|next| next.role == "tool")
                    .any(|next| next.tool_call_id.as_deref() == Some(tc.id.as_str()));
                if !has_result {
                    interrupted_results.push((tc.id.clone(), tc.name.clone()));
                }
            }
            continue;
        }
        if let Some(tool_call_id) = m.tool_call_id.as_ref() {
            msg["tool_call_id"] = serde_json::json!(tool_call_id);
            if let Some(name) = signed_tool_names.get(tool_call_id) {
                msg["name"] = serde_json::json!(name);
            }
        }
        out.push(msg);
    }
    append_interrupted_results(&mut out, interrupted_results);
    Ok(out)
}

fn append_interrupted_results(out: &mut Vec<serde_json::Value>, results: Vec<(String, String)>) {
    out.extend(results.into_iter().map(|(id, name)| {
        serde_json::json!({
            "role": "tool",
            "name": name,
            "tool_call_id": id,
            "content": "Tool execution was interrupted before a result was recorded. It was not retried."
        })
    }));
}

// The fields OpenAI documents on a chat-completions reply and this code reads
// nothing from are named as `IgnoredAny` rather than left to `extra`, for the
// same reason `models.rs` does it: the warning `extra` feeds is for shapes this
// code has not seen, and a standard reply tripping it on every stream is a
// warning nobody reads. `default`, because relays omit them.
#[derive(Deserialize)]
pub struct ChatChunk {
    /// This is the required OpenAI chat-completions envelope field. A relay
    /// returning a different success body must fail here instead of being
    /// mistaken for an empty token chunk.
    pub choices: Vec<ChunkChoice>,
    pub usage: Option<ChunkUsage>,
    #[serde(default, rename = "id")]
    _id: IgnoredAny,
    #[serde(default, rename = "object")]
    _object: IgnoredAny,
    #[serde(default, rename = "created")]
    _created: IgnoredAny,
    #[serde(default, rename = "model")]
    _model: IgnoredAny,
    #[serde(default, rename = "service_tier")]
    _service_tier: IgnoredAny,
    #[serde(default, rename = "system_fingerprint")]
    _system_fingerprint: IgnoredAny,
    #[serde(default, rename = "obfuscation")]
    _obfuscation: IgnoredAny,
    #[serde(default, rename = "moderation")]
    _moderation: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl ChatChunk {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("chat_chunk", &self.extra);
        for choice in &self.choices {
            choice.warn_ignored_fields();
        }
        if let Some(usage) = &self.usage {
            usage.warn_ignored_fields();
        }
    }
}

fn parse_chat_chunk(raw: &str) -> Result<ChatChunk, ProviderError> {
    serde_json::from_str(raw).map_err(|error| {
        embedded_upstream_error(raw.as_bytes())
            .map(ProviderError::Upstream)
            .unwrap_or_else(|| ProviderError::Parse(error.to_string()))
    })
}

#[derive(Deserialize)]
pub struct ChunkChoice {
    pub delta: Option<Delta>,
    /// `stop | length | tool_calls | content_filter | function_call`, passed
    /// through as the string it arrived as.
    pub finish_reason: Option<String>,
    #[serde(default, rename = "index")]
    _index: IgnoredAny,
    #[serde(default, rename = "logprobs")]
    _logprobs: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl ChunkChoice {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("chunk_choice", &self.extra);
        if let Some(delta) = &self.delta {
            delta.warn_ignored_fields();
        }
    }
}

#[derive(Deserialize)]
pub struct Delta {
    pub content: Option<String>,
    /// The model declining, in its own words. It arrives *instead of*
    /// `content`, so a reader that only draws `content` shows a blank reply.
    pub refusal: Option<String>,
    pub reasoning_content: Option<String>,
    pub tool_calls: Option<Vec<DeltaToolCall>>,
    pub extra_content: Option<GoogleExtraContent>,
    #[serde(default, rename = "role")]
    _role: IgnoredAny,
    #[serde(default, rename = "function_call")]
    _function_call: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl Delta {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("delta", &self.extra);
        if let Some(extra_content) = &self.extra_content {
            extra_content.warn_ignored_fields();
        }
        if let Some(tool_calls) = &self.tool_calls {
            for tool_call in tool_calls {
                tool_call.warn_ignored_fields();
            }
        }
    }
}

#[derive(Deserialize)]
pub struct DeltaToolCall {
    pub index: usize,
    pub id: Option<String>,
    /// `function` or `custom`. Only the opening delta of a call carries it.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    pub function: Option<DeltaFunction>,
    /// A `custom` call's `{name, input}` body. This app never requests a custom
    /// tool, so one arriving is ignored (see `is_unrequested_custom_call`).
    #[serde(default, rename = "custom")]
    _custom: IgnoredAny,
    pub extra_content: Option<GoogleExtraContent>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

/// A `type: "custom"` call names a tool this app never asked for: the request
/// body only ever sends `type: "function"` definitions. Dispatching one would
/// mean routing a `{name, input}` body through machinery built for JSON
/// arguments, so it is logged and dropped rather than half-handled.
fn is_unrequested_custom_call(kind: Option<&str>, id: Option<&str>) -> bool {
    if kind != Some("custom") {
        return false;
    }
    tracing::warn!(
        call_id = id.unwrap_or(""),
        "upstream announced a custom tool call this app never requested; ignoring it"
    );
    true
}

impl DeltaToolCall {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("delta_tool_call", &self.extra);
        if let Some(function) = &self.function {
            function.warn_ignored_fields();
        }
        if let Some(extra_content) = &self.extra_content {
            extra_content.warn_ignored_fields();
        }
    }
}

#[derive(Deserialize)]
pub struct DeltaFunction {
    pub name: Option<String>,
    pub arguments: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl DeltaFunction {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("delta_function", &self.extra);
    }
}

#[derive(Deserialize)]
pub struct ChunkUsage {
    pub prompt_tokens: Option<i32>,
    pub completion_tokens: Option<i32>,
    pub total_tokens: Option<i32>,
    /// DeepSeek's flat split of the prompt, where hit + miss == `prompt_tokens`;
    /// only the hit half is read, the miss half being derivable.
    pub prompt_cache_hit_tokens: Option<i32>,
    /// OpenAI's own chat-completions shape for the same information, one object
    /// deeper. It needs a struct rather than another `Option<i32>` because serde
    /// cannot reach into a nested object from a flat field, and a
    /// `serde_json::Value` here would push "is this key present" down into the
    /// normaliser where it is easy to get wrong.
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    /// Where the reasoning count lives in this dialect. Read because the two
    /// endpoints speaking it disagree about whether `completion_tokens` already
    /// includes it — see `billable_completion_tokens`.
    pub completion_tokens_details: Option<CompletionTokensDetails>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl ChunkUsage {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("chunk_usage", &self.extra);
        if let Some(details) = &self.prompt_tokens_details {
            details.warn_ignored_fields();
        }
        if let Some(details) = &self.completion_tokens_details {
            details.warn_ignored_fields();
        }
    }
}

/// Only `cached_tokens` is priced on; the documented siblings are named so a
/// standard reply does not warn. Anything undocumented is still captured by
/// `ExtraIgnore` and warned rather than disappearing silently.
#[derive(Deserialize)]
pub struct PromptTokensDetails {
    pub cached_tokens: Option<i32>,
    #[serde(default, rename = "audio_tokens")]
    _audio_tokens: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl PromptTokensDetails {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("prompt_tokens_details", &self.extra);
    }
}

/// The reasoning half of the same story, for the same reason.
#[derive(Deserialize)]
pub struct CompletionTokensDetails {
    pub reasoning_tokens: Option<i32>,
    #[serde(default, rename = "audio_tokens")]
    _audio_tokens: IgnoredAny,
    #[serde(default, rename = "accepted_prediction_tokens")]
    _accepted_prediction_tokens: IgnoredAny,
    #[serde(default, rename = "rejected_prediction_tokens")]
    _rejected_prediction_tokens: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl CompletionTokensDetails {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("completion_tokens_details", &self.extra);
    }
}

#[derive(Deserialize)]
struct ChatResponseDto {
    choices: Vec<ResponseChoiceDto>,
    usage: Option<ChunkUsage>,
    #[serde(default, rename = "id")]
    _id: IgnoredAny,
    #[serde(default, rename = "object")]
    _object: IgnoredAny,
    #[serde(default, rename = "created")]
    _created: IgnoredAny,
    #[serde(default, rename = "model")]
    _model: IgnoredAny,
    #[serde(default, rename = "service_tier")]
    _service_tier: IgnoredAny,
    #[serde(default, rename = "system_fingerprint")]
    _system_fingerprint: IgnoredAny,
    #[serde(default, rename = "metadata")]
    _metadata: IgnoredAny,
    #[serde(default, rename = "moderation")]
    _moderation: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl ChatResponseDto {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("chat_response", &self.extra);
        for choice in &self.choices {
            choice.warn_ignored_fields();
        }
        if let Some(usage) = &self.usage {
            usage.warn_ignored_fields();
        }
    }
}

fn parse_chat_response(raw: &[u8]) -> Result<ChatResponseDto, ProviderError> {
    serde_json::from_slice(raw).map_err(|error| {
        embedded_upstream_error(raw)
            .map(ProviderError::Upstream)
            .unwrap_or_else(|| ProviderError::Parse(error.to_string()))
    })
}

#[derive(Deserialize)]
struct ResponseChoiceDto {
    message: ResponseMessageDto,
    /// Read only to be logged beside a refusal; the turn's stop reason for a
    /// non-streaming call is decided by the caller.
    finish_reason: Option<String>,
    #[serde(default, rename = "index")]
    _index: IgnoredAny,
    #[serde(default, rename = "logprobs")]
    _logprobs: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl ResponseChoiceDto {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("response_choice", &self.extra);
        self.message.warn_ignored_fields();
    }
}

#[derive(Deserialize)]
struct ResponseMessageDto {
    content: Option<String>,
    /// See `Delta::refusal`.
    refusal: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<ResponseToolCallDto>>,
    extra_content: Option<GoogleExtraContent>,
    #[serde(default, rename = "role")]
    _role: IgnoredAny,
    #[serde(default, rename = "function_call")]
    _function_call: IgnoredAny,
    #[serde(default, rename = "annotations")]
    _annotations: IgnoredAny,
    #[serde(default, rename = "audio")]
    _audio: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

/// What a non-streaming reply says, with a refusal folded in after the prose.
/// `None` only when neither was given: an empty `content` beside a refusal is
/// what a refused reply looks like, and the refusal is the answer.
fn reply_text(content: Option<String>, refusal: Option<String>, finish_reason: Option<&str>) -> Option<String> {
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

impl ResponseMessageDto {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("response_message", &self.extra);
        if let Some(extra_content) = &self.extra_content {
            extra_content.warn_ignored_fields();
        }
        if let Some(tool_calls) = &self.tool_calls {
            for tool_call in tool_calls {
                tool_call.warn_ignored_fields();
            }
        }
    }
}

#[derive(Deserialize)]
struct ResponseToolCallDto {
    id: String,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    /// Required for a `function` call; a `custom` call carries `custom` instead,
    /// which is why this is not simply a required field.
    function: Option<ResponseFunctionDto>,
    #[serde(default, rename = "custom")]
    _custom: IgnoredAny,
    extra_content: Option<GoogleExtraContent>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl ResponseToolCallDto {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("response_tool_call", &self.extra);
        if let Some(function) = &self.function {
            function.warn_ignored_fields();
        }
        if let Some(extra_content) = &self.extra_content {
            extra_content.warn_ignored_fields();
        }
    }
}

#[derive(Deserialize)]
struct ResponseFunctionDto {
    name: String,
    arguments: String,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl ResponseFunctionDto {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("response_function", &self.extra);
    }
}

/// The one place both chat-completions dialects become the same thing.
///
/// They spell the cached prefix differently — DeepSeek puts
/// `prompt_cache_hit_tokens` at the top level, OpenAI nests `cached_tokens`
/// under `prompt_tokens_details` — but they agree that `prompt_tokens` is
/// already the whole prompt, so only the cache field has to be reconciled.
///
/// Neither dialect has a notion of a *paid* cache write, so `cache_write_tokens`
/// stays `None`: writing `Some(0)` would claim the endpoint reported a zero it
/// never mentioned, and the cost formula would then have no way to tell a
/// provider without caching apart from one whose cache was merely cold.
///
/// The DeepSeek field wins when both are present. A gateway emitting both is
/// almost certainly a DeepSeek proxy padding its response into OpenAI's shape,
/// and its native field is the one its own billing derives from.
pub fn normalise_openai_usage(u: &ChunkUsage) -> TokenUsage {
    let cache_read = u
        .prompt_cache_hit_tokens
        .or_else(|| u.prompt_tokens_details.as_ref().and_then(|d| d.cached_tokens));
    TokenUsage {
        prompt_tokens: u.prompt_tokens,
        completion_tokens: billable_completion_tokens(u),
        total_tokens: u.total_tokens,
        cache_read_tokens: cache_read,
        cache_write_tokens: None,
        // Chat-completions has no server-side tools — xAI answers a request
        // carrying one with a 422 — so there is never anything to bill here.
        billable_tool_calls: None,
    }
}

/// Output tokens as they are *billed*, which is not always what
/// `completion_tokens` says.
///
/// Reasoning tokens cost the output rate everywhere that has them, and OpenAI
/// and DeepSeek both count them inside `completion_tokens`. xAI does not: a
/// measured `grok-4.6` reply reported `prompt 214 / completion 1 /
/// reasoning 59 / total 274`, and its own `cost_in_usd_ticks` billed all sixty
/// output tokens. Taking `completion_tokens` at face value there under-reports
/// a reasoning-heavy turn by an order of magnitude — the same failure mode as
/// the cache-rate bug, and just as invisible, since the number stays plausible.
///
/// The provider's own `total_tokens` is what decides, rather than which vendor
/// we think we are talking to. A dialect that already includes reasoning
/// satisfies `total - prompt == completion` and is left alone; one that does not
/// leaves exactly `reasoning_tokens` unaccounted for, and only then are they
/// added. Anything else — a missing total, an arithmetic that adds up to
/// neither — is not evidence, so nothing is changed. That way a relay with a
/// vague usage block can only ever be reported as it reported itself, never
/// inflated by us.
fn billable_completion_tokens(u: &ChunkUsage) -> Option<i32> {
    let completion = u.completion_tokens?;
    let reasoning = u
        .completion_tokens_details
        .as_ref()
        .and_then(|d| d.reasoning_tokens)
        .unwrap_or(0);
    if reasoning <= 0 {
        return Some(completion);
    }
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

pub fn parse_openai_sse_events(chunk: &ChatChunk) -> (Vec<StreamEvent>, Option<String>, Option<TokenUsage>) {
    parse_openai_sse_events_for(chunk, None)
}

fn parse_openai_sse_events_for(
    chunk: &ChatChunk,
    google_model: Option<&str>,
) -> (Vec<StreamEvent>, Option<String>, Option<TokenUsage>) {
    use super::state::{GoogleSignatureLocation, ProviderStateUpdate};

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
            if let (Some(model), Some(signature)) = (
                google_model,
                delta
                    .extra_content
                    .as_ref()
                    .and_then(GoogleExtraContent::thought_signature),
            ) {
                events.push(StreamEvent::ProviderStateUpdate {
                    update: ProviderStateUpdate::GoogleThoughtSignatureDelta {
                        protocol: GOOGLE_OPENAI_CHAT_PROTOCOL.into(),
                        model: model.to_string(),
                        location: GoogleSignatureLocation::Message,
                        delta: signature.to_string(),
                    },
                });
            }
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
            // A refusal is the reply. Shown as text rather than given an event
            // of its own, because every reader of the transcript already knows
            // how to draw text and none of them knows a blank turn was a "no".
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
                    if let (Some(model), Some(signature)) = (
                        google_model,
                        tc.extra_content
                            .as_ref()
                            .and_then(GoogleExtraContent::thought_signature),
                    ) {
                        events.push(StreamEvent::ProviderStateUpdate {
                            update: ProviderStateUpdate::GoogleThoughtSignatureDelta {
                                protocol: GOOGLE_OPENAI_CHAT_PROTOCOL.into(),
                                model: model.to_string(),
                                location: GoogleSignatureLocation::ToolCall {
                                    index: tc.index,
                                    call_id: tc.id.clone(),
                                },
                                delta: signature.to_string(),
                            },
                        });
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
impl ChatProvider for OpenAICompatProvider {
    #[cfg(test)]
    fn adapter_name(&self) -> &'static str {
        "OpenAICompatProvider"
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
        let google_model = (self.flavor == OpenAICompatFlavor::Google).then(|| params.model.clone());

        let stream = resp
            .bytes
            .map(|r| r.map_err(ProviderError::Transport))
            .eventsource()
            .flat_map(move |event| {
                let events: Vec<Result<StreamEvent, ProviderError>> = match event {
                    Ok(ev) => {
                        if ev.data == "[DONE]" {
                            return futures::stream::iter(vec![]);
                        }
                        match parse_chat_chunk(&ev.data) {
                            Ok(chunk) => {
                                chunk.warn_ignored_fields();
                                let (mut stream_events, finish_reason, usage) =
                                    parse_openai_sse_events_for(&chunk, google_model.as_deref());
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

        let parsed = parse_chat_response(&resp.body)?;
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

        let parsed = parse_chat_response(&resp.body)?;
        parsed.warn_ignored_fields();

        let usage = parsed.usage.as_ref().map(normalise_openai_usage);
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError::Parse("response choices is empty".into()))?;
        let finish_reason = choice.finish_reason;
        let message = choice.message;
        let provider_state = if self.flavor == OpenAICompatFlavor::Google {
            google_state_from_message(&message, &params.model)?
        } else {
            None
        };
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

fn google_state_from_message(
    message: &ResponseMessageDto,
    model: &str,
) -> Result<Option<super::state::ProviderState>, ProviderError> {
    use super::state::{GoogleSignatureLocation, ProviderStateAccumulator, ProviderStateUpdate};

    let mut state = ProviderStateAccumulator::default();
    if let Some(extra) = &message.extra_content
        && let Some(signature) = extra.thought_signature()
    {
        state
            .apply(ProviderStateUpdate::GoogleThoughtSignatureDelta {
                protocol: GOOGLE_OPENAI_CHAT_PROTOCOL.into(),
                model: model.to_string(),
                location: GoogleSignatureLocation::Message,
                delta: signature.to_string(),
            })
            .map_err(ProviderError::Parse)?;
    }
    if let Some(tool_calls) = &message.tool_calls {
        for (index, call) in tool_calls.iter().enumerate() {
            let Some(extra) = &call.extra_content else {
                continue;
            };
            if let Some(signature) = extra.thought_signature() {
                state
                    .apply(ProviderStateUpdate::GoogleThoughtSignatureDelta {
                        protocol: GOOGLE_OPENAI_CHAT_PROTOCOL.into(),
                        model: model.to_string(),
                        location: GoogleSignatureLocation::ToolCall {
                            index,
                            call_id: Some(call.id.clone()),
                        },
                        delta: signature.to_string(),
                    })
                    .map_err(ProviderError::Parse)?;
            }
        }
    }
    Ok(state.finish())
}

#[cfg(test)]
mod xai_tests {
    use super::*;
    use crate::client::RequestBody;

    fn params() -> ChatParams {
        ChatParams {
            model: "grok-4.6".into(),
            thinking_enabled: true,
            thinking_effort: Some("xhigh".into()),
            temperature: Some(0.7),
            cache_key: Some("conv-42".into()),
            ..Default::default()
        }
    }

    /// The header is the whole reason this flavor exists. Without it a
    /// conversation lands on whichever server the balancer picks and pays full
    /// input price against a cold cache — nothing about the reply says so.
    #[test]
    fn the_cache_key_becomes_the_routing_header() {
        let provider = OpenAICompatProvider::new_xai("https://api.x.ai/v1", "key");
        let req = provider
            .build_request(&[ChatMessage::user("hello")], None, &params(), true)
            .unwrap();
        assert_eq!(req.headers.get("x-grok-conv-id").unwrap(), "conv-42");

        let Some(RequestBody::Json(body)) = req.body else {
            panic!("JSON body")
        };
        // Measured against the live API: all three are accepted on grok-4.6.
        assert_eq!(body["reasoning_effort"], "xhigh");
        assert_eq!(body["temperature"], 0.7);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn no_cache_key_means_no_header_rather_than_an_empty_one() {
        let provider = OpenAICompatProvider::new_xai("https://api.x.ai/v1", "key");
        let mut p = params();
        p.cache_key = None;
        let req = provider
            .build_request(&[ChatMessage::user("hello")], None, &p, true)
            .unwrap();
        assert!(req.headers.get("x-grok-conv-id").is_none());
    }

    /// It is a vendor header. An OpenAI-compatible relay that receives one it
    /// does not know may reject the request outright.
    #[test]
    fn a_generic_endpoint_is_not_sent_the_vendor_header() {
        let provider = OpenAICompatProvider::new("https://api.example.test/v1", "key");
        let req = provider
            .build_request(&[ChatMessage::user("hello")], None, &params(), true)
            .unwrap();
        assert!(req.headers.get("x-grok-conv-id").is_none());
    }

    fn usage(json: &str) -> ChunkUsage {
        serde_json::from_str(json).expect("a chat-completions usage body")
    }

    /// A real `grok-4.6` reply: 214 prompt, 1 content token, 59 reasoning
    /// tokens, 274 total. xAI's own `cost_in_usd_ticks` billed 60 output
    /// tokens, so reporting 1 understates that turn ~60x.
    #[test]
    fn grok_reasoning_tokens_are_counted_as_output() {
        let u = normalise_openai_usage(&usage(
            r#"{"prompt_tokens":214,"completion_tokens":1,"total_tokens":274,
                "prompt_tokens_details":{"cached_tokens":128},
                "completion_tokens_details":{"reasoning_tokens":59}}"#,
        ));
        assert_eq!(u.completion_tokens, Some(60));
        assert_eq!(u.prompt_tokens, Some(214), "the prompt is untouched");
        assert_eq!(u.cache_read_tokens, Some(128));
        assert_eq!(u.uncached_prompt_tokens(), 86);
    }

    /// OpenAI counts reasoning *inside* `completion_tokens`, and its own total
    /// says so. Adding them there would double-bill every o-series turn.
    #[test]
    fn an_inclusive_dialect_is_left_alone() {
        let u = normalise_openai_usage(&usage(
            r#"{"prompt_tokens":100,"completion_tokens":80,"total_tokens":180,
                "completion_tokens_details":{"reasoning_tokens":64}}"#,
        ));
        assert_eq!(u.completion_tokens, Some(80));
    }

    /// Without a total there is no evidence either way, and a guess that
    /// inflates the bill is worse than one that reports what was said.
    #[test]
    fn an_ambiguous_usage_block_is_reported_as_given() {
        let no_total = normalise_openai_usage(&usage(
            r#"{"prompt_tokens":100,"completion_tokens":10,
                "completion_tokens_details":{"reasoning_tokens":50}}"#,
        ));
        assert_eq!(no_total.completion_tokens, Some(10));

        // A total that matches neither reading: something else is in it.
        let odd = normalise_openai_usage(&usage(
            r#"{"prompt_tokens":100,"completion_tokens":10,"total_tokens":200,
                "completion_tokens_details":{"reasoning_tokens":50}}"#,
        ));
        assert_eq!(odd.completion_tokens, Some(10));
    }

    /// The usage-only chunk that closes an xAI stream carries `choices: []`.
    /// Required-but-empty is not the same as missing, and the accounting for
    /// the whole turn arrives in it.
    #[test]
    fn the_trailing_usage_chunk_parses_with_empty_choices() {
        let chunk: ChatChunk = serde_json::from_str(
            r#"{"choices":[],"usage":{"prompt_tokens":292,"completion_tokens":13,"total_tokens":366,
                "prompt_tokens_details":{"cached_tokens":256},
                "completion_tokens_details":{"reasoning_tokens":61}}}"#,
        )
        .expect("the closing chunk of a real xAI stream");
        let (events, finish, usage) = parse_openai_sse_events(&chunk);
        assert!(events.is_empty());
        assert_eq!(finish, None);
        assert_eq!(usage.expect("usage").completion_tokens, Some(74));
    }
}

#[cfg(test)]
mod google_tests {
    use super::*;
    use crate::client::RequestBody;
    use crate::provider::state::{
        GoogleSignatureLocation, GoogleThoughtSignature, ProviderState, ProviderStatePayload, ProviderStateProducer,
    };

    fn google_state(location: GoogleSignatureLocation) -> ProviderState {
        ProviderState {
            version: 1,
            producer: ProviderStateProducer {
                vendor: "google".into(),
                protocol: "openai_chat_completions".into(),
                model: "gemini-3.7-flash".into(),
            },
            payload: ProviderStatePayload::GoogleThoughtSignatures {
                signatures: vec![GoogleThoughtSignature {
                    location,
                    signature: "signed-state".into(),
                }],
            },
        }
    }

    #[test]
    fn google_request_uses_effort_and_thought_summaries_without_sampling() {
        let provider = OpenAICompatProvider::new_google("https://example.test", "key");
        let params = ChatParams {
            model: "gemini-3.7-flash".into(),
            thinking_enabled: true,
            thinking_effort: Some("high".into()),
            temperature: Some(0.8),
            top_p: Some(0.9),
            ..Default::default()
        };
        let req = provider
            .build_request(&[ChatMessage::user("hello")], None, &params, true)
            .unwrap();
        let Some(RequestBody::Json(body)) = req.body else {
            panic!("JSON body")
        };
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(
            body["extra_body"]["google"]["thinking_config"]["include_thoughts"],
            true
        );
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
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
    fn signed_tool_call_is_replayed_and_an_interrupted_result_is_closed() {
        let mut assistant = ChatMessage::assistant_with_tools(
            "",
            Some("summary".into()),
            vec![ToolCall {
                id: "call-1".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"a"}"#.into(),
            }],
        );
        assistant.provider_state = Some(google_state(GoogleSignatureLocation::ToolCall {
            index: 0,
            call_id: Some("call-1".into()),
        }));
        let wire = serialize_google_messages(&[assistant], "gemini-3.7-flash").unwrap();
        assert_eq!(
            wire[0]["tool_calls"][0]["extra_content"]["google"]["thought_signature"],
            "signed-state"
        );
        assert_eq!(wire[1]["role"], "tool");
        assert_eq!(wire[1]["tool_call_id"], "call-1");
        assert!(wire[1]["content"].as_str().unwrap().contains("not retried"));
    }

    #[test]
    fn unsigned_foreign_tool_history_is_flattened() {
        let assistant = ChatMessage::assistant_with_tools(
            "",
            None,
            vec![ToolCall {
                id: "foreign".into(),
                name: "search".into(),
                arguments: "{}".into(),
            }],
        );
        let wire = serialize_google_messages(
            &[assistant, ChatMessage::tool_result("foreign", "done")],
            "gemini-3.7-flash",
        )
        .unwrap();
        assert_eq!(wire[0]["role"], "assistant");
        assert!(wire[0].get("tool_calls").is_none());
        assert!(wire[0]["content"].as_str().unwrap().contains("Historical tool call"));
        assert_eq!(wire[1]["role"], "user");
    }

    #[test]
    fn google_compat_rejects_malformed_historical_tool_arguments() {
        let assistant = ChatMessage::assistant_with_tools(
            "",
            None,
            vec![ToolCall {
                id: "broken-call".into(),
                name: "search".into(),
                arguments: "{not-json".into(),
            }],
        );
        let error = serialize_google_messages(&[assistant], "gemini-3.7-flash").unwrap_err();
        assert!(matches!(error, ProviderError::Parse(ref message) if message.contains("broken-call")));
    }

    #[test]
    fn parallel_calls_keep_the_signature_on_the_first_call_only() {
        let mut assistant = ChatMessage::assistant_with_tools(
            "",
            None,
            vec![
                ToolCall {
                    id: "first".into(),
                    name: "one".into(),
                    arguments: "{}".into(),
                },
                ToolCall {
                    id: "second".into(),
                    name: "two".into(),
                    arguments: "{}".into(),
                },
            ],
        );
        assistant.provider_state = Some(google_state(GoogleSignatureLocation::ToolCall {
            index: 0,
            call_id: Some("first".into()),
        }));
        let messages = [
            assistant,
            ChatMessage::tool_result("first", "one-result"),
            ChatMessage::tool_result("second", "two-result"),
        ];
        let wire = serialize_google_messages(&messages, "gemini-3.7-flash").unwrap();
        assert_eq!(
            wire[0]["tool_calls"][0]["extra_content"]["google"]["thought_signature"],
            "signed-state"
        );
        assert!(wire[0]["tool_calls"][1].get("extra_content").is_none());
        assert_eq!(wire[1]["name"], "one");
        assert_eq!(wire[2]["name"], "two");
    }

    #[test]
    fn interrupted_parallel_results_follow_the_recorded_prefix() {
        let mut assistant = ChatMessage::assistant_with_tools(
            "",
            None,
            vec![
                ToolCall {
                    id: "first".into(),
                    name: "one".into(),
                    arguments: "{}".into(),
                },
                ToolCall {
                    id: "second".into(),
                    name: "two".into(),
                    arguments: "{}".into(),
                },
            ],
        );
        assistant.provider_state = Some(google_state(GoogleSignatureLocation::ToolCall {
            index: 0,
            call_id: Some("first".into()),
        }));
        let wire = serialize_google_messages(
            &[assistant, ChatMessage::tool_result("first", "one-result")],
            "gemini-3.7-flash",
        )
        .unwrap();
        assert_eq!(wire[1]["tool_call_id"], "first");
        assert_eq!(wire[2]["tool_call_id"], "second");
        assert!(wire[2]["content"].as_str().unwrap().contains("not retried"));
    }

    #[test]
    fn final_empty_chunk_still_emits_message_signature() {
        let chunk: ChatChunk = serde_json::from_value(serde_json::json!({
            "choices": [{
                "delta": {
                    "content": "",
                    "extra_content": {"google": {"thought_signature": "tail-signature"}}
                },
                "finish_reason": "stop"
            }]
        }))
        .unwrap();
        let (events, finish, _) = parse_openai_sse_events_for(&chunk, Some("gemini-3.7-flash"));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, StreamEvent::ProviderStateUpdate { .. }))
        );
        assert_eq!(finish.as_deref(), Some("stop"));
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

#[cfg(test)]
mod openai_tests {
    use super::*;
    use crate::client::RequestBody;

    /// A chunk shaped exactly as OpenAI documents `chat.completion.chunk`,
    /// with every envelope, choice, delta, tool-call and usage field present.
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

    /// Every documented field is named, so a standard reply feeds nothing to
    /// `warn_extra_fields` — while a field nobody documented still lands in
    /// `extra`, which is the only reason that warning is worth keeping.
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

    /// The two `*_tokens_details` objects carry more than the one field each
    /// that is priced on, and the rest used to warn on every OpenAI stream.
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

    /// A refused reply carries `refusal` *instead of* `content`. Without this
    /// the transcript showed a blank assistant turn that had said no.
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
        assert_eq!(
            reply_text(None, Some(String::new()), None),
            None,
            "an empty refusal is no reply"
        );
    }

    /// This app only ever sends `type: "function"` definitions, so a `custom`
    /// call is one it never asked for: dropped, never started.
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

    /// Chat-completions takes `verbosity` at the top level, where the
    /// Responses API nests it under `text`.
    #[test]
    fn verbosity_is_sent_at_the_top_level_when_set() {
        let provider = OpenAICompatProvider::new("https://api.openai.com/v1", "key");
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
}

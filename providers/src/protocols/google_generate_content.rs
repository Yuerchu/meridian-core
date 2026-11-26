use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::StreamExt;
use serde::Deserialize;
use serde::de::IgnoredAny;

use crate::dto::{ExtraIgnore, embedded_upstream_error, warn_extra_fields};
use crate::state::{
    GOOGLE_GENERATE_CONTENT_PROTOCOL, GoogleSignatureLocation, ProviderStateAccumulator, ProviderStateUpdate,
};
use crate::{
    AgentResponse, ChatMessage, ChatParams, ChatProvider, ChatStream, MessageContentPart, ProviderError,
    SenderRendering, ServerToolCall, ServerToolKind, StreamEvent, ThinkingStyle, TokenUsage, ToolCall, ToolDefinition,
};
use meridian_provider_sdk::client::{HttpTransport, Request, RequestBody, ReqwestTransport};

/// Preference key controlling whether `gemini_generate_content` still sends
/// `includeThoughts: true` when `thinking_enabled` is false (e.g. background
/// summary/title passes or thinking-off requests on relays that expect it).
pub const GOOGLE_INCLUDE_THOUGHTS_WHEN_OFF_PREF: &str = "google.include_thoughts_when_off";

pub struct GoogleGenerateContentProvider {
    api_root: String,
    api_key: String,
    include_thoughts_when_off: bool,
}

impl GoogleGenerateContentProvider {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        Self {
            api_root: google_api_root(base_url),
            api_key: api_key.to_string(),
            include_thoughts_when_off: false,
        }
    }

    pub fn with_include_thoughts_when_off(mut self, include_thoughts_when_off: bool) -> Self {
        self.include_thoughts_when_off = include_thoughts_when_off;
        self
    }

    fn build_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
        params: &ChatParams,
        stream: bool,
    ) -> Result<Request, ProviderError> {
        let (system, contents) = serialize_contents(messages, &params.model)?;
        let mut body = serde_json::json!({ "contents": contents });
        if !system.is_empty() {
            body["systemInstruction"] = serde_json::json!({ "parts": [{ "text": system }] });
        }

        let mut generation = serde_json::Map::new();
        crate::mixins::GoogleSamplingParamsMixin::from_chat_params(params).apply_camel_case(&mut generation);
        if params.thinking_style != ThinkingStyle::None {
            let mut thinking = serde_json::Map::new();
            let include_thoughts = params.thinking_enabled || self.include_thoughts_when_off;
            if params.thinking_style == ThinkingStyle::Budget {
                if params.thinking_effort.is_some() {
                    return Err(ProviderError::Parse(
                        "Gemini thinkingBudget does not support thinking effort".into(),
                    ));
                }
                // 摘要会再次清除 enabled；不能把无法关闭思考的 Pro 编成预算 0。
                let budget = if params.thinking_enabled {
                    params.thinking_budget
                } else {
                    params.supports_thinking_off.then_some(0)
                };
                if let Some(budget) = budget {
                    if budget < -1 {
                        return Err(ProviderError::Parse("Gemini thinkingBudget must be at least -1".into()));
                    }
                    if budget == 0 && !params.supports_thinking_off {
                        return Err(ProviderError::Parse(format!(
                            "thinking cannot be disabled for model '{}'",
                            params.model
                        )));
                    }
                    thinking.insert("thinkingBudget".into(), serde_json::json!(budget));
                }
                if include_thoughts {
                    thinking.insert("includeThoughts".into(), serde_json::Value::Bool(true));
                }
            } else {
                if include_thoughts {
                    thinking.insert("includeThoughts".into(), serde_json::Value::Bool(true));
                }
                if let Some(effort) = params.thinking_effort.as_deref() {
                    // 原生 Gemini REST 的档位枚举使用大写。
                    thinking.insert("thinkingLevel".into(), serde_json::json!(effort.to_ascii_uppercase()));
                }
            }
            if !thinking.is_empty() {
                generation.insert("thinkingConfig".into(), serde_json::Value::Object(thinking));
            }
        }
        if !generation.is_empty() {
            body["generationConfig"] = serde_json::Value::Object(generation);
        }

        let mut wire_tools = Vec::new();
        for kind in &params.server_tools {
            wire_tools.push(match kind {
                ServerToolKind::GoogleSearch => serde_json::json!({ "googleSearch": {} }),
                ServerToolKind::UrlContext => serde_json::json!({ "urlContext": {} }),
                unsupported => {
                    return Err(ProviderError::InvalidRequest(format!(
                        "server tool `{unsupported}` is not supported by Gemini GenerateContent"
                    )));
                }
            });
        }
        if let Some(tools) = tools.filter(|items| !items.is_empty()) {
            wire_tools.push(serde_json::json!({
                "functionDeclarations": tools.iter().map(|tool| serde_json::json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parametersJsonSchema": tool.parameters,
                })).collect::<Vec<_>>()
            }));
        }
        if !wire_tools.is_empty() {
            body["tools"] = serde_json::Value::Array(wire_tools);
        }

        let model = params.model.strip_prefix("models/").unwrap_or(&params.model);
        let method = if stream {
            format!("{}/v1beta/models/{model}:streamGenerateContent?alt=sse", self.api_root)
        } else {
            format!("{}/v1beta/models/{model}:generateContent", self.api_root)
        };
        let mut request = Request::new(http::Method::POST, method);
        request
            .headers
            .insert("x-goog-api-key", crate::auth_header_value(&self.api_key));
        request.body = Some(RequestBody::Json(body));
        Ok(request)
    }
}

/// Gemini 3+ models strictly validate `thoughtSignature` on historical tool
/// calls and reject unsigned calls with a 400. Gemini 2.5 and earlier accept
/// unsigned native function calls (and emit no signature at all when thinking
/// is disabled), so flattening them would break ordinary multi-turn tool use.
pub(crate) fn model_requires_tool_thought_signature(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    let id = lower.strip_prefix("models/").unwrap_or(&lower);
    !["gemini-2.5-", "gemini-2.0-", "gemini-1."]
        .iter()
        .any(|prefix| id.starts_with(prefix))
}

pub fn google_api_root(base_url: &str) -> String {
    let mut root = base_url.trim().trim_end_matches('/');
    for suffix in ["/v1beta/openai", "/v1/openai", "/v1beta", "/v1"] {
        if let Some(stripped) = root.strip_suffix(suffix) {
            root = stripped.trim_end_matches('/');
            break;
        }
    }
    root.to_string()
}

fn serialize_contents(
    messages: &[ChatMessage],
    model: &str,
) -> Result<(String, Vec<serde_json::Value>), ProviderError> {
    let system = messages
        .iter()
        .filter(|message| message.role == "system")
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut out = Vec::new();
    let mut flattened_calls = HashSet::<String>::new();
    let mut consumed_results = HashSet::<String>::new();

    for (message_index, message) in messages.iter().enumerate() {
        if message.role == "system" {
            continue;
        }
        if message.role == "tool" {
            let id = message.tool_call_id.as_deref().unwrap_or("");
            if consumed_results.contains(id) {
                continue;
            }
            let (text, media) = tool_result_wire(message)?;
            if flattened_calls.contains(id) {
                push_content(
                    &mut out,
                    "user",
                    vec![serde_json::json!({
                        "text": format!("[Historical tool result for {id}]\n{text}")
                    })],
                );
            } else {
                push_content(
                    &mut out,
                    "user",
                    vec![function_response_part(id, "unknown_tool", &text)],
                );
            }
            push_content(&mut out, "user", media);
            continue;
        }
        flattened_calls.clear();
        consumed_results.clear();

        if message.role == "assistant"
            && let Some(tool_calls) = message.tool_calls.as_ref()
        {
            let parsed_arguments = tool_calls
                .iter()
                .map(|call| crate::decode_tool_arguments(&call.arguments, &call.id).map_err(ProviderError::Parse))
                .collect::<Result<Vec<_>, _>>()?;
            let signatures = message
                .provider_state
                .as_ref()
                .and_then(|state| state.google_signatures_for(GOOGLE_GENERATE_CONTENT_PROTOCOL, model));
            let has_tool_signature = signatures.is_some_and(|items| {
                items
                    .iter()
                    .any(|item| matches!(item.location, GoogleSignatureLocation::ToolCall { .. }))
            });
            if model_requires_tool_thought_signature(model) && !has_tool_signature {
                let mut text = message.content.clone();
                for call in tool_calls {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&format!("[Historical tool call: {}({})]", call.name, call.arguments));
                    flattened_calls.insert(call.id.clone());
                }
                push_content(&mut out, "model", vec![serde_json::json!({ "text": text })]);
                continue;
            }

            let mut parts = assistant_text_parts(message, signatures);
            for (index, (call, args)) in tool_calls.iter().zip(parsed_arguments).enumerate() {
                let mut part = serde_json::json!({
                    "functionCall": { "name": call.name, "args": args }
                });
                if !is_synthetic_call_id(&call.id) {
                    part["functionCall"]["id"] = serde_json::json!(call.id);
                }
                if let Some(signature) = signatures.and_then(|items| {
                    items.iter().find(|item| match &item.location {
                        GoogleSignatureLocation::ToolCall { index: stored, call_id } => {
                            *stored == index || call_id.as_deref() == Some(call.id.as_str())
                        }
                        _ => false,
                    })
                }) {
                    part["thoughtSignature"] = serde_json::json!(signature.signature);
                }
                parts.push(part);
            }
            push_content(&mut out, "model", parts);

            let recorded_results = messages[message_index + 1..]
                .iter()
                .take_while(|next| next.role == "tool")
                .filter_map(|next| Some((next.tool_call_id.as_deref()?, next)))
                .collect::<HashMap<_, _>>();
            let mut result_parts = Vec::with_capacity(tool_calls.len());
            let mut media_parts = Vec::new();
            for call in tool_calls {
                let result = match recorded_results.get(call.id.as_str()) {
                    Some(message) => {
                        let (text, media) = tool_result_wire(message)?;
                        media_parts.extend(media);
                        text
                    }
                    None => "Tool execution was interrupted before a result was recorded. It was not retried.".into(),
                };
                result_parts.push(function_response_part(&call.id, &call.name, &result));
                consumed_results.insert(call.id.clone());
            }
            // Pictures a tool returned follow every response of the batch, in
            // the same user turn: `functionResponse.response` is a JSON object
            // and has no place for bytes on the models this adapter serves.
            result_parts.extend(media_parts);
            push_content(&mut out, "user", result_parts);
            continue;
        }

        if message.role == "assistant" {
            let signatures = message
                .provider_state
                .as_ref()
                .and_then(|state| state.google_signatures_for(GOOGLE_GENERATE_CONTENT_PROTOCOL, model));
            push_content(&mut out, "model", assistant_text_parts(message, signatures));
        } else {
            let rendered = crate::render_message(message, SenderRendering::Prefix).map_err(ProviderError::Parse)?;
            push_content(&mut out, "user", rendered_parts(&rendered.content)?);
        }
    }
    Ok((system, out))
}

fn assistant_text_parts(
    message: &ChatMessage,
    signatures: Option<&[crate::state::GoogleThoughtSignature]>,
) -> Vec<serde_json::Value> {
    let mut parts = Vec::new();
    if let Some(reasoning) = message.reasoning_content.as_deref().filter(|text| !text.is_empty()) {
        parts.push(serde_json::json!({ "text": reasoning, "thought": true }));
    }
    if !message.content.is_empty() {
        parts.push(serde_json::json!({ "text": message.content }));
    }
    let mut content_signatures = signatures
        .into_iter()
        .flatten()
        .filter_map(|item| match &item.location {
            GoogleSignatureLocation::ContentPart { index } => Some((*index, &item.signature)),
            _ => None,
        })
        .collect::<Vec<_>>();
    content_signatures.sort_by_key(|(index, _)| *index);
    for (_, signature) in content_signatures {
        // Streaming Gemini responses may place the signature in a final empty
        // text part. Preserve that part instead of dropping it with the empty
        // token; moving the signature onto another part changes its meaning.
        parts.push(serde_json::json!({ "text": "", "thoughtSignature": signature }));
    }
    if parts.is_empty() && message.tool_calls.is_none() {
        parts.push(serde_json::json!({ "text": "" }));
    }
    parts
}

fn rendered_parts(content: &str) -> Result<Vec<serde_json::Value>, ProviderError> {
    let Some(parts) = crate::decode_message_parts(content).map_err(ProviderError::Parse)? else {
        return Ok(vec![serde_json::json!({ "text": content })]);
    };
    parts
        .into_iter()
        .map(|part| match part {
            MessageContentPart::Text { text } => Ok(serde_json::json!({ "text": text })),
            MessageContentPart::ImageUrl { image_url } => media_part(Some(&image_url.url)),
            MessageContentPart::File { file } => media_part(Some(&file.url)),
            MessageContentPart::Sticker { .. } => Err(ProviderError::Parse(
                "unresolved sticker part reached the Google GenerateContent adapter".into(),
            )),
        })
        .collect()
}

fn media_part(url: Option<&str>) -> Result<serde_json::Value, ProviderError> {
    let Some(url) = url else {
        return Ok(serde_json::json!({ "text": "[Unavailable attachment]" }));
    };
    if let Some(data) = url.strip_prefix("data:")
        && let Some((mime_type, payload)) = data.split_once(";base64,")
    {
        return Ok(serde_json::json!({ "inlineData": { "mimeType": mime_type, "data": payload } }));
    }
    crate::require_wire_attachment_url(url, "gemini-generate-content")?;
    Ok(serde_json::json!({ "fileData": { "fileUri": url } }))
}

/// A tool result as the text of its `functionResponse`, and the parts that
/// carry its pictures: a caption naming the call, then each image inline.
fn tool_result_wire(message: &ChatMessage) -> Result<(String, Vec<serde_json::Value>), ProviderError> {
    let Some(parts) = crate::tool_result_parts(message).map_err(ProviderError::Parse)? else {
        return Ok((message.content.clone(), Vec::new()));
    };
    let mut media = Vec::new();
    for part in &parts {
        if let crate::ToolWirePart::Image(url) = part {
            if media.is_empty() {
                let id = message.tool_call_id.as_deref().unwrap_or("");
                media.push(serde_json::json!({ "text": crate::tool_media_caption(id) }));
            }
            media.push(media_part(Some(url))?);
        }
    }
    Ok((crate::tool_result_wire_text(&parts), media))
}

fn function_response_part(id: &str, name: &str, output: &str) -> serde_json::Value {
    let mut response = serde_json::json!({
        "functionResponse": { "name": name, "response": { "output": output } }
    });
    if !id.is_empty() && !is_synthetic_call_id(id) {
        response["functionResponse"]["id"] = serde_json::json!(id);
    }
    response
}

fn is_synthetic_call_id(id: &str) -> bool {
    id.starts_with("gemini-call-")
}

fn push_content(out: &mut Vec<serde_json::Value>, role: &str, parts: Vec<serde_json::Value>) {
    if parts.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut()
        && last.get("role").and_then(|value| value.as_str()) == Some(role)
        && let Some(existing) = last.get_mut("parts").and_then(|value| value.as_array_mut())
    {
        existing.extend(parts);
        return;
    }
    out.push(serde_json::json!({ "role": role, "parts": parts }));
}

fn default_empty_object() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateContentChunk {
    candidates: Option<Vec<GeminiCandidate>>,
    prompt_feedback: Option<GeminiPromptFeedback>,
    usage_metadata: Option<GeminiUsage>,
    response_id: Option<String>,
    model_version: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl GenerateContentChunk {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("gemini_generate_content", &self.extra);
        if let Some(candidates) = &self.candidates {
            for candidate in candidates {
                candidate.warn_ignored_fields();
            }
        }
        if let Some(feedback) = &self.prompt_feedback {
            feedback.warn_ignored_fields();
        }
        if let Some(usage) = &self.usage_metadata {
            usage.warn_ignored_fields();
        }
    }

    fn validate(&self, stream_started: bool) -> Result<(), ProviderError> {
        if self.candidates.is_some() || self.prompt_feedback.is_some() {
            return Ok(());
        }
        if stream_started && self.usage_metadata.is_some() {
            return Ok(());
        }
        Err(ProviderError::Parse("missing field `candidates`".into()))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiPromptFeedback {
    block_reason: Option<String>,
    #[serde(default, rename = "blockReasonMessage")]
    _block_reason_message: IgnoredAny,
    #[serde(default, rename = "safetyRatings")]
    _safety_ratings: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl GeminiPromptFeedback {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("gemini_prompt_feedback", &self.extra);
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiCandidate {
    content: Option<GeminiContent>,
    finish_reason: Option<String>,
    grounding_metadata: Option<GeminiGroundingMetadata>,
    url_context_metadata: Option<GeminiUrlContextMetadata>,
    #[serde(default, rename = "index")]
    _index: IgnoredAny,
    #[serde(default, rename = "safetyRatings")]
    _safety_ratings: IgnoredAny,
    #[serde(default, rename = "citationMetadata")]
    _citation_metadata: IgnoredAny,
    #[serde(default, rename = "avgLogprobs")]
    _avg_logprobs: IgnoredAny,
    #[serde(default, rename = "logprobsResult")]
    _logprobs_result: IgnoredAny,
    #[serde(default, rename = "finishMessage")]
    _finish_message: IgnoredAny,
    #[serde(default, rename = "tokenCount")]
    _token_count: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl GeminiCandidate {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("gemini_candidate", &self.extra);
        if let Some(content) = &self.content {
            content.warn_ignored_fields();
        }
        if let Some(grounding) = &self.grounding_metadata {
            grounding.warn_ignored_fields();
        }
        if let Some(url_context) = &self.url_context_metadata {
            url_context.warn_ignored_fields();
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGroundingMetadata {
    #[serde(default)]
    web_search_queries: Vec<String>,
    #[serde(default)]
    grounding_chunks: Vec<GeminiGroundingChunk>,
    #[serde(default, rename = "searchEntryPoint")]
    _search_entry_point: IgnoredAny,
    #[serde(default, rename = "groundingSupports")]
    _grounding_supports: IgnoredAny,
    #[serde(default, rename = "retrievalMetadata")]
    _retrieval_metadata: IgnoredAny,
    #[serde(default, rename = "retrievalQueries")]
    _retrieval_queries: IgnoredAny,
    #[serde(default, rename = "googleMapsWidgetContextToken")]
    _google_maps_widget_context_token: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl GeminiGroundingMetadata {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("gemini_grounding_metadata", &self.extra);
        for chunk in &self.grounding_chunks {
            chunk.warn_ignored_fields();
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGroundingChunk {
    web: Option<GeminiGroundingWeb>,
    #[serde(default, rename = "retrievedContext")]
    _retrieved_context: IgnoredAny,
    #[serde(default, rename = "maps")]
    _maps: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl GeminiGroundingChunk {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("gemini_grounding_chunk", &self.extra);
        if let Some(web) = &self.web {
            web.warn_ignored_fields();
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGroundingWeb {
    uri: Option<String>,
    #[serde(default, rename = "title")]
    _title: IgnoredAny,
    #[serde(default, rename = "domain")]
    _domain: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl GeminiGroundingWeb {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("gemini_grounding_web", &self.extra);
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiUrlContextMetadata {
    #[serde(default)]
    url_metadata: Vec<GeminiUrlMetadata>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl GeminiUrlContextMetadata {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("gemini_url_context_metadata", &self.extra);
        for item in &self.url_metadata {
            item.warn_ignored_fields();
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiUrlMetadata {
    retrieved_url: Option<String>,
    #[serde(default, rename = "urlRetrievalStatus")]
    _url_retrieval_status: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl GeminiUrlMetadata {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("gemini_url_metadata", &self.extra);
    }
}

#[derive(Deserialize)]
struct GeminiContent {
    #[serde(default)]
    parts: Vec<GeminiPart>,
    #[serde(default, rename = "role")]
    _role: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl GeminiContent {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("gemini_content", &self.extra);
        for part in &self.parts {
            part.warn_ignored_fields();
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiPart {
    text: Option<String>,
    thought: Option<bool>,
    thought_signature: Option<String>,
    function_call: Option<GeminiFunctionCall>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl GeminiPart {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("gemini_part", &self.extra);
        if let Some(call) = &self.function_call {
            call.warn_ignored_fields();
        }
    }
}

#[derive(Deserialize)]
struct GeminiFunctionCall {
    id: Option<String>,
    name: String,
    #[serde(default = "default_empty_object")]
    args: serde_json::Value,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl GeminiFunctionCall {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("gemini_function_call", &self.extra);
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiUsage {
    prompt_token_count: Option<i32>,
    candidates_token_count: Option<i32>,
    thoughts_token_count: Option<i32>,
    tool_use_prompt_token_count: Option<i32>,
    total_token_count: Option<i32>,
    cached_content_token_count: Option<i32>,
    #[serde(default, rename = "promptTokensDetails")]
    _prompt_tokens_details: IgnoredAny,
    #[serde(default, rename = "candidatesTokensDetails")]
    _candidates_tokens_details: IgnoredAny,
    #[serde(default, rename = "cacheTokensDetails")]
    _cache_tokens_details: IgnoredAny,
    #[serde(default, rename = "toolUsePromptTokensDetails")]
    _tool_use_prompt_tokens_details: IgnoredAny,
    #[serde(default, rename = "trafficType")]
    _traffic_type: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl GeminiUsage {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("gemini_usage", &self.extra);
    }

    fn normalise(&self, server_tool_calls: Option<i32>) -> TokenUsage {
        // Gemini's native GenerateContent API reports `thoughtsTokenCount`
        // beside `candidatesTokenCount` and bills thoughts at the output rate.
        // Fold them into `completion_tokens` unless `totalTokenCount` proves
        // the upstream already included thoughts inside `candidatesTokenCount`.
        let completion_tokens = match (self.candidates_token_count, self.thoughts_token_count) {
            (Some(candidates), Some(thoughts)) if thoughts > 0 => {
                let already_included = match (self.total_token_count, self.prompt_token_count) {
                    (Some(total), Some(prompt)) => {
                        let tool_use = self.tool_use_prompt_token_count.unwrap_or(0);
                        total.saturating_sub(prompt).saturating_sub(tool_use) == candidates
                    }
                    _ => false,
                };
                if already_included {
                    Some(candidates)
                } else {
                    Some(candidates.saturating_add(thoughts))
                }
            }
            (Some(candidates), _) => Some(candidates),
            (None, Some(thoughts)) => Some(thoughts),
            (None, None) => None,
        };
        TokenUsage {
            prompt_tokens: self.prompt_token_count,
            completion_tokens,
            total_tokens: self.total_token_count,
            cache_read_tokens: self.cached_content_token_count,
            cache_write_tokens: None,
            billable_tool_calls: server_tool_calls.filter(|calls| *calls > 0),
        }
    }
}

fn normalise_finish_reason(reason: &str) -> String {
    match reason {
        "STOP" => "stop".into(),
        "MAX_TOKENS" => "max_tokens".into(),
        "SAFETY"
        | "RECITATION"
        | "BLOCKLIST"
        | "PROHIBITED_CONTENT"
        | "SPII"
        | "IMAGE_SAFETY"
        | "IMAGE_PROHIBITED_CONTENT" => "content_filter".into(),
        other => other.to_ascii_lowercase(),
    }
}

fn push_unique(target: &mut Vec<String>, incoming: impl IntoIterator<Item = String>) -> bool {
    let mut changed = false;
    for item in incoming {
        if !item.is_empty() && !target.iter().any(|existing| existing == &item) {
            target.push(item);
            changed = true;
        }
    }
    changed
}

#[derive(Default)]
struct GeminiParseState {
    message_started: bool,
    response_model_emitted: bool,
    next_part_index: usize,
    next_tool_index: usize,
    google_search_announced: bool,
    google_search_queries: Vec<String>,
    google_search_sources: Vec<String>,
    url_context_announced: bool,
    url_context_urls: Vec<String>,
    server_tool_calls: i32,
}

#[cfg(test)]
fn parse_chunk(chunk: &GenerateContentChunk, model: &str) -> Vec<StreamEvent> {
    parse_chunk_with_state(chunk, model, &mut GeminiParseState::default())
}

fn parse_chunk_with_state(chunk: &GenerateContentChunk, model: &str, state: &mut GeminiParseState) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    if !state.message_started
        && let Some(id) = chunk.response_id.as_ref().filter(|id| !id.is_empty())
    {
        events.push(StreamEvent::MessageStart { message_id: id.clone() });
        state.message_started = true;
    }
    if !state.response_model_emitted
        && let Some(ref model) = chunk.model_version
    {
        events.push(StreamEvent::ResponseModel { model: model.clone() });
        state.response_model_emitted = true;
    }
    let candidates = chunk.candidates.as_deref().unwrap_or(&[]);
    for candidate in candidates {
        if let Some(content) = &candidate.content {
            for part in &content.parts {
                let part_index = state.next_part_index;
                state.next_part_index += 1;
                let location = if let Some(call) = &part.function_call {
                    let tool_index = state.next_tool_index;
                    state.next_tool_index += 1;
                    let id = call.id.clone().unwrap_or_else(|| format!("gemini-call-{tool_index}"));
                    let arguments = serde_json::to_string(&call.args).unwrap_or_else(|_| "{}".into());
                    events.push(StreamEvent::ToolCallStart {
                        index: tool_index,
                        id: id.clone(),
                        name: call.name.clone(),
                    });
                    events.push(StreamEvent::ToolCallDelta {
                        index: tool_index,
                        arguments,
                    });
                    GoogleSignatureLocation::ToolCall {
                        index: tool_index,
                        call_id: Some(id),
                    }
                } else {
                    if let Some(text) = part.text.as_ref().filter(|text| !text.is_empty()) {
                        if part.thought.unwrap_or(false) {
                            events.push(StreamEvent::Reasoning { content: text.clone() });
                        } else {
                            events.push(StreamEvent::Text { content: text.clone() });
                        }
                    }
                    GoogleSignatureLocation::ContentPart { index: part_index }
                };
                if let Some(signature) = part.thought_signature.as_ref().filter(|value| !value.is_empty()) {
                    events.push(StreamEvent::ProviderStateUpdate {
                        update: ProviderStateUpdate::GoogleThoughtSignatureDelta {
                            protocol: GOOGLE_GENERATE_CONTENT_PROTOCOL.into(),
                            model: model.to_string(),
                            location,
                            delta: signature.clone(),
                        },
                    });
                }
            }
        }
        if let Some(grounding) = &candidate.grounding_metadata {
            let queries_changed = push_unique(
                &mut state.google_search_queries,
                grounding.web_search_queries.iter().cloned(),
            );
            let sources_changed = push_unique(
                &mut state.google_search_sources,
                grounding
                    .grounding_chunks
                    .iter()
                    .filter_map(|chunk| chunk.web.as_ref()?.uri.clone()),
            );
            if !state.google_search_queries.is_empty() || !state.google_search_sources.is_empty() {
                let first = !state.google_search_announced;
                if first {
                    state.google_search_announced = true;
                    state.server_tool_calls = state.server_tool_calls.saturating_add(1);
                }
                if first || queries_changed || sources_changed {
                    let arguments = (!state.google_search_queries.is_empty())
                        .then(|| serde_json::json!({ "queries": state.google_search_queries }).to_string());
                    events.push(StreamEvent::ServerToolCall(ServerToolCall {
                        id: "gemini-google-search".into(),
                        name: "google_search".into(),
                        arguments,
                        sources: state.google_search_sources.clone(),
                        completed: true,
                    }));
                }
            }
        }
        if let Some(url_context) = &candidate.url_context_metadata {
            let urls_changed = push_unique(
                &mut state.url_context_urls,
                url_context
                    .url_metadata
                    .iter()
                    .filter_map(|item| item.retrieved_url.clone()),
            );
            if !state.url_context_urls.is_empty() {
                let first = !state.url_context_announced;
                if first {
                    state.url_context_announced = true;
                    state.server_tool_calls = state.server_tool_calls.saturating_add(1);
                }
                if first || urls_changed {
                    let arguments = Some(serde_json::json!({ "urls": state.url_context_urls }).to_string());
                    events.push(StreamEvent::ServerToolCall(ServerToolCall {
                        id: "gemini-url-context".into(),
                        name: "url_context".into(),
                        arguments,
                        sources: state.url_context_urls.clone(),
                        completed: true,
                    }));
                }
            }
        }
        if let Some(reason) = candidate.finish_reason.as_ref() {
            events.push(StreamEvent::Stop {
                reason: normalise_finish_reason(reason),
                usage: None,
            });
        }
    }
    if candidates.is_empty()
        && let Some(block_reason) = chunk
            .prompt_feedback
            .as_ref()
            .and_then(|feedback| feedback.block_reason.as_deref())
            .filter(|reason| !reason.is_empty())
    {
        tracing::info!(
            block_reason,
            "Gemini promptFeedback blocked the request; reporting refusal"
        );
        events.push(StreamEvent::Stop {
            reason: "content_filter".into(),
            usage: None,
        });
    }
    if let Some(usage) = &chunk.usage_metadata {
        events.push(StreamEvent::UsageUpdate {
            usage: usage.normalise(Some(state.server_tool_calls)),
        });
    }
    events
}

fn parse_agent_response(chunk: GenerateContentChunk, model: &str) -> Result<AgentResponse, ProviderError> {
    chunk.validate(false)?;
    chunk.warn_ignored_fields();
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    let mut state = ProviderStateAccumulator::default();
    let mut parse_state = GeminiParseState::default();
    for event in parse_chunk_with_state(&chunk, model, &mut parse_state) {
        match event {
            StreamEvent::Text { content } => text.push_str(&content),
            StreamEvent::Reasoning { content } => reasoning.push_str(&content),
            StreamEvent::ToolCallStart { index, id, name } => {
                if tool_calls.len() <= index {
                    tool_calls.resize_with(index + 1, || ToolCall {
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                    });
                }
                tool_calls[index].id = id;
                tool_calls[index].name = name;
            }
            StreamEvent::ToolCallDelta { index, arguments } => {
                if let Some(call) = tool_calls.get_mut(index) {
                    call.arguments.push_str(&arguments);
                }
            }
            StreamEvent::ProviderStateUpdate { update } => state.apply(update).map_err(ProviderError::Parse)?,
            _ => {}
        }
    }
    Ok(AgentResponse {
        text,
        reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
        tool_calls,
        usage: chunk
            .usage_metadata
            .as_ref()
            .map(|usage| usage.normalise(Some(parse_state.server_tool_calls))),
        provider_state: state.finish(),
    })
}

#[async_trait]
impl ChatProvider for GoogleGenerateContentProvider {
    fn adapter_name(&self) -> &'static str {
        "GoogleGenerateContentProvider"
    }

    async fn stream_chat_with_tools(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        params: ChatParams,
    ) -> Result<ChatStream, ProviderError> {
        let transport = ReqwestTransport::shared();
        let request = self.build_request(&messages, Some(&tools), &params, true)?;
        let response = transport.stream(request).await?;
        let model = params.model.clone();
        let mut parse_state = GeminiParseState::default();
        let stream = response
            .bytes
            .map(|result| result.map_err(ProviderError::Transport))
            .eventsource()
            .flat_map(move |event| {
                let events = match event {
                    Ok(event) => match serde_json::from_str::<GenerateContentChunk>(&event.data) {
                        Ok(chunk) => match chunk.validate(parse_state.message_started) {
                            Ok(()) => {
                                chunk.warn_ignored_fields();
                                parse_chunk_with_state(&chunk, &model, &mut parse_state)
                                    .into_iter()
                                    .map(Ok)
                                    .collect()
                            }
                            Err(error) => vec![Err(error)],
                        },
                        Err(error) => vec![Err(embedded_upstream_error(event.data.as_bytes())
                            .map(ProviderError::Upstream)
                            .unwrap_or_else(|| ProviderError::Parse(error.to_string())))],
                    },
                    Err(error) => vec![Err(ProviderError::Parse(error.to_string()))],
                };
                futures::stream::iter(events)
            });
        Ok(Box::pin(stream))
    }

    async fn chat(&self, messages: Vec<ChatMessage>, params: ChatParams) -> Result<String, ProviderError> {
        Ok(self.chat_with_tools(messages, Vec::new(), params).await?.text)
    }

    async fn chat_with_tools(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        params: ChatParams,
    ) -> Result<AgentResponse, ProviderError> {
        let transport = ReqwestTransport::shared();
        let request = self.build_request(&messages, Some(&tools), &params, false)?;
        let response = transport.execute(request).await?;
        let parsed: GenerateContentChunk = serde_json::from_slice(&response.body).map_err(|error| {
            embedded_upstream_error(&response.body)
                .map(ProviderError::Upstream)
                .unwrap_or_else(|| ProviderError::Parse(error.to_string()))
        })?;
        parse_agent_response(parsed, &params.model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities;
    use crate::state::{GoogleThoughtSignature, ProviderState, ProviderStatePayload, ProviderStateProducer};

    fn resolved_params(model: &str, budget: Option<i32>, level: Option<&str>) -> ChatParams {
        let caps = capabilities::resolve("google", Some("gemini_generate_content"), model);
        let (thinking_enabled, thinking_budget, thinking_effort) =
            capabilities::resolve_thinking(true, budget, level).unwrap();
        let mut params = ChatParams {
            model: model.into(),
            thinking_enabled,
            thinking_budget,
            thinking_effort,
            ..Default::default()
        };
        capabilities::filter_params(&mut params, &caps).unwrap();
        params
    }

    fn thinking_config(params: &ChatParams, stream: bool) -> Result<serde_json::Value, ProviderError> {
        thinking_config_with(params, stream, false)
    }

    fn thinking_config_with(
        params: &ChatParams,
        stream: bool,
        include_thoughts_when_off: bool,
    ) -> Result<serde_json::Value, ProviderError> {
        let provider = GoogleGenerateContentProvider::new("https://relay.example", "secret")
            .with_include_thoughts_when_off(include_thoughts_when_off);
        let request = provider.build_request(&[ChatMessage::user("hi")], None, params, stream)?;
        let Some(RequestBody::Json(body)) = request.body else {
            panic!("expected JSON request body");
        };
        Ok(body["generationConfig"]["thinkingConfig"].clone())
    }

    #[test]
    fn a_tool_picture_follows_the_function_responses_in_the_same_turn() {
        let mut assistant = ChatMessage::assistant_with_tools(
            "",
            None,
            vec![
                ToolCall {
                    id: "call-a".into(),
                    name: "shot".into(),
                    arguments: "{}".into(),
                },
                ToolCall {
                    id: "call-b".into(),
                    name: "other".into(),
                    arguments: "{}".into(),
                },
            ],
        );
        assistant.provider_state = Some(ProviderState {
            version: 1,
            producer: ProviderStateProducer {
                vendor: "google".into(),
                protocol: GOOGLE_GENERATE_CONTENT_PROTOCOL.into(),
                model: "gemini-3.7-flash".into(),
            },
            payload: ProviderStatePayload::GoogleThoughtSignatures {
                signatures: vec![GoogleThoughtSignature {
                    location: GoogleSignatureLocation::ToolCall {
                        index: 0,
                        call_id: Some("call-a".into()),
                    },
                    signature: "signed-call".into(),
                }],
            },
        });
        let (_, contents) = serialize_contents(
            &[
                assistant,
                ChatMessage::tool_result_parts("call-a", r#"[{"type":"text","text":"shot"},{"type":"image_url","image_url":{"url":"data:image/png;base64,QUJD"}}]"#),
                ChatMessage::tool_result("call-b", "plain"),
            ],
            "gemini-3.7-flash",
        )
        .unwrap();
        let parts = contents[1]["parts"].as_array().unwrap();
        assert_eq!(
            parts[0]["functionResponse"]["response"]["output"],
            "shot\n[1 image(s) follow the tool results]"
        );
        assert_eq!(parts[1]["functionResponse"]["response"]["output"], "plain");
        assert_eq!(parts[2]["text"], "[Images returned by tool call call-a]");
        assert_eq!(
            parts[3]["inlineData"],
            serde_json::json!({"mimeType": "image/png", "data": "QUJD"})
        );
    }

    #[test]
    fn root_accepts_official_openai_compat_urls() {
        assert_eq!(
            google_api_root("https://generativelanguage.googleapis.com/v1beta/openai/"),
            "https://generativelanguage.googleapis.com"
        );
        assert_eq!(google_api_root("https://relay.example"), "https://relay.example");
        assert_eq!(google_api_root("https://relay.example/v1"), "https://relay.example");
    }

    #[test]
    fn request_uses_native_thinking_level_and_tool_schema() {
        let provider = GoogleGenerateContentProvider::new("https://relay.example", "secret");
        let mut params = resolved_params("gemini-3.7-flash", Some(4097), Some("high"));
        assert_eq!(params.thinking_budget, None);
        params.thinking_budget = Some(4097);
        let tools = vec![ToolDefinition {
            name: "lookup".into(),
            description: "Look something up".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }),
        }];
        for stream in [false, true] {
            let request = provider
                .build_request(&[ChatMessage::user("hi")], Some(&tools), &params, stream)
                .unwrap();
            let method = if stream {
                "streamGenerateContent?alt=sse"
            } else {
                "generateContent"
            };
            assert_eq!(
                request.url,
                format!("https://relay.example/v1beta/models/gemini-3.7-flash:{method}")
            );
            let Some(RequestBody::Json(body)) = request.body else {
                panic!("expected JSON request body");
            };
            assert_eq!(
                body["generationConfig"]["thinkingConfig"],
                serde_json::json!({ "includeThoughts": true, "thinkingLevel": "HIGH" })
            );
            assert_eq!(
                body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["required"][0],
                "query"
            );
        }
    }

    #[test]
    fn gemini_25_requests_preserve_numeric_budgets() {
        for model in ["gemini-2.5-flash", "gemini-2.5-flash-lite", "gemini-2.5-pro"] {
            for budget in [Some(4097), None, Some(-1)] {
                let params = resolved_params(model, budget, None);
                let mut expected = serde_json::json!({ "includeThoughts": true });
                if let Some(budget) = budget {
                    expected["thinkingBudget"] = serde_json::json!(budget);
                }
                for stream in [false, true] {
                    assert_eq!(
                        thinking_config(&params, stream).unwrap(),
                        expected,
                        "{model}: budget={budget:?}, stream={stream}"
                    );
                }
            }
        }
    }

    #[test]
    fn gemini_25_flash_off_sends_zero() {
        for model in ["gemini-2.5-flash", "gemini-2.5-flash-lite"] {
            let off = resolved_params(model, Some(4097), Some("off"));
            let zero = resolved_params(model, Some(0), None);
            let summary = without_thinking(resolved_params(model, Some(4097), None));
            assert!(!off.thinking_enabled);
            assert!(zero.thinking_enabled);
            assert!(!summary.thinking_enabled);
            for params in [&off, &summary] {
                assert!(params.supports_thinking_off);
                for stream in [false, true] {
                    assert_eq!(
                        thinking_config(params, stream).unwrap(),
                        serde_json::json!({ "thinkingBudget": 0 }),
                        "{model}: stream={stream}"
                    );
                    assert_eq!(
                        thinking_config_with(params, stream, true).unwrap(),
                        serde_json::json!({ "includeThoughts": true, "thinkingBudget": 0 }),
                        "{model}: include_thoughts_when_off stream={stream}"
                    );
                }
            }
            for stream in [false, true] {
                assert_eq!(
                    thinking_config(&zero, stream).unwrap(),
                    serde_json::json!({ "includeThoughts": true, "thinkingBudget": 0 }),
                    "{model}: explicit zero stream={stream}"
                );
            }
        }
    }

    fn without_thinking(params: ChatParams) -> ChatParams {
        ChatParams {
            thinking_enabled: false,
            thinking_budget: None,
            thinking_effort: None,
            server_tools: Vec::new(),
            cache_key: None,
            ..params
        }
    }

    #[test]
    fn gemini_25_pro_off_and_summary_leave_budget_unspecified() {
        let off = resolved_params("gemini-2.5-pro", Some(4097), Some("off"));
        let summary = without_thinking(resolved_params("gemini-2.5-pro", Some(4097), None));
        assert!(off.thinking_enabled);
        assert!(!summary.thinking_enabled);
        assert!(!off.supports_thinking_off);
        assert!(!summary.supports_thinking_off);
        for stream in [false, true] {
            assert_eq!(
                thinking_config(&off, stream).unwrap(),
                serde_json::json!({ "includeThoughts": true })
            );
            assert_eq!(thinking_config(&summary, stream).unwrap(), serde_json::Value::Null);
            assert_eq!(
                thinking_config_with(&summary, stream, true).unwrap(),
                serde_json::json!({ "includeThoughts": true })
            );
        }
    }

    #[test]
    fn gemini_25_rejects_invalid_budget_and_effort() {
        for (model, budget, expected) in [
            ("gemini-2.5-flash", -2, "must be at least -1"),
            ("gemini-2.5-pro", 0, "cannot be disabled"),
        ] {
            let mut params = resolved_params(model, None, None);
            params.thinking_budget = Some(budget);
            for stream in [false, true] {
                let error = thinking_config(&params, stream).unwrap_err();
                assert!(matches!(error, ProviderError::Parse(message) if message.contains(expected)));
            }
        }
        for enabled in [false, true] {
            let mut params = resolved_params("gemini-2.5-flash", Some(4097), None);
            params.thinking_enabled = enabled;
            params.thinking_effort = Some("high".into());
            for stream in [false, true] {
                let error = thinking_config(&params, stream).unwrap_err();
                assert!(matches!(
                    error,
                    ProviderError::Parse(message) if message.contains("does not support thinking effort")
                ));
            }
        }
    }

    #[test]
    fn final_empty_signature_part_is_not_discarded() {
        let chunk: GenerateContentChunk = serde_json::from_value(serde_json::json!({
            "candidates": [{
                "content": { "role": "model", "parts": [
                    { "text": "OK" },
                    { "text": "", "thoughtSignature": "signed-tail" }
                ]},
                "finishReason": "STOP"
            }],
            "usageMetadata": { "promptTokenCount": 2, "candidatesTokenCount": 1, "totalTokenCount": 3 }
        }))
        .unwrap();
        let events = parse_chunk(&chunk, "gemini-3.7-flash");
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::ProviderStateUpdate {
                update: ProviderStateUpdate::GoogleThoughtSignatureDelta { protocol, delta, .. }
            } if protocol == GOOGLE_GENERATE_CONTENT_PROTOCOL && delta == "signed-tail"
        )));
    }

    #[test]
    fn native_state_is_replayed_as_a_signature_only_part() {
        let message = ChatMessage {
            provider_state: Some(ProviderState {
                version: 1,
                producer: ProviderStateProducer {
                    vendor: "google".into(),
                    protocol: GOOGLE_GENERATE_CONTENT_PROTOCOL.into(),
                    model: "gemini-3.7-flash".into(),
                },
                payload: ProviderStatePayload::GoogleThoughtSignatures {
                    signatures: vec![GoogleThoughtSignature {
                        location: GoogleSignatureLocation::ContentPart { index: 1 },
                        signature: "signed-tail".into(),
                    }],
                },
            }),
            ..ChatMessage::assistant("OK")
        };
        let (_, contents) = serialize_contents(&[message], "gemini-3.7-flash").unwrap();
        assert_eq!(contents[0]["parts"][1]["text"], "");
        assert_eq!(contents[0]["parts"][1]["thoughtSignature"], "signed-tail");
    }

    #[test]
    fn malformed_historical_tool_arguments_abort_serialization() {
        let assistant = ChatMessage::assistant_with_tools(
            "",
            None,
            vec![ToolCall {
                id: "broken-call".into(),
                name: "lookup".into(),
                arguments: "{not-json".into(),
            }],
        );
        let error = serialize_contents(&[assistant], "gemini-3.7-flash").unwrap_err();
        assert!(matches!(error, ProviderError::Parse(ref message) if message.contains("broken-call")));
    }

    #[test]
    fn signed_parallel_tool_results_keep_call_order_and_fill_interrupted_calls() {
        let calls = vec![
            ToolCall {
                id: "call-a".into(),
                name: "first".into(),
                arguments: r#"{"value":1}"#.into(),
            },
            ToolCall {
                id: "call-b".into(),
                name: "second".into(),
                arguments: r#"{"value":2}"#.into(),
            },
        ];
        let mut assistant = ChatMessage::assistant_with_tools("", None, calls);
        assistant.provider_state = Some(ProviderState {
            version: 1,
            producer: ProviderStateProducer {
                vendor: "google".into(),
                protocol: GOOGLE_GENERATE_CONTENT_PROTOCOL.into(),
                model: "gemini-3.7-flash".into(),
            },
            payload: ProviderStatePayload::GoogleThoughtSignatures {
                signatures: vec![GoogleThoughtSignature {
                    location: GoogleSignatureLocation::ToolCall {
                        index: 0,
                        call_id: Some("call-a".into()),
                    },
                    signature: "signed-call".into(),
                }],
            },
        });

        let (_, contents) = serialize_contents(
            &[
                assistant,
                ChatMessage::tool_result("call-b", "second-result"),
                ChatMessage::user("continue"),
            ],
            "gemini-3.7-flash",
        )
        .unwrap();
        let result_parts = contents[1]["parts"].as_array().unwrap();
        assert_eq!(result_parts[0]["functionResponse"]["name"], "first");
        assert_eq!(
            result_parts[0]["functionResponse"]["response"]["output"],
            "Tool execution was interrupted before a result was recorded. It was not retried."
        );
        assert_eq!(result_parts[1]["functionResponse"]["name"], "second");
        assert_eq!(
            result_parts[1]["functionResponse"]["response"]["output"],
            "second-result"
        );
    }

    #[test]
    fn missing_candidates_is_rejected() {
        let chunk: GenerateContentChunk = serde_json::from_value(serde_json::json!({
            "usageMetadata": { "totalTokenCount": 3 }
        }))
        .unwrap();
        let error = chunk.validate(false).expect_err("candidates is required");
        assert!(error.to_string().contains("missing field `candidates`"));
        assert!(
            chunk.validate(true).is_ok(),
            "tail usage-only chunk after stream start is valid"
        );
    }

    #[test]
    fn leftover_file_uri_does_not_become_empty_mimetype_filedata() {
        let error = media_part(Some("file:///C:/data/files/a.png")).unwrap_err();
        assert!(error.to_string().contains("unresolved attachment"), "{error}");
    }

    #[test]
    fn non_thinking_model_omits_thinking_config() {
        let params = ChatParams {
            model: "gemini-2.0-flash".into(),
            thinking_enabled: false,
            thinking_style: ThinkingStyle::None,
            ..Default::default()
        };
        assert_eq!(thinking_config(&params, true).unwrap(), serde_json::Value::Null);
        assert_eq!(
            thinking_config_with(&params, true, true).unwrap(),
            serde_json::Value::Null
        );
    }

    #[test]
    fn thoughts_token_count_is_folded_into_completion_tokens_without_double_counting() {
        let separate: GeminiUsage = serde_json::from_value(serde_json::json!({
            "promptTokenCount": 25,
            "candidatesTokenCount": 85,
            "thoughtsTokenCount": 140,
            "totalTokenCount": 250
        }))
        .unwrap();
        let norm = separate.normalise(None);
        assert_eq!(norm.completion_tokens, Some(225));
        assert_eq!(norm.total_tokens, Some(250));

        let already_included: GeminiUsage = serde_json::from_value(serde_json::json!({
            "promptTokenCount": 25,
            "candidatesTokenCount": 225,
            "thoughtsTokenCount": 140,
            "totalTokenCount": 250
        }))
        .unwrap();
        assert_eq!(already_included.normalise(None).completion_tokens, Some(225));

        let thoughts_only: GeminiUsage = serde_json::from_value(serde_json::json!({
            "promptTokenCount": 10,
            "thoughtsTokenCount": 30,
            "totalTokenCount": 40
        }))
        .unwrap();
        assert_eq!(thoughts_only.normalise(None).completion_tokens, Some(30));
    }

    #[test]
    fn finish_reason_is_normalised_for_truncation_and_refusal() {
        assert_eq!(normalise_finish_reason("STOP"), "stop");
        assert_eq!(normalise_finish_reason("MAX_TOKENS"), "max_tokens");
        for blocked in ["SAFETY", "RECITATION", "BLOCKLIST", "PROHIBITED_CONTENT", "SPII"] {
            assert_eq!(normalise_finish_reason(blocked), "content_filter", "{blocked}");
        }
    }

    #[test]
    fn prompt_feedback_block_empty_parts_and_omitted_args_parse_cleanly() {
        let blocked: GenerateContentChunk = serde_json::from_value(serde_json::json!({
            "promptFeedback": { "blockReason": "SAFETY", "safetyRatings": [] }
        }))
        .unwrap();
        blocked.validate(false).unwrap();
        let events = parse_chunk(&blocked, "gemini-2.5-flash");
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::Stop { reason, .. } if reason == "content_filter"
        )));

        let empty_parts: GenerateContentChunk = serde_json::from_value(serde_json::json!({
            "candidates": [{
                "index": 0,
                "content": { "role": "model" },
                "finishReason": "SAFETY",
                "safetyRatings": []
            }]
        }))
        .unwrap();
        empty_parts.validate(false).unwrap();
        let events = parse_chunk(&empty_parts, "gemini-2.5-flash");
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::Stop { reason, .. } if reason == "content_filter"
        )));

        let no_args_call: GenerateContentChunk = serde_json::from_value(serde_json::json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{ "functionCall": { "name": "get_time" } }]
                }
            }]
        }))
        .unwrap();
        let events = parse_chunk(&no_args_call, "gemini-2.5-flash");
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::ToolCallDelta { arguments, .. } if arguments == "{}"
        )));
    }

    #[test]
    fn standard_response_fields_report_no_ignored_fields() {
        let chunk: GenerateContentChunk = serde_json::from_value(serde_json::json!({
            "responseId": "resp-1",
            "modelVersion": "gemini-2.5-flash",
            "candidates": [{
                "index": 0,
                "content": { "role": "model", "parts": [{ "text": "ok" }] },
                "finishReason": "STOP",
                "safetyRatings": [],
                "avgLogprobs": -0.1
            }],
            "usageMetadata": {
                "promptTokenCount": 5,
                "candidatesTokenCount": 2,
                "thoughtsTokenCount": 3,
                "totalTokenCount": 10,
                "promptTokensDetails": [],
                "candidatesTokensDetails": [],
                "trafficType": "ON_DEMAND"
            }
        }))
        .unwrap();
        assert!(chunk.extra.is_empty());
        let candidate = &chunk.candidates.as_ref().unwrap()[0];
        assert!(candidate.extra.is_empty());
        assert!(candidate.content.as_ref().unwrap().extra.is_empty());
        assert!(chunk.usage_metadata.as_ref().unwrap().extra.is_empty());
    }

    #[test]
    fn gemini_25_unsigned_tool_calls_stay_native() {
        let assistant = ChatMessage::assistant_with_tools(
            "",
            None,
            vec![ToolCall {
                id: "call-1".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"README.md"}"#.into(),
            }],
        );
        let (_, contents) = serialize_contents(
            &[assistant, ChatMessage::tool_result("call-1", "hello")],
            "gemini-2.5-flash",
        )
        .unwrap();
        assert_eq!(contents[0]["role"], "model");
        assert_eq!(contents[0]["parts"][0]["functionCall"]["name"], "read_file");
        assert_eq!(contents[1]["role"], "user");
        assert_eq!(contents[1]["parts"][0]["functionResponse"]["name"], "read_file");
        assert_eq!(
            contents[1]["parts"][0]["functionResponse"]["response"]["output"],
            "hello"
        );
    }

    #[test]
    fn google_search_and_url_context_request_response_and_billing() {
        let provider = GoogleGenerateContentProvider::new("https://generativelanguage.googleapis.com", "secret");
        let params = ChatParams {
            model: "gemini-2.5-flash".into(),
            server_tools: vec![ServerToolKind::GoogleSearch, ServerToolKind::UrlContext],
            ..Default::default()
        };
        let req = provider
            .build_request(
                &[ChatMessage::user("check news")],
                Some(&[ToolDefinition {
                    name: "local_fn".into(),
                    description: "desc".into(),
                    parameters: serde_json::json!({"type": "object"}),
                }]),
                &params,
                true,
            )
            .unwrap();
        let Some(RequestBody::Json(body)) = req.body else {
            panic!("expected JSON body");
        };
        assert_eq!(body["tools"][0], serde_json::json!({ "googleSearch": {} }));
        assert_eq!(body["tools"][1], serde_json::json!({ "urlContext": {} }));
        assert_eq!(body["tools"][2]["functionDeclarations"][0]["name"], "local_fn");

        let chunk: GenerateContentChunk = serde_json::from_value(serde_json::json!({
            "candidates": [{
                "content": { "role": "model", "parts": [{ "text": "Found it." }] },
                "finishReason": "STOP",
                "groundingMetadata": {
                    "webSearchQueries": ["meridian news"],
                    "groundingChunks": [{ "web": { "uri": "https://example.com/a", "title": "A" } }]
                },
                "urlContextMetadata": {
                    "urlMetadata": [{
                        "retrievedUrl": "https://example.com/b",
                        "urlRetrievalStatus": "URL_RETRIEVAL_STATUS_SUCCESS"
                    }]
                }
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "totalTokenCount": 15
            }
        }))
        .unwrap();
        let mut state = GeminiParseState::default();
        let events = parse_chunk_with_state(&chunk, "gemini-2.5-flash", &mut state);
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::ServerToolCall(call)
                if call.name == "google_search"
                    && call.completed
                    && call.sources == ["https://example.com/a"]
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::ServerToolCall(call)
                if call.name == "url_context"
                    && call.completed
                    && call.sources == ["https://example.com/b"]
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::UsageUpdate { usage } if usage.billable_tool_calls == Some(2)
        )));

        let non_stream = parse_agent_response(chunk, "gemini-2.5-flash").unwrap();
        assert_eq!(non_stream.usage.unwrap().billable_tool_calls, Some(2));
    }
}

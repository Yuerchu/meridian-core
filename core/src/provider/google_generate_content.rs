use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::StreamExt;
use serde::Deserialize;

use super::dto::{ExtraIgnore, embedded_upstream_error, warn_extra_fields};
use super::state::{
    GOOGLE_GENERATE_CONTENT_PROTOCOL, GoogleSignatureLocation, ProviderStateAccumulator, ProviderStateUpdate,
};
use super::{
    AgentResponse, ChatMessage, ChatParams, ChatProvider, ChatStream, MessageContentPart, ProviderError,
    SenderRendering, StreamEvent, TokenUsage, ToolCall, ToolDefinition,
};
use crate::client::{HttpTransport, Request, RequestBody, ReqwestTransport};

pub struct GoogleGenerateContentProvider {
    api_root: String,
    api_key: String,
}

impl GoogleGenerateContentProvider {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        Self {
            api_root: google_api_root(base_url),
            api_key: api_key.to_string(),
        }
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
        if let Some(max) = params.max_tokens {
            generation.insert("maxOutputTokens".into(), serde_json::json!(max));
        }
        if let Some(temperature) = params.temperature {
            generation.insert("temperature".into(), serde_json::json!(temperature));
        }
        if let Some(top_p) = params.top_p {
            generation.insert("topP".into(), serde_json::json!(top_p));
        }
        let mut thinking = serde_json::Map::new();
        thinking.insert("includeThoughts".into(), serde_json::Value::Bool(true));
        if let Some(effort) = params.thinking_effort.as_deref() {
            // Meridian keeps effort tiers in a vendor-neutral lowercase form;
            // the native Gemini REST enum is uppercase.
            thinking.insert("thinkingLevel".into(), serde_json::json!(effort.to_ascii_uppercase()));
        }
        generation.insert("thinkingConfig".into(), serde_json::Value::Object(thinking));
        body["generationConfig"] = serde_json::Value::Object(generation);

        if let Some(tools) = tools.filter(|items| !items.is_empty()) {
            body["tools"] = serde_json::json!([{
                "functionDeclarations": tools.iter().map(|tool| serde_json::json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parametersJsonSchema": tool.parameters,
                })).collect::<Vec<_>>()
            }]);
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
            .insert("x-goog-api-key", super::auth_header_value(&self.api_key));
        request.body = Some(RequestBody::Json(body));
        Ok(request)
    }
}

pub(crate) fn google_api_root(base_url: &str) -> String {
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
            } else if flattened_calls.contains(id) {
                push_content(
                    &mut out,
                    "user",
                    vec![serde_json::json!({
                        "text": format!("[Historical tool result for {id}]\n{}", message.content)
                    })],
                );
            } else {
                push_content(
                    &mut out,
                    "user",
                    vec![function_response_part(id, "unknown_tool", &message.content)],
                );
            }
            continue;
        }
        flattened_calls.clear();
        consumed_results.clear();

        if message.role == "assistant"
            && let Some(tool_calls) = message.tool_calls.as_ref()
        {
            let parsed_arguments = tool_calls
                .iter()
                .map(|call| super::decode_tool_arguments(&call.arguments, &call.id).map_err(ProviderError::Parse))
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
            if !has_tool_signature {
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
                .filter_map(|next| Some((next.tool_call_id.as_deref()?, next.content.as_str())))
                .collect::<HashMap<_, _>>();
            let mut result_parts = Vec::with_capacity(tool_calls.len());
            for call in tool_calls {
                let result = recorded_results
                    .get(call.id.as_str())
                    .copied()
                    .unwrap_or("Tool execution was interrupted before a result was recorded. It was not retried.");
                result_parts.push(function_response_part(&call.id, &call.name, result));
                consumed_results.insert(call.id.clone());
            }
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
            let rendered = super::render_message(message, SenderRendering::Prefix).map_err(ProviderError::Parse)?;
            push_content(&mut out, "user", rendered_parts(&rendered.content)?);
        }
    }
    Ok((system, out))
}

fn assistant_text_parts(
    message: &ChatMessage,
    signatures: Option<&[super::state::GoogleThoughtSignature]>,
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
    if parts.is_empty() {
        parts.push(serde_json::json!({ "text": "" }));
    }
    parts
}

fn rendered_parts(content: &str) -> Result<Vec<serde_json::Value>, ProviderError> {
    let Some(parts) = super::decode_message_parts(content).map_err(ProviderError::Parse)? else {
        return Ok(vec![serde_json::json!({ "text": content })]);
    };
    parts
        .into_iter()
        .map(|part| match part {
            MessageContentPart::Text { text } => Ok(serde_json::json!({ "text": text })),
            MessageContentPart::ImageUrl { image_url } => Ok(media_part(Some(&image_url.url))),
            MessageContentPart::File { file } => Ok(media_part(Some(&file.url))),
            MessageContentPart::Sticker { .. } => Err(ProviderError::Parse(
                "unresolved sticker part reached the Google GenerateContent adapter".into(),
            )),
        })
        .collect()
}

fn media_part(url: Option<&str>) -> serde_json::Value {
    let Some(url) = url else {
        return serde_json::json!({ "text": "[Unavailable attachment]" });
    };
    if let Some(data) = url.strip_prefix("data:")
        && let Some((mime_type, payload)) = data.split_once(";base64,")
    {
        return serde_json::json!({ "inlineData": { "mimeType": mime_type, "data": payload } });
    }
    serde_json::json!({ "fileData": { "fileUri": url } })
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateContentChunk {
    candidates: Vec<GeminiCandidate>,
    usage_metadata: Option<GeminiUsage>,
    response_id: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl GenerateContentChunk {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("gemini_generate_content", &self.extra);
        for candidate in &self.candidates {
            candidate.warn_ignored_fields();
        }
        if let Some(usage) = &self.usage_metadata {
            usage.warn_ignored_fields();
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiCandidate {
    content: Option<GeminiContent>,
    finish_reason: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl GeminiCandidate {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("gemini_candidate", &self.extra);
        if let Some(content) = &self.content {
            content.warn_ignored_fields();
        }
    }
}

#[derive(Deserialize)]
struct GeminiContent {
    parts: Vec<GeminiPart>,
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
    total_token_count: Option<i32>,
    cached_content_token_count: Option<i32>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl GeminiUsage {
    fn warn_ignored_fields(&self) {
        warn_extra_fields("gemini_usage", &self.extra);
    }

    fn normalise(&self) -> TokenUsage {
        TokenUsage {
            prompt_tokens: self.prompt_token_count,
            completion_tokens: self.candidates_token_count,
            total_tokens: self.total_token_count,
            cache_read_tokens: self.cached_content_token_count,
            cache_write_tokens: None,
            // Chat-completions has no server-side tools; the Responses adapter is
            // the only one with anything to report here.
            billable_tool_calls: None,
        }
    }
}

#[derive(Default)]
struct GeminiParseState {
    message_started: bool,
    next_part_index: usize,
    next_tool_index: usize,
}

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
    for candidate in &chunk.candidates {
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
        if let Some(reason) = candidate.finish_reason.as_ref() {
            events.push(StreamEvent::Stop {
                reason: reason.clone(),
                usage: None,
            });
        }
    }
    if let Some(usage) = &chunk.usage_metadata {
        events.push(StreamEvent::UsageUpdate {
            usage: usage.normalise(),
        });
    }
    events
}

fn parse_agent_response(chunk: GenerateContentChunk, model: &str) -> Result<AgentResponse, ProviderError> {
    chunk.warn_ignored_fields();
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    let mut state = ProviderStateAccumulator::default();
    for event in parse_chunk(&chunk, model) {
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
        usage: chunk.usage_metadata.as_ref().map(GeminiUsage::normalise),
        provider_state: state.finish(),
    })
}

#[async_trait]
impl ChatProvider for GoogleGenerateContentProvider {
    #[cfg(test)]
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
                        Ok(chunk) => {
                            chunk.warn_ignored_fields();
                            parse_chunk_with_state(&chunk, &model, &mut parse_state)
                                .into_iter()
                                .map(Ok)
                                .collect()
                        }
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
    use crate::provider::state::{GoogleThoughtSignature, ProviderState, ProviderStatePayload, ProviderStateProducer};

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
        let params = ChatParams {
            model: "gemini-3.7-flash".into(),
            thinking_enabled: true,
            thinking_effort: Some("high".into()),
            ..Default::default()
        };
        let tools = vec![ToolDefinition {
            name: "lookup".into(),
            description: "Look something up".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }),
        }];
        let request = provider
            .build_request(&[ChatMessage::user("hi")], Some(&tools), &params, true)
            .unwrap();
        assert_eq!(
            request.url,
            "https://relay.example/v1beta/models/gemini-3.7-flash:streamGenerateContent?alt=sse"
        );
        let Some(RequestBody::Json(body)) = request.body else {
            panic!("expected JSON request body");
        };
        assert_eq!(body["generationConfig"]["thinkingConfig"]["thinkingLevel"], "HIGH");
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["required"][0],
            "query"
        );
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
        let error = serde_json::from_value::<GenerateContentChunk>(serde_json::json!({
            "usageMetadata": { "totalTokenCount": 3 }
        }))
        .err()
        .expect("candidates is required");
        assert!(error.to_string().contains("missing field `candidates`"));
    }
}

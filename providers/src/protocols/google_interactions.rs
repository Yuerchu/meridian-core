//! Google Gemini Interactions API (Vertex AI / Gemini Enterprise Agent Platform).
//!
//! Interactions is a different wire protocol from GenerateContent.  Meridian
//! keeps the conversation locally, so requests deliberately use `store=false`
//! and send the local history as interaction steps rather than relying on the
//! service's `previous_interaction_id` state.
//!
//! Google express mode uses `x-goog-api-key`; standard Vertex AI uses a
//! service-account JSON document to mint an OAuth 2.0 bearer token.

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::StreamExt;
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

use crate::dto::{ExtraIgnore, embedded_upstream_error, warn_extra_fields, warn_unknown_event};
use crate::mixins::GoogleSamplingParamsMixin;
use crate::mixins::vertex_auth::VertexServiceAccountAuthMixin;
#[cfg(test)]
use crate::mixins::vertex_auth::parse_vertex_service_account as parse_service_account;
use crate::state::{ProviderStateAccumulator, ProviderStateUpdate};
use crate::{
    AgentResponse, ChatMessage, ChatParams, ChatProvider, ChatStream, MessageContentPart, ProviderError,
    SenderRendering, ServerToolCall, ServerToolKind, StreamEvent, ThinkingStyle, TokenUsage, ToolCall, ToolDefinition,
};
use meridian_provider_sdk::client::{HttpTransport, Request, RequestBody, ReqwestTransport};

const DEFAULT_INTERACTIONS_SUFFIX: &str = "/v1beta1/locations/global/interactions";

pub struct GoogleInteractionsProvider {
    endpoint: String,
    auth: GoogleInteractionsAuth,
}

enum GoogleInteractionsAuth {
    ApiKey(String),
    VertexServiceAccount(VertexServiceAccountAuthMixin),
}

impl GoogleInteractionsProvider {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        Self {
            endpoint: interactions_endpoint(base_url),
            auth: GoogleInteractionsAuth::ApiKey(api_key.to_string()),
        }
    }

    pub fn new_vertex(base_url: &str, service_account_json: &str) -> Result<Self, String> {
        let auth = VertexServiceAccountAuthMixin::from_json(service_account_json)?;
        let endpoint = vertex_interactions_endpoint(base_url, auth.project_id())?;
        Ok(Self {
            endpoint,
            auth: GoogleInteractionsAuth::VertexServiceAccount(auth),
        })
    }

    fn build_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
        params: &ChatParams,
        stream: bool,
    ) -> Result<Request, ProviderError> {
        let model = params.model.strip_prefix("models/").unwrap_or(&params.model);
        let (system, input) = serialize_input(messages, model)?;
        let mut body = Map::new();
        body.insert("model".into(), json!(model));
        body.insert("input".into(), Value::Array(input));
        body.insert("stream".into(), Value::Bool(stream));
        // Meridian persists the canonical history. Keeping a second copy in
        // Vertex would make retention and replay semantics diverge.
        body.insert("store".into(), Value::Bool(false));
        if !system.is_empty() {
            body.insert("system_instruction".into(), json!(system));
        }

        let mut generation = Map::new();
        GoogleSamplingParamsMixin::from_chat_params(params).apply_snake_case(&mut generation);
        if params.thinking_style != ThinkingStyle::Budget
            && let Some(effort) = params.thinking_effort.as_deref()
        {
            generation.insert("thinking_level".into(), json!(effort));
        }
        if params.thinking_enabled && params.thinking_style != ThinkingStyle::None {
            generation.insert("thinking_summaries".into(), json!("auto"));
        }
        if !generation.is_empty() {
            body.insert("generation_config".into(), Value::Object(generation));
        }

        let mut wire_tools = Vec::new();
        for kind in &params.server_tools {
            wire_tools.push(match kind {
                ServerToolKind::GoogleSearch => json!({
                    "type": "google_search",
                    "search_types": ["web_search"],
                }),
                ServerToolKind::UrlContext => json!({ "type": "url_context" }),
                unsupported => {
                    return Err(ProviderError::InvalidRequest(format!(
                        "server tool `{unsupported}` is not supported by Gemini Interactions"
                    )));
                }
            });
        }
        if let Some(tools) = tools.filter(|items| !items.is_empty()) {
            wire_tools.extend(tools.iter().map(|tool| {
                json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                })
            }));
        }
        if !wire_tools.is_empty() {
            body.insert("tools".into(), Value::Array(wire_tools));
        }

        let mut request = Request::new(http::Method::POST, self.endpoint.clone());
        request.body = Some(RequestBody::Json(Value::Object(body)));
        Ok(request)
    }

    async fn authenticated_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
        params: &ChatParams,
        stream: bool,
    ) -> Result<Request, ProviderError> {
        let mut request = self.build_request(messages, tools, params, stream)?;
        match &self.auth {
            GoogleInteractionsAuth::ApiKey(api_key) => {
                request
                    .headers
                    .insert("x-goog-api-key", crate::auth_header_value(api_key));
            }
            GoogleInteractionsAuth::VertexServiceAccount(vertex_auth) => {
                vertex_auth.apply_bearer(&mut request).await?;
            }
        }
        Ok(request)
    }
}

fn vertex_interactions_endpoint(base_url: &str, project_id: &str) -> Result<String, String> {
    if project_id
        .chars()
        .any(|character| !(character.is_ascii_alphanumeric() || character == '-' || character == '_'))
    {
        return Err("Vertex Service Account project_id contains an invalid URL character".into());
    }
    let base = base_url.trim().trim_end_matches('/');
    let without_interactions = base.strip_suffix("/interactions").unwrap_or(base);
    if without_interactions.contains("/projects/") {
        return Ok(format!("{without_interactions}/interactions"));
    }
    for version in ["/v1beta1/locations/", "/v1/locations/"] {
        if let Some(index) = without_interactions.find(version) {
            let prefix = &without_interactions[..index];
            let locations = &without_interactions[index + version.len()..];
            let version_name = version.split('/').nth(1).unwrap_or("v1beta1");
            return Ok(format!(
                "{prefix}/{version_name}/projects/{project_id}/locations/{locations}/interactions"
            ));
        }
    }
    if without_interactions.ends_with("/v1beta1") || without_interactions.ends_with("/v1") {
        return Ok(format!(
            "{without_interactions}/projects/{project_id}/locations/global/interactions"
        ));
    }
    Ok(format!(
        "{without_interactions}/v1beta1/projects/{project_id}/locations/global/interactions"
    ))
}

fn interactions_endpoint(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if base.ends_with("/interactions") {
        return base.to_string();
    }
    if (base.contains("/v1beta1/") && base.contains("/locations/"))
        || (base.contains("/v1/") && base.contains("/locations/"))
        || base.ends_with("/v1beta1")
        || base.ends_with("/v1")
        || base.contains("/projects/")
    {
        let base = if base.ends_with("/v1beta1") || base.ends_with("/v1") {
            format!("{base}/locations/global")
        } else {
            base.to_string()
        };
        return format!("{base}/interactions");
    }
    format!("{base}{DEFAULT_INTERACTIONS_SUFFIX}")
}

fn serialize_input(messages: &[ChatMessage], model: &str) -> Result<(String, Vec<Value>), ProviderError> {
    let model = model.strip_prefix("models/").unwrap_or(model);
    let prefixed_model = format!("models/{model}");
    let system = messages
        .iter()
        .filter(|message| message.role == "system")
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut input = Vec::new();
    let mut consumed_results = HashSet::<String>::new();

    for (message_index, message) in messages.iter().enumerate() {
        if message.role == "system" {
            continue;
        }
        if message.role == "tool" {
            let call_id = message.tool_call_id.as_deref().unwrap_or_default();
            if consumed_results.contains(call_id) {
                continue;
            }
            input.push(json!({
                "type": "function_result",
                "call_id": call_id,
                "result": function_result_content(message)?,
            }));
            continue;
        }
        consumed_results.clear();

        if message.role == "assistant" {
            let interaction_steps = message.provider_state.as_ref().and_then(|state| {
                state
                    .google_interaction_steps_for(model)
                    .or_else(|| state.google_interaction_steps_for(&prefixed_model))
            });
            if let Some(steps) = interaction_steps {
                for step in steps {
                    let value: Value = serde_json::from_str(&step.step_json).map_err(|error| {
                        ProviderError::Parse(format!(
                            "Google Interactions historical step {} is invalid JSON: {error}",
                            step.position
                        ))
                    })?;
                    input.push(value);
                }
            }
            if !message.content.is_empty() && interaction_steps.is_none() {
                input.push(json!({
                    "type": "model_output",
                    "content": [{ "type": "text", "text": message.content }],
                }));
            }
            if let Some(tool_calls) = message.tool_calls.as_ref() {
                let decoded_arguments = tool_calls
                    .iter()
                    .map(|call| crate::decode_tool_arguments(&call.arguments, &call.id).map_err(ProviderError::Parse))
                    .collect::<Result<Vec<_>, _>>()?;
                if interaction_steps.is_none() {
                    for (call, arguments) in tool_calls.iter().zip(decoded_arguments) {
                        input.push(json!({
                            "type": "function_call",
                            "id": call.id,
                            "name": call.name,
                            "arguments": arguments,
                        }));
                    }
                }
                let recorded_results = messages[message_index + 1..]
                    .iter()
                    .take_while(|next| next.role == "tool")
                    .filter_map(|next| Some((next.tool_call_id.as_deref()?, next)))
                    .collect::<HashMap<_, _>>();
                for call in tool_calls {
                    let result = match recorded_results.get(call.id.as_str()) {
                        Some(message) => function_result_content(message)?,
                        None => vec![json!({
                            "type": "text",
                            "text": "Tool execution was interrupted before a result was recorded. It was not retried.",
                        })],
                    };
                    input.push(json!({
                        "type": "function_result",
                        "call_id": call.id,
                        "name": call.name,
                        "result": result,
                    }));
                    consumed_results.insert(call.id.clone());
                }
            }
            continue;
        }

        let rendered = crate::render_message(message, SenderRendering::Prefix).map_err(ProviderError::Parse)?;
        input.push(json!({
            "type": "user_input",
            "content": rendered_parts(&rendered.content)?,
        }));
    }
    Ok((system, input))
}

fn rendered_parts(content: &str) -> Result<Vec<Value>, ProviderError> {
    let Some(parts) = crate::decode_message_parts(content).map_err(ProviderError::Parse)? else {
        return Ok(vec![json!({ "type": "text", "text": content })]);
    };
    parts
        .into_iter()
        .map(|part| match part {
            MessageContentPart::Text { text } => Ok(json!({ "type": "text", "text": text })),
            MessageContentPart::ImageUrl { image_url } => interaction_media("image", &image_url.url),
            MessageContentPart::File { file } => interaction_media("document", &file.url),
            MessageContentPart::Sticker { .. } => Err(ProviderError::Parse(
                "unresolved sticker part reached the Google Interactions adapter".into(),
            )),
        })
        .collect()
}

/// A `function_result.result`: the same content list a user input takes, so
/// pictures a tool returned go back inside the result they belong to.
fn function_result_content(message: &ChatMessage) -> Result<Vec<Value>, ProviderError> {
    match crate::tool_result_parts(message).map_err(ProviderError::Parse)? {
        None => Ok(vec![json!({ "type": "text", "text": message.content })]),
        Some(parts) => parts
            .into_iter()
            .map(|part| match part {
                crate::ToolWirePart::Text(text) => Ok(json!({ "type": "text", "text": text })),
                crate::ToolWirePart::Image(url) => interaction_media("image", &url),
            })
            .collect(),
    }
}

fn interaction_media(kind: &str, url: &str) -> Result<Value, ProviderError> {
    if let Some(data) = url.strip_prefix("data:")
        && let Some((mime_type, payload)) = data.split_once(";base64,")
    {
        return Ok(json!({ "type": kind, "data": payload, "mime_type": mime_type }));
    }
    crate::require_wire_attachment_url(url, "gemini-interactions")?;
    Ok(json!({ "type": kind, "uri": url }))
}

#[derive(Debug, Deserialize)]
struct InteractionUsage {
    total_tokens: Option<i32>,
    total_input_tokens: Option<i32>,
    total_output_tokens: Option<i32>,
    total_thought_tokens: Option<i32>,
    total_cached_tokens: Option<i32>,
    /// The API documents modality breakdowns, but Meridian bills one total
    /// prompt/output count. Keep them out of the extra-field warning path.
    #[serde(default, rename = "input_tokens_by_modality")]
    _input_tokens_by_modality: IgnoredAny,
    #[serde(default, rename = "output_tokens_by_modality")]
    _output_tokens_by_modality: IgnoredAny,
    /// Tool-use tokens are not provider invocation counts and have no matching
    /// field in the app's token ledger.
    #[serde(default, rename = "total_tool_use_tokens")]
    _total_tool_use_tokens: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl InteractionUsage {
    fn normalise(&self) -> TokenUsage {
        // Interactions reports thought tokens beside output tokens. Meridian's
        // billing contract treats reasoning as output, so fold the two into
        // one completion count rather than under-reporting thinking replies.
        let completion_tokens = match (self.total_output_tokens, self.total_thought_tokens) {
            (Some(output), Some(thought)) => Some(output.saturating_add(thought)),
            (output, None) => output,
            (None, Some(thought)) => Some(thought),
        };
        TokenUsage {
            prompt_tokens: self.total_input_tokens,
            completion_tokens,
            total_tokens: self.total_tokens,
            cache_read_tokens: self.total_cached_tokens,
            cache_write_tokens: None,
            billable_tool_calls: None,
        }
    }
}

#[derive(Debug, Default)]
struct InteractionParseState {
    model: String,
    message_started: bool,
    response_model_emitted: bool,
    current_step: Option<String>,
    next_step_index: usize,
    current_step_index: Option<usize>,
    current_step_value: Option<Value>,
    current_step_arguments: String,
    current_tool_index: Option<usize>,
    current_tool_arguments: String,
    next_tool_index: usize,
    current_server_tool_id: Option<String>,
    current_server_result: Option<Value>,
    active_server_tools: HashMap<String, ActiveServerTool>,
    server_tool_calls: i32,
}

#[derive(Debug, Clone)]
struct ActiveServerTool {
    name: String,
    arguments: Option<String>,
}

fn parse_usage(value: Option<&Value>, server_tool_calls: Option<i32>) -> Option<TokenUsage> {
    let usage = value
        .cloned()
        .and_then(|value| serde_json::from_value::<InteractionUsage>(value).ok());
    if let Some(usage) = usage.as_ref() {
        warn_extra_fields("gemini_interactions_usage", &usage.extra);
    }
    usage.map(|usage| {
        let mut normalised = usage.normalise();
        normalised.billable_tool_calls = server_tool_calls.filter(|calls| *calls > 0);
        normalised
    })
}

fn server_tool_type(step_type: &str) -> Option<(&'static str, bool)> {
    match step_type {
        "google_search_call" => Some(("google_search", false)),
        "url_context_call" => Some(("url_context", false)),
        "google_search_result" => Some(("google_search", true)),
        "url_context_result" => Some(("url_context", true)),
        _ => None,
    }
}

fn server_tool_id(step: &Value) -> Option<String> {
    step.get("id")
        .or_else(|| step.get("call_id"))
        .and_then(Value::as_str)
        .or_else(|| {
            step.get("result")
                .and_then(|result| result.get("call_id"))
                .and_then(Value::as_str)
        })
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn server_tool_arguments(step: &Value) -> Option<String> {
    step.get("arguments")
        .or_else(|| step.get("input"))
        .filter(|value| !value.is_null())
        .map(value_as_json_text)
}

fn collect_urls(value: &Value, sources: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            if let Some(url) = object.get("url").and_then(Value::as_str).filter(|url| !url.is_empty())
                && !sources.iter().any(|source| source == url)
            {
                sources.push(url.to_string());
            }
            for child in object.values() {
                collect_urls(child, sources);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_urls(item, sources);
            }
        }
        _ => {}
    }
}

fn server_tool_sources(value: &Value) -> Vec<String> {
    let mut sources = Vec::new();
    collect_urls(value, &mut sources);
    sources
}

fn merge_server_arguments(current: &mut Option<String>, incoming: &Value) {
    let text = value_as_json_text(incoming);
    if text.is_empty() {
        return;
    }
    if incoming.is_string()
        && let Some(existing) = current.as_mut()
        && !existing.trim_end().ends_with('}')
    {
        existing.push_str(&text);
        return;
    }
    *current = Some(text);
}

fn announce_server_tool(
    id: String,
    name: String,
    arguments: Option<String>,
    sources: Vec<String>,
    completed: bool,
    events: &mut Vec<StreamEvent>,
) {
    events.push(StreamEvent::ServerToolCall(ServerToolCall {
        id,
        name,
        arguments,
        sources,
        completed,
    }));
}

fn plain_text_item(value: &Value) -> Option<&str> {
    let object = value.as_object()?;
    if object.len() != 2 || object.get("type").and_then(Value::as_str) != Some("text") {
        return None;
    }
    object.get("text").and_then(Value::as_str)
}

fn append_step_content(step: &mut Value, field: &str, content: Value) {
    let Some(object) = step.as_object_mut() else {
        return;
    };
    let list = object.entry(field).or_insert_with(|| Value::Array(Vec::new()));
    if let Some(items) = list.as_array_mut() {
        if let Some(incoming_text) = plain_text_item(&content)
            && let Some(last) = items.last_mut()
            && plain_text_item(last).is_some()
            && let Some(existing_text) = last.get("text").and_then(Value::as_str).map(str::to_string)
        {
            last["text"] = Value::String(format!("{existing_text}{incoming_text}"));
            return;
        }
        items.push(content);
    } else {
        *list = Value::Array(vec![content]);
    }
}

fn merge_step_delta(delta: &Value, state: &mut InteractionParseState) {
    let Some(step) = state.current_step_value.as_mut() else {
        return;
    };
    let delta_type = delta.get("type").and_then(Value::as_str).unwrap_or_default();
    match delta_type {
        "text" => {
            if let Some(text) = delta.get("text").and_then(Value::as_str) {
                let field = if state.current_step.as_deref() == Some("thought") {
                    "summary"
                } else {
                    "content"
                };
                append_step_content(step, field, json!({"type":"text", "text": text}));
            }
        }
        "thought" => {
            if let Some(text) = delta.get("text").and_then(Value::as_str) {
                append_step_content(step, "summary", json!({"type": "text", "text": text}));
            } else if let Some(content) = delta.get("content") {
                match content {
                    Value::Array(items) => {
                        for item in items {
                            append_step_content(step, "content", item.clone());
                        }
                    }
                    Value::Object(_) => append_step_content(step, "content", content.clone()),
                    _ => {}
                }
            }
        }
        "thought_summary" => {
            if let Some(content) = delta.get("content") {
                match content {
                    Value::Array(items) => {
                        for item in items {
                            append_step_content(step, "summary", item.clone());
                        }
                    }
                    _ => append_step_content(step, "summary", content.clone()),
                }
            }
        }
        "arguments_delta" => {
            if let Some(partial) = delta.get("partial_arguments").and_then(Value::as_str) {
                state.current_step_arguments.push_str(partial);
            }
        }
        "text_annotation_delta" => {
            if let Some(annotations) = delta.get("annotations")
                && let Some(items) = step.get_mut("content").and_then(Value::as_array_mut)
                && let Some(last) = items.last_mut().and_then(Value::as_object_mut)
            {
                let list = last.entry("annotations").or_insert_with(|| Value::Array(Vec::new()));
                let incoming: Vec<Value> = match annotations {
                    Value::Array(array) => array.clone(),
                    other => vec![other.clone()],
                };
                if let Some(existing) = list.as_array_mut() {
                    existing.extend(incoming);
                } else {
                    *list = Value::Array(incoming);
                }
            }
        }
        "thought_signature" => {
            if let Some(signature) = delta.get("signature") {
                step["signature"] = signature.clone();
            }
        }
        _ => {
            if let Some(object) = delta.as_object()
                && let Some(target) = step.as_object_mut()
            {
                for (key, value) in object {
                    if key != "type" {
                        target.insert(key.clone(), value.clone());
                    }
                }
            }
        }
    }
}

fn finish_interaction_step(
    stop_step: Option<&Value>,
    events: &mut Vec<StreamEvent>,
    state: &mut InteractionParseState,
) {
    let mut step = match (state.current_step_value.take(), stop_step) {
        (Some(mut accumulated), Some(stop)) => {
            if let (Some(target), Some(source)) = (accumulated.as_object_mut(), stop.as_object()) {
                for (key, value) in source {
                    if key == "result" || key == "signature" || !target.contains_key(key) {
                        target.insert(key.clone(), value.clone());
                    }
                }
            }
            accumulated
        }
        (Some(accumulated), None) => accumulated,
        (None, Some(stop)) => stop.clone(),
        (None, None) => {
            state.current_step_index = None;
            state.current_step_arguments.clear();
            return;
        }
    };
    if !state.current_step_arguments.is_empty() {
        let arguments = serde_json::from_str::<Value>(&state.current_step_arguments)
            .unwrap_or_else(|_| Value::String(state.current_step_arguments.clone()));
        step["arguments"] = arguments;
    }
    let position = state.current_step_index.take().unwrap_or_else(|| {
        let idx = state.next_step_index;
        state.next_step_index = state.next_step_index.saturating_add(1);
        idx
    });
    if !state.model.is_empty()
        && let Ok(step_json) = serde_json::to_string(&step)
    {
        events.push(StreamEvent::ProviderStateUpdate {
            update: ProviderStateUpdate::GoogleInteractionsStep {
                model: state.model.clone(),
                position,
                step_json,
            },
        });
    }
    state.current_step_arguments.clear();
}

fn finish_server_tool_result(step: Option<&Value>, events: &mut Vec<StreamEvent>, state: &mut InteractionParseState) {
    let result = step.cloned().or_else(|| state.current_server_result.take());
    let Some(result) = result else {
        return;
    };
    let result_type = result.get("type").and_then(Value::as_str).unwrap_or_default();
    let Some((default_name, _)) = server_tool_type(result_type) else {
        return;
    };
    let id = server_tool_id(&result)
        .or_else(|| state.current_server_tool_id.clone())
        .unwrap_or_else(|| "interaction-server-tool".into());
    let active = state.active_server_tools.remove(&id);
    let name = active
        .as_ref()
        .map(|tool| tool.name.clone())
        .unwrap_or_else(|| default_name.to_string());
    let arguments = active
        .and_then(|tool| tool.arguments)
        .or_else(|| server_tool_arguments(&result));
    announce_server_tool(id.clone(), name, arguments, server_tool_sources(&result), true, events);
    state.current_server_tool_id = None;
    state.current_server_result = None;
}

fn parse_step_value(step: &Value, index: usize, events: &mut Vec<StreamEvent>, state: &mut InteractionParseState) {
    let Some(step_type) = step.get("type").and_then(Value::as_str) else {
        return;
    };
    state.current_step = Some(step_type.to_string());
    state.current_step_index = Some(index);
    state.current_step_value = Some(step.clone());
    state.current_step_arguments = step
        .get("arguments")
        .filter(|value| value.is_string())
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    state.current_server_result = None;
    match step_type {
        "model_output" => append_content_events(step.get("content"), events, false),
        "thought" => append_content_events(step.get("summary").or_else(|| step.get("content")), events, true),
        "function_call" => {
            let index = state.next_tool_index;
            state.next_tool_index += 1;
            state.current_tool_index = Some(index);
            let id = step
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("interaction-call-{index}"));
            let name = step.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
            state.current_tool_arguments.clear();
            if let Some(arguments) = step.get("arguments") {
                state.current_step_arguments = value_as_json_text(arguments);
            }
            events.push(StreamEvent::ToolCallStart { index, id, name });
            if let Some(arguments) = step.get("arguments") {
                state.current_tool_arguments = value_as_json_text(arguments);
                events.push(StreamEvent::ToolCallDelta {
                    index,
                    arguments: state.current_tool_arguments.clone(),
                });
            }
        }
        call_type @ ("google_search_call" | "url_context_call") => {
            let (name, _) = server_tool_type(call_type).expect("known Google server tool");
            let id = server_tool_id(step).unwrap_or_else(|| {
                let id = format!("interaction-server-tool-{}", state.server_tool_calls);
                id
            });
            let arguments = server_tool_arguments(step);
            if let Some(arguments) = step.get("arguments") {
                state.current_step_arguments = value_as_json_text(arguments);
            }
            state.current_tool_index = None;
            state.current_server_tool_id = Some(id.clone());
            state.server_tool_calls = state.server_tool_calls.saturating_add(1);
            state.active_server_tools.insert(
                id.clone(),
                ActiveServerTool {
                    name: name.to_string(),
                    arguments: arguments.clone(),
                },
            );
            announce_server_tool(id, name.to_string(), arguments, Vec::new(), false, events);
        }
        "google_search_result" | "url_context_result" => {
            state.current_tool_index = None;
            state.current_server_tool_id = server_tool_id(step).or_else(|| state.current_server_tool_id.clone());
            state.current_server_result = Some(step.clone());
        }
        _ => warn_unknown_event("gemini_interactions_step", step_type),
    }
}

fn append_content_events(content: Option<&Value>, events: &mut Vec<StreamEvent>, reasoning: bool) {
    let Some(content) = content else {
        return;
    };
    let items: Vec<&Value> = match content {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![content],
        _ => return,
    };
    for item in items {
        if let Some(text) = item.get("text").and_then(Value::as_str).filter(|text| !text.is_empty()) {
            if reasoning {
                events.push(StreamEvent::Reasoning {
                    content: text.to_string(),
                });
            } else {
                events.push(StreamEvent::Text {
                    content: text.to_string(),
                });
            }
        }
    }
}

fn value_as_json_text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| serde_json::to_string(value).unwrap_or_else(|_| "{}".into()))
}

fn normalise_interaction_stop_reason(interaction: Option<&Value>) -> String {
    let raw = interaction
        .and_then(|item| item.get("finish_reason").or_else(|| item.get("status")))
        .and_then(Value::as_str)
        .unwrap_or("completed");
    match raw.to_ascii_lowercase().as_str() {
        "max_tokens" | "length" | "incomplete" => "max_tokens".into(),
        "refusal" | "content_filter" | "safety" | "recitation" | "prohibited_content" | "blocklist" => {
            "content_filter".into()
        }
        _ => "completed".into(),
    }
}

fn parse_event(value: Value, state: &mut InteractionParseState) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    let event_type = value.get("event_type").and_then(Value::as_str).unwrap_or_default();
    if !state.message_started
        && let Some(id) = value
            .get("interaction")
            .and_then(|interaction| interaction.get("id"))
            .and_then(Value::as_str)
            .or_else(|| value.get("interaction_id").and_then(Value::as_str))
    {
        events.push(StreamEvent::MessageStart {
            message_id: id.to_string(),
        });
        state.message_started = true;
    }
    if !state.response_model_emitted
        && let Some(model) = value
            .get("interaction")
            .and_then(|interaction| interaction.get("model").or_else(|| interaction.get("model_version")))
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
    {
        events.push(StreamEvent::ResponseModel {
            model: model.to_string(),
        });
        state.response_model_emitted = true;
    }

    match event_type {
        "interaction.created" | "interaction.start" => {}
        "step.start" => {
            if let Some(step) = value.get("step") {
                let index = value
                    .get("index")
                    .and_then(Value::as_u64)
                    .map(|index| index as usize)
                    .unwrap_or(state.next_step_index);
                state.next_step_index = state.next_step_index.max(index.saturating_add(1));
                parse_step_value(step, index, &mut events, state);
            }
        }
        "step.delta" => {
            if let Some(delta) = value.get("delta") {
                merge_step_delta(delta, state);
                let delta_type = delta.get("type").and_then(Value::as_str).unwrap_or_default();
                match delta_type {
                    "text" => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            if state.current_step.as_deref() == Some("thought") {
                                events.push(StreamEvent::Reasoning {
                                    content: text.to_string(),
                                });
                            } else {
                                events.push(StreamEvent::Text {
                                    content: text.to_string(),
                                });
                            }
                        }
                    }
                    "function_call" => {
                        let index = if let Some(index) = state.current_tool_index {
                            index
                        } else {
                            let index = state.next_tool_index;
                            state.next_tool_index += 1;
                            state.current_tool_index = Some(index);
                            events.push(StreamEvent::ToolCallStart {
                                index,
                                id: delta
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .filter(|id| !id.is_empty())
                                    .map(str::to_string)
                                    .unwrap_or_else(|| format!("interaction-call-{index}")),
                                name: delta
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                            });
                            index
                        };
                        if let Some(arguments) = delta.get("arguments") {
                            state.current_tool_arguments.push_str(&value_as_json_text(arguments));
                            events.push(StreamEvent::ToolCallDelta {
                                index,
                                arguments: value_as_json_text(arguments),
                            });
                        }
                    }
                    "thought" => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            events.push(StreamEvent::Reasoning {
                                content: text.to_string(),
                            });
                        } else {
                            append_content_events(delta.get("content"), &mut events, true);
                        }
                    }
                    "thought_summary" => {
                        append_content_events(delta.get("content"), &mut events, true);
                    }
                    "thought_signature" => {
                        // Already folded into `current_step_value["signature"]`
                        // by `merge_step_delta` for stateless replay.
                    }
                    "arguments_delta" => {
                        let partial = delta
                            .get("partial_arguments")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if partial.is_empty() {
                            // An empty delta carries no new argument bytes.
                        } else if let Some(index) = state.current_tool_index {
                            state.current_tool_arguments.push_str(partial);
                            events.push(StreamEvent::ToolCallDelta {
                                index,
                                arguments: partial.to_string(),
                            });
                        } else if let Some(id) = state.current_server_tool_id.clone()
                            && let Some(active) = state.active_server_tools.get_mut(&id)
                        {
                            merge_server_arguments(&mut active.arguments, &Value::String(partial.to_string()));
                            announce_server_tool(
                                id,
                                active.name.clone(),
                                active.arguments.clone(),
                                Vec::new(),
                                false,
                                &mut events,
                            );
                        } else {
                            warn_unknown_event("gemini_interactions_delta", delta_type);
                        }
                    }
                    "text_annotation_delta" => {
                        // URL citations are metadata for the generated text.
                        // The result step already carries the source list used
                        // by the server-tool card; retain this event as a
                        // recognised no-op so it does not look like a protocol
                        // regression in logs.
                    }
                    "google_search_call" | "url_context_call" => {
                        let (default_name, _) = server_tool_type(delta_type).expect("known Google server tool");
                        let id = server_tool_id(delta)
                            .or_else(|| state.current_server_tool_id.clone())
                            .unwrap_or_else(|| "interaction-server-tool".into());
                        let active = state.active_server_tools.entry(id.clone()).or_insert_with(|| {
                            state.server_tool_calls = state.server_tool_calls.saturating_add(1);
                            ActiveServerTool {
                                name: default_name.to_string(),
                                arguments: None,
                            }
                        });
                        if let Some(arguments) = delta.get("arguments").or_else(|| delta.get("input")) {
                            merge_server_arguments(&mut active.arguments, arguments);
                        }
                        state.current_server_tool_id = Some(id.clone());
                        announce_server_tool(
                            id,
                            active.name.clone(),
                            active.arguments.clone(),
                            Vec::new(),
                            false,
                            &mut events,
                        );
                    }
                    "google_search_result" | "url_context_result" => {
                        if state.current_server_result.is_none() {
                            state.current_server_result = Some(delta.clone());
                        } else if let Some(result) = state.current_server_result.as_mut()
                            && let (Some(target), Some(source)) = (result.as_object_mut(), delta.as_object())
                        {
                            for (key, value) in source {
                                target.insert(key.clone(), value.clone());
                            }
                        }
                        if let Some(id) = server_tool_id(delta) {
                            state.current_server_tool_id = Some(id);
                        }
                    }
                    _ if state.current_server_tool_id.is_some()
                        && (delta.get("arguments").is_some() || delta.get("input").is_some()) =>
                    {
                        let id = state.current_server_tool_id.clone().unwrap_or_default();
                        if let Some(active) = state.active_server_tools.get_mut(&id)
                            && let Some(arguments) = delta.get("arguments").or_else(|| delta.get("input"))
                        {
                            merge_server_arguments(&mut active.arguments, arguments);
                            announce_server_tool(
                                id,
                                active.name.clone(),
                                active.arguments.clone(),
                                Vec::new(),
                                false,
                                &mut events,
                            );
                        }
                    }
                    _ => warn_unknown_event("gemini_interactions_delta", delta_type),
                }
            }
        }
        "step.stop" => {
            let result_type = value
                .get("step")
                .and_then(|step| step.get("type"))
                .and_then(Value::as_str)
                .or(state.current_step.as_deref());
            if result_type.and_then(server_tool_type).is_some_and(|(_, result)| result) {
                finish_server_tool_result(value.get("step"), &mut events, state);
            }
            if let Some(index) = state.current_tool_index.take() {
                events.push(StreamEvent::ToolCallDone {
                    index,
                    arguments: std::mem::take(&mut state.current_tool_arguments),
                });
            }
            finish_interaction_step(value.get("step"), &mut events, state);
            state.current_step = None;
        }
        "interaction.completed" => {
            let reason = normalise_interaction_stop_reason(value.get("interaction"));
            if let Some(usage) = parse_usage(
                value
                    .get("interaction")
                    .and_then(|interaction| interaction.get("usage")),
                Some(state.server_tool_calls),
            ) {
                events.push(StreamEvent::UsageUpdate { usage: usage.clone() });
                events.push(StreamEvent::Stop {
                    reason,
                    usage: Some(usage),
                });
            } else {
                events.push(StreamEvent::Stop { reason, usage: None });
            }
        }
        "interaction.failed" | "interaction.cancelled" => {
            let message = value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Vertex Interactions request failed")
                .to_string();
            events.push(StreamEvent::Error { message });
        }
        "error" => {
            let message = value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Vertex Interactions request failed")
                .to_string();
            events.push(StreamEvent::Error { message });
        }
        "interaction.status_update" => {
            if matches!(
                value.get("status").and_then(Value::as_str),
                Some("failed" | "cancelled")
            ) {
                let message = value
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Vertex Interactions request failed")
                    .to_string();
                events.push(StreamEvent::Error { message });
            }
        }
        _ => warn_unknown_event("gemini_interactions", event_type),
    }
    events
}

fn parse_response(value: Value, model: &str) -> Result<AgentResponse, ProviderError> {
    if matches!(
        value.get("status").and_then(Value::as_str),
        Some("failed" | "cancelled")
    ) || value.get("error").is_some_and(|err| !err.is_null())
    {
        let message = value
            .get("error")
            .and_then(|error| error.get("message").and_then(Value::as_str).or_else(|| error.as_str()))
            .unwrap_or("Vertex Interactions request failed")
            .to_string();
        return Err(ProviderError::Upstream(message));
    }
    let model = model.strip_prefix("models/").unwrap_or(model);
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    let mut state = ProviderStateAccumulator::default();
    let mut server_tool_calls = 0i32;
    if let Some(steps) = value.get("steps").and_then(Value::as_array) {
        for (position, step) in steps.iter().enumerate() {
            if !model.is_empty()
                && let Ok(step_json) = serde_json::to_string(step)
            {
                state
                    .apply(ProviderStateUpdate::GoogleInteractionsStep {
                        model: model.to_string(),
                        position,
                        step_json,
                    })
                    .map_err(ProviderError::Parse)?;
            }
            match step.get("type").and_then(Value::as_str) {
                Some("model_output") => {
                    let mut events = Vec::new();
                    append_content_events(step.get("content"), &mut events, false);
                    for event in events {
                        if let StreamEvent::Text { content } = event {
                            text.push_str(&content);
                        }
                    }
                }
                Some("thought") => {
                    let mut events = Vec::new();
                    append_content_events(step.get("summary").or_else(|| step.get("content")), &mut events, true);
                    for event in events {
                        if let StreamEvent::Reasoning { content } = event {
                            reasoning.push_str(&content);
                        }
                    }
                }
                Some("function_call") => {
                    let tool_index = tool_calls.len();
                    tool_calls.push(ToolCall {
                        id: step
                            .get("id")
                            .and_then(Value::as_str)
                            .filter(|id| !id.is_empty())
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("interaction-call-{tool_index}")),
                        name: step.get("name").and_then(Value::as_str).unwrap_or_default().into(),
                        arguments: value_as_json_text(step.get("arguments").unwrap_or(&Value::Object(Map::new()))),
                    });
                }
                // Provider-run tools have already been executed and their
                // results are folded into the interaction. The non-streaming
                // response has no event sink for a transient tool card, but
                // each call is still billable.
                Some("google_search_call") | Some("url_context_call") => {
                    server_tool_calls = server_tool_calls.saturating_add(1);
                }
                Some("google_search_result") | Some("url_context_result") => {}
                Some(step_type) => warn_unknown_event("gemini_interactions_step", step_type),
                None => warn_unknown_event("gemini_interactions_step", "<missing>"),
            }
        }
    }
    let usage = parse_usage(value.get("usage"), Some(server_tool_calls));
    Ok(AgentResponse {
        text,
        reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
        tool_calls,
        usage,
        provider_state: state.finish(),
    })
}

#[async_trait]
impl ChatProvider for GoogleInteractionsProvider {
    fn adapter_name(&self) -> &'static str {
        "GoogleInteractionsProvider"
    }

    async fn stream_chat_with_tools(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        params: ChatParams,
    ) -> Result<ChatStream, ProviderError> {
        let transport = ReqwestTransport::shared();
        let request = self
            .authenticated_request(&messages, Some(&tools), &params, true)
            .await?;
        let response = transport.stream(request).await?;
        let model = params
            .model
            .strip_prefix("models/")
            .unwrap_or(&params.model)
            .to_string();
        let mut parse_state = InteractionParseState {
            model,
            ..Default::default()
        };
        let stream = response
            .bytes
            .map(|result| result.map_err(ProviderError::Transport))
            .eventsource()
            .flat_map(move |event| {
                let events = match event {
                    Ok(event) if event.data == "[DONE]" => Vec::new(),
                    Ok(event) => match serde_json::from_str::<Value>(&event.data) {
                        Ok(value) => parse_event(value, &mut parse_state).into_iter().map(Ok).collect(),
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
        let request = self
            .authenticated_request(&messages, Some(&tools), &params, false)
            .await?;
        let response = transport.execute(request).await?;
        let value: Value = serde_json::from_slice(&response.body).map_err(|error| {
            embedded_upstream_error(&response.body)
                .map(ProviderError::Upstream)
                .unwrap_or_else(|| ProviderError::Parse(error.to_string()))
        })?;
        parse_response(value, &params.model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ProviderStateAccumulator;

    #[test]
    fn a_tool_picture_goes_back_inside_its_function_result() {
        let assistant = ChatMessage::assistant_with_tools(
            "",
            None,
            vec![ToolCall {
                id: "c1".into(),
                name: "shot".into(),
                arguments: "{}".into(),
            }],
        );
        let (_, input) =
            serialize_input(&[assistant, ChatMessage::tool_result_parts("c1", r#"[{"type":"text","text":"shot"},{"type":"image_url","image_url":{"url":"data:image/png;base64,QUJD"}}]"#)], "gemini-test").unwrap();
        let result = input.iter().find(|step| step["type"] == "function_result").unwrap();
        assert_eq!(
            result["result"],
            json!([
                {"type": "text", "text": "shot"},
                {"type": "image", "data": "QUJD", "mime_type": "image/png"},
            ])
        );
    }

    #[test]
    fn endpoint_defaults_to_vertex_global_interactions() {
        assert_eq!(
            interactions_endpoint("https://aiplatform.googleapis.com"),
            "https://aiplatform.googleapis.com/v1beta1/locations/global/interactions"
        );
        assert_eq!(
            interactions_endpoint("https://aiplatform.googleapis.com/v1beta1/projects/p/locations/global"),
            "https://aiplatform.googleapis.com/v1beta1/projects/p/locations/global/interactions"
        );
        assert_eq!(
            interactions_endpoint("https://aiplatform.googleapis.com/v1beta1/project-p/locations/global"),
            "https://aiplatform.googleapis.com/v1beta1/project-p/locations/global/interactions"
        );
    }

    #[test]
    fn service_account_endpoint_includes_project_from_credentials() {
        assert_eq!(
            vertex_interactions_endpoint(
                "https://aiplatform.googleapis.com/v1beta1/locations/global",
                "billing-project"
            )
            .unwrap(),
            "https://aiplatform.googleapis.com/v1beta1/projects/billing-project/locations/global/interactions"
        );
        assert_eq!(
            vertex_interactions_endpoint("https://aiplatform.googleapis.com", "billing-project").unwrap(),
            "https://aiplatform.googleapis.com/v1beta1/projects/billing-project/locations/global/interactions"
        );
    }

    #[test]
    fn service_account_json_is_validated_without_logging_credentials() {
        let credentials = parse_service_account(
            r#"{
                "type": "service_account",
                "project_id": "billing-project",
                "client_email": "vertex@example.iam.gserviceaccount.com",
                "private_key": "-----BEGIN PRIVATE KEY-----\\nnot-a-key\\n-----END PRIVATE KEY-----"
            }"#,
        )
        .unwrap();
        assert_eq!(credentials.project_id, "billing-project");
        assert!(parse_service_account("{}").is_err());
    }

    #[test]
    fn request_uses_stateless_interactions_shape() {
        let provider = GoogleInteractionsProvider::new("https://aiplatform.googleapis.com", "secret");
        let request = provider
            .build_request(
                &[ChatMessage::user("hello")],
                None,
                &ChatParams {
                    model: "gemini-3.7-flash".into(),
                    max_tokens: Some(128),
                    thinking_effort: Some("high".into()),
                    ..Default::default()
                },
                true,
            )
            .unwrap();
        assert_eq!(
            request.url,
            "https://aiplatform.googleapis.com/v1beta1/locations/global/interactions"
        );
        let Some(RequestBody::Json(body)) = request.body else {
            panic!("expected JSON request body");
        };
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["generation_config"]["thinking_level"], "high");
        assert_eq!(body["input"][0]["type"], "user_input");
    }

    #[test]
    fn request_declares_google_server_tools_before_functions() {
        let provider = GoogleInteractionsProvider::new("https://aiplatform.googleapis.com", "secret");
        let request = provider
            .build_request(
                &[ChatMessage::user("find and read this")],
                Some(&[ToolDefinition {
                    name: "local_lookup".into(),
                    description: "Look something up locally".into(),
                    parameters: json!({"type":"object"}),
                }]),
                &ChatParams {
                    model: "gemini-3.8-flash".into(),
                    server_tools: vec![ServerToolKind::GoogleSearch, ServerToolKind::UrlContext],
                    ..Default::default()
                },
                true,
            )
            .unwrap();
        let Some(RequestBody::Json(body)) = request.body else {
            panic!("expected JSON request body");
        };
        assert_eq!(
            body["tools"][0],
            json!({"type":"google_search","search_types":["web_search"]})
        );
        assert_eq!(body["tools"][1], json!({"type":"url_context"}));
        assert_eq!(body["tools"][2]["type"], "function");
    }

    #[test]
    fn stream_delta_emits_text_and_usage() {
        let mut state = InteractionParseState::default();
        let created = parse_event(
            json!({
                "event_type": "interaction.created",
                "interaction": { "id": "ix-1", "status": "in_progress" }
            }),
            &mut state,
        );
        assert!(matches!(created.first(), Some(StreamEvent::MessageStart { message_id }) if message_id == "ix-1"));
        let _ = parse_event(
            json!({ "event_type": "step.start", "step": { "type": "model_output" } }),
            &mut state,
        );
        let delta = parse_event(
            json!({ "event_type": "step.delta", "delta": { "type": "text", "text": "hi" } }),
            &mut state,
        );
        assert!(matches!(delta.first(), Some(StreamEvent::Text { content }) if content == "hi"));
        let completed = parse_event(
            json!({
                "event_type": "interaction.completed",
                "interaction": { "id": "ix-1", "usage": { "total_input_tokens": 2, "total_output_tokens": 1, "total_tokens": 3 } }
            }),
            &mut state,
        );
        assert!(
            completed
                .iter()
                .any(|event| matches!(event, StreamEvent::UsageUpdate { usage } if usage.total_tokens == Some(3)))
        );
        assert!(
            completed
                .iter()
                .any(|event| matches!(event, StreamEvent::Stop { reason, .. } if reason == "completed"))
        );
    }

    #[test]
    fn thought_usage_is_folded_into_completion_tokens() {
        let usage = parse_usage(
            Some(&json!({
                "total_input_tokens": 10,
                "total_output_tokens": 7,
                "total_thought_tokens": 13,
                "total_tokens": 30,
            })),
            None,
        )
        .unwrap();
        assert_eq!(usage.completion_tokens, Some(20));
        assert_eq!(usage.total_tokens, Some(30));
    }

    #[test]
    fn official_thought_summary_and_arguments_delta_events_are_read() {
        let mut state = InteractionParseState::default();
        let _ = parse_event(json!({"event_type":"step.start","step":{"type":"thought"}}), &mut state);
        let thought = parse_event(
            json!({
                "event_type":"step.delta",
                "delta":{"type":"thought_summary","content":{"type":"text","text":"I will check the source."}}
            }),
            &mut state,
        );
        assert!(matches!(
            thought.first(),
            Some(StreamEvent::Reasoning { content }) if content == "I will check the source."
        ));

        let _ = parse_event(
            json!({
                "event_type":"step.start",
                "step":{"type":"function_call","id":"fn-1","name":"lookup"}
            }),
            &mut state,
        );
        let first = parse_event(
            json!({"event_type":"step.delta","delta":{"type":"arguments_delta","partial_arguments":"{\"q\":"}}),
            &mut state,
        );
        assert!(matches!(
            first.first(),
            Some(StreamEvent::ToolCallDelta { arguments, .. }) if arguments == "{\"q\":"
        ));
        let _ = parse_event(
            json!({"event_type":"step.delta","delta":{"type":"arguments_delta","partial_arguments":"\"vertex\"}"}}),
            &mut state,
        );
        let done = parse_event(json!({"event_type":"step.stop"}), &mut state);
        assert!(matches!(
            done.first(),
            Some(StreamEvent::ToolCallDone { arguments, .. }) if arguments == r#"{"q":"vertex"}"#
        ));
    }

    #[test]
    fn stateless_history_replays_raw_thought_and_output_steps() {
        let mut state = InteractionParseState {
            model: "gemini-3.8-flash".into(),
            ..Default::default()
        };
        let mut accumulator = ProviderStateAccumulator::default();
        for (index, step, delta) in [
            (
                0,
                json!({"type":"thought"}),
                json!({"type":"thought_summary","content":{"type":"text","text":"reason"}}),
            ),
            (
                1,
                json!({"type":"model_output"}),
                json!({"type":"text","text":"answer"}),
            ),
        ] {
            let _ = parse_event(json!({"event_type":"step.start","index":index,"step":step}), &mut state);
            let _ = parse_event(
                json!({"event_type":"step.delta","index":index,"delta":delta}),
                &mut state,
            );
            let events = parse_event(json!({"event_type":"step.stop","index":index}), &mut state);
            for event in events {
                if let StreamEvent::ProviderStateUpdate { update } = event {
                    accumulator.apply(update).unwrap();
                }
            }
        }
        let provider_state = accumulator.finish().unwrap();
        let mut assistant = ChatMessage::assistant("answer");
        assistant.provider_state = Some(provider_state);
        let (_, input) = serialize_input(&[assistant, ChatMessage::user("next")], "gemini-3.8-flash").unwrap();
        assert_eq!(input[0]["type"], "thought");
        assert_eq!(input[1]["type"], "model_output");
        assert_eq!(input[1]["content"][0]["text"], "answer");
    }

    #[test]
    fn google_search_and_url_context_stream_as_server_tool_cards() {
        let mut state = InteractionParseState::default();
        let started = parse_event(
            json!({
                "event_type": "step.start",
                "step": {
                    "type": "google_search_call",
                    "id": "search-1",
                    "arguments": {"queries": ["Vertex Interactions API"]}
                }
            }),
            &mut state,
        );
        assert!(matches!(
            started.first(),
            Some(StreamEvent::ServerToolCall(call))
                if call.id == "search-1"
                    && call.name == "google_search"
                    && call.arguments.as_deref() == Some(r#"{"queries":["Vertex Interactions API"]}"#)
                    && !call.completed
        ));

        let _ = parse_event(
            json!({
                "event_type": "step.stop",
                "step": {"type": "google_search_call", "id": "search-1"}
            }),
            &mut state,
        );
        let result = parse_event(
            json!({
                "event_type": "step.start",
                "step": {
                    "type": "google_search_result",
                    "call_id": "search-1",
                    "result": [{"url": "https://cloud.google.com/vertex-ai"}]
                }
            }),
            &mut state,
        );
        assert!(result.is_empty(), "result is closed by step.stop");
        let finished = parse_event(
            json!({
                "event_type": "step.stop",
                "step": {
                    "type": "google_search_result",
                    "call_id": "search-1",
                    "result": [{"url": "https://cloud.google.com/vertex-ai"}]
                }
            }),
            &mut state,
        );
        assert!(matches!(
            finished.first(),
            Some(StreamEvent::ServerToolCall(call))
                if call.id == "search-1"
                    && call.completed
                    && call.sources == ["https://cloud.google.com/vertex-ai"]
        ));

        let _ = parse_event(
            json!({
                "event_type": "step.start",
                "step": {
                    "type": "url_context_call",
                    "id": "fetch-1",
                    "arguments": {"urls": ["https://example.com"]}
                }
            }),
            &mut state,
        );
        let _ = parse_event(
            json!({
                "event_type": "step.stop",
                "step": {
                    "type": "url_context_call",
                    "id": "fetch-1"
                }
            }),
            &mut state,
        );
        let finished = parse_event(
            json!({
                "event_type": "step.stop",
                "step": {
                    "type": "url_context_result",
                    "call_id": "fetch-1",
                    "result": {"url": "https://example.com", "status": "success"}
                }
            }),
            &mut state,
        );
        assert!(matches!(
            finished.first(),
            Some(StreamEvent::ServerToolCall(call))
                if call.id == "fetch-1" && call.name == "url_context" && call.completed
        ));
    }

    #[test]
    fn tool_history_is_replayed_as_a_call_and_one_result() {
        let assistant = ChatMessage::assistant_with_tools(
            "",
            None,
            vec![ToolCall {
                id: "call-1".into(),
                name: "lookup".into(),
                arguments: r#"{"query":"rust"}"#.into(),
            }],
        );
        let (_, input) = serialize_input(
            &[
                assistant,
                ChatMessage::tool_result("call-1", "found"),
                ChatMessage::user("continue"),
            ],
            "gemini-3.8-flash",
        )
        .unwrap();
        assert_eq!(input.iter().filter(|step| step["type"] == "function_call").count(), 1);
        assert_eq!(input.iter().filter(|step| step["type"] == "function_result").count(), 1);
    }

    #[test]
    fn streaming_text_and_thought_deltas_merge_instead_of_fragmenting() {
        let mut state = InteractionParseState {
            model: "gemini-3.8-flash".into(),
            ..Default::default()
        };
        let _ = parse_event(
            json!({"event_type":"step.start","index":0,"step":{"type":"thought"}}),
            &mut state,
        );
        let _ = parse_event(
            json!({"event_type":"step.delta","index":0,"delta":{"type":"thought","text":"part 1 "}}),
            &mut state,
        );
        let _ = parse_event(
            json!({"event_type":"step.delta","index":0,"delta":{"type":"thought","text":"part 2"}}),
            &mut state,
        );
        let _ = parse_event(
            json!({"event_type":"step.delta","index":0,"delta":{"type":"thought_signature","signature":"sig-1"}}),
            &mut state,
        );
        let thought_stop = parse_event(json!({"event_type":"step.stop","index":0}), &mut state);
        let thought_step: Value = match thought_stop.first() {
            Some(StreamEvent::ProviderStateUpdate {
                update: ProviderStateUpdate::GoogleInteractionsStep { step_json, .. },
            }) => serde_json::from_str(step_json).unwrap(),
            other => panic!("expected step update, got {other:?}"),
        };
        assert_eq!(
            thought_step["summary"],
            json!([{"type": "text", "text": "part 1 part 2"}])
        );
        assert_eq!(thought_step["signature"], "sig-1");

        let _ = parse_event(
            json!({"event_type":"step.start","index":1,"step":{"type":"model_output"}}),
            &mut state,
        );
        for chunk in ["Hel", "lo ", "world"] {
            let _ = parse_event(
                json!({"event_type":"step.delta","index":1,"delta":{"type":"text","text":chunk}}),
                &mut state,
            );
        }
        let _ = parse_event(
            json!({
                "event_type":"step.delta",
                "index":1,
                "delta":{"type":"text_annotation_delta","annotations":[{"url":"https://a.test"}]}
            }),
            &mut state,
        );
        let text_stop = parse_event(json!({"event_type":"step.stop","index":1}), &mut state);
        let text_step: Value = match text_stop.first() {
            Some(StreamEvent::ProviderStateUpdate {
                update: ProviderStateUpdate::GoogleInteractionsStep { step_json, .. },
            }) => serde_json::from_str(step_json).unwrap(),
            other => panic!("expected step update, got {other:?}"),
        };
        assert_eq!(text_step["content"].as_array().unwrap().len(), 1);
        assert_eq!(text_step["content"][0]["text"], "Hello world");
        assert_eq!(
            text_step["content"][0]["annotations"],
            json!([{"url": "https://a.test"}])
        );
    }

    #[test]
    fn parallel_tool_history_emits_all_calls_before_results_and_clears_consumed_ids() {
        let round1 = ChatMessage::assistant_with_tools(
            "",
            None,
            vec![
                ToolCall {
                    id: "c1".into(),
                    name: "first".into(),
                    arguments: "{}".into(),
                },
                ToolCall {
                    id: "c2".into(),
                    name: "second".into(),
                    arguments: "{}".into(),
                },
            ],
        );
        let round2 = ChatMessage::assistant_with_tools(
            "",
            None,
            vec![ToolCall {
                id: "c1".into(),
                name: "third".into(),
                arguments: "{}".into(),
            }],
        );
        let (_, input) = serialize_input(
            &[
                round1,
                ChatMessage::tool_result("c1", "r1"),
                ChatMessage::tool_result("c2", "r2"),
                round2,
                ChatMessage::tool_result("c1", "r3"),
            ],
            "models/gemini-3.8-flash",
        )
        .unwrap();
        let types: Vec<&str> = input.iter().map(|step| step["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            vec![
                "function_call",
                "function_call",
                "function_result",
                "function_result",
                "function_call",
                "function_result"
            ]
        );
        assert_eq!(input[5]["result"][0]["text"], "r3");
    }

    #[test]
    fn non_streaming_response_preserves_provider_state_and_server_tool_billing() {
        let resp = parse_response(
            json!({
                "status": "completed",
                "steps": [
                    {"type": "thought", "summary": [{"type": "text", "text": "plan"}], "signature": "sig"},
                    {"type": "google_search_call", "id": "s1", "arguments": {"queries": ["rust"]}},
                    {"type": "google_search_result", "call_id": "s1", "result": [{"url": "https://rust-lang.org"}]},
                    {"type": "model_output", "content": [{"type": "text", "text": "done"}]}
                ],
                "usage": {
                    "total_input_tokens": 10,
                    "total_output_tokens": 4,
                    "total_thought_tokens": 6,
                    "total_tokens": 20
                }
            }),
            "models/gemini-3.8-flash",
        )
        .unwrap();
        assert_eq!(resp.text, "done");
        assert_eq!(resp.reasoning_content.as_deref(), Some("plan"));
        let usage = resp.usage.unwrap();
        assert_eq!(usage.completion_tokens, Some(10));
        assert_eq!(usage.billable_tool_calls, Some(1));
        let steps = resp
            .provider_state
            .unwrap()
            .google_interaction_steps_for("gemini-3.8-flash")
            .unwrap()
            .to_vec();
        assert_eq!(steps.len(), 4);
    }

    #[test]
    fn request_strips_models_prefix_and_emits_response_model_and_stop_reason() {
        let provider = GoogleInteractionsProvider::new("https://aiplatform.googleapis.com", "secret");
        let req = provider
            .build_request(
                &[ChatMessage::user("hi")],
                None,
                &ChatParams {
                    model: "models/gemini-3.8-flash".into(),
                    thinking_style: ThinkingStyle::EffortOnly,
                    thinking_enabled: true,
                    ..Default::default()
                },
                true,
            )
            .unwrap();
        let Some(RequestBody::Json(body)) = req.body else {
            panic!("expected JSON body");
        };
        assert_eq!(body["model"], "gemini-3.8-flash");

        let mut state = InteractionParseState::default();
        let created = parse_event(
            json!({
                "event_type": "interaction.created",
                "interaction": { "id": "ix-2", "model": "gemini-3.8-flash-001", "status": "in_progress" }
            }),
            &mut state,
        );
        assert!(created.iter().any(|event| matches!(
            event,
            StreamEvent::ResponseModel { model } if model == "gemini-3.8-flash-001"
        )));

        let truncated = parse_event(
            json!({
                "event_type": "interaction.completed",
                "interaction": { "id": "ix-2", "status": "incomplete" }
            }),
            &mut state,
        );
        assert!(truncated.iter().any(|event| matches!(
            event,
            StreamEvent::Stop { reason, .. } if reason == "max_tokens"
        )));
    }

    #[tokio::test]
    async fn vertex_token_cache_is_shared_across_provider_instances() {
        let sa = r#"{
            "type": "service_account",
            "project_id": "cache-test-project",
            "client_email": "cache-test@example.iam.gserviceaccount.com",
            "private_key": "-----BEGIN PRIVATE KEY-----\ncache-test\n-----END PRIVATE KEY-----",
            "token_uri": "https://oauth2.test.invalid/token"
        }"#;
        let first = GoogleInteractionsProvider::new_vertex("https://aiplatform.googleapis.com", sa).unwrap();
        let second = GoogleInteractionsProvider::new_vertex("https://aiplatform.googleapis.com", sa).unwrap();
        let GoogleInteractionsAuth::VertexServiceAccount(ref first_auth) = first.auth else {
            panic!("expected vertex auth");
        };
        let GoogleInteractionsAuth::VertexServiceAccount(ref second_auth) = second.auth else {
            panic!("expected vertex auth");
        };
        crate::mixins::vertex_auth::insert_cached_token_for_test(
            first_auth.cache_key().clone(),
            "shared-oauth-token".into(),
            3600,
        )
        .await;
        let token = second_auth.access_token().await.unwrap();
        assert_eq!(token, "shared-oauth-token");
        crate::mixins::vertex_auth::remove_cached_token_for_test(first_auth.cache_key()).await;
    }
}

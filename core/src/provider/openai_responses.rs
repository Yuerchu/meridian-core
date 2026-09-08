use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::StreamExt;
use serde::Deserialize;
use serde::de::IgnoredAny;
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use super::dto::{ExtraIgnore, warn_extra_fields};
use super::{
    AgentResponse, ChatMessage, ChatParams, ChatProvider, ChatStream, MessageContentPart, ProviderError, StreamEvent,
    TokenUsage, ToolCall, ToolDefinition,
};
use crate::client::{HttpTransport, Request, RequestBody, ReqwestTransport};

pub struct OpenAIResponsesProvider {
    base_url: String,
    api_key: String,
}

impl OpenAIResponsesProvider {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
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
        let (instructions, input) = serialize_responses_input(messages)?;

        let mut body = serde_json::json!({
            "model": params.model,
            "input": input,
            "stream": stream,
            "store": false,
        });
        if let Some(instructions) = instructions {
            body["instructions"] = serde_json::json!(instructions);
        }
        if let Some(t) = params.temperature {
            body["temperature"] = serde_json::json!(t);
        }
        if let Some(p) = params.top_p {
            body["top_p"] = serde_json::json!(p);
        }
        if let Some(m) = params.max_tokens {
            body["max_output_tokens"] = serde_json::json!(m);
        }
        if let Some(ref effort) = params.thinking_effort {
            // Without `summary` the reasoning summary events are never sent,
            // so a reasoning model would think in silence.
            body["reasoning"] = serde_json::json!({"effort": effort, "summary": "auto"});
        }
        if let Some(ref verbosity) = params.verbosity {
            body["text"] = serde_json::json!({"verbosity": verbosity});
        }
        if params.fast {
            // The config-facing name is "fast"; the wire value is the priority tier.
            body["service_tier"] = serde_json::json!("priority");
        }
        // Server-side tools sit in the same array as our own, and go first: the
        // tool list is the front of what a provider caches, and ours change with
        // the mode while these do not.
        let mut wire_tools: Vec<serde_json::Value> = params
            .server_tools
            .iter()
            .map(|name| serde_json::json!({ "type": name }))
            .collect();
        if let Some(tools) = tools {
            wire_tools.extend(tools.iter().map(|t| {
                serde_json::json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                    "strict": false,
                })
            }));
        }
        if !wire_tools.is_empty() {
            body["tools"] = serde_json::Value::Array(wire_tools);
            body["tool_choice"] = serde_json::json!("auto");
        }
        // xAI's spelling of the cache-routing key on this API; the header is the
        // chat-completions form. DeepSeek ignores the field, which its own
        // compatibility table says is what happens to anything it does not
        // support — its cache is managed for it.
        if let Some(key) = params.cache_key.as_deref() {
            body["prompt_cache_key"] = serde_json::json!(key);
        }

        let mut req = Request::new(http::Method::POST, format!("{}/responses", self.base_url));
        req.headers.insert(
            http::header::AUTHORIZATION,
            super::auth_header_value(&format!("Bearer {}", self.api_key)),
        );
        req.body = Some(RequestBody::Json(body));
        Ok(req)
    }
}

fn responses_user_content(content: &str) -> Result<Vec<serde_json::Value>, ProviderError> {
    let Some(parts) = super::decode_message_parts(content).map_err(ProviderError::Parse)? else {
        return Ok(vec![serde_json::json!({ "type": "input_text", "text": content })]);
    };

    parts
        .into_iter()
        .map(|part| match part {
            MessageContentPart::Text { text } => Ok(serde_json::json!({
                "type": "input_text",
                "text": text,
            })),
            MessageContentPart::ImageUrl { image_url } => Ok(serde_json::json!({
                "type": "input_image",
                "image_url": image_url.url,
            })),
            MessageContentPart::File { file } => Ok(serde_json::json!({
                "type": "input_file",
                "file_data": file.url,
                "filename": file.name,
            })),
            MessageContentPart::Sticker { .. } => Err(ProviderError::Parse(
                "unresolved sticker part reached the OpenAI Responses adapter".into(),
            )),
        })
        .collect()
}

fn serialize_responses_input(
    messages: &[ChatMessage],
) -> Result<(Option<String>, Vec<serde_json::Value>), ProviderError> {
    let mut instructions: Option<String> = None;
    let mut input = Vec::new();

    for m in messages {
        match m.role.as_str() {
            "system" => {
                if let Some(ref mut existing) = instructions {
                    existing.push('\n');
                    existing.push_str(&m.content);
                } else {
                    instructions = Some(m.content.clone());
                }
            }
            "user" => {
                // Responses input items have no `name` field (unlike
                // chat-completions), so the speaker goes in as a prefix.
                let rendered =
                    super::render_message(m, super::SenderRendering::Prefix).map_err(ProviderError::Parse)?;
                input.push(serde_json::json!({
                    "type": "message",
                    "role": "user",
                    "content": responses_user_content(&rendered.content)?,
                }));
            }
            "assistant" => {
                if !m.content.is_empty() {
                    input.push(serde_json::json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": m.content}],
                    }));
                }
                if let Some(ref tool_calls) = m.tool_calls {
                    for tc in tool_calls {
                        input.push(serde_json::json!({
                            "type": "function_call",
                            "name": tc.name,
                            "arguments": tc.arguments,
                            "call_id": tc.id,
                        }));
                    }
                }
            }
            "tool" => {
                if let Some(ref call_id) = m.tool_call_id {
                    input.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": m.content,
                    }));
                }
            }
            _ => {}
        }
    }

    // The sender note arrives inside the system prompt; every format renders the
    // marker now, so explaining it is no longer a per-adapter concern.
    Ok((instructions, input))
}

#[derive(Default)]
pub(super) struct StreamState {
    call_id_to_index: HashMap<String, usize>,
    next_index: usize,
}

#[derive(Deserialize)]
pub(super) struct ResponseUsage {
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    total_tokens: Option<i64>,
    /// The Responses API's equivalent of chat-completions'
    /// `prompt_tokens_details`. Same nesting, same reason for a struct of its
    /// own: serde cannot reach into a nested object from a flat field.
    input_tokens_details: Option<ResponseInputTokensDetails>,
    /// `{reasoning_tokens}`: a subset of `output_tokens`, not an addition to
    /// it, so nothing is read from it (see `normalise_responses_usage`). Named
    /// rather than left to `extra` because every reasoning reply carries it.
    #[serde(default, rename = "output_tokens_details")]
    _output_tokens_details: IgnoredAny,
    /// Absent on every endpoint that has no server-side tools, which is why it
    /// is an `Option` rather than a defaulted struct: "none ran" and "this API
    /// has none" both read as nothing to bill, and neither is a zero worth
    /// recording.
    server_side_tool_usage_details: Option<ServerToolUsage>,
    /// xAI's total beside the itemised details; deliberately not read, see
    /// `ServerToolUsage`.
    #[serde(default, rename = "num_server_side_tools_used")]
    _num_server_side_tools_used: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

/// Deserialise a usage object and report the fields it carried that this
/// code does not know. The three call sites parse the same shape, and the
/// warning belongs beside the parse rather than in `normalise_responses_usage`,
/// which tests feed already-typed values.
fn read_usage(value: Option<&serde_json::Value>) -> Option<TokenUsage> {
    let usage = serde_json::from_value::<ResponseUsage>(value?.clone()).ok()?;
    warn_extra_fields("responses_usage", &usage.extra);
    if let Some(details) = usage.input_tokens_details.as_ref() {
        warn_extra_fields("responses_input_tokens_details", &details.extra);
    }
    if let Some(details) = usage.server_side_tool_usage_details.as_ref() {
        warn_extra_fields("responses_server_tool_usage", &details.extra);
    }
    Some(normalise_responses_usage(&usage))
}

#[derive(Deserialize)]
struct ResponseInputTokensDetails {
    cached_tokens: Option<i64>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

/// What the provider ran on its own side, itemised.
///
/// Read per tool rather than off `num_server_side_tools_used`, because that
/// total includes invocations that carry no charge: image understanding inside a
/// search, X video understanding, remote MCP calls. Billing those would inflate
/// every search that happened to look at a picture.
///
/// The three counted here are the three this app can ask for, and xAI charges
/// $5/1000 for each. The ones deliberately absent are priced differently
/// (`attachment_search` at $10, `collections_search` at $2.50) and are never
/// requested — if one ever appears, it is worth a line in the log rather than a
/// number invented at a rate nobody configured.
#[derive(Deserialize, Default)]
struct ServerToolUsage {
    #[serde(default)]
    web_search_calls: i64,
    #[serde(default)]
    x_search_calls: i64,
    /// xAI's own field name for what its pricing table calls `code_execution`.
    #[serde(default)]
    code_interpreter_calls: i64,
    #[serde(default)]
    file_search_calls: i64,
    #[serde(default)]
    document_search_calls: i64,
    #[serde(default)]
    image_generation_calls: i64,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl ServerToolUsage {
    fn billable(&self) -> i64 {
        let unpriced = self.file_search_calls + self.document_search_calls + self.image_generation_calls;
        if unpriced > 0 {
            // Not counted, and not silently: these are billed at rates this app
            // has nowhere to store, so the total below is short by whatever they
            // cost.
            tracing::warn!(
                calls = unpriced,
                "the provider ran tools this app has no rate for; their cost is not included"
            );
        }
        self.web_search_calls + self.x_search_calls + self.code_interpreter_calls
    }
}

/// `input_tokens` here is the whole prompt, as in chat-completions — only the
/// field names differ. `output_tokens` already includes reasoning tokens
/// (`output_tokens_details.reasoning_tokens` is a subset of it, not an addition
/// to it), so there is nothing to add on and nothing extra to bill.
pub(super) fn normalise_responses_usage(u: &ResponseUsage) -> TokenUsage {
    TokenUsage {
        prompt_tokens: u.input_tokens.map(|v| v as i32),
        completion_tokens: u.output_tokens.map(|v| v as i32),
        total_tokens: u.total_tokens.map(|v| v as i32),
        billable_tool_calls: u.server_side_tool_usage_details.as_ref().map(|d| d.billable() as i32),
        cache_read_tokens: u
            .input_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens)
            .map(|v| v as i32),
        // The Responses API bills no premium for putting a prefix into cache,
        // so there is no figure to report — not a zero it never mentioned.
        cache_write_tokens: None,
    }
}

/// `response.error` on a `response.failed` event.
#[derive(Deserialize)]
struct ResponseError {
    code: Option<String>,
    message: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

/// The top-level `error` stream event. Not `ResponseError`: this one carries
/// the event's own envelope (`type`, `param`, `sequence_number`), which would
/// otherwise trip the unknown-field warning on every error.
#[derive(Deserialize)]
struct StreamErrorEvent {
    code: Option<String>,
    message: Option<String>,
    #[serde(default, rename = "type")]
    _type: IgnoredAny,
    #[serde(default, rename = "param")]
    _param: IgnoredAny,
    #[serde(default, rename = "sequence_number")]
    _sequence_number: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

/// Every `ResponseStreamEvent` type the specification lists that this adapter
/// has nothing to do with. Kept as a list rather than a `_ =>` arm so that a
/// name absent from both this and the `match` is a *new* event — something
/// worth a warning — instead of one more thing silently dropped.
const IGNORED_EVENTS: &[&str] = &[
    "response.created",
    "response.in_progress",
    "response.queued",
    "response.content_part.added",
    "response.content_part.done",
    "response.output_text.done",
    "response.output_text.annotation.added",
    "response.refusal.done",
    "response.reasoning_summary_part.done",
    "response.reasoning_summary_text.done",
    "response.reasoning_text.done",
    "response.web_search_call.in_progress",
    "response.web_search_call.searching",
    "response.web_search_call.completed",
    "response.file_search_call.in_progress",
    "response.file_search_call.searching",
    "response.file_search_call.completed",
    "response.code_interpreter_call.in_progress",
    "response.code_interpreter_call.interpreting",
    "response.code_interpreter_call.completed",
    "response.code_interpreter_call_code.delta",
    "response.code_interpreter_call_code.done",
    "response.image_generation_call.in_progress",
    "response.image_generation_call.generating",
    "response.image_generation_call.completed",
    "response.image_generation_call.partial_image",
    "response.mcp_call.in_progress",
    "response.mcp_call.completed",
    "response.mcp_call.failed",
    "response.mcp_call_arguments.delta",
    "response.mcp_call_arguments.done",
    "response.mcp_list_tools.in_progress",
    "response.mcp_list_tools.completed",
    "response.mcp_list_tools.failed",
    "response.custom_tool_call_input.delta",
    "response.custom_tool_call_input.done",
    "response.audio.delta",
    "response.audio.done",
    "response.audio.transcript.delta",
    "response.audio.transcript.done",
];

/// Output item types the provider runs on its own side and reports back as
/// already done. Only these become a `ServerToolCall`; see `server_tool_call`.
const SERVER_TOOL_ITEMS: &[&str] = &[
    "web_search_call",
    "file_search_call",
    "code_interpreter_call",
    "image_generation_call",
    "mcp_call",
    "mcp_list_tools",
    "custom_tool_call",
    // xAI's collections search. Not in OpenAI's list, but measured on the
    // wire and drawn as a card since before the list existed; it is priced
    // differently and the usage counter already excludes it.
    "document_search_call",
];

/// Output item types with a path of their own through the parser, and so not
/// worth a warning when `server_tool_call` declines them.
const OWN_PATH_ITEMS: &[&str] = &["message", "reasoning", "function_call"];

/// Warn once per process about a wire name this adapter does not know — an
/// event type, an output item type. Streams repeat a name for every token, so
/// once is the only useful frequency; the set is capped like `warn_extra_fields`.
fn warn_unknown_once(kind: &'static str, name: &str) {
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let fingerprint = format!("{kind}:{name}");
    let should_warn = WARNED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .map(|mut warned| {
            if warned.len() >= 256 {
                false
            } else {
                warned.insert(fingerprint)
            }
        })
        .unwrap_or(true);
    if should_warn {
        tracing::warn!(kind, name, "Responses stream carried a name this adapter does not know");
    }
}

/// Serialise a `web_search_call`'s `action` the way a card can show it. The
/// specification has three shapes and only the first was read to begin with:
/// `search{query, queries, sources}`, `open_page{url}`, `find_in_page{pattern,
/// url}`. Empty is how the opening event spells "not known yet", and an empty
/// string shown as a query reads as a search for nothing — so `None` until
/// something is there to show.
fn web_search_arguments(action: &serde_json::Value) -> Option<String> {
    let non_empty = |key: &str| action[key].as_str().filter(|s| !s.is_empty());
    let mut args = serde_json::Map::new();
    match action["type"].as_str() {
        Some("open_page") => {
            if let Some(url) = non_empty("url") {
                args.insert("url".into(), url.into());
            }
        }
        Some("find_in_page") => {
            if let Some(pattern) = non_empty("pattern") {
                args.insert("pattern".into(), pattern.into());
            }
            if let Some(url) = non_empty("url") {
                args.insert("url".into(), url.into());
            }
        }
        // `search`, and the absent type xAI's older events carry.
        _ => {
            if let Some(query) = non_empty("query") {
                args.insert("query".into(), query.into());
            }
            if let Some(queries) = action["queries"].as_array().filter(|q| !q.is_empty()) {
                args.insert("queries".into(), serde_json::Value::Array(queries.clone()));
            }
        }
    }
    (!args.is_empty()).then(|| serde_json::Value::Object(args).to_string())
}

/// Read a provider-side tool call out of an output item, if that is what it is.
///
/// **Two wire shapes, and they agree on almost nothing.** Measured against
/// `grok-4.6`:
///
/// * `{"type": "web_search_call", "action": {"query": …, "sources": […]}}` —
///   the tool's name is the item type with `_call` removed.
/// * `{"type": "custom_tool_call", "name": "x_keyword_search",
///    "input": "{\"query\":…}"}` — the name is a field and the arguments are a
///   JSON *string*. This is how xAI delivers `x_search`, which it decomposes
///   into `x_user_search` and `x_keyword_search` calls.
///
/// The second shape was excluded here at first, on the reading that
/// `custom_tool_call` belongs to DeepSeek's `apply_patch` compatibility tool.
/// It does — and xAI reuses the same envelope for its own searches, so
/// excluding it made every X search invisible: no card, no query, nothing
/// between the question and a minute of silence.
///
/// Treating an unrequested `custom_tool_call` as provider-side is safe because
/// this app never asks for one: `build_request` emits `function` entries and the
/// server-tool types, never `{"type": "custom"}`. If that ever changes, the
/// check has to become "did we ask for a custom tool by this name" — and the
/// consequence of getting it wrong is a card drawn for something we should have
/// run, which is why it is written down here.
///
/// The item types that qualify are a whitelist (`SERVER_TOOL_ITEMS`), not the
/// complement of a blacklist. The specification's output items also include
/// calls the *client* is supposed to execute and answer with a matching
/// `_call_output` (`computer_call`, `local_shell_call`, `shell_call`,
/// `apply_patch_call`, `tool_search_call`) and items that are not calls at all
/// (`compaction`, `program`, `configuration_update`). Announcing one of those
/// as provider-side would claim it had already run — a card for work nobody
/// did, and a model waiting for a reply that never comes — so anything outside
/// the list is warned about once and dropped, and a new server-side item type
/// is an entry here rather than a card by default.
fn server_tool_call(item: &serde_json::Value, completed: bool) -> Option<super::ServerToolCall> {
    let item_type = item["type"].as_str()?;
    if !SERVER_TOOL_ITEMS.contains(&item_type) {
        if !OWN_PATH_ITEMS.contains(&item_type) {
            warn_unknown_once("output_item", item_type);
        }
        return None;
    }
    let id = item["id"].as_str().unwrap_or_default().to_string();

    if item_type == "custom_tool_call" {
        return Some(super::ServerToolCall {
            id,
            // Absent on the opening event, which carries only the id.
            name: item["name"].as_str().unwrap_or_default().to_string(),
            arguments: item["input"].as_str().filter(|i| !i.is_empty()).map(str::to_string),
            // This shape itemises nothing; the citations arrive as annotations
            // on the answer instead.
            sources: Vec::new(),
            completed,
        });
    }

    // The tool's name is the item type with `_call` removed; `mcp_list_tools`
    // has no suffix and is its own name.
    let name = item_type.strip_suffix("_call").unwrap_or(item_type);
    let action = &item["action"];
    let arguments = if item_type == "web_search_call" {
        web_search_arguments(action)
    } else {
        // Empty is how the opening event spells "not known yet", and an empty
        // string shown as a query reads as a search for nothing.
        action["query"]
            .as_str()
            .filter(|q| !q.is_empty())
            .map(|query| serde_json::json!({ "query": query }).to_string())
    };
    Some(super::ServerToolCall {
        id,
        name: name.to_string(),
        arguments,
        sources: action["sources"]
            .as_array()
            .map(|sources| {
                sources
                    .iter()
                    .filter_map(|source| source["url"].as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        completed,
    })
}

pub(super) fn parse_responses_event(
    event_type: &str,
    data: &str,
    state: &mut StreamState,
) -> Vec<Result<StreamEvent, ProviderError>> {
    match event_type {
        // A refusal is the model's answer to the question, and the reader
        // should see it as one: it goes out as text, not as an error.
        "response.output_text.delta" | "response.refusal.delta" => {
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(data);
            match parsed {
                Ok(v) => {
                    if let Some(delta) = v["delta"].as_str()
                        && !delta.is_empty()
                    {
                        return vec![Ok(StreamEvent::Text {
                            content: delta.to_string(),
                        })];
                    }
                    vec![]
                }
                Err(e) => vec![Err(ProviderError::Parse(e.to_string()))],
            }
        }
        // Two spellings of the same thing. xAI streams a summary of its
        // reasoning under the first; DeepSeek streams the chain itself under the
        // second (`response.reasoning_text.delta`, per its compatibility table)
        // and produces no summary at all. Handling only one leaves that
        // provider's thinking invisible while it happens — which reads as the
        // model having stalled.
        // A summary comes as parts, each a paragraph of its own — typically a
        // bold title with its body — and the text deltas carry no separator.
        // Without one here the second part runs straight on from the first
        // (`**one****two**`), which is what the transcript showed.
        "response.reasoning_summary_part.added" => {
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(data);
            match parsed {
                Ok(v) => {
                    if v["summary_index"].as_u64().unwrap_or(0) > 0 {
                        return vec![Ok(StreamEvent::Reasoning {
                            content: "\n\n".to_string(),
                        })];
                    }
                    vec![]
                }
                Err(e) => vec![Err(ProviderError::Parse(e.to_string()))],
            }
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(data);
            match parsed {
                Ok(v) => {
                    if let Some(delta) = v["delta"].as_str()
                        && !delta.is_empty()
                    {
                        return vec![Ok(StreamEvent::Reasoning {
                            content: delta.to_string(),
                        })];
                    }
                    vec![]
                }
                Err(e) => vec![Err(ProviderError::Parse(e.to_string()))],
            }
        }
        "response.output_item.added" => {
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(data);
            match parsed {
                Ok(v) => {
                    let item = &v["item"];
                    if item["type"].as_str() == Some("function_call") {
                        let call_id = item["call_id"].as_str().unwrap_or("").to_string();
                        let item_id = item["id"].as_str().unwrap_or("").to_string();
                        let name = item["name"].as_str().unwrap_or("").to_string();
                        let index = state.next_index;
                        state.next_index += 1;
                        if !call_id.is_empty() {
                            state.call_id_to_index.insert(call_id.clone(), index);
                        }
                        if !item_id.is_empty() && item_id != call_id {
                            state.call_id_to_index.insert(item_id, index);
                        }
                        return vec![Ok(StreamEvent::ToolCallStart {
                            index,
                            id: call_id,
                            name,
                        })];
                    }
                    if let Some(call) = server_tool_call(item, false) {
                        return vec![Ok(StreamEvent::ServerToolCall(call))];
                    }
                    vec![]
                }
                Err(e) => vec![Err(ProviderError::Parse(e.to_string()))],
            }
        }
        "response.output_item.done" => {
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(data);
            match parsed {
                Ok(v) => {
                    let item = &v["item"];
                    if item["type"].as_str() == Some("function_call") {
                        let call_id = item["call_id"].as_str().or_else(|| item["id"].as_str()).unwrap_or("");
                        let arguments = item["arguments"].as_str().unwrap_or("{}").to_string();
                        if let Some(&index) = state.call_id_to_index.get(call_id) {
                            return vec![Ok(StreamEvent::ToolCallDone { index, arguments })];
                        }
                    }
                    // Where the substance of a server-side call actually is: the
                    // `added` event carries an empty query and no sources.
                    if let Some(call) = server_tool_call(item, true) {
                        return vec![Ok(StreamEvent::ServerToolCall(call))];
                    }
                    vec![]
                }
                Err(e) => vec![Err(ProviderError::Parse(e.to_string()))],
            }
        }
        "response.function_call_arguments.delta" => {
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(data);
            match parsed {
                Ok(v) => {
                    let delta = v["delta"].as_str().unwrap_or("");
                    if delta.is_empty() {
                        return vec![];
                    }
                    let call_id = v["call_id"].as_str().or_else(|| v["item_id"].as_str()).unwrap_or("");
                    if let Some(&index) = state.call_id_to_index.get(call_id) {
                        return vec![Ok(StreamEvent::ToolCallDelta {
                            index,
                            arguments: delta.to_string(),
                        })];
                    }
                    vec![]
                }
                Err(e) => vec![Err(ProviderError::Parse(e.to_string()))],
            }
        }
        "response.function_call_arguments.done" => {
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(data);
            match parsed {
                Ok(v) => {
                    let arguments = v["arguments"].as_str().unwrap_or("{}").to_string();
                    let call_id = v["call_id"].as_str().or_else(|| v["item_id"].as_str()).unwrap_or("");
                    if let Some(&index) = state.call_id_to_index.get(call_id) {
                        return vec![Ok(StreamEvent::ToolCallDone { index, arguments })];
                    }
                    vec![]
                }
                Err(e) => vec![Err(ProviderError::Parse(e.to_string()))],
            }
        }
        "response.completed" => {
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(data);
            match parsed {
                Ok(v) => {
                    let response = &v["response"];
                    let usage = read_usage(response.get("usage"));
                    let mut events = Vec::new();
                    if let Some(u) = usage {
                        events.push(Ok(StreamEvent::UsageUpdate { usage: u }));
                    }
                    events.push(Ok(StreamEvent::Stop {
                        reason: "stop".into(),
                        usage: None,
                    }));
                    events
                }
                Err(e) => vec![Err(ProviderError::Parse(e.to_string()))],
            }
        }
        "response.failed" => {
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(data);
            match parsed {
                Ok(v) => {
                    let response = &v["response"];
                    let error = response
                        .get("error")
                        .and_then(|e| serde_json::from_value::<ResponseError>(e.clone()).ok());
                    if let Some(error) = error.as_ref() {
                        warn_extra_fields("responses_error", &error.extra);
                    }
                    let code = error.as_ref().and_then(|e| e.code.as_deref()).unwrap_or("unknown");
                    let message = error
                        .as_ref()
                        .and_then(|e| e.message.as_deref())
                        .unwrap_or("Unknown error");
                    vec![Err(ProviderError::Api {
                        status: 400,
                        body: format!("{}: {}", code, message),
                    })]
                }
                Err(e) => vec![Err(ProviderError::Parse(e.to_string()))],
            }
        }
        "response.incomplete" => {
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(data);
            match parsed {
                Ok(v) => {
                    let reason = v["response"]["incomplete_details"]["reason"]
                        .as_str()
                        .unwrap_or("unknown");
                    let usage = read_usage(v["response"].get("usage"));
                    let mut events = Vec::new();
                    if let Some(u) = usage {
                        events.push(Ok(StreamEvent::UsageUpdate { usage: u }));
                    }
                    events.push(Ok(StreamEvent::Stop {
                        reason: reason.to_string(),
                        usage: None,
                    }));
                    events
                }
                Err(e) => vec![Err(ProviderError::Parse(e.to_string()))],
            }
        }
        // The stream's own error event, distinct from `response.failed`: it
        // carries the code at the top level and there is no response to read
        // it off. Nothing after it is coming, so it ends the stream as an error.
        "error" => {
            let parsed: Result<StreamErrorEvent, _> = serde_json::from_str(data);
            match parsed {
                Ok(error) => {
                    warn_extra_fields("responses_stream_error", &error.extra);
                    let code = error.code.as_deref().unwrap_or("unknown");
                    let message = error.message.as_deref().unwrap_or("Unknown error");
                    let status = if code.contains("rate_limit") { 429 } else { 400 };
                    vec![Err(ProviderError::Api {
                        status,
                        body: format!("{}: {}", code, message),
                    })]
                }
                Err(e) => vec![Err(ProviderError::Parse(e.to_string()))],
            }
        }
        other if IGNORED_EVENTS.contains(&other) => vec![],
        other => {
            warn_unknown_once("event", other);
            vec![]
        }
    }
}

#[async_trait]
impl ChatProvider for OpenAIResponsesProvider {
    #[cfg(test)]
    fn adapter_name(&self) -> &'static str {
        "OpenAIResponsesProvider"
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

        let mut state = StreamState::default();

        let stream = resp
            .bytes
            .map(|r| r.map_err(ProviderError::Transport))
            .eventsource()
            .flat_map(move |event| {
                let events: Vec<Result<StreamEvent, ProviderError>> = match event {
                    Ok(ev) => parse_responses_event(&ev.event, &ev.data, &mut state),
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

        let parsed: serde_json::Value =
            serde_json::from_slice(&resp.body).map_err(|e| ProviderError::Parse(e.to_string()))?;

        let mut text = String::new();
        if let Some(output) = parsed["output"].as_array() {
            for item in output {
                if item["type"].as_str() == Some("message")
                    && let Some(content) = item["content"].as_array()
                {
                    for part in content {
                        if part["type"].as_str() == Some("output_text")
                            && let Some(t) = part["text"].as_str()
                        {
                            text.push_str(t);
                        }
                    }
                }
            }
        }

        if text.is_empty() {
            Err(ProviderError::Parse("no content in response".into()))
        } else {
            Ok(text)
        }
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

        let parsed: serde_json::Value =
            serde_json::from_slice(&resp.body).map_err(|e| ProviderError::Parse(e.to_string()))?;

        let mut text = String::new();
        let mut reasoning_content: Option<String> = None;
        let mut tool_calls = Vec::new();

        if let Some(output) = parsed["output"].as_array() {
            for item in output {
                match item["type"].as_str() {
                    Some("message") => {
                        if let Some(content) = item["content"].as_array() {
                            for part in content {
                                if part["type"].as_str() == Some("output_text")
                                    && let Some(t) = part["text"].as_str()
                                {
                                    text.push_str(t);
                                }
                            }
                        }
                    }
                    Some("function_call") => {
                        if let (Some(call_id), Some(name)) = (item["call_id"].as_str(), item["name"].as_str()) {
                            let arguments = item["arguments"].as_str().unwrap_or("{}").to_string();
                            tool_calls.push(ToolCall {
                                id: call_id.to_string(),
                                name: name.to_string(),
                                arguments,
                            });
                        }
                    }
                    Some("reasoning") => {
                        if let Some(summary) = item["summary"].as_array() {
                            let mut parts = Vec::new();
                            for s in summary {
                                if let Some(t) = s["text"].as_str() {
                                    parts.push(t);
                                }
                            }
                            // Paragraphs, for the same reason the streaming
                            // path separates `reasoning_summary_part`s.
                            if !parts.is_empty() {
                                reasoning_content = Some(parts.join("\n\n"));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        let usage = read_usage(parsed.get("usage"));

        Ok(AgentResponse {
            text,
            reasoning_content,
            tool_calls,
            usage,
            provider_state: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ServerToolKind;

    fn body_for(model: &str, mutate: impl FnOnce(&mut ChatParams)) -> serde_json::Value {
        let caps = crate::provider::capabilities::resolve("openai", Some("responses"), model);
        let mut params = ChatParams {
            model: model.into(),
            ..Default::default()
        };
        mutate(&mut params);
        crate::provider::capabilities::filter_params(&mut params, &caps).unwrap();
        let provider = OpenAIResponsesProvider::new("https://example.test", "k");
        let req = provider
            .build_request(&[ChatMessage::user("hi")], None, &params, false)
            .unwrap();
        match req.body {
            Some(RequestBody::Json(v)) => v,
            _ => panic!("expected a JSON body"),
        }
    }

    #[test]
    fn user_multimodal_parts_become_native_responses_content() {
        let body = serde_json::json!([
            { "type": "text", "text": "look here" },
            { "type": "image_url", "image_url": { "url": "data:image/jpeg;base64,QUJD" } }
        ])
        .to_string();
        let message = ChatMessage::user_from(
            &body,
            crate::provider::SenderRef {
                user_id: 42,
                nickname: Some("Alice".into()),
            },
        );

        let (_, input) = serialize_responses_input(&[message]).expect("serialize multimodal user input");
        let content = input[0]["content"].as_array().expect("message content array");
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[0]["text"], "<sender>Alice(42)</sender>: ");
        assert_eq!(
            content[1],
            serde_json::json!({ "type": "input_text", "text": "look here" })
        );
        assert_eq!(content[2]["type"], "input_image");
        assert_eq!(content[2]["image_url"], "data:image/jpeg;base64,QUJD");
        assert!(
            content
                .iter()
                .filter_map(|part| part["text"].as_str())
                .all(|text| !text.contains("base64")),
            "the image payload must never be embedded in input_text"
        );
    }

    #[test]
    fn user_file_parts_become_input_files() {
        let body = serde_json::json!([
            {
                "type": "file",
                "file": {
                    "url": "data:application/pdf;base64,QUJD",
                    "mime_type": "application/pdf",
                    "name": "report.pdf"
                }
            }
        ])
        .to_string();

        let (_, input) = serialize_responses_input(&[ChatMessage::user(&body)]).expect("serialize file input");
        assert_eq!(
            input[0]["content"][0],
            serde_json::json!({
                "type": "input_file",
                "file_data": "data:application/pdf;base64,QUJD",
                "filename": "report.pdf"
            })
        );
    }

    #[test]
    fn ordinary_user_text_keeps_the_existing_responses_shape() {
        let (_, input) = serialize_responses_input(&[ChatMessage::user("hello")]).expect("serialize text input");
        assert_eq!(
            input[0]["content"],
            serde_json::json!([{ "type": "input_text", "text": "hello" }])
        );
    }

    #[test]
    fn reasoning_effort_is_nested_and_verbosity_defaults_from_catalog() {
        let body = body_for("gpt-5.6-sol", |p| p.thinking_effort = Some("max".into()));
        assert_eq!(body["reasoning"]["effort"], "max");
        assert_eq!(body["text"]["verbosity"], "low", "catalog default applies when unset");
    }

    #[test]
    fn fast_maps_to_priority_service_tier() {
        let body = body_for("gpt-5.6-sol", |p| p.fast = true);
        assert_eq!(body["service_tier"], "priority");
    }

    #[test]
    fn unsupported_effort_is_rejected_before_the_wire() {
        let caps = crate::provider::capabilities::resolve("openai", Some("responses"), "gpt-5.2");
        let mut params = ChatParams {
            model: "gpt-5.2".into(),
            thinking_effort: Some("max".into()),
            ..Default::default()
        };

        let error = crate::provider::capabilities::filter_params(&mut params, &caps).unwrap_err();
        assert!(error.contains("not supported by model 'gpt-5.2'"), "{error}");
    }

    /// Server-side tools share the array with our own, and go first — the tool
    /// list is the front of what a provider caches and these do not change with
    /// the mode.
    #[test]
    fn server_tools_lead_the_tool_array() {
        let provider = OpenAIResponsesProvider::new("https://api.x.ai/v1", "k");
        let params = ChatParams {
            model: "grok-4.6".into(),
            server_tools: vec![ServerToolKind::WebSearch, ServerToolKind::XSearch],
            cache_key: Some("conv-9".into()),
            ..Default::default()
        };
        let defs = [ToolDefinition {
            name: "read_file".into(),
            description: "read".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let req = provider
            .build_request(&[ChatMessage::user("hi")], Some(&defs), &params, true)
            .unwrap();
        let Some(RequestBody::Json(body)) = req.body else {
            panic!("JSON body")
        };
        let tools = body["tools"].as_array().expect("a tools array");
        assert_eq!(tools[0], serde_json::json!({"type": "web_search"}));
        assert_eq!(tools[1], serde_json::json!({"type": "x_search"}));
        assert_eq!(tools[2]["type"], "function");
        assert_eq!(tools[2]["name"], "read_file");
        // This API's spelling of the cache key; the header is the
        // chat-completions form and must not appear here.
        assert_eq!(body["prompt_cache_key"], "conv-9");
        assert!(req.headers.get("x-grok-conv-id").is_none());
    }

    /// A turn with server-side tools and none of our own still sends an array —
    /// otherwise switching the local tools off switches the provider's off too.
    #[test]
    fn server_tools_alone_still_produce_a_tool_array() {
        let provider = OpenAIResponsesProvider::new("https://api.x.ai/v1", "k");
        let params = ChatParams {
            model: "grok-4.6".into(),
            server_tools: vec![ServerToolKind::WebSearch],
            ..Default::default()
        };
        let req = provider
            .build_request(&[ChatMessage::user("hi")], None, &params, true)
            .unwrap();
        let Some(RequestBody::Json(body)) = req.body else {
            panic!("JSON body")
        };
        assert_eq!(body["tools"], serde_json::json!([{"type": "web_search"}]));
    }

    /// Verbatim from a live `grok-4.6` stream. The opening event carries an
    /// empty query and no sources; both arrive on completion, which is why a
    /// card has to be revised rather than drawn once.
    #[test]
    fn a_search_item_is_read_at_both_ends_of_its_life() {
        let opening: serde_json::Value = serde_json::from_str(
            r#"{"id":"ws_abc-0","type":"web_search_call","status":"in_progress",
                "action":{"type":"search","query":"","sources":[]}}"#,
        )
        .unwrap();
        let started = server_tool_call(&opening, false).expect("a server tool call");
        assert_eq!(started.name, "web_search");
        assert_eq!(started.id, "ws_abc-0");
        assert_eq!(started.arguments, None, "an empty query is not a search for nothing");
        assert!(!started.completed);

        let finished: serde_json::Value = serde_json::from_str(
            r#"{"id":"ws_abc-0","type":"web_search_call","status":"completed",
                "action":{"type":"search","query":"What is xAI",
                "sources":[{"type":"url","url":"https://x.ai/about"},
                           {"type":"url","url":"https://docs.x.ai/models"}]}}"#,
        )
        .unwrap();
        let done = server_tool_call(&finished, true).expect("a server tool call");
        assert_eq!(done.id, started.id, "the same card, revised");
        assert_eq!(done.arguments.as_deref(), Some(r#"{"query":"What is xAI"}"#));
        assert_eq!(done.sources.len(), 2);
        assert!(done.completed);
    }

    /// The other wire shape, verbatim from a live stream. Asking xAI for
    /// `x_search` produces `custom_tool_call` items whose real name is a field
    /// and whose arguments are a JSON string — nothing like the shape above.
    ///
    /// This was excluded to begin with, on the reading that `custom_tool_call`
    /// is DeepSeek's `apply_patch` envelope. It is that too, and the cost of
    /// excluding it was that every X search happened invisibly.
    #[test]
    fn an_x_search_is_read_out_of_the_custom_tool_shape() {
        let item: serde_json::Value = serde_json::from_str(
            r#"{"call_id":"xs_call-f1f-0","input":"{\"query\":\"from:thsottiaux\",\"limit\":\"5\"}",
                "name":"x_keyword_search","type":"custom_tool_call","id":"ctc_1c3-0","status":"completed"}"#,
        )
        .unwrap();
        let call = server_tool_call(&item, true).expect("a server tool call");
        assert_eq!(call.name, "x_keyword_search");
        assert_eq!(
            call.id, "ctc_1c3-0",
            "the item id, not the call id — the card keys on it"
        );
        assert_eq!(
            call.arguments.as_deref(),
            Some(r#"{"query":"from:thsottiaux","limit":"5"}"#)
        );
        assert!(call.sources.is_empty(), "this shape itemises none");

        // The opening event carries only the id.
        let opening = serde_json::json!({"id": "ctc_1c3-0", "type": "custom_tool_call"});
        let started = server_tool_call(&opening, false).expect("still announced");
        assert_eq!(started.id, "ctc_1c3-0");
        assert_eq!(started.arguments, None);
    }

    /// Our own calls are not server-side ones. Reading a `function_call` as one
    /// would draw a card for it *and* skip running it.
    #[test]
    fn a_function_call_is_never_mistaken_for_a_server_tool() {
        for item in [
            serde_json::json!({"id": "fc_1", "type": "function_call", "name": "read_file", "call_id": "c1"}),
            serde_json::json!({"id": "msg_1", "type": "message"}),
            serde_json::json!({"id": "rs_1", "type": "reasoning"}),
        ] {
            assert!(server_tool_call(&item, true).is_none(), "{item}");
        }
    }

    /// The whitelist, not its complement: an output item that is not a
    /// provider-side call — a client-executed call, or no call at all — must
    /// not be drawn as one, because a card claims the work was already done.
    #[test]
    fn an_item_outside_the_whitelist_is_not_a_server_tool() {
        for item in [
            serde_json::json!({"id": "cp_1", "type": "compaction", "status": "completed"}),
            serde_json::json!({"id": "sh_1", "type": "shell_call", "status": "completed"}),
            serde_json::json!({"id": "cu_1", "type": "computer_call", "status": "completed"}),
            serde_json::json!({"id": "zz_1", "type": "future_thing_call", "status": "completed"}),
        ] {
            assert!(server_tool_call(&item, true).is_none(), "{item}");
        }
        let listed = serde_json::json!({"id": "mcpl_1", "type": "mcp_list_tools"});
        assert_eq!(
            server_tool_call(&listed, true).expect("whitelisted").name,
            "mcp_list_tools"
        );
        // xAI's collections search is measured, listed, and still a card.
        let collections = serde_json::json!({"id": "ds_1", "type": "document_search_call", "status": "completed"});
        assert_eq!(
            server_tool_call(&collections, true).expect("whitelisted").name,
            "document_search"
        );
    }

    /// The other two `action` shapes of a `web_search_call`; only `search`
    /// was read before, so a page open or an in-page find showed no arguments.
    #[test]
    fn web_search_actions_serialise_by_their_shape() {
        let open: serde_json::Value = serde_json::from_str(
            r#"{"id":"ws_1","type":"web_search_call","status":"completed",
                "action":{"type":"open_page","url":"https://x.ai/about"}}"#,
        )
        .unwrap();
        let call = server_tool_call(&open, true).expect("a server tool call");
        assert_eq!(call.arguments.as_deref(), Some(r#"{"url":"https://x.ai/about"}"#));

        let find: serde_json::Value = serde_json::from_str(
            r#"{"id":"ws_2","type":"web_search_call","status":"completed",
                "action":{"type":"find_in_page","pattern":"pricing","url":"https://x.ai/about"}}"#,
        )
        .unwrap();
        let call = server_tool_call(&find, true).expect("a server tool call");
        assert_eq!(
            call.arguments.as_deref(),
            Some(r#"{"pattern":"pricing","url":"https://x.ai/about"}"#)
        );
    }

    #[test]
    fn a_stream_error_event_ends_the_stream_as_an_api_error() {
        let mut state = StreamState::default();
        let out = parse_responses_event(
            "error",
            r#"{"type":"error","code":"rate_limit_exceeded","message":"slow down","param":null,"sequence_number":3}"#,
            &mut state,
        );
        assert_eq!(out.len(), 1);
        match &out[0] {
            Err(ProviderError::Api { status, body }) => {
                assert_eq!(*status, 429);
                assert_eq!(body, "rate_limit_exceeded: slow down");
            }
            other => panic!("expected an API error, got {other:?}"),
        }

        let out = parse_responses_event(
            "error",
            r#"{"type":"error","code":"server_error","message":"x"}"#,
            &mut state,
        );
        assert!(matches!(out.first(), Some(Err(ProviderError::Api { status: 400, .. }))));
    }

    #[test]
    fn a_refusal_delta_is_text() {
        let mut state = StreamState::default();
        let out = parse_responses_event("response.refusal.delta", r#"{"delta":"I cannot"}"#, &mut state);
        assert!(matches!(out.first(), Some(Ok(StreamEvent::Text { content })) if content == "I cannot"));
        assert!(parse_responses_event("response.refusal.done", r#"{"refusal":"I cannot"}"#, &mut state).is_empty());
    }

    #[test]
    fn unknown_and_ignored_events_produce_nothing() {
        let mut state = StreamState::default();
        assert!(parse_responses_event("response.created", r#"{"response":{}}"#, &mut state).is_empty());
        assert!(parse_responses_event("response.audio.delta", r#"{"delta":"AAA="}"#, &mut state).is_empty());
        assert!(parse_responses_event("response.something_new.delta", "not even json", &mut state).is_empty());
    }

    /// Reasoning summaries are sent only when asked for; without `summary`
    /// the `reasoning_summary_text.delta` events never arrive.
    #[test]
    fn asking_for_effort_asks_for_the_summary_too() {
        let body = body_for("gpt-5.6-sol", |p| p.thinking_effort = Some("max".into()));
        assert_eq!(body["reasoning"]["summary"], "auto");
        let body = body_for("gpt-5.6-sol", |p| p.thinking_effort = None);
        assert!(body.get("reasoning").is_none(), "no effort, no reasoning object");
    }

    #[test]
    fn a_compaction_item_is_neither_a_call_nor_a_card() {
        let mut state = StreamState::default();
        let out = parse_responses_event(
            "response.output_item.done",
            r#"{"item":{"id":"cp_1","type":"compaction","encrypted_content":"..."}}"#,
            &mut state,
        );
        assert!(out.is_empty(), "{out:?}");
    }

    /// A summary arrives as parts and the deltas carry no separator between
    /// them; the first part opens nothing, every later one opens a paragraph.
    #[test]
    fn a_later_summary_part_opens_a_new_paragraph() {
        let mut state = StreamState::default();
        let first = parse_responses_event(
            "response.reasoning_summary_part.added",
            r#"{"summary_index":0,"part":{"type":"summary_text","text":""}}"#,
            &mut state,
        );
        assert!(first.is_empty(), "{first:?}");
        let second = parse_responses_event(
            "response.reasoning_summary_part.added",
            r#"{"summary_index":1,"part":{"type":"summary_text","text":""}}"#,
            &mut state,
        );
        assert!(
            matches!(second.first(), Some(Ok(StreamEvent::Reasoning { content })) if content == "\n\n"),
            "{second:?}"
        );
    }

    /// DeepSeek streams its chain of thought under a different event name than
    /// xAI's summary, and produces no summary at all. Handling only one leaves
    /// that provider's thinking invisible, which reads as a stalled model.
    #[test]
    fn both_spellings_of_a_reasoning_delta_are_understood() {
        let mut state = StreamState::default();
        for event in ["response.reasoning_summary_text.delta", "response.reasoning_text.delta"] {
            let out = parse_responses_event(event, r#"{"delta":"thinking"}"#, &mut state);
            assert!(
                matches!(out.first(), Some(Ok(StreamEvent::Reasoning { content })) if content == "thinking"),
                "{event} produced {out:?}",
            );
        }
    }
}

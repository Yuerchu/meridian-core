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
    /// Whether to send exactly what Codex sends — see [`CodexShape`].
    codex_shape: bool,
    /// The Codex release to claim, when the user has overridden it. Resolved
    /// from `codex.client_version` where the provider row is read; `None` takes
    /// [`codex_identity::DEFAULT_CODEX_CLIENT_VERSION`].
    client_version: Option<String>,
    /// Stable for the life of this provider, which is one turn. Only ever put
    /// on the wire under `codex_shape`, where it is one of the four headers
    /// Codex sends; the backend groups requests by it, so a fresh one per
    /// request would look like a fresh conversation each time.
    session_id: String,
}

/// What `codex_request_shape` turns on, in one place so the two halves cannot
/// drift apart.
///
/// The case is a Codex backend reached through an ordinary API key: a row that
/// is `openai` / `responses` / `standard` here, because that is what it looks
/// like from outside, and so gets this app's Responses request instead of
/// Codex's. The difference runs both ways and neither half is a refusal — the
/// request succeeds either way, and the only sign is worse answers:
///
/// * **Dropped.** `temperature`, `top_p` and `max_output_tokens`. Codex's
///   request struct has no field for any of them
///   (`codex-api/src/common.rs::ResponsesApiRequest`), and this app's own
///   `CodexProvider` already records that the backend rejects
///   `max_output_tokens` outright.
/// * **Added.** `include: ["reasoning.encrypted_content"]`, which `store: false`
///   makes the only way reasoning can survive to the next request, and
///   `parallel_tool_calls`, which Codex sends unconditionally.
/// * **Headers.** `originator`, `session_id` and a matching `user-agent`, plus
///   `Accept: text/event-stream` on the streaming call.
///
/// Off by default, and read by this adapter alone. `chatgpt_codex` rows go to
/// `CodexProvider`, which is this shape by construction; a chat-completions row
/// has no such shape to follow.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexShape {
    pub enabled: bool,
    /// The user's `codex.client_version` override, if they set one.
    pub client_version: Option<String>,
}

impl CodexShape {
    /// The ordinary row: no Codex shape at all.
    pub fn off() -> Self {
        Self::default()
    }
}

impl OpenAIResponsesProvider {
    pub fn new(base_url: &str, api_key: &str, codex_shape: CodexShape) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            codex_shape: codex_shape.enabled,
            client_version: codex_shape.client_version,
            session_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    fn build_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
        params: &ChatParams,
        stream: bool,
    ) -> Result<Request, ProviderError> {
        self.build_responses_request(messages, tools, params, stream, Compaction::No)
    }

    /// The one body builder, because a second one does not stay in step.
    ///
    /// `compact_remote` used to write its own, carrying over only `include` —
    /// so a lite model reached under the switch sent
    /// `x-openai-internal-codex-responses-lite: true` beside a body with no
    /// `reasoning.context`, which is the 400 the backend spells out in as many
    /// words. Every remote compaction on `gpt-6-astra` was rejected, and the
    /// comment above that call already said the shape had to match: the intent
    /// was right and the second copy is what was wrong. Codex has no second
    /// copy either — its compaction goes through the ordinary `Prompt` and
    /// `stream()` (`core/src/compact.rs`).
    fn build_responses_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
        params: &ChatParams,
        stream: bool,
        compaction: Compaction,
    ) -> Result<Request, ProviderError> {
        let (instructions, mut input) = serialize_responses_input(messages, self.reasoning_replay(params))?;
        if compaction == Compaction::Yes {
            // "Must be the final input item" — pushed before the lite prefix is
            // spliced in, which goes to the front and cannot displace it.
            input.push(serde_json::json!({ "type": "compaction_trigger" }));
        }
        // Only the Codex shape knows how to send the lite form, and a plain
        // OpenAI-compatible endpoint has never heard of an `additional_tools`
        // item — so the model's own flag is not enough to turn it on.
        let lite = self.codex_shape && params.responses_lite;

        let mut body = serde_json::json!({
            "model": params.model,
            "stream": stream,
            "store": false,
        });
        match (lite, instructions) {
            // The lite prefix, in Codex's order: the tools first, then the base
            // instructions, then the conversation
            // (`core/src/client.rs`, `use_responses_lite`). Both are `developer`
            // items and both are spliced at the *front*, which is what keeps
            // them at the head of the cached prefix.
            (true, instructions) => {
                let mut prefix = vec![additional_tools_item(params, tools)?];
                if let Some(instructions) = instructions.filter(|text| !text.is_empty()) {
                    prefix.push(base_instructions_item(params, &instructions));
                }
                input.splice(0..0, prefix);
            }
            (false, Some(instructions)) => {
                body["instructions"] = serde_json::json!(instructions);
            }
            (false, None) => {}
        }
        body["input"] = serde_json::Value::Array(input);
        if self.codex_shape {
            // The half of `CodexShape` that adds. `store: false` above is what
            // makes the first one load-bearing rather than tidy: without it the
            // reasoning items arrive with nothing replayable inside them, and
            // the model starts every round having forgotten why it called the
            // last tool.
            body["include"] = serde_json::json!(["reasoning.encrypted_content"]);
            // Codex computes this as `prompt.parallel_tool_calls && !use_responses_lite`,
            // so a lite model is sent `false` however the turn was configured.
            body["parallel_tool_calls"] = serde_json::json!(!lite);
        } else {
            // The half that drops. Codex's request struct has no field for any
            // of these three, and sending one to a backend that expects Codex's
            // shape is not refused -- it is answered worse.
            if let Some(t) = params.temperature {
                body["temperature"] = serde_json::json!(t);
            }
            if let Some(p) = params.top_p {
                body["top_p"] = serde_json::json!(p);
            }
            // Not on a compaction, which is the one request whose output is not
            // a reply: it is a summary of the whole history, and under
            // `store: false` it is what *replaces* that history. A ceiling
            // sized for chat answers would cut it off, and the truncation
            // would be permanent.
            if let Some(m) = params.max_tokens
                && compaction == Compaction::No
            {
                body["max_output_tokens"] = serde_json::json!(m);
            }
        }
        // Built as a map rather than a literal because the lite shape needs a
        // `reasoning` object even when nothing else would have produced one —
        // see `context` below, and `Reasoning` in `codex-api/src/common.rs`,
        // whose three fields are each skipped when absent.
        let mut reasoning = serde_json::Map::new();
        if let Some(effort) = params.thinking_effort.as_deref() {
            // Without `summary` the reasoning summary events are never sent,
            // so a reasoning model would think in silence.
            reasoning.insert("effort".into(), effort.into());
            reasoning.insert("summary".into(), "auto".into());
        }
        if lite {
            // **Required, and the backend says so in as many words.** Measured:
            // `unsupported_value` / `X-OpenAI-Internal-Codex-Responses-Lite
            // requires reasoning.context to be all_turns`. Codex writes exactly
            // this conditional — `use_responses_lite.then_some(AllTurns)` — and
            // omits the field otherwise, where the server's own default is
            // `current_turn`.
            //
            // It also pairs with what is already being sent: `all_turns` is the
            // half of the contract that makes the replayed
            // `reasoning.encrypted_content` above worth replaying.
            reasoning.insert("context".into(), "all_turns".into());
        }
        if !reasoning.is_empty() {
            body["reasoning"] = serde_json::Value::Object(reasoning);
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
        //
        // Skipped entirely under the lite shape, where the whole array has
        // already gone into `input` as an `additional_tools` item. Sending both
        // would describe the same tools twice in two grammars.
        if !lite {
            let mut wire_tools: Vec<serde_json::Value> = params
                .server_tools
                .iter()
                .map(|name| serde_json::json!({ "type": name }))
                .collect();
            if let Some(tools) = tools {
                wire_tools.extend(tools.iter().map(wire_function));
            }
            if !wire_tools.is_empty() {
                body["tools"] = serde_json::Value::Array(wire_tools);
                body["tool_choice"] = serde_json::json!("auto");
            }
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
        if self.codex_shape {
            self.insert_codex_headers(&mut req, params, stream);
        }
        req.body = Some(RequestBody::Json(body));
        Ok(req)
    }

    /// Which model's stored reasoning may be replayed into this request, if any.
    ///
    /// Keyed on the model rather than merely on the switch, because a stored
    /// item is encrypted and bound to the model that produced it: handing
    /// another one back is a 400, which is why `codex_reasoning_for` takes a
    /// name at all. `None` is the ordinary case and means the assistant rows go
    /// in as they always did.
    fn reasoning_replay<'a>(&self, params: &'a ChatParams) -> Option<&'a str> {
        self.codex_shape.then_some(params.model.as_str())
    }

    /// The header half of [`CodexShape`], read off Codex itself rather than off
    /// this app's own `CodexProvider`.
    ///
    /// **That distinction cost three of the five headers below.** `codex.rs` was
    /// written against an older Codex and sends `session_id` with an underscore;
    /// current Codex sends `session-id` *and* `thread-id` with hyphens
    /// (`codex-api/src/requests/headers.rs::build_session_headers`), plus
    /// `x-client-request-id`. The underscore spelling appears nowhere in the
    /// Codex tree any more. Copying our own adapter would have reproduced a
    /// header set no Codex backend has seen in a while — which is the exact
    /// failure this switch exists to prevent, arrived at from the other side.
    ///
    /// What Codex sends on `POST /responses`, and where each comes from here:
    ///
    /// | header | Codex | here |
    /// |---|---|---|
    /// | `originator` | `codex_cli_rs` | ours — see below |
    /// | `user-agent` | `{originator}/{v} ({os} {ver}; {arch}) {terminal}` | same shape, our values |
    /// | `session-id` | the CLI session | this provider's lifetime, one turn |
    /// | `thread-id` | the thread | the conversation (`cache_key`) |
    /// | `x-client-request-id` | the thread | the same conversation |
    /// | `accept` | `text/event-stream` on the stream | same |
    ///
    /// **The identity values stay ours, and that is the one place this switch
    /// stops short of its own name.** Codex classifies the value server-side —
    /// `login::default_client::is_first_party_originator` matches `codex_cli_rs`,
    /// `codex-tui`, `codex_vscode` and anything starting `Codex ` — so sending
    /// `meridian` is visibly not one of them. Claiming otherwise would also mean
    /// inventing a `codex_cli_rs` version number for a product whose releases we
    /// do not track, which goes stale silently. `CodexProvider`'s own note
    /// records that our name was *measured to be accepted*; that is about the
    /// request not being refused, which is a different question from whether the
    /// answer is as good, and nothing here settles the second.
    ///
    /// Two Codex headers are deliberately absent. `x-openai-subagent` marks a
    /// review/compaction pass, and this app does not plumb that distinction to
    /// the adapter — Codex omits it for an ordinary turn, which is the common
    /// case, and a wrong value is worse than none. `x-openai-internal-codex-residency`
    /// is set only under a managed policy Codex reads from its own config.
    fn insert_codex_headers(&self, req: &mut Request, params: &ChatParams, stream: bool) {
        let mut set = |name: &'static str, value: &str| {
            if let Ok(value) = http::HeaderValue::from_str(value) {
                req.headers.insert(http::HeaderName::from_static(name), value);
            }
        };
        set("originator", super::codex_identity::CODEX_ORIGINATOR);
        set(
            "user-agent",
            &super::codex_identity::user_agent(super::codex_identity::client_version(self.client_version.as_deref())),
        );
        set("session-id", &self.session_id);
        // The conversation, which is what `cache_key` holds and what Codex means
        // by a thread. Absent for the background passes that clear it
        // (`without_thinking`) — Codex likewise sends neither when it has no
        // thread, so an empty string here would be worse than the omission.
        let thread = params.cache_key.as_deref();
        if let Some(thread) = thread {
            set("thread-id", thread);
            set("x-client-request-id", thread);
            set("x-codex-window-id", &format!("{thread}:0"));
        }
        // What this app can honestly say about the turn. Absent rather than
        // invented where there is no equivalent — see `codex_metadata`.
        if let Some(metadata) = params.codex_turn.as_ref() {
            set("x-codex-installation-id", &metadata.installation_id);
            set(
                "x-codex-turn-metadata",
                &metadata.to_json(&self.session_id, thread, params).to_string(),
            );
        }
        // Announced only where it is true of this request: the same flag the
        // turn loop reads before it sends a `compaction_trigger`.
        if params.supports_remote_compaction {
            set("x-codex-beta-features", "remote_compaction_v2");
        }
        // The model's own shape, and the one header here that changes how the
        // body above was built rather than merely describing it.
        if params.responses_lite {
            set("x-openai-internal-codex-responses-lite", "true");
        }
        if stream {
            req.headers.insert(
                http::header::ACCEPT,
                http::HeaderValue::from_static("text/event-stream"),
            );
        }
    }
}

/// One of our tools as the Responses API spells a function.
fn wire_function(tool: &ToolDefinition) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters,
        "strict": false,
    })
}

/// A deterministic id for a prefix item, in Codex's `{prefix}_{uuid}` form.
///
/// **Derived rather than random, and that is the point.** These two items are
/// rebuilt from scratch on every request, so a fresh uuid each time would put a
/// different id at the head of the prompt on every round — which is a different
/// prefix, and the prefix is what a provider caches. Codex uses a v5 over the
/// thread's own namespace for the same reason; here the conversation stands in
/// for the thread, and a request without one falls back to the model so the id
/// is at least stable within a session.
fn prefix_item_id(prefix: &str, params: &ChatParams, payload: &[u8]) -> String {
    let namespace = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        params.cache_key.as_deref().unwrap_or(params.model.as_str()).as_bytes(),
    );
    format!("{prefix}_{}", uuid::Uuid::new_v5(&namespace, payload))
}

/// The tool array, as the lite shape carries it: one input item rather than a
/// top-level field.
///
/// Codex groups plain functions under a single `functions` namespace
/// (`tools/src/tool_spec.rs::create_tools_json_for_responses_lite`,
/// `DEFAULT_FUNCTION_NAMESPACE`) whose description is empty, and leaves
/// anything that is not a function — the provider-side tools — as its own entry
/// beside it. The namespace takes the position of the first function, so the
/// order the turn chose survives.
fn additional_tools_item(
    params: &ChatParams,
    tools: Option<&[ToolDefinition]>,
) -> Result<serde_json::Value, ProviderError> {
    let mut entries: Vec<serde_json::Value> = params
        .server_tools
        .iter()
        .map(|name| serde_json::json!({ "type": name }))
        .collect();
    let functions: Vec<serde_json::Value> = tools.unwrap_or_default().iter().map(wire_function).collect();
    if !functions.is_empty() {
        entries.push(serde_json::json!({
            "type": "namespace",
            "name": "functions",
            "description": "",
            "tools": functions,
        }));
    }

    let payload = serde_json::to_vec(&entries).map_err(|e| ProviderError::Parse(e.to_string()))?;
    Ok(serde_json::json!({
        "type": "additional_tools",
        "id": prefix_item_id("at", params, &payload),
        "role": "developer",
        "tools": entries,
    }))
}

/// The system prompt, as the lite shape carries it.
///
/// `developer`, not `system`: `BaseInstructionsFragment` declares that role and
/// `requires_separate_message`, so it arrives as its own item rather than being
/// folded into the first user message.
fn base_instructions_item(params: &ChatParams, instructions: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "message",
        "id": prefix_item_id("msg", params, instructions.as_bytes()),
        "role": "developer",
        "content": [{ "type": "input_text", "text": instructions }],
    })
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

/// `reasoning_replay` names the model whose stored reasoning items go back into
/// the input, or `None` to leave them out — see
/// [`OpenAIResponsesProvider::reasoning_replay`].
fn serialize_responses_input(
    messages: &[ChatMessage],
    reasoning_replay: Option<&str>,
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
            "assistant" => match reasoning_replay {
                // The identical merge `CodexProvider` does, from the one
                // implementation of it -- the interleaving rule there is subtle
                // and a second copy would be a second chance to get it wrong.
                Some(model) => input.extend(super::codex::replay_assistant_turn(m, model)),
                None => {
                    if !m.content.is_empty() {
                        input.push(assistant_message_item(
                            &m.content,
                            m.provider_state.as_ref().and_then(|s| s.responses_phase()),
                        ));
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
            },
            "tool" => {
                if let Some(ref call_id) = m.tool_call_id {
                    input.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": m.content,
                    }));
                }
            }
            "compaction" => {
                input.push(serde_json::json!({
                    "type": "compaction",
                    "encrypted_content": m.content,
                }));
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
    /// Seen on a Codex backend and **not understood**. It appears nowhere in
    /// the Codex tree, so there is no reading of it to copy; declared here
    /// rather than left to `extra` so the unknown-field warning goes back to
    /// meaning "the wire moved" instead of firing on every reply. If it ever
    /// turns out to carry something billable, this is the line to change.
    #[serde(default, rename = "attribution")]
    _attribution: IgnoredAny,
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
    /// Prompt tokens this request *put into* the cache, as distinct from the
    /// ones it read out of it.
    ///
    /// Measured on a Codex backend, and Codex reads the same field
    /// (`codex-api/src/sse/responses.rs`, `cache_write_input_tokens`). This
    /// adapter used to hardcode the figure to `None` under a comment saying the
    /// Responses API bills no premium for a cache write — which was true of the
    /// endpoints it had seen and is not a statement this struct should have
    /// been making at all. The tokens are reported; whether they cost extra is
    /// what `model_configs.cache_write_price` is for.
    cache_write_tokens: Option<i64>,
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
        // Reported where the upstream reports it, and `None` where it says
        // nothing — not a zero it never mentioned. This was hardcoded to `None`
        // on the reading that a cache write is never charged a premium here,
        // which conflated "costs no more" with "did not happen": the tokens
        // then fell into `uncached_prompt_tokens` and were billed as ordinary
        // input. The total came out the same, because `cost_of` falls back to
        // the input price for a blank `cache_write_price` — but the breakdown
        // was wrong, and the count reached neither the ledger nor the one
        // person who could decide whether this upstream needs that price set.
        cache_write_tokens: u
            .input_tokens_details
            .as_ref()
            .and_then(|d| d.cache_write_tokens)
            .map(|v| v as i32),
    }
}

/// `response.error` on a `response.failed` event.
#[derive(Deserialize)]
struct ResponseError {
    code: Option<String>,
    message: Option<String>,
    /// The other place a code arrives. OpenAI's own errors carry both — `type`
    /// is the family (`invalid_request_error`) and `code` the specific reason —
    /// and a relay routinely sends only one of them. Read as a fallback rather
    /// than merged, so a row carrying both is classified on the narrower value.
    #[serde(rename = "type")]
    error_type: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

impl ResponseError {
    fn code(&self) -> Option<&str> {
        self.code.as_deref().or(self.error_type.as_deref())
    }
}

/// What a Responses error code means for the retry that follows it.
///
/// The taxonomy is Codex's (`codex-api/src/sse/responses.rs`, `response.failed`),
/// including the part that is easy to read as an oversight and is not: **a code
/// nobody recognises is retryable**. This adapter had the opposite default —
/// every `response.failed` became a flat 400 — and 400 is the one status that
/// says "asking again cannot help", so an upstream reporting `server_is_overloaded`
/// ended the turn on its first attempt while describing itself as our mistake.
///
/// The terminal list is wider than Codex's by five codes. Codex talks to one
/// backend and can afford to assume a failure it has never seen is transient;
/// here the other end is any relay, and the cost of guessing wrong is five
/// identical requests. Every code added is one the OpenAI reference documents
/// as a client error — a parameter this build sent that the upstream will refuse
/// every time — rather than one inferred from its wording.
///
/// A status rather than an enum of our own because that is the vocabulary the
/// whole retry path already speaks: `ProviderError::Api`, `TransportError::Http`
/// and a gateway echoing `status: 503` all end up as one number read by
/// `agent::stream::is_retryable_stream_error`.
fn status_for_error_code(code: &str) -> u16 {
    match code {
        // Its own path out of the turn loop: `is_context_window_error` matches
        // on the code as well as on this status, so both spellings agree.
        "context_length_exceeded" => 413,
        // Money, in its four spellings. Not 403: the credential is fine and the
        // account is not, and a retry cannot change either.
        "insufficient_quota"
        | "credit_balance_exhausted"
        | "organization_spend_limit_exceeded"
        | "project_spend_limit_exceeded"
        | "usage_not_included" => 402,
        // A judgement about this content. The same bytes get the same answer.
        "cyber_policy" | "bio_policy" | "misalignment_policy_violation" => 400,
        // A request this build composed wrongly. Retrying sends the identical
        // body, so the only thing five attempts buy is five refusals.
        "invalid_prompt"
        | "invalid_request"
        | "invalid_request_error"
        | "unsupported_parameter"
        | "unsupported_value"
        | "model_not_found"
        | "unsupported_country_region_territory" => 400,
        "rate_limit_exceeded" | "slow_down" => 429,
        "server_is_overloaded" => 503,
        // Codex's default, and the reason this function exists.
        _ => 503,
    }
}

/// The top-level `error` stream event. Not `ResponseError`: this one carries
/// the event's own envelope (`type`, `param`, `sequence_number`), which would
/// otherwise trip the unknown-field warning on every error.
///
/// Two wire shapes exist: the standard Responses API puts `code` and `message`
/// at the top level, while the Codex backend nests them inside an `error`
/// object. Both are accepted; the nested form wins when both are present.
#[derive(Deserialize)]
struct StreamErrorEvent {
    code: Option<String>,
    message: Option<String>,
    error: Option<StreamErrorInner>,
    #[serde(default, rename = "type")]
    _type: IgnoredAny,
    #[serde(default, rename = "param")]
    _param: IgnoredAny,
    #[serde(default, rename = "sequence_number")]
    _sequence_number: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Deserialize)]
struct StreamErrorInner {
    code: Option<String>,
    message: Option<String>,
    /// As on [`ResponseError`]. Only on the *nested* object: the envelope's own
    /// `type` is the literal `"error"`, which is the event's name rather than
    /// anything about what went wrong, and reading it as a code would classify
    /// every stream error as one unknown kind.
    #[serde(rename = "type")]
    error_type: Option<String>,
}

impl StreamErrorEvent {
    fn code(&self) -> Option<&str> {
        self.error
            .as_ref()
            .and_then(|e| e.code.as_deref().or(e.error_type.as_deref()))
            .or(self.code.as_deref())
    }

    fn message(&self) -> Option<&str> {
        self.error
            .as_ref()
            .and_then(|e| e.message.as_deref())
            .or(self.message.as_deref())
    }
}

/// Every `ResponseStreamEvent` type the specification lists that this adapter
/// has nothing to do with. Kept as a list rather than a `_ =>` arm so that a
/// name absent from both this and the `match` is a *new* event — something
/// worth a warning — instead of one more thing silently dropped.
const IGNORED_EVENTS: &[&str] = &[
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
const OWN_PATH_ITEMS: &[&str] = &["message", "reasoning", "function_call", "compaction"];

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

/// Whether the request being built is a remote compaction.
///
/// A bool would have been a third argument spelled `true` at one call site and
/// `false` at the other, which is how the two came apart the first time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compaction {
    Yes,
    No,
}

/// One assistant message going back into `input`, carrying the phase the
/// upstream gave it.
///
/// `phase` is written only when there is one to write: the field is `optional`
/// on the wire, Codex's own copy is `skip_serializing_if = "Option::is_none"`,
/// and a relay old enough not to know the field is one that never sent a phase
/// to begin with — so omission is both what the specification asks for and what
/// keeps this from being a new way to get a 400.
pub(super) fn assistant_message_item(
    content: &str,
    phase: Option<super::state::ResponseMessagePhase>,
) -> serde_json::Value {
    let mut item = serde_json::json!({
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": content}],
    });
    if let Some(phase) = phase {
        item["phase"] = serde_json::json!(phase.as_wire());
    }
    item
}

/// Turn a completed `message` output item into the phase it carried.
///
/// Its own interception beside [`super::codex::reasoning_update`] and for the
/// same reason: the shared parser has no use for a message item's metadata and
/// drops it, while this is the one thing about that item the *next* request
/// needs.
pub(super) fn message_phase_update(data: &str, protocol: &str, model: &str) -> Option<StreamEvent> {
    let value: serde_json::Value = serde_json::from_str(data).ok()?;
    let item = value.get("item")?;
    if item.get("type")?.as_str()? != "message" {
        return None;
    }
    let raw = item.get("phase")?.as_str()?;
    let Some(phase) = super::state::ResponseMessagePhase::from_wire(raw) else {
        warn_unknown_once("message_phase", raw);
        return None;
    };
    Some(StreamEvent::ProviderStateUpdate {
        update: super::state::ProviderStateUpdate::ResponsesMessagePhase {
            protocol: protocol.to_string(),
            model: model.to_string(),
            phase,
        },
    })
}

/// The name of the header a Codex backend answers with, in both its spellings.
///
/// Case-insensitive, because these arrive as JSON object keys inside an event
/// rather than as a `HeaderMap` that would fold the case for us.
pub(super) const OPENAI_MODEL_HEADERS: [&str; 2] = ["openai-model", "x-openai-model"];

/// Read `openai-model` off the real HTTP response headers.
///
/// Its own function because a `HeaderMap` already folds the case, so the
/// JSON-object reader below cannot be reused for it.
pub(super) fn http_response_model(headers: &http::HeaderMap) -> Option<String> {
    OPENAI_MODEL_HEADERS
        .iter()
        .find_map(|name| headers.get(*name))
        .and_then(|value| value.to_str().ok())
        .filter(|model| !model.is_empty())
        .map(str::to_string)
}

/// Read `openai-model` out of a JSON object of headers.
fn model_from_header_object(headers: &serde_json::Value) -> Option<&str> {
    headers.as_object()?.iter().find_map(|(name, value)| {
        OPENAI_MODEL_HEADERS
            .iter()
            .any(|candidate| name.eq_ignore_ascii_case(candidate))
            .then(|| value.as_str())
            .flatten()
    })
}

/// Which model actually answered, from the places it can be said.
///
/// **The header wins over `response.model`, and that ordering is Codex's.** It
/// has a test named `process_sse_ignores_response_model_field_in_payload`: on
/// that backend the payload's `model` is the alias that was *asked for* and the
/// `openai-model` header is what actually served it — so reading the payload is
/// precisely the way to miss the substitution this column exists to catch
/// (migration 60).
///
/// The payload is kept as a fallback rather than dropped, which is where this
/// departs from Codex deliberately: Codex talks to one backend, and an ordinary
/// OpenAI-compatible endpoint sends no such header while `model` on the
/// response object is part of the documented public shape. Header first,
/// payload second, silence third.
///
/// Empty is treated as absent throughout: a relay that strips the field and one
/// that blanks it are the same fact, and an empty `response_model_id` would
/// read as a substitution to something unnamed rather than as silence.
fn response_model_event(event: &serde_json::Value) -> Option<Result<StreamEvent, ProviderError>> {
    let model = model_from_header_object(&event["response"]["headers"])
        .or_else(|| model_from_header_object(&event["headers"]))
        .or_else(|| event["response"]["model"].as_str())
        .filter(|model| !model.is_empty())?;
    Some(Ok(StreamEvent::ResponseModel {
        model: model.to_string(),
    }))
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
                    if item["type"].as_str() == Some("compaction")
                        && let Some(encrypted) = item["encrypted_content"].as_str()
                    {
                        return vec![Ok(StreamEvent::CompactionResult {
                            encrypted_content: encrypted.to_string(),
                        })];
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
        // **Which model actually answered.** `messages.response_model_id`
        // (migration 60) exists to catch a relay quietly substituting one, and
        // it was NULL for every row this adapter ever wrote: three adapters
        // emit `ResponseModel` and this was not one of them, so the whole
        // Responses API — every OpenAI, xAI, DeepSeek-on-responses and Codex
        // row — had nothing to compare against the model that was asked for.
        //
        // Read here because `response.created` is the first frame of the
        // stream, so a substitution is known before a single token arrives.
        // `response.completed` repeats it for a relay that omits the opening
        // event; the engine keeps whichever came first.
        "response.created" => match serde_json::from_str::<serde_json::Value>(data) {
            Ok(v) => response_model_event(&v).into_iter().collect(),
            Err(e) => vec![Err(ProviderError::Parse(e.to_string()))],
        },
        "response.completed" => {
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(data);
            match parsed {
                Ok(v) => {
                    let response = &v["response"];
                    let usage = read_usage(response.get("usage"));
                    let mut events = Vec::new();
                    events.extend(response_model_event(&v));
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
                    let code = error.as_ref().and_then(|e| e.code()).unwrap_or("unknown");
                    let message = error
                        .as_ref()
                        .and_then(|e| e.message.as_deref())
                        .unwrap_or("Unknown error");
                    vec![Err(ProviderError::Api {
                        status: status_for_error_code(code),
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
                    let code = error.code().unwrap_or("unknown");
                    let message = error.message().unwrap_or("Unknown error");
                    vec![Err(ProviderError::Api {
                        status: status_for_error_code(code),
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

        // **The third place the model can be said, and the first to arrive.**
        // Codex reads `openai-model` off the HTTP response headers before a
        // single frame is parsed (`codex-api/src/sse/responses.rs`), which the
        // event parser cannot do because it never sees them. Emitted ahead of
        // the stream so it wins the engine's first-one-kept rule over anything
        // the payload later claims.
        let header_model = http_response_model(&resp.headers);

        let mut state = StreamState::default();
        // Only under `CodexShape`: asking for `include` is what makes these
        // items replayable, so capturing them without it would store reasoning
        // with nothing inside it and then send it back as input the upstream
        // refuses.
        let capture_reasoning = self.reasoning_replay(&params).map(str::to_string);
        // The phase, unlike the reasoning, is captured whichever shape this is:
        // it needs nothing asked for in the request, and the upstream only ever
        // sends one if it has one. Which protocol it is stored under decides
        // whether it can share a payload with the reasoning above.
        let phase_protocol = if self.codex_shape {
            super::state::CODEX_RESPONSES_PROTOCOL
        } else {
            super::state::RESPONSES_PROTOCOL
        };
        let phase_model = params.model.clone();

        let stream = resp
            .bytes
            .map(|r| r.map_err(ProviderError::Transport))
            .eventsource()
            .flat_map(move |event| {
                let events: Vec<Result<StreamEvent, ProviderError>> = match event {
                    Ok(ev) => {
                        let mut out = Vec::new();
                        // Intercept first, then delegate: the shared parser has
                        // no interest in reasoning items or a message item's
                        // phase, and drops both.
                        if ev.event == "response.output_item.done" {
                            if let Some(model) = capture_reasoning.as_deref()
                                && let Some(captured) = super::codex::reasoning_update(&ev.data, model)
                            {
                                out.push(Ok(captured));
                            }
                            if let Some(captured) = message_phase_update(&ev.data, phase_protocol, &phase_model) {
                                out.push(Ok(captured));
                            }
                        }
                        out.extend(parse_responses_event(&ev.event, &ev.data, &mut state));
                        out
                    }
                    Err(e) => vec![Err(ProviderError::Parse(e.to_string()))],
                };
                futures::stream::iter(events)
            });

        let stream =
            futures::stream::iter(header_model.map(|model| Ok(StreamEvent::ResponseModel { model }))).chain(stream);

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

    async fn compact_remote(
        &self,
        messages: &[ChatMessage],
        params: &ChatParams,
    ) -> Result<super::RemoteCompactResult, ProviderError> {
        // The history being compacted is the same history a turn sends, so the
        // request carrying it is built by the same function -- see
        // [`Self::build_responses_request`] for what a second copy of it cost.
        // No tools: a compaction asks for a summary, not for work.
        let req = self.build_responses_request(messages, None, params, true, Compaction::Yes)?;

        let transport = ReqwestTransport::shared();
        let resp = transport.stream(req).await?;

        let mut encrypted_content: Option<String> = None;
        let mut usage: Option<super::TokenUsage> = None;
        let mut state = StreamState::default();

        let mut event_stream = resp.bytes.map(|r| r.map_err(ProviderError::Transport)).eventsource();

        while let Some(event) = event_stream.next().await {
            let ev = match event {
                Ok(ev) => ev,
                Err(e) => return Err(ProviderError::Parse(e.to_string())),
            };
            for result in parse_responses_event(&ev.event, &ev.data, &mut state) {
                match result {
                    Ok(StreamEvent::CompactionResult { encrypted_content: ec }) => {
                        encrypted_content = Some(ec);
                    }
                    Ok(StreamEvent::Stop { usage: u, .. }) => {
                        usage = u;
                    }
                    Ok(StreamEvent::Error { message }) => {
                        return Err(ProviderError::Upstream(message));
                    }
                    _ => {}
                }
            }
        }

        let encrypted = encrypted_content
            .ok_or_else(|| ProviderError::Parse("compaction response contained no compaction item".into()))?;

        Ok(super::RemoteCompactResult {
            compaction_message: ChatMessage::compaction(encrypted),
            usage: usage.unwrap_or_default(),
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
        let provider = OpenAIResponsesProvider::new("https://example.test", "k", CodexShape::off());
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

        let (_, input) = serialize_responses_input(&[message], None).expect("serialize multimodal user input");
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

        let (_, input) = serialize_responses_input(&[ChatMessage::user(&body)], None).expect("serialize file input");
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
        let (_, input) = serialize_responses_input(&[ChatMessage::user("hello")], None).expect("serialize text input");
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
        let provider = OpenAIResponsesProvider::new("https://api.x.ai/v1", "k", CodexShape::off());
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
        let provider = OpenAIResponsesProvider::new("https://api.x.ai/v1", "k", CodexShape::off());
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

    fn codex_shaped_body(mutate: impl FnOnce(&mut ChatParams)) -> (serde_json::Value, http::HeaderMap) {
        let provider = OpenAIResponsesProvider::new(
            "https://relay.invalid/v1",
            "k",
            CodexShape {
                enabled: true,
                client_version: None,
            },
        );
        let mut params = ChatParams {
            model: "gpt-5.6-sol".into(),
            cache_key: Some("conv-9".into()),
            ..Default::default()
        };
        mutate(&mut params);
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
        (body, req.headers)
    }

    /// **The switch runs both ways, and the dropped half is the easy one to
    /// forget.** A Codex backend reached through an API key answers a request
    /// carrying `temperature` — it does not refuse it — so the only evidence of
    /// getting this wrong is worse replies.
    #[test]
    fn the_codex_shape_drops_what_codex_never_sends() {
        let (body, _) = codex_shaped_body(|p| {
            p.temperature = Some(0.7);
            p.top_p = Some(0.9);
            p.max_tokens = Some(4096);
        });
        for absent in ["temperature", "top_p", "max_output_tokens"] {
            assert!(body.get(absent).is_none(), "{absent} is still on the wire: {body}");
        }

        // And with the switch off they are exactly where they were.
        let body = body_for("gpt-5.6-sol", |p| {
            p.temperature = Some(0.7);
            p.top_p = Some(0.9);
            p.max_tokens = Some(4096);
        });
        assert_eq!(body["temperature"], 0.7);
        assert_eq!(body["top_p"], 0.9);
        assert_eq!(body["max_output_tokens"], 4096);
    }

    /// The added half. `include` is the one that is load-bearing rather than
    /// cosmetic: with `store: false` and no `include`, the reasoning items come
    /// back with nothing replayable inside them.
    #[test]
    fn the_codex_shape_adds_what_codex_always_sends() {
        // The headers are their own test below; this one is about the body.
        let (body, _) = codex_shaped_body(|_| {});
        assert_eq!(body["include"], serde_json::json!(["reasoning.encrypted_content"]));
        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(body["store"], false);

        let body = body_for("gpt-5.6-sol", |_| {});
        assert!(body.get("include").is_none(), "off by default");
        assert!(body.get("parallel_tool_calls").is_none(), "off by default");
    }

    /// **The lite shape moves the tools and the system prompt into `input`.**
    /// Measured: a capture of a real Codex client on `gpt-6-astra` carried
    /// `x-openai-internal-codex-responses-lite: true`, which is Codex's
    /// `model_info.use_responses_lite`. Getting it wrong is not a refused
    /// request — the endpoint answers either way — so the only sign is that the
    /// tools and the prompt are not where the model was tuned to find them,
    /// at the very front of the cached prefix.
    #[test]
    fn a_lite_model_carries_its_tools_and_prompt_inside_the_input() {
        let provider = OpenAIResponsesProvider::new(
            "https://relay.invalid/v1",
            "k",
            CodexShape {
                enabled: true,
                client_version: None,
            },
        );
        let params = ChatParams {
            model: "gpt-6-astra".into(),
            cache_key: Some("conv-9".into()),
            responses_lite: true,
            server_tools: vec![ServerToolKind::WebSearch],
            ..Default::default()
        };
        let defs = [ToolDefinition {
            name: "read_file".into(),
            description: "read".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let messages = [
            ChatMessage {
                role: "system".into(),
                ..ChatMessage::user("you are a helpful assistant")
            },
            ChatMessage::user("hi"),
        ];
        let req = provider.build_request(&messages, Some(&defs), &params, true).unwrap();
        let Some(RequestBody::Json(body)) = req.body else {
            panic!("JSON body")
        };

        assert!(body.get("tools").is_none(), "no top-level tools under lite: {body}");
        assert!(body.get("instructions").is_none(), "no top-level instructions");
        assert_eq!(body["parallel_tool_calls"], false, "Codex forces this off under lite");

        let input = body["input"].as_array().expect("an input array");
        assert_eq!(input[0]["type"], "additional_tools");
        assert_eq!(input[0]["role"], "developer");
        assert!(
            input[0]["id"].as_str().unwrap().starts_with("at_"),
            "{}",
            input[0]["id"]
        );
        // The provider-side tool stays its own entry; ours are grouped under
        // the `functions` namespace, which is what Codex's lite builder does.
        assert_eq!(input[0]["tools"][0], serde_json::json!({"type": "web_search"}));
        assert_eq!(input[0]["tools"][1]["type"], "namespace");
        assert_eq!(input[0]["tools"][1]["name"], "functions");
        assert_eq!(input[0]["tools"][1]["tools"][0]["name"], "read_file");

        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["role"], "developer", "not `system`");
        assert_eq!(input[1]["content"][0]["text"], "you are a helpful assistant");
        assert!(input[1]["id"].as_str().unwrap().starts_with("msg_"));

        assert_eq!(input[2]["role"], "user", "the conversation follows the prefix");
        assert_eq!(
            req.headers.get("x-openai-internal-codex-responses-lite").unwrap(),
            "true"
        );
    }

    /// **`messages.response_model_id` was NULL for every row this adapter ever
    /// wrote.** The column exists (migration 60) to catch a relay quietly
    /// answering with a different model than the one asked for, and three
    /// adapters emit `ResponseModel` — this was not one of them, so the whole
    /// Responses API had nothing to compare the request against. Nothing
    /// failed; the column was simply always empty, which reads exactly like
    /// "no substitution ever happened".
    #[test]
    fn the_model_that_answered_is_reported_from_both_ends_of_the_stream() {
        let mut state = StreamState::default();
        // The opening frame, so a substitution is known before a token lands.
        let out = parse_responses_event(
            "response.created",
            r#"{"response":{"id":"resp_1","model":"gpt-6-astra-2026-09-01"}}"#,
            &mut state,
        );
        assert!(
            matches!(out.first(), Some(Ok(StreamEvent::ResponseModel { model })) if model == "gpt-6-astra-2026-09-01"),
            "{out:?}"
        );

        // **The header outranks the payload, which is the whole point.** Codex
        // has a test called `process_sse_ignores_response_model_field_in_payload`:
        // on that backend `response.model` is the alias that was asked for and
        // the header is what served it, so reading the payload is exactly how
        // the substitution gets missed. Both spellings, either nesting, any
        // case — these are JSON keys, not a `HeaderMap` that folds it for us.
        for event in [
            r#"{"response":{"model":"asked-for","headers":{"OpenAI-Model":"actually-served"}}}"#,
            r#"{"response":{"model":"asked-for"},"headers":{"x-openai-model":"actually-served"}}"#,
        ] {
            let out = parse_responses_event("response.created", event, &mut state);
            assert!(
                matches!(out.first(), Some(Ok(StreamEvent::ResponseModel { model })) if model == "actually-served"),
                "{event} produced {out:?}"
            );
        }

        // And off the real response headers, which the parser never sees — the
        // earliest of the three, so it wins the engine's first-one-kept rule.
        let mut headers = http::HeaderMap::new();
        headers.insert("openai-model", http::HeaderValue::from_static("actually-served"));
        assert_eq!(http_response_model(&headers).as_deref(), Some("actually-served"));
        assert_eq!(http_response_model(&http::HeaderMap::new()), None);

        // And again on completion, for a relay that omits the opening event.
        // The engine keeps whichever arrived first, so repeating is free.
        let out = parse_responses_event(
            "response.completed",
            r#"{"response":{"model":"gpt-6-astra-2026-09-01","usage":{"input_tokens":1,"output_tokens":1}}}"#,
            &mut state,
        );
        assert!(
            out.iter().any(
                |event| matches!(event, Ok(StreamEvent::ResponseModel { model }) if model == "gpt-6-astra-2026-09-01")
            ),
            "{out:?}"
        );

        // A relay that strips the field and one that blanks it say the same
        // thing, and neither may become an empty `response_model_id` — that
        // would read as a substitution to something unnamed.
        for silent in [r#"{"response":{"id":"resp_1"}}"#, r#"{"response":{"model":""}}"#] {
            assert!(
                parse_responses_event("response.created", silent, &mut state).is_empty(),
                "{silent}"
            );
        }
    }

    /// **Measured on a Codex backend**, which reports cache writes separately
    /// from cache reads. This adapter dropped them: the field was hardcoded to
    /// `None` under a comment saying the Responses API charges no premium for a
    /// cache write, which conflated "costs no more" with "did not happen".
    ///
    /// The cost of getting it wrong was not the total — `cost_of` bills a
    /// blank `cache_write_price` at the input rate, so the number came out the
    /// same — but the breakdown put those tokens under `input` instead of
    /// `cache`, and the count reached no ledger at all.
    #[test]
    fn a_cache_write_is_reported_where_the_upstream_reports_one() {
        let usage = read_usage(Some(&serde_json::json!({
            "input_tokens": 1000,
            "output_tokens": 50,
            "total_tokens": 1050,
            "input_tokens_details": { "cached_tokens": 800, "cache_write_tokens": 150 },
        })))
        .expect("usage");
        assert_eq!(usage.cache_read_tokens, Some(800));
        assert_eq!(usage.cache_write_tokens, Some(150));
        // The three parts of the prompt, each billed once: 1000 - 800 - 150.
        assert_eq!(usage.uncached_prompt_tokens(), 50);

        // And an upstream that says nothing still reports nothing: `None` is
        // "not mentioned", which is not the same claim as zero.
        let usage = read_usage(Some(&serde_json::json!({
            "input_tokens": 1000,
            "output_tokens": 50,
            "input_tokens_details": { "cached_tokens": 800 },
        })))
        .expect("usage");
        assert_eq!(usage.cache_write_tokens, None);
        assert_eq!(usage.uncached_prompt_tokens(), 200);
    }

    /// **Measured against the real backend**, which refused the first attempt
    /// with `unsupported_value` / `X-OpenAI-Internal-Codex-Responses-Lite
    /// requires reasoning.context to be all_turns`.
    ///
    /// The trap is that it is required even on a request that would otherwise
    /// carry no `reasoning` at all: this adapter only emitted the object when
    /// an effort was set, so a lite model with thinking off had nowhere to put
    /// `context`. Codex builds `Reasoning` unconditionally and lets its three
    /// fields skip themselves, which is why the bug does not exist there.
    #[test]
    fn a_lite_request_always_says_reasoning_covers_every_turn() {
        let provider = OpenAIResponsesProvider::new(
            "https://relay.invalid/v1",
            "k",
            CodexShape {
                enabled: true,
                client_version: None,
            },
        );
        let lite = |effort: Option<&str>| {
            let params = ChatParams {
                model: "gpt-6-astra".into(),
                responses_lite: true,
                thinking_effort: effort.map(str::to_string),
                ..Default::default()
            };
            let req = provider
                .build_request(&[ChatMessage::user("hi")], None, &params, true)
                .unwrap();
            match req.body {
                Some(RequestBody::Json(body)) => body,
                _ => panic!("JSON body"),
            }
        };

        // With an effort, beside it.
        let body = lite(Some("high"));
        assert_eq!(body["reasoning"]["context"], "all_turns");
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["reasoning"]["summary"], "auto");

        // And with none, which is the case that was refused: the object has to
        // exist for `context` to be in it.
        let body = lite(None);
        assert_eq!(body["reasoning"]["context"], "all_turns");
        assert!(body["reasoning"].get("effort").is_none(), "nothing invented beside it");
        assert!(body["reasoning"].get("summary").is_none());

        // Not on an ordinary row: the server's own default is `current_turn`,
        // and Codex omits the field rather than restating it.
        let plain = OpenAIResponsesProvider::new("https://api.openai.com/v1", "k", CodexShape::off());
        let req = plain
            .build_request(
                &[ChatMessage::user("hi")],
                None,
                &ChatParams {
                    model: "gpt-5.6-sol".into(),
                    thinking_effort: Some("high".into()),
                    ..Default::default()
                },
                true,
            )
            .unwrap();
        let Some(RequestBody::Json(body)) = req.body else {
            panic!("JSON body")
        };
        assert!(body["reasoning"].get("context").is_none());
        assert_eq!(body["reasoning"]["effort"], "high", "the old shape is untouched");
    }

    /// **The prefix ids are derived, not random.** They are rebuilt on every
    /// request, so a fresh uuid each time would put a different id at the head
    /// of the prompt every round — and the head of the prompt is what a
    /// provider caches.
    #[test]
    fn the_lite_prefix_keeps_the_same_ids_across_requests() {
        let params = ChatParams {
            model: "gpt-6-astra".into(),
            cache_key: Some("conv-9".into()),
            responses_lite: true,
            ..Default::default()
        };
        let defs = [ToolDefinition {
            name: "read_file".into(),
            description: "read".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let first = additional_tools_item(&params, Some(&defs)).unwrap();
        let second = additional_tools_item(&params, Some(&defs)).unwrap();
        assert_eq!(first["id"], second["id"], "same tools, same id");

        let other = ChatParams {
            cache_key: Some("conv-10".into()),
            ..params.clone()
        };
        assert_ne!(
            first["id"],
            additional_tools_item(&other, Some(&defs)).unwrap()["id"],
            "a different conversation is a different prefix"
        );
    }

    /// **The lite header and the lite body are one contract**, and the backend
    /// says so in as many words: measured, `unsupported_value` /
    /// `X-OpenAI-Internal-Codex-Responses-Lite requires reasoning.context to
    /// be all_turns`.
    ///
    /// Asserted over *every* request this adapter builds rather than over the
    /// one that happened to be written first. `compact_remote` used to write
    /// its own body and carried over only `include`, so it sent the header
    /// beside a body with no `reasoning.context` — every remote compaction on
    /// a lite model was a 400, and nothing in the suite was looking at the
    /// second builder at all.
    #[test]
    fn the_lite_header_never_travels_without_the_lite_body() {
        let provider = OpenAIResponsesProvider::new(
            "https://relay.invalid/v1",
            "k",
            CodexShape {
                enabled: true,
                client_version: None,
            },
        );
        let params = ChatParams {
            model: "gpt-6-astra".into(),
            responses_lite: true,
            max_tokens: Some(4096),
            ..Default::default()
        };
        let built = |compaction| {
            let req = provider
                .build_responses_request(&[ChatMessage::user("hi")], None, &params, true, compaction)
                .unwrap();
            let lite_header = req
                .headers
                .get("x-openai-internal-codex-responses-lite")
                .map(|v| v.to_str().unwrap().to_string());
            match req.body {
                Some(RequestBody::Json(body)) => (lite_header, body),
                _ => panic!("JSON body"),
            }
        };

        for compaction in [Compaction::No, Compaction::Yes] {
            let (lite_header, body) = built(compaction);
            assert_eq!(lite_header.as_deref(), Some("true"), "{compaction:?}");
            assert_eq!(body["reasoning"]["context"], "all_turns", "{compaction:?}");
            assert_eq!(
                body["input"][0]["type"], "additional_tools",
                "{compaction:?}: the lite prefix leads the input"
            );
            assert!(
                body.get("instructions").is_none(),
                "{compaction:?}: lite moves them into input"
            );
            assert_eq!(body["parallel_tool_calls"], false, "{compaction:?}");
        }
    }

    /// The trigger has to be the last input item and the lite prefix has to
    /// lead, so the two are pushed from opposite ends and cannot displace each
    /// other.
    #[test]
    fn a_compaction_trigger_ends_the_input_under_the_lite_prefix() {
        let provider = OpenAIResponsesProvider::new(
            "https://relay.invalid/v1",
            "k",
            CodexShape {
                enabled: true,
                client_version: None,
            },
        );
        let params = ChatParams {
            model: "gpt-6-astra".into(),
            responses_lite: true,
            max_tokens: Some(4096),
            ..Default::default()
        };
        let req = provider
            .build_responses_request(
                &[ChatMessage::user("one"), ChatMessage::assistant("two")],
                None,
                &params,
                true,
                Compaction::Yes,
            )
            .unwrap();
        let Some(RequestBody::Json(body)) = req.body else {
            panic!("JSON body")
        };
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "additional_tools");
        assert_eq!(input.last().unwrap()["type"], "compaction_trigger");
        assert_eq!(input.iter().filter(|i| i["type"] == "compaction_trigger").count(), 1);
        // A summary of the whole history is not a chat reply: a ceiling sized
        // for one would cut it off, and under `store: false` the truncation
        // replaces the history permanently.
        assert!(body.get("max_output_tokens").is_none());
    }

    /// The flag is the model's, but only the Codex shape can send it: an
    /// ordinary endpoint has never heard of an `additional_tools` item.
    #[test]
    fn a_lite_model_on_a_plain_row_keeps_the_ordinary_shape() {
        let provider = OpenAIResponsesProvider::new("https://api.openai.com/v1", "k", CodexShape::off());
        let params = ChatParams {
            model: "gpt-6-astra".into(),
            responses_lite: true,
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
        assert_eq!(body["tools"][0]["name"], "read_file", "still a top-level array");
        assert!(body["input"].as_array().unwrap()[0]["type"] != "additional_tools");
        assert!(req.headers.get("x-openai-internal-codex-responses-lite").is_none());
    }

    /// **The header names are pinned by spelling, because the wrong one is
    /// never refused.** An unknown header is ignored, so a request carrying
    /// `session_id` instead of `session-id` succeeds and simply stops being
    /// read — which is how this app's own `CodexProvider` went on sending a
    /// name that exists nowhere in Codex any more. Nothing but an assertion
    /// like this one can catch it.
    ///
    /// Read off Codex, not off `codex.rs`:
    /// `codex-api/src/requests/headers.rs::build_session_headers` and
    /// `codex-api/src/endpoint/responses.rs::stream_request`.
    #[test]
    fn the_codex_shape_sends_codex_own_header_names() {
        let (_, headers) = codex_shaped_body(|_| {});
        assert_eq!(
            headers.get("originator").unwrap(),
            crate::provider::codex_identity::CODEX_ORIGINATOR
        );
        assert_eq!(headers.get(http::header::ACCEPT).unwrap(), "text/event-stream");
        // Hyphens. The underscore spelling is the defect this pins.
        assert!(headers.get("session-id").is_some(), "session-id");
        assert!(
            headers.get("session_id").is_none(),
            "the dead spelling must not come back"
        );
        // The conversation, under both names Codex gives it.
        assert_eq!(headers.get("thread-id").unwrap(), "conv-9");
        assert_eq!(headers.get("x-client-request-id").unwrap(), "conv-9");
        // Codex's user-agent shape, down to the trailing terminal token.
        let ua = headers.get("user-agent").unwrap().to_str().unwrap();
        assert!(
            ua.starts_with(&format!(
                "{}/{}",
                crate::provider::codex_identity::CODEX_ORIGINATOR,
                crate::provider::codex_identity::DEFAULT_CODEX_CLIENT_VERSION
            )) && ua.ends_with(") unknown"),
            "{ua}"
        );
        // The window id Codex derives from the thread, and the beta feature it
        // announces only where remote compaction is really on.
        assert_eq!(headers.get("x-codex-window-id").unwrap(), "conv-9:0");
        assert!(headers.get("x-codex-beta-features").is_none(), "not on this model");

        // A background pass clears the cache key, and Codex sends no thread
        // headers when it has no thread -- an empty value would be worse.
        let provider = OpenAIResponsesProvider::new(
            "https://relay.invalid/v1",
            "k",
            CodexShape {
                enabled: true,
                client_version: None,
            },
        );
        let req = provider
            .build_request(
                &[ChatMessage::user("hi")],
                None,
                &ChatParams {
                    model: "gpt-5.6-sol".into(),
                    ..Default::default()
                },
                true,
            )
            .unwrap();
        assert!(req.headers.get("thread-id").is_none());
        assert!(req.headers.get("x-client-request-id").is_none());

        // And none of it on an ordinary OpenAI-compatible row.
        let plain = OpenAIResponsesProvider::new("https://api.openai.com/v1", "k", CodexShape::off());
        let req = plain
            .build_request(
                &[ChatMessage::user("hi")],
                None,
                &ChatParams {
                    model: "gpt-5.6-sol".into(),
                    cache_key: Some("conv-9".into()),
                    ..Default::default()
                },
                true,
            )
            .unwrap();
        for absent in ["originator", "session-id", "thread-id", "x-client-request-id"] {
            assert!(req.headers.get(absent).is_none(), "{absent} leaked onto a plain row");
        }
    }

    /// Reasoning goes back only under the switch, and only for the model that
    /// produced it. Stored items are encrypted and model-bound; replaying one
    /// against another model is a 400.
    #[test]
    fn reasoning_is_replayed_only_under_the_codex_shape() {
        let mut assistant = ChatMessage::assistant("thinking done");
        use crate::provider::state;
        assistant.provider_state = Some(state::ProviderState {
            version: 1,
            producer: state::ProviderStateProducer {
                vendor: "openai".into(),
                protocol: state::CODEX_RESPONSES_PROTOCOL.into(),
                model: "gpt-5.6-sol".into(),
            },
            payload: state::ProviderStatePayload::CodexReasoning {
                items: vec![state::CodexReasoningItem {
                    position: 0,
                    item_json: r#"{"type":"reasoning","id":"rs_1","encrypted_content":"gAAAA"}"#.into(),
                }],
            },
        });

        let (_, replayed) = serialize_responses_input(&[assistant.clone()], Some("gpt-5.6-sol")).unwrap();
        assert_eq!(replayed[0]["type"], "reasoning", "it goes back ahead of the prose");
        assert_eq!(replayed[0]["encrypted_content"], "gAAAA");

        let (_, plain) = serialize_responses_input(&[assistant], None).unwrap();
        assert!(
            plain.iter().all(|item| item["type"] != "reasoning"),
            "nothing is replayed with the switch off: {plain:?}"
        );
    }

    fn done_event(item: serde_json::Value) -> String {
        serde_json::json!({ "output_index": 0, "item": item }).to_string()
    }

    /// The phase is a fact the upstream states and this app hands back.
    /// OpenAI's own documentation asks for it: dropping it on a follow-up
    /// "can degrade performance" for `gpt-5.3-codex` and beyond, which is a
    /// failure with no error attached to it — the request succeeds and the
    /// answers get worse.
    #[test]
    fn a_message_items_phase_is_captured_and_sent_back() {
        use crate::provider::state;

        let captured = message_phase_update(
            &done_event(serde_json::json!({
                "type": "message",
                "role": "assistant",
                "phase": "final_answer",
                "content": [{"type": "output_text", "text": "done"}],
            })),
            state::RESPONSES_PROTOCOL,
            "gpt-6-astra",
        );
        let Some(StreamEvent::ProviderStateUpdate {
            update: state::ProviderStateUpdate::ResponsesMessagePhase { phase, protocol, model },
        }) = captured
        else {
            panic!("expected a phase update")
        };
        assert_eq!(phase, state::ResponseMessagePhase::FinalAnswer);
        assert_eq!(protocol, state::RESPONSES_PROTOCOL);
        assert_eq!(model, "gpt-6-astra");

        let mut assistant = ChatMessage::assistant("done");
        let mut acc = state::ProviderStateAccumulator::default();
        acc.apply(state::ProviderStateUpdate::ResponsesMessagePhase {
            protocol: state::RESPONSES_PROTOCOL.into(),
            model: "gpt-6-astra".into(),
            phase: state::ResponseMessagePhase::FinalAnswer,
        })
        .unwrap();
        assistant.provider_state = acc.finish();

        let (_, input) = serialize_responses_input(&[assistant], None).unwrap();
        assert_eq!(input[0]["phase"], "final_answer");
    }

    /// Absent is the common answer and its own one: a model that never sent a
    /// phase must not be handed one this app made up.
    #[test]
    fn an_unstated_phase_is_left_off_the_wire() {
        assert!(
            message_phase_update(
                &done_event(serde_json::json!({ "type": "message", "role": "assistant" })),
                crate::provider::state::RESPONSES_PROTOCOL,
                "gpt-6-astra",
            )
            .is_none()
        );
        // A label this app has not heard of is not stored either — sending it
        // back would be handing the upstream a string we cannot vouch for.
        assert!(
            message_phase_update(
                &done_event(serde_json::json!({ "type": "message", "phase": "rumination" })),
                crate::provider::state::RESPONSES_PROTOCOL,
                "gpt-6-astra",
            )
            .is_none()
        );
        // And a reasoning item is somebody else's business.
        assert!(
            message_phase_update(
                &done_event(serde_json::json!({ "type": "reasoning", "phase": "commentary" })),
                crate::provider::state::RESPONSES_PROTOCOL,
                "gpt-6-astra",
            )
            .is_none()
        );

        let (_, input) = serialize_responses_input(&[ChatMessage::assistant("plain")], None).unwrap();
        assert!(input[0].get("phase").is_none(), "{:?}", input[0]);
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

        // Unrecognised, and therefore retryable -- Codex's default, and the
        // opposite of the flat 400 this branch used to produce.
        let out = parse_responses_event(
            "error",
            r#"{"type":"error","code":"server_error","message":"x"}"#,
            &mut state,
        );
        assert!(matches!(out.first(), Some(Err(ProviderError::Api { status: 503, .. }))));
    }

    /// The failure that prompted this table. An upstream reporting itself
    /// overloaded arrived as a flat 400, which the turn loop reads as "asking
    /// again cannot help" -- so a transient outage ended the turn on its first
    /// attempt, described as a fault in our own request.
    #[test]
    fn a_failed_response_is_classified_by_its_code_rather_than_flattened() {
        let mut state = StreamState::default();
        let out = parse_responses_event(
            "response.failed",
            r#"{"response":{"error":{"code":"server_is_overloaded",
                "message":"Our servers are currently overloaded. Please try again later."}}}"#,
            &mut state,
        );
        match out.first() {
            Some(Err(ProviderError::Api { status, body })) => {
                assert_eq!(*status, 503, "an overloaded server is asked again, not blamed");
                assert!(body.starts_with("server_is_overloaded: "), "{body}");
            }
            other => panic!("expected an API error, got {other:?}"),
        }
    }

    /// Each arm of the table, and the two defaults that bracket it: a code we
    /// know to be permanent stops the turn, and one we have never seen is
    /// treated as transient.
    #[test]
    fn the_error_code_table_separates_permanent_from_transient() {
        for (code, expected) in [
            ("context_length_exceeded", 413),
            ("insufficient_quota", 402),
            ("project_spend_limit_exceeded", 402),
            ("usage_not_included", 402),
            ("cyber_policy", 400),
            ("misalignment_policy_violation", 400),
            ("invalid_prompt", 400),
            ("unsupported_parameter", 400),
            ("model_not_found", 400),
            ("rate_limit_exceeded", 429),
            ("slow_down", 429),
            ("server_is_overloaded", 503),
            ("something_nobody_here_has_seen", 503),
        ] {
            assert_eq!(status_for_error_code(code), expected, "{code}");
        }
    }

    /// OpenAI's own errors carry the family in `type` and the reason in `code`,
    /// and a relay routinely sends only one. Read from `code` where both are
    /// present, since that is the narrower answer.
    #[test]
    fn an_error_type_stands_in_for_a_missing_code() {
        let mut state = StreamState::default();
        let out = parse_responses_event(
            "response.failed",
            r#"{"response":{"error":{"type":"invalid_request_error","message":"bad"}}}"#,
            &mut state,
        );
        match out.first() {
            Some(Err(ProviderError::Api { status, body })) => {
                assert_eq!(*status, 400);
                assert!(body.starts_with("invalid_request_error: "), "{body}");
            }
            other => panic!("expected an API error, got {other:?}"),
        }
    }

    #[test]
    fn a_nested_stream_error_is_read_from_the_error_object() {
        let mut state = StreamState::default();
        let out = parse_responses_event(
            "error",
            r#"{"type":"error","error":{"code":"invalid_request","message":"Unsupported parameter: max_output_tokens"},"sequence_number":0}"#,
            &mut state,
        );
        assert_eq!(out.len(), 1);
        match &out[0] {
            Err(ProviderError::Api { status, body }) => {
                assert_eq!(*status, 400);
                assert!(body.contains("Unsupported parameter"), "{body}");
            }
            other => panic!("expected an API error, got {other:?}"),
        }
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
    fn a_compaction_item_emits_compaction_result() {
        let mut state = StreamState::default();
        let out = parse_responses_event(
            "response.output_item.done",
            r#"{"item":{"id":"cp_1","type":"compaction","encrypted_content":"gAAAAB..."}}"#,
            &mut state,
        );
        assert_eq!(out.len(), 1);
        match &out[0] {
            Ok(StreamEvent::CompactionResult { encrypted_content }) => {
                assert_eq!(encrypted_content, "gAAAAB...");
            }
            other => panic!("expected CompactionResult, got {other:?}"),
        }
    }

    #[test]
    fn compaction_message_serializes_as_compaction_input_item() {
        let msgs = [ChatMessage::compaction("gAAAAB_encrypted".into())];
        let (instructions, input) = serialize_responses_input(&msgs, None).unwrap();
        assert!(instructions.is_none());
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "compaction");
        assert_eq!(input[0]["encrypted_content"], "gAAAAB_encrypted");
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

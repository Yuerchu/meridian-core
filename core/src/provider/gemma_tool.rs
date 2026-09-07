use std::collections::HashMap;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::StreamExt;

use super::openai_compat::{ChatChunk, ChunkUsage, normalise_openai_usage, parse_openai_sse_events};
use super::{
    AgentResponse, ChatMessage, ChatParams, ChatProvider, ChatStream, ProviderError, StreamEvent, ToolCall,
    ToolDefinition,
};
use crate::client::{HttpTransport, Request, RequestBody, ReqwestTransport};

const TOOL_CALL_START: &str = "<|tool_call>";
const TOOL_CALL_END: &str = "<tool_call|>";
const QUOTE: &str = "<|\"|>";

pub struct GemmaToolProvider {
    base_url: String,
    api_key: String,
}

impl GemmaToolProvider {
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
        let tool_prompt = tools.map(format_tools_for_prompt).unwrap_or_default();
        let prepared = inject_tool_prompt(messages, &tool_prompt);

        let mut body = serde_json::json!({
            "model": params.model,
            "messages": serialize_gemma_messages(&prepared)?,
            "stream": stream,
        });
        if stream {
            body["stream_options"] = serde_json::json!({"include_usage": true});
        }
        if let Some(t) = params.temperature {
            body["temperature"] = serde_json::json!(t);
        }
        if let Some(p) = params.top_p {
            body["top_p"] = serde_json::json!(p);
        }
        if let Some(m) = params.max_tokens {
            body["max_tokens"] = serde_json::json!(m);
        }

        let mut req = Request::new(http::Method::POST, format!("{}/chat/completions", self.base_url));
        req.headers.insert(
            http::header::AUTHORIZATION,
            super::auth_header_value(&format!("Bearer {}", self.api_key)),
        );
        req.body = Some(RequestBody::Json(body));
        Ok(req)
    }
}

// --- Tool prompt injection ---

fn format_tools_for_prompt(tools: &[ToolDefinition]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n\n# Available Tools\n\n");
    for tool in tools {
        out.push_str(&format!("## {}\n{}\n", tool.name, tool.description));
        let required: Vec<String> = tool
            .parameters
            .get("required")
            .and_then(|r| r.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        if let Some(props) = tool.parameters.get("properties").and_then(|p| p.as_object())
            && !props.is_empty()
        {
            out.push_str("Parameters:\n");
            for (name, schema) in props {
                let typ = schema.get("type").and_then(|t| t.as_str()).unwrap_or("string");
                let desc = schema.get("description").and_then(|d| d.as_str()).unwrap_or("");
                let req_mark = if required.contains(name) { ", required" } else { "" };
                out.push_str(&format!("- {name} ({typ}{req_mark}): {desc}\n"));
            }
        }
        out.push('\n');
    }
    out
}

fn inject_tool_prompt(messages: &[ChatMessage], tool_prompt: &str) -> Vec<ChatMessage> {
    if tool_prompt.is_empty() {
        return messages.to_vec();
    }
    let mut result = messages.to_vec();
    if let Some(first) = result.first_mut()
        && first.role == "system"
    {
        first.content.push_str(tool_prompt);
        return result;
    }
    result.insert(
        0,
        ChatMessage {
            role: "system".into(),
            content: tool_prompt.to_string(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            tool_error: false,
            provider_state: None,
            origin: super::MessageOrigin::Assistant,
        },
    );
    result
}

// --- Message serialization ---

fn content_value(content: &str) -> Result<serde_json::Value, ProviderError> {
    if let Some(parts) = super::decode_message_parts(content).map_err(ProviderError::Parse)? {
        return serde_json::to_value(parts).map_err(|error| ProviderError::Parse(error.to_string()));
    }
    Ok(serde_json::Value::String(content.to_string()))
}

fn serialize_gemma_messages(messages: &[ChatMessage]) -> Result<Vec<serde_json::Value>, ProviderError> {
    let mut id_to_name: HashMap<String, String> = HashMap::new();
    for msg in messages {
        if let Some(ref tcs) = msg.tool_calls {
            for tc in tcs {
                id_to_name.insert(tc.id.clone(), tc.name.clone());
            }
        }
    }

    messages
        .iter()
        .map(|m| match m.role.as_str() {
            "assistant" => {
                let mut content = m.content.clone();
                if let Some(ref tcs) = m.tool_calls {
                    for tc in tcs {
                        let gemma_args = json_to_gemma_args(&tc.arguments, &tc.id)?;
                        content.push_str(&format!(
                            "\n{TOOL_CALL_START}call:{}{{{}}}{}",
                            tc.name, gemma_args, TOOL_CALL_END
                        ));
                    }
                }
                Ok(serde_json::json!({ "role": "assistant", "content": content }))
            }
            "tool" => {
                let tool_name = m
                    .tool_call_id
                    .as_ref()
                    .and_then(|id| id_to_name.get(id))
                    .map(|n| n.as_str())
                    .unwrap_or("unknown");
                let content = format!(
                    "<|tool_response>response:{tool_name}{{result:{QUOTE}{}{QUOTE}}}<tool_response|>",
                    m.content
                );
                Ok(serde_json::json!({ "role": "tool", "content": content }))
            }
            // User and system rows: chat-completions shape, so identity rides
            // the native `name` field rather than the body.
            _ => {
                let rendered =
                    super::render_message(m, super::SenderRendering::NameField).map_err(ProviderError::Parse)?;
                let mut msg = serde_json::json!({
                    "role": m.role,
                    "content": content_value(&rendered.content)?,
                });
                if let Some(ref name) = rendered.name {
                    msg["name"] = serde_json::json!(name);
                }
                Ok(msg)
            }
        })
        .collect()
}

// --- Gemma format parsing ---

fn parse_gemma_call(content: &str) -> Option<(String, String)> {
    let rest = content.trim().strip_prefix("call:")?;
    let brace_start = rest.find('{')?;
    let name = rest[..brace_start].to_string();
    let brace_end = rest.rfind('}')?;
    if brace_end <= brace_start {
        return None;
    }
    let args_raw = &rest[brace_start + 1..brace_end];
    Some((name, parse_gemma_args(args_raw)))
}

fn parse_gemma_args(raw: &str) -> String {
    if raw.is_empty() {
        return "{}".to_string();
    }
    let mut result = serde_json::Map::new();
    for pair in split_on_comma(raw) {
        let pair = pair.trim().to_string();
        if let Some(colon) = pair.find(':') {
            let key = pair[..colon].trim();
            let value = pair[colon + 1..].trim();
            if !key.is_empty() {
                result.insert(key.to_string(), parse_gemma_value(value));
            }
        }
    }
    serde_json::to_string(&serde_json::Value::Object(result)).unwrap_or_else(|_| "{}".to_string())
}

fn parse_gemma_value(raw: &str) -> serde_json::Value {
    if raw.starts_with(QUOTE)
        && let Some(end) = raw[QUOTE.len()..].find(QUOTE)
    {
        let inner = &raw[QUOTE.len()..QUOTE.len() + end];
        return serde_json::Value::String(inner.to_string());
    }
    if raw == "true" {
        return serde_json::Value::Bool(true);
    }
    if raw == "false" {
        return serde_json::Value::Bool(false);
    }
    if raw == "null" {
        return serde_json::Value::Null;
    }
    if let Ok(n) = raw.parse::<i64>() {
        return serde_json::json!(n);
    }
    if let Ok(n) = raw.parse::<f64>() {
        return serde_json::json!(n);
    }
    if raw.starts_with('[') && raw.ends_with(']') {
        let inner = &raw[1..raw.len() - 1];
        let arr: Vec<serde_json::Value> = split_on_comma(inner)
            .iter()
            .map(|e| parse_gemma_value(e.trim()))
            .collect();
        return serde_json::Value::Array(arr);
    }
    serde_json::Value::String(raw.to_string())
}

fn split_on_comma(raw: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    let mut bracket_depth: i32 = 0;
    let mut i = 0;
    while i < raw.len() {
        if raw[i..].starts_with(QUOTE) {
            in_string = !in_string;
            current.push_str(QUOTE);
            i += QUOTE.len();
            continue;
        }
        let ch = match raw[i..].chars().next() {
            Some(c) => c,
            None => break,
        };
        let ch_len = ch.len_utf8();
        if !in_string {
            match ch {
                '[' => bracket_depth += 1,
                ']' => bracket_depth -= 1,
                ',' if bracket_depth == 0 => {
                    parts.push(std::mem::take(&mut current));
                    i += ch_len;
                    continue;
                }
                _ => {}
            }
        }
        current.push(ch);
        i += ch_len;
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

// --- JSON to Gemma format ---

fn json_to_gemma_args(json: &str, call_id: &str) -> Result<String, ProviderError> {
    let obj = super::decode_tool_arguments(json, call_id).map_err(ProviderError::Parse)?;
    let map = obj.as_object().expect("decode_tool_arguments returns an object");
    Ok(map
        .iter()
        .map(|(k, v)| format!("{}:{}", k, value_to_gemma(v)))
        .collect::<Vec<_>>()
        .join(","))
}

fn value_to_gemma(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => format!("{QUOTE}{s}{QUOTE}"),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Array(arr) => {
            let elements: Vec<String> = arr.iter().map(value_to_gemma).collect();
            format!("[{}]", elements.join(","))
        }
        serde_json::Value::Object(_) => {
            let s = serde_json::to_string(v).expect("a JSON value is serializable");
            format!("{QUOTE}{s}{QUOTE}")
        }
    }
}

// --- Stream adapter: intercept tool call tokens from text ---

/// Length of the longest suffix of `haystack` that is also a prefix of `needle`.
fn longest_suffix_prefix_len(haystack: &str, needle: &str) -> usize {
    let max = haystack.len().min(needle.len());
    for len in (1..=max).rev() {
        if haystack.is_char_boundary(haystack.len() - len)
            && needle.is_char_boundary(len)
            && haystack[haystack.len() - len..] == needle[..len]
        {
            return len;
        }
    }
    0
}

struct GemmaParseState {
    in_tool_call: bool,
    tool_call_buffer: String,
    tool_index: usize,
    /// Trailing bytes of the last chunk that may start a tool-call marker split
    /// across a chunk boundary; prepended to the next chunk before scanning.
    pending: String,
}

impl GemmaParseState {
    fn new() -> Self {
        Self {
            in_tool_call: false,
            tool_call_buffer: String::new(),
            tool_index: 0,
            pending: String::new(),
        }
    }

    fn process_text(&mut self, text: &str) -> Vec<StreamEvent> {
        if self.in_tool_call {
            self.tool_call_buffer.push_str(text);
            return self.try_complete_tool_call();
        }
        // Prepend any partial marker held back from the previous chunk.
        let combined = if self.pending.is_empty() {
            text.to_string()
        } else {
            format!("{}{}", std::mem::take(&mut self.pending), text)
        };
        if let Some(pos) = combined.find(TOOL_CALL_START) {
            let before = &combined[..pos];
            let after = &combined[pos + TOOL_CALL_START.len()..];
            let mut events = Vec::new();
            if !before.is_empty() {
                events.push(StreamEvent::Text {
                    content: before.to_string(),
                });
            }
            self.in_tool_call = true;
            self.tool_call_buffer = after.to_string();
            events.extend(self.try_complete_tool_call());
            events
        } else {
            // Hold back a trailing partial marker so it isn't emitted as text and
            // lost when the marker spans a chunk boundary.
            let hold = longest_suffix_prefix_len(&combined, TOOL_CALL_START);
            let split = combined.len() - hold;
            self.pending = combined[split..].to_string();
            let emit = &combined[..split];
            if emit.is_empty() {
                vec![]
            } else {
                vec![StreamEvent::Text {
                    content: emit.to_string(),
                }]
            }
        }
    }

    fn try_complete_tool_call(&mut self) -> Vec<StreamEvent> {
        let Some(end_pos) = self.tool_call_buffer.find(TOOL_CALL_END) else {
            return vec![];
        };
        let call_content = self.tool_call_buffer[..end_pos].to_string();
        let after = self.tool_call_buffer[end_pos + TOOL_CALL_END.len()..].to_string();

        self.in_tool_call = false;
        self.tool_call_buffer.clear();

        let mut events = Vec::new();
        if let Some((name, args_json)) = parse_gemma_call(&call_content) {
            let id = uuid::Uuid::new_v4().to_string();
            events.push(StreamEvent::ToolCallStart {
                index: self.tool_index,
                id,
                name,
            });
            events.push(StreamEvent::ToolCallDelta {
                index: self.tool_index,
                arguments: args_json,
            });
            self.tool_index += 1;
        }
        if !after.is_empty() {
            events.extend(self.process_text(&after));
        }
        events
    }

    fn flush(&mut self) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if self.in_tool_call && !self.tool_call_buffer.is_empty() {
            self.in_tool_call = false;
            events.push(StreamEvent::Text {
                content: format!("{TOOL_CALL_START}{}", std::mem::take(&mut self.tool_call_buffer)),
            });
        }
        // A held-back partial marker that never completed is just text.
        if !self.pending.is_empty() {
            events.push(StreamEvent::Text {
                content: std::mem::take(&mut self.pending),
            });
        }
        events
    }
}

fn adapt_gemma_stream(inner: ChatStream) -> ChatStream {
    let (tx, rx) = futures::channel::mpsc::unbounded();

    tokio::spawn(async move {
        let mut state = GemmaParseState::new();
        let mut inner = inner;

        while let Some(event) = inner.next().await {
            let out = match event {
                Ok(StreamEvent::Text { content }) => state.process_text(&content).into_iter().map(Ok).collect(),
                Ok(StreamEvent::Stop { reason, usage }) => {
                    let mut evs: Vec<Result<StreamEvent, ProviderError>> = state.flush().into_iter().map(Ok).collect();
                    evs.push(Ok(StreamEvent::Stop { reason, usage }));
                    evs
                }
                other => vec![other],
            };
            for e in out {
                if tx.unbounded_send(e).is_err() {
                    return;
                }
            }
        }
        for e in state.flush() {
            let _ = tx.unbounded_send(Ok(e));
        }
    });

    Box::pin(rx)
}

// Reached only from `chat_with_tools`, the non-streaming trait path nothing
// drives yet; its tests are live.
#[allow(dead_code)]
fn extract_tool_calls_from_text(text: &str) -> (String, Vec<ToolCall>) {
    let mut clean = String::new();
    let mut calls = Vec::new();
    let mut remaining = text;

    while let Some(start) = remaining.find(TOOL_CALL_START) {
        clean.push_str(&remaining[..start]);
        remaining = &remaining[start + TOOL_CALL_START.len()..];
        if let Some(end) = remaining.find(TOOL_CALL_END) {
            if let Some((name, args)) = parse_gemma_call(&remaining[..end]) {
                calls.push(ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name,
                    arguments: args,
                });
            }
            remaining = &remaining[end + TOOL_CALL_END.len()..];
        } else {
            clean.push_str(TOOL_CALL_START);
        }
    }
    clean.push_str(remaining);
    (clean.trim().to_string(), calls)
}

// --- ChatProvider impl ---

#[async_trait]
impl ChatProvider for GemmaToolProvider {
    #[cfg(test)]
    fn adapter_name(&self) -> &'static str {
        "GemmaToolProvider"
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

        let raw_stream: ChatStream = Box::pin(
            resp.bytes
                .map(|r| r.map_err(ProviderError::Transport))
                .eventsource()
                .flat_map(move |event| {
                    let events: Vec<Result<StreamEvent, ProviderError>> = match event {
                        Ok(ev) => {
                            if ev.data == "[DONE]" {
                                return futures::stream::iter(vec![]);
                            }
                            match serde_json::from_str::<ChatChunk>(&ev.data) {
                                Ok(chunk) => {
                                    let (mut stream_events, finish_reason, usage) = parse_openai_sse_events(&chunk);
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
                                Err(e) => vec![Err(ProviderError::Parse(e.to_string()))],
                            }
                        }
                        Err(e) => vec![Err(ProviderError::Parse(e.to_string()))],
                    };
                    futures::stream::iter(events)
                }),
        );

        if tools.is_empty() {
            Ok(raw_stream)
        } else {
            Ok(adapt_gemma_stream(raw_stream))
        }
    }

    async fn chat(&self, messages: Vec<ChatMessage>, params: ChatParams) -> Result<String, ProviderError> {
        let transport = ReqwestTransport::shared();
        let req = self.build_request(&messages, None, &params, false)?;
        let resp = transport.execute(req).await?;

        let parsed: serde_json::Value =
            serde_json::from_slice(&resp.body).map_err(|e| ProviderError::Parse(e.to_string()))?;

        parsed["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| s.to_string())
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

        let parsed: serde_json::Value =
            serde_json::from_slice(&resp.body).map_err(|e| ProviderError::Parse(e.to_string()))?;

        let raw_content = parsed["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();

        let (text, tool_calls) = extract_tool_calls_from_text(&raw_content);

        // Through the shared struct, which is how this path picks up the cache
        // fields it never parsed: self-hosted vLLM and SGLang endpoints emit
        // `prompt_tokens_details` too, and the hand-written mapping that used to
        // be here dropped it along with everything else it did not name.
        let usage = parsed
            .get("usage")
            .and_then(|u| serde_json::from_value::<ChunkUsage>(u.clone()).ok())
            .map(|u| normalise_openai_usage(&u));

        Ok(AgentResponse {
            text,
            reasoning_content: None,
            tool_calls,
            usage,
            provider_state: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_partial_marker_held_back_across_chunks() {
        let mut state = GemmaParseState::new();
        let split = TOOL_CALL_START.len() / 2;
        let head = &TOOL_CALL_START[..split];
        let tail = &TOOL_CALL_START[split..];

        // Chunk 1 ends mid-marker: only clean text is emitted, partial held back.
        let ev1 = state.process_text(&format!("abc{head}"));
        let text1: String = ev1
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Text { content } => Some(content.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text1, "abc");

        // Chunk 2 completes the marker: we enter tool-call mode, marker not leaked.
        let ev2 = state.process_text(tail);
        assert!(state.in_tool_call);
        let text2: String = ev2
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Text { content } => Some(content.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text2, "");
    }

    #[test]
    fn test_parse_gemma_call_no_args() {
        let (name, args) = parse_gemma_call("call:list_memories{}").unwrap();
        assert_eq!(name, "list_memories");
        assert_eq!(args, "{}");
    }

    #[test]
    fn test_parse_gemma_call_string_arg() {
        let (name, args) = parse_gemma_call("call:get_weather{city:<|\"|>\u{5317}\u{4eac}<|\"|>}").unwrap();
        assert_eq!(name, "get_weather");
        let v: serde_json::Value = serde_json::from_str(&args).unwrap();
        assert_eq!(v["city"], "\u{5317}\u{4eac}");
    }

    #[test]
    fn test_parse_gemma_call_mixed_args() {
        let input = "call:web_search{query:<|\"|>Python latest version<|\"|>,limit:10}";
        let (name, args) = parse_gemma_call(input).unwrap();
        assert_eq!(name, "web_search");
        let v: serde_json::Value = serde_json::from_str(&args).unwrap();
        assert_eq!(v["query"], "Python latest version");
        assert_eq!(v["limit"], 10);
    }

    #[test]
    fn test_parse_gemma_call_boolean() {
        let input = "call:set_alarm{time:<|\"|>08:00<|\"|>,repeat:true}";
        let (name, args) = parse_gemma_call(input).unwrap();
        assert_eq!(name, "set_alarm");
        let v: serde_json::Value = serde_json::from_str(&args).unwrap();
        assert_eq!(v["time"], "08:00");
        assert_eq!(v["repeat"], true);
    }

    #[test]
    fn test_parse_gemma_call_array() {
        let input = "call:send_message{targets:[<|\"|>user_a<|\"|>,<|\"|>user_b<|\"|>],text:<|\"|>hello<|\"|>}";
        let (name, args) = parse_gemma_call(input).unwrap();
        assert_eq!(name, "send_message");
        let v: serde_json::Value = serde_json::from_str(&args).unwrap();
        assert_eq!(v["targets"], serde_json::json!(["user_a", "user_b"]));
        assert_eq!(v["text"], "hello");
    }

    #[test]
    fn test_json_to_gemma_roundtrip() {
        let json = r#"{"city":"北京","limit":10,"verbose":true}"#;
        let gemma = json_to_gemma_args(json, "call-1").unwrap();
        let back = parse_gemma_args(&gemma);
        let original: serde_json::Value = serde_json::from_str(json).unwrap();
        let restored: serde_json::Value = serde_json::from_str(&back).unwrap();
        assert_eq!(original, restored);
    }

    #[test]
    fn test_extract_tool_calls_from_text() {
        let text = "我帮你查一下天气。\n<|tool_call>call:get_weather{city:<|\"|>北京<|\"|>}<tool_call|>";
        let (clean, calls) = extract_tool_calls_from_text(text);
        assert_eq!(clean, "我帮你查一下天气。");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get_weather");
        let v: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(v["city"], "北京");
    }

    #[test]
    fn test_stream_state_normal_text() {
        let mut state = GemmaParseState::new();
        let events = state.process_text("hello world");
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::Text { content } if content == "hello world"));
    }

    #[test]
    fn test_stream_state_tool_call_single_chunk() {
        let mut state = GemmaParseState::new();
        let events = state.process_text("说明文字<|tool_call>call:test{key:<|\"|>val<|\"|>}<tool_call|>");
        assert!(events.len() >= 3);
        assert!(matches!(&events[0], StreamEvent::Text { content } if content == "说明文字"));
        assert!(matches!(&events[1], StreamEvent::ToolCallStart { name, .. } if name == "test"));
    }

    #[test]
    fn test_stream_state_tool_call_multi_chunk() {
        let mut state = GemmaParseState::new();

        let e1 = state.process_text("<|tool_call>");
        assert!(e1.is_empty());

        let e2 = state.process_text("call:foo{");
        assert!(e2.is_empty());

        let e3 = state.process_text("}<tool_call|>");
        assert!(e3.len() >= 2);
        assert!(matches!(&e3[0], StreamEvent::ToolCallStart { name, .. } if name == "foo"));
    }
}

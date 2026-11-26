//! Google Gemini OpenAI-compatible Chat Completions extension over
//! [`OpenAIChatProvider`].
//!
//! Handles Google's `google.thinking_config`, Gemini 3+ thought signature
//! replay, and extracts Google's `extra_content` from [`OpenAIChatDeltaResponse`]
//! / [`OpenAIChatMessageResponse`] via `extra.remove("extra_content")` so the
//! OpenAI base structs stay completely vendor-free while unknown fields still
//! trigger `warn_extra_fields`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::dto::{ExtraIgnore, warn_extra_fields};
use crate::mixins::ToolImageCarryMixin;
use crate::protocols::google_generate_content::model_requires_tool_thought_signature;
use crate::protocols::openai_chat::{
    OpenAIChatChunkResponse, OpenAIChatExtension, OpenAIChatMessageResponse, OpenAIChatProvider, content_value,
};
use crate::state::{
    GOOGLE_OPENAI_CHAT_PROTOCOL, GoogleSignatureLocation, ProviderState, ProviderStateAccumulator, ProviderStateUpdate,
};
use crate::{ChatMessage, ChatParams, ProviderError, StreamEvent, ThinkingStyle};

/// Preference key selecting how the Google OpenAI-compatible adapter encodes
/// thinking parameters (`auto`, `google_thinking_config`, or `reasoning_effort`).
pub const GOOGLE_COMPAT_THINKING_MODE_PREF: &str = "google.compat_thinking_mode";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GoogleCompatThinkingMode {
    #[default]
    Auto,
    GoogleThinkingConfig,
    ReasoningEffort,
}

impl GoogleCompatThinkingMode {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim() {
            "auto" => Ok(Self::Auto),
            "google_thinking_config" => Ok(Self::GoogleThinkingConfig),
            "reasoning_effort" => Ok(Self::ReasoningEffort),
            other => Err(format!("unknown google compat thinking mode `{other}`")),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::GoogleThinkingConfig => "google_thinking_config",
            Self::ReasoningEffort => "reasoning_effort",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleExtraContentResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub google: Option<GoogleThoughtSignatureResponse>,
    #[serde(default, flatten)]
    pub extra: ExtraIgnore,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleThoughtSignatureResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
    #[serde(default, flatten)]
    pub extra: ExtraIgnore,
}

impl GoogleExtraContentResponse {
    pub fn thought_signature(&self) -> Option<&str> {
        self.google
            .as_ref()?
            .thought_signature
            .as_deref()
            .filter(|s| !s.is_empty())
    }

    pub fn warn_ignored_fields(&self) {
        warn_extra_fields("google_extra_content", &self.extra);
        if let Some(google) = &self.google {
            warn_extra_fields("google_thought_signature", &google.extra);
        }
    }
}

fn take_google_extra_content(extra: &mut ExtraIgnore) -> Option<GoogleExtraContentResponse> {
    let raw = extra.remove("extra_content")?;
    if raw.is_null() {
        return None;
    }
    match serde_json::from_value::<GoogleExtraContentResponse>(raw) {
        Ok(parsed) => {
            parsed.warn_ignored_fields();
            Some(parsed)
        }
        Err(error) => {
            tracing::warn!(error = %error, "failed to decode Google extra_content payload");
            None
        }
    }
}

fn google_extra_value(signature: &str) -> Value {
    serde_json::to_value(GoogleExtraContentResponse {
        google: Some(GoogleThoughtSignatureResponse {
            thought_signature: Some(signature.to_string()),
            extra: ExtraIgnore::default(),
        }),
        extra: ExtraIgnore::default(),
    })
    .expect("GoogleExtraContentResponse is serializable")
}

fn is_google_api_host(base_url: &str) -> bool {
    base_url
        .parse::<http::Uri>()
        .ok()
        .and_then(|uri| {
            uri.host().map(|host| {
                host.eq_ignore_ascii_case("generativelanguage.googleapis.com")
                    || host.eq_ignore_ascii_case("aiplatform.googleapis.com")
            })
        })
        .unwrap_or(false)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct GoogleOpenAIChatExtension {
    pub thinking_mode: GoogleCompatThinkingMode,
}

impl OpenAIChatExtension for GoogleOpenAIChatExtension {
    fn adapter_name(&self) -> &'static str {
        "GoogleOpenAIChatProvider"
    }

    fn serialize_messages(&self, messages: &[ChatMessage], model: &str) -> Result<Vec<Value>, ProviderError> {
        serialize_google_messages(messages, model)
    }

    fn customize_request(
        &self,
        base_url: &str,
        params: &ChatParams,
        body: &mut Map<String, Value>,
        _headers: &mut http::HeaderMap,
    ) -> Result<(), ProviderError> {
        if params.thinking_style != ThinkingStyle::None {
            let use_google_thinking_config = match self.thinking_mode {
                GoogleCompatThinkingMode::Auto => is_google_api_host(base_url),
                GoogleCompatThinkingMode::GoogleThinkingConfig => true,
                GoogleCompatThinkingMode::ReasoningEffort => false,
            };
            if use_google_thinking_config {
                if model_requires_tool_thought_signature(&params.model) {
                    let mut thinking_config = Map::new();
                    if params.thinking_enabled {
                        thinking_config.insert("include_thoughts".into(), Value::Bool(true));
                    }
                    if let Some(ref effort) = params.thinking_effort {
                        thinking_config.insert("thinking_level".into(), serde_json::json!(effort));
                    }
                    if !thinking_config.is_empty() {
                        body.insert(
                            "google".into(),
                            serde_json::json!({
                                "thinking_config": Value::Object(thinking_config)
                            }),
                        );
                    }
                } else if let Some(ref effort) = params.thinking_effort {
                    body.insert("reasoning_effort".into(), serde_json::json!(effort));
                } else if params.thinking_enabled {
                    body.insert(
                        "google".into(),
                        serde_json::json!({
                            "thinking_config": { "include_thoughts": true }
                        }),
                    );
                }
            } else if let Some(ref effort) = params.thinking_effort {
                body.insert("reasoning_effort".into(), serde_json::json!(effort));
            }
        }
        if let Some(ref verbosity) = params.verbosity {
            body.insert("verbosity".into(), serde_json::json!(verbosity));
        }
        Ok(())
    }

    fn extract_chunk_state_events(&self, chunk: &mut OpenAIChatChunkResponse, model: &str) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        let Some(choice) = chunk.choices.first_mut() else {
            return events;
        };
        let Some(delta) = choice.delta.as_mut() else {
            return events;
        };
        if let Some(extra_content) = take_google_extra_content(&mut delta.extra)
            && let Some(signature) = extra_content.thought_signature()
        {
            events.push(StreamEvent::ProviderStateUpdate {
                update: ProviderStateUpdate::GoogleThoughtSignatureDelta {
                    protocol: GOOGLE_OPENAI_CHAT_PROTOCOL.into(),
                    model: model.to_string(),
                    location: GoogleSignatureLocation::Message,
                    delta: signature.to_string(),
                },
            });
        }
        if let Some(tool_calls) = delta.tool_calls.as_mut() {
            for tc in tool_calls {
                if let Some(extra_content) = take_google_extra_content(&mut tc.extra)
                    && let Some(signature) = extra_content.thought_signature()
                {
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
            }
        }
        events
    }

    fn extract_response_state(
        &self,
        message: &mut OpenAIChatMessageResponse,
        model: &str,
    ) -> Result<Option<ProviderState>, ProviderError> {
        let mut state = ProviderStateAccumulator::default();
        if let Some(extra_content) = take_google_extra_content(&mut message.extra)
            && let Some(signature) = extra_content.thought_signature()
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
        if let Some(tool_calls) = message.tool_calls.as_mut() {
            for (index, call) in tool_calls.iter_mut().enumerate() {
                let Some(extra_content) = take_google_extra_content(&mut call.extra) else {
                    continue;
                };
                if let Some(signature) = extra_content.thought_signature() {
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
}

/// Concrete Google OpenAI-compatible provider composed over [`OpenAIChatProvider`].
pub struct GoogleOpenAIChatProvider;

impl GoogleOpenAIChatProvider {
    pub fn build(base_url: &str, api_key: &str, thinking_mode: GoogleCompatThinkingMode) -> OpenAIChatProvider {
        OpenAIChatProvider::with_extension(base_url, api_key, Arc::new(GoogleOpenAIChatExtension { thinking_mode }))
    }
}

pub fn serialize_google_messages(messages: &[ChatMessage], model: &str) -> Result<Vec<Value>, ProviderError> {
    let mut flattened_calls = HashSet::<String>::new();
    let mut signed_tool_names = HashMap::<String, String>::new();
    let mut interrupted_results = Vec::<(String, String)>::new();
    let mut carry = ToolImageCarryMixin::default();
    let mut out = Vec::new();

    for (message_index, m) in messages.iter().enumerate() {
        if m.role != "tool" {
            append_interrupted_results(&mut out, std::mem::take(&mut interrupted_results));
            carry.flush(&mut out);
            flattened_calls.clear();
            signed_tool_names.clear();
        }
        if m.role == "assistant"
            && let Some(tool_calls) = m.tool_calls.as_ref()
        {
            for call in tool_calls {
                crate::decode_tool_arguments(&call.arguments, &call.id).map_err(ProviderError::Parse)?;
            }
            let signatures = m
                .provider_state
                .as_ref()
                .and_then(|s| s.google_signatures_for(GOOGLE_OPENAI_CHAT_PROTOCOL, model));
            let has_tool_signature = signatures.is_some_and(|items| {
                items
                    .iter()
                    .any(|item| matches!(item.location, GoogleSignatureLocation::ToolCall { .. }))
            });
            if model_requires_tool_thought_signature(model) && !has_tool_signature {
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
            let text = carry.take(m).map_err(ProviderError::Parse)?;
            out.push(serde_json::json!({
                "role": "user",
                "content": format!(
                    "[Historical tool result for {}]\n{}",
                    m.tool_call_id.as_deref().unwrap_or("unknown"),
                    text.as_deref().unwrap_or(&m.content)
                )
            }));
            continue;
        }

        let rendered = crate::render_message(m, crate::SenderRendering::NameField).map_err(ProviderError::Parse)?;
        let content = match carry.take(m).map_err(ProviderError::Parse)? {
            Some(text) => Value::String(text),
            None => content_value(&m.role, &rendered.content)?,
        };
        let mut msg = serde_json::json!({ "role": m.role, "content": content });
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
            msg["extra_content"] = google_extra_value(&message_signature.signature);
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
                    wire["extra_content"] = google_extra_value(&signature.signature);
                }
                wire_calls.push(wire);
            }
            msg["tool_calls"] = Value::Array(wire_calls);
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
    carry.flush(&mut out);
    Ok(out)
}

fn append_interrupted_results(out: &mut Vec<Value>, results: Vec<(String, String)>) {
    out.extend(results.into_iter().map(|(id, name)| {
        serde_json::json!({
            "role": "tool",
            "name": name,
            "tool_call_id": id,
            "content": "Tool execution was interrupted before a result was recorded. It was not retried."
        })
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolCall;
    use crate::protocols::openai_chat::{ChatChunk, parse_openai_sse_events};
    use crate::state::{GoogleThoughtSignature, ProviderStatePayload, ProviderStateProducer};
    use meridian_provider_sdk::client::RequestBody;

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
    fn google_request_uses_top_level_google_config_on_official_host_and_effort_on_relay() {
        let params = ChatParams {
            model: "gemini-3.7-flash".into(),
            thinking_enabled: true,
            thinking_style: ThinkingStyle::EffortOnly,
            thinking_effort: Some("high".into()),
            ..Default::default()
        };

        let official = GoogleOpenAIChatProvider::build(
            "https://generativelanguage.googleapis.com/v1beta/openai",
            "key",
            GoogleCompatThinkingMode::Auto,
        );
        let req = official
            .build_request(&[ChatMessage::user("hello")], None, &params, true)
            .unwrap();
        let Some(RequestBody::Json(body)) = req.body else {
            panic!("JSON body")
        };
        assert!(body.get("extra_body").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(
            body["google"]["thinking_config"],
            serde_json::json!({ "include_thoughts": true, "thinking_level": "high" })
        );

        let relay = GoogleOpenAIChatProvider::build("https://relay.example/v1", "key", GoogleCompatThinkingMode::Auto);
        let req = relay
            .build_request(&[ChatMessage::user("hello")], None, &params, true)
            .unwrap();
        let Some(RequestBody::Json(body)) = req.body else {
            panic!("JSON body")
        };
        assert!(body.get("extra_body").is_none());
        assert!(body.get("google").is_none());
        assert_eq!(body["reasoning_effort"], "high");

        let forced_google = GoogleOpenAIChatProvider::build(
            "https://relay.example/v1",
            "key",
            GoogleCompatThinkingMode::GoogleThinkingConfig,
        );
        let req = forced_google
            .build_request(&[ChatMessage::user("hello")], None, &params, true)
            .unwrap();
        let Some(RequestBody::Json(body)) = req.body else {
            panic!("JSON body")
        };
        assert_eq!(
            body["google"]["thinking_config"],
            serde_json::json!({ "include_thoughts": true, "thinking_level": "high" })
        );

        let forced_effort = GoogleOpenAIChatProvider::build(
            "https://generativelanguage.googleapis.com/v1beta/openai",
            "key",
            GoogleCompatThinkingMode::ReasoningEffort,
        );
        let req = forced_effort
            .build_request(&[ChatMessage::user("hello")], None, &params, true)
            .unwrap();
        let Some(RequestBody::Json(body)) = req.body else {
            panic!("JSON body")
        };
        assert!(body.get("google").is_none());
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn google_compat_honours_sampling_and_non_thinking_params() {
        let provider = GoogleOpenAIChatProvider::build(
            "https://generativelanguage.googleapis.com/v1beta/openai",
            "key",
            GoogleCompatThinkingMode::Auto,
        );
        let params = ChatParams {
            model: "gemini-2.5-flash".into(),
            thinking_enabled: false,
            thinking_style: ThinkingStyle::None,
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
        assert_eq!(body["temperature"], 0.8);
        assert_eq!(body["top_p"], 0.9);
        assert!(body.get("google").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn gemini_25_unsigned_tool_calls_stay_native_in_openai_compat() {
        let assistant = ChatMessage::assistant_with_tools(
            "",
            None,
            vec![ToolCall {
                id: "call-25".into(),
                name: "search".into(),
                arguments: r#"{"q":"rust"}"#.into(),
            }],
        );
        let wire = serialize_google_messages(
            &[assistant, ChatMessage::tool_result("call-25", "done")],
            "gemini-2.5-flash",
        )
        .unwrap();
        assert_eq!(wire[0]["role"], "assistant");
        assert_eq!(wire[0]["tool_calls"][0]["function"]["name"], "search");
        assert_eq!(wire[1]["role"], "tool");
        assert_eq!(wire[1]["tool_call_id"], "call-25");
        assert_eq!(wire[1]["name"], "search");
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
    fn final_empty_chunk_still_emits_message_signature_and_keeps_unknown_extra_fields() {
        let mut chunk: ChatChunk = serde_json::from_value(serde_json::json!({
            "choices": [{
                "delta": {
                    "content": "",
                    "extra_content": {"google": {"thought_signature": "tail-signature"}},
                    "unknown_relay_field": "should_stay_for_warn"
                },
                "finish_reason": "stop"
            }]
        }))
        .unwrap();
        let ext = GoogleOpenAIChatExtension::default();
        let state_events = ext.extract_chunk_state_events(&mut chunk, "gemini-3.7-flash");
        assert!(
            state_events
                .iter()
                .any(|event| matches!(event, StreamEvent::ProviderStateUpdate { .. }))
        );
        let delta = chunk.choices[0].delta.as_ref().unwrap();
        assert!(!delta.extra.contains_key("extra_content"), "claimed field removed");
        assert!(
            delta.extra.contains_key("unknown_relay_field"),
            "unknown fields must remain so warn_extra_fields still catches drift"
        );
        let (_, finish, _) = parse_openai_sse_events(&chunk);
        assert_eq!(finish.as_deref(), Some("stop"));
    }
}

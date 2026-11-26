//! DeepSeek Chat Completions extension over [`OpenAIChatProvider`].
//!
//! Composes the standard OpenAI Chat Completions protocol body with DeepSeek's
//! documented extensions:
//! - Omits `OpenAISamplingParamsMixin` (`temperature`/`top_p` are unused/ignored
//!   on DeepSeek reasoner).
//! - Replays `reasoning_content` only on assistant messages with `tool_calls`.
//! - Sends `thinking: { type: "disabled" }` when thinking is turned off.

use std::sync::Arc;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::mixins::ToolImageCarryMixin;
use crate::protocols::openai_chat::{OpenAIChatExtension, OpenAIChatProvider};
use crate::{ChatMessage, ChatParams, ProviderError};

/// Typed DeepSeek-specific request extension overlaid onto `OpenAIChatRequestBase`.
#[derive(Debug, Clone, Serialize)]
pub struct DeepSeekThinkingRequest {
    #[serde(rename = "type")]
    pub kind: &'static str,
}

#[derive(Debug, Default)]
pub struct DeepSeekChatExtension;

impl OpenAIChatExtension for DeepSeekChatExtension {
    fn adapter_name(&self) -> &'static str {
        "DeepSeekChatProvider"
    }

    fn supports_sampling_params(&self) -> bool {
        false
    }

    fn serialize_messages(&self, messages: &[ChatMessage], _model: &str) -> Result<Vec<Value>, ProviderError> {
        let mut carry = ToolImageCarryMixin::default();
        let mut out = Vec::with_capacity(messages.len());
        for m in messages {
            if m.role != "tool" {
                carry.flush(&mut out);
            }
            out.push(serialize_deepseek_message(m, &mut carry)?);
        }
        carry.flush(&mut out);
        Ok(out)
    }

    fn customize_request(
        &self,
        _base_url: &str,
        params: &ChatParams,
        body: &mut Map<String, Value>,
        _headers: &mut http::HeaderMap,
    ) -> Result<(), ProviderError> {
        if !params.thinking_enabled {
            let thinking = serde_json::to_value(DeepSeekThinkingRequest { kind: "disabled" })
                .map_err(|e| ProviderError::Parse(e.to_string()))?;
            body.insert("thinking".into(), thinking);
        }
        if let Some(ref effort) = params.thinking_effort {
            body.insert("reasoning_effort".into(), serde_json::json!(effort));
        }
        Ok(())
    }
}

fn serialize_deepseek_message(m: &ChatMessage, carry: &mut ToolImageCarryMixin) -> Result<Value, ProviderError> {
    let rendered = crate::render_message(m, crate::SenderRendering::NameField).map_err(ProviderError::Parse)?;
    let content = carry.take(m).map_err(ProviderError::Parse)?.unwrap_or(rendered.content);
    let mut msg = serde_json::json!({ "role": m.role, "content": content });
    if let Some(ref name) = rendered.name {
        msg["name"] = serde_json::json!(name);
    }
    // DeepSeek requires reasoning_content only for assistant messages with
    // tool_calls; stripping it on plain assistant replies preserves shorter
    // cache prefixes.
    if let Some(ref rc) = m.reasoning_content
        && m.tool_calls.is_some()
    {
        msg["reasoning_content"] = serde_json::json!(rc);
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

/// Concrete DeepSeek Chat Completions provider composed over [`OpenAIChatProvider`].
pub struct DeepSeekChatProvider;

impl DeepSeekChatProvider {
    pub fn build(base_url: &str, api_key: &str) -> OpenAIChatProvider {
        OpenAIChatProvider::with_extension(base_url, api_key, Arc::new(DeepSeekChatExtension))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChatProvider, ToolCall};
    use meridian_provider_sdk::client::RequestBody;

    #[test]
    fn deepseek_omits_sampling_params_and_sends_disabled_thinking_when_off() {
        let provider = DeepSeekChatProvider::build("https://api.deepseek.com", "key");
        assert_eq!(provider.adapter_name(), "DeepSeekChatProvider");
        let params = ChatParams {
            model: "deepseek-reasoner".into(),
            thinking_enabled: false,
            temperature: Some(0.7),
            top_p: Some(0.9),
            max_tokens: Some(4096),
            ..Default::default()
        };
        let req = provider
            .build_request(&[ChatMessage::user("hello")], None, &params, true)
            .unwrap();
        let Some(RequestBody::Json(body)) = req.body else {
            panic!("JSON body");
        };
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["thinking"], serde_json::json!({ "type": "disabled" }));
    }

    #[test]
    fn deepseek_only_replays_reasoning_content_on_assistant_tool_calls() {
        let ext = DeepSeekChatExtension;
        let mut plain = ChatMessage::assistant("plain answer");
        plain.reasoning_content = Some("plain thought".into());
        let messages = vec![
            plain,
            ChatMessage::assistant_with_tools(
                "",
                Some("tool thought".into()),
                vec![ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                }],
            ),
        ];
        let wire = ext.serialize_messages(&messages, "deepseek-reasoner").unwrap();
        assert!(wire[0].get("reasoning_content").is_none());
        assert_eq!(wire[1]["reasoning_content"], "tool thought");
    }
}

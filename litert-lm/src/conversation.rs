use crate::LiteRtError;
use crate::runtime::Runtime;
use crate::stream::{CallbackContext, StreamChunk, stream_trampoline};
use crate::sys::{self, LiteRtLmConversation, LiteRtLmConversationOptionalArgs, LiteRtLmEngine};
use std::os::raw::c_void;
use std::ptr;
use std::sync::mpsc;

/// Configuration for creating a [`Conversation`].
#[derive(Default)]
pub struct ConversationConfig {
    /// JSON array of tool definitions (OpenAI function-calling format).
    pub tools_json: Option<String>,

    /// System instruction as a JSON content object.
    pub system_message_json: Option<String>,

    /// Initial messages to replay into the KV cache (for session restore).
    pub messages_json: Option<String>,

    /// Enable constrained decoding for tool calls.
    pub constrained_decoding: bool,
}

/// A multi-turn conversation backed by an in-memory KV cache.
///
/// `Conversation` is `Send` but not `Sync` — it must not be used from two
/// threads simultaneously (the C API does not synchronise per-conversation).
///
/// The KV cache lives as long as the `Conversation`. Sending another message
/// appends to the existing cache (incremental prefill). After `cancel`, the
/// conversation can still be reused (verified by probe, despite upstream
/// documentation saying otherwise).
pub struct Conversation {
    rt: Runtime,
    ptr: *mut LiteRtLmConversation,
}

unsafe impl Send for Conversation {}

struct OptionalArgs<'a> {
    rt: &'a Runtime,
    ptr: *mut LiteRtLmConversationOptionalArgs,
}

impl Drop for OptionalArgs<'_> {
    fn drop(&mut self) {
        unsafe { (self.rt.sym().conversation_optional_args_delete)(self.ptr) };
    }
}

impl Conversation {
    pub(crate) fn new(
        rt: &Runtime,
        engine: *mut LiteRtLmEngine,
        config: ConversationConfig,
    ) -> Result<Self, LiteRtError> {
        let sym = rt.sym();
        let tools = config.tools_json.as_deref().map(sys::cstr).transpose()?;
        let system = config.system_message_json.as_deref().map(sys::cstr).transpose()?;
        let messages = config.messages_json.as_deref().map(sys::cstr).transpose()?;
        rt.clear_error();
        let cfg = unsafe { (sym.conversation_config_create)() };
        if cfg.is_null() {
            return Err(rt.failure("conversation_config_create"));
        }
        if let Some(ref value) = tools {
            unsafe { (sym.conversation_config_set_tools)(cfg, value.as_ptr()) };
        }
        if let Some(ref value) = system {
            unsafe { (sym.conversation_config_set_system_message)(cfg, value.as_ptr()) };
        }
        if let Some(ref value) = messages {
            unsafe { (sym.conversation_config_set_messages)(cfg, value.as_ptr()) };
        }
        if config.constrained_decoding {
            unsafe { (sym.conversation_config_set_enable_constrained_decoding)(cfg, true) };
        }
        if unsafe { (sym.get_last_error_code)() } != 0 {
            let error = rt.failure("conversation_config");
            unsafe { (sym.conversation_config_delete)(cfg) };
            return Err(error);
        }
        rt.clear_error();
        let conv = unsafe { (sym.conversation_create)(engine, cfg) };
        let result = if conv.is_null() {
            Err(rt.failure("conversation_create"))
        } else {
            Ok(Self {
                rt: rt.clone(),
                ptr: conv,
            })
        };
        unsafe { (sym.conversation_config_delete)(cfg) };
        result
    }

    /// Send a message and receive streaming chunks.
    ///
    /// `message_json` is a JSON object with `role` and `content` (or
    /// `tool_calls` for tool results), e.g.:
    /// ```json
    /// {"role":"user","content":[{"type":"text","text":"Hello"}]}
    /// ```
    ///
    /// `extra_context` is an optional JSON object for per-turn options like
    /// `{"enable_thinking": true}`.
    ///
    /// Stream a message, calling `on_chunk` for each chunk.
    ///
    /// The closure returns `true` to continue or `false` to request
    /// cancellation. After cancel the stream is drained to the final chunk
    /// before returning, so the conversation is left in a clean state.
    ///
    /// Blocks the current thread until the stream ends — call from
    /// `spawn_blocking` in an async context.
    pub fn send_message_stream(
        &self,
        message_json: &str,
        extra_context: Option<&str>,
        max_output_tokens: i32,
        mut on_chunk: impl FnMut(StreamChunk) -> bool,
    ) -> Result<(), LiteRtError> {
        let sym = self.rt.sym();
        let msg = sys::cstr(message_json)?;
        let extra = extra_context.map(sys::cstr).transpose()?;
        if max_output_tokens <= 0 {
            return Err(LiteRtError::OperationFailed(
                "a positive output budget is required".into(),
            ));
        }
        self.rt.clear_error();
        let args = unsafe { (sym.conversation_optional_args_create)() };
        if args.is_null() {
            return Err(self.rt.failure("conversation_optional_args_create"));
        }
        let args = OptionalArgs {
            rt: &self.rt,
            ptr: args,
        };
        unsafe { (sym.conversation_optional_args_set_max_output_tokens)(args.ptr, max_output_tokens) };
        if unsafe { (sym.get_last_error_code)() } != 0 {
            return Err(self.rt.failure("conversation_optional_args_set_max_output_tokens"));
        }

        let (tx, rx) = mpsc::channel();
        let ctx = CallbackContext {
            tx,
            get_text: sym.stream_chunk_get_text,
            is_final: sym.stream_chunk_is_final,
            get_error: sym.stream_chunk_get_error,
        };

        self.rt.clear_error();
        let ret = unsafe {
            (sym.conversation_send_message_stream)(
                self.ptr,
                msg.as_ptr(),
                extra.as_ref().map(|c| c.as_ptr()).unwrap_or(ptr::null()),
                args.ptr,
                stream_trampoline,
                &ctx as *const CallbackContext as *mut c_void,
            )
        };

        if ret != 0 {
            return Err(self
                .rt
                .failure(format!("conversation_send_message_stream (return={ret})")));
        }

        // Block here until the final chunk. `ctx` stays alive on this stack
        // frame for the entire duration, so the worker thread's callbacks
        // always reach a valid pointer.
        let mut cancelled = false;
        for chunk in rx {
            let is_final = chunk.is_final;
            if !cancelled && !on_chunk(chunk) {
                self.cancel();
                cancelled = true;
            }
            if is_final {
                break;
            }
        }

        Ok(())
    }

    /// Cancel an in-progress streaming operation.
    ///
    /// Probe result (2026-09-12): calling cancel on a completed or
    /// never-started conversation is harmless. The conversation remains usable
    /// for subsequent messages.
    pub fn cancel(&self) {
        unsafe { (self.rt.sym().conversation_cancel_process)(self.ptr) };
    }

    /// Returns the total number of tokens currently in the KV cache.
    pub fn token_count(&self) -> i32 {
        unsafe { (self.rt.sym().conversation_get_token_count)(self.ptr) }
    }

    /// Clone this conversation, duplicating its KV cache state.
    ///
    /// The clone is independent: messages sent to one do not affect the other.
    /// Probe result: cloning 1682 tokens costs ~10ms.
    pub fn try_clone(&self) -> Result<Self, LiteRtError> {
        let cloned = unsafe { (self.rt.sym().conversation_clone)(self.ptr) };
        if cloned.is_null() {
            return Err(LiteRtError::OperationFailed("conversation_clone returned null".into()));
        }
        Ok(Self {
            rt: self.rt.clone(),
            ptr: cloned,
        })
    }
}

impl Drop for Conversation {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { (self.rt.sym().conversation_delete)(self.ptr) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_config_defaults() {
        let cfg = ConversationConfig::default();
        assert!(cfg.tools_json.is_none());
        assert!(cfg.system_message_json.is_none());
        assert!(cfg.messages_json.is_none());
        assert!(!cfg.constrained_decoding);
    }

    #[test]
    fn tool_call_chunk_parses_as_object() {
        let raw = r#"{"role":"assistant","tool_calls":[{"type":"function","function":{"name":"product","arguments":{"numbers":[3.0,7.0,11.0]}}}]}"#;
        let v: serde_json::Value = serde_json::from_str(raw).unwrap();
        let calls = v["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        let func = &calls[0]["function"];
        assert_eq!(func["name"].as_str().unwrap(), "product");
        // arguments is an object, not a string
        assert!(func["arguments"].is_object());
        let nums = func["arguments"]["numbers"].as_array().unwrap();
        assert_eq!(nums.len(), 3);
    }

    #[test]
    fn text_chunk_parses() {
        let raw = r#"{"role":"assistant","content":[{"type":"text","text":"Four"}]}"#;
        let v: serde_json::Value = serde_json::from_str(raw).unwrap();
        let content = v["content"].as_array().unwrap();
        assert_eq!(content[0]["text"].as_str().unwrap(), "Four");
    }

    #[test]
    fn tool_result_serializes() {
        let result = serde_json::json!({
            "role": "tool",
            "content": [{
                "type": "tool_response",
                "name": "product",
                "response": {"result": 231}
            }]
        });
        let s = serde_json::to_string(&result).unwrap();
        assert!(s.contains("tool_response"));
        assert!(s.contains("231"));
    }
}

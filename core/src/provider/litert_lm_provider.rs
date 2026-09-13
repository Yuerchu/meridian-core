use super::{
    AgentResponse, ChatMessage, ChatParams, ChatProvider, ChatStream, ProviderError, StreamEvent,
    TokenUsage, ToolCall, ToolDefinition,
};
use async_trait::async_trait;
use litert_lm::conversation::ConversationConfig;
use litert_lm::engine::{Backend, Engine, EngineConfig};
use litert_lm::runtime::{LogSeverity, Runtime};
use litert_lm::stream::StreamChunk;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

static RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();
static ENGINE_CACHE: Mutex<Option<CachedEngine>> = Mutex::new(None);

struct CachedEngine {
    model_path: String,
    backend: Backend,
    max_tokens: i32,
    engine: Arc<Engine>,
}

#[cfg(target_os = "windows")]
const DLL_NAME: &str = "litert-lm.dll";
#[cfg(target_os = "linux")]
const DLL_NAME: &str = "liblitert-lm.so";
#[cfg(target_os = "macos")]
const DLL_NAME: &str = "liblitert-lm.dylib";
#[cfg(target_os = "android")]
const DLL_NAME: &str = "liblitert-lm.so";

fn discover_dll() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("LITERT_LM_LIB_PATH") {
        let p = PathBuf::from(&path);
        if is_real_dll(&p) {
            return Ok(p);
        }
        return Err(format!("LITERT_LM_LIB_PATH set to `{path}` but file does not exist or is a placeholder"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(DLL_NAME);
            if is_real_dll(&candidate) {
                return Ok(candidate);
            }
        }
    }
    Err(format!(
        "LiteRT-LM runtime not found: place `{DLL_NAME}` next to the executable or set LITERT_LM_LIB_PATH"
    ))
}

fn is_real_dll(path: &PathBuf) -> bool {
    path.metadata().map(|m| m.len() > 1024).unwrap_or(false)
}

fn get_or_init_runtime() -> Result<&'static Runtime, String> {
    RUNTIME
        .get_or_init(|| {
            let path = discover_dll()?;
            tracing::info!(?path, "loading LiteRT-LM runtime");
            Runtime::load(&path).map_err(|e| format!("failed to load LiteRT-LM: {e}"))
        })
        .as_ref()
        .map_err(|e| e.clone())
}

pub fn is_available() -> bool {
    discover_dll().is_ok()
}

pub struct LiteRtLmProvider {
    shared: Arc<Shared>,
}

struct Shared {
    runtime: Runtime,
    model_path: String,
    backend: Backend,
    state: Mutex<ProviderState>,
}

struct ProviderState {
    engine: Option<Arc<Engine>>,
}

impl LiteRtLmProvider {
    pub fn new(runtime: Runtime, model_path: &str, backend: Backend) -> Self {
        runtime.set_log_level(LogSeverity::Error);
        Self {
            shared: Arc::new(Shared {
                runtime,
                model_path: model_path.to_string(),
                backend,
                state: Mutex::new(ProviderState { engine: None }),
            }),
        }
    }

    pub fn from_model_path(model_path: &str) -> Result<Self, String> {
        let rt = get_or_init_runtime()?.clone();
        Ok(Self::new(rt, model_path, Backend::Cpu))
    }
}

fn messages_to_litert_json(messages: &[ChatMessage]) -> serde_json::Value {
    let mut arr = Vec::new();
    for m in messages {
        let mut msg = serde_json::Map::new();
        msg.insert("role".into(), serde_json::Value::String(m.role.clone()));

        if let Some(ref tool_calls) = m.tool_calls {
            let calls: Vec<serde_json::Value> = tool_calls
                .iter()
                .map(|tc| {
                    let args: serde_json::Value =
                        serde_json::from_str(&tc.arguments).unwrap_or(serde_json::Value::Object(
                            serde_json::Map::new(),
                        ));
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": tc.name,
                            "arguments": args
                        }
                    })
                })
                .collect();
            msg.insert("tool_calls".into(), serde_json::Value::Array(calls));
        } else if m.role == "tool" {
            let content = serde_json::json!([{
                "type": "tool_response",
                "name": m.tool_call_id.as_deref().unwrap_or(""),
                "response": m.content
            }]);
            msg.insert("content".into(), content);
        } else {
            let content = serde_json::json!([{
                "type": "text",
                "text": m.content
            }]);
            msg.insert("content".into(), content);
        }

        arr.push(serde_json::Value::Object(msg));
    }
    serde_json::Value::Array(arr)
}

fn tools_to_litert_json(tools: &[ToolDefinition]) -> String {
    let arr: Vec<serde_json::Value> = tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters
                }
            })
        })
        .collect();
    serde_json::to_string(&arr).unwrap_or_else(|_| "[]".into())
}

fn parse_chunk_to_events(chunk: &StreamChunk) -> Vec<StreamEvent> {
    let mut events = Vec::new();

    if let Some(ref err) = chunk.error {
        events.push(StreamEvent::Error {
            message: err.clone(),
        });
        return events;
    }

    if chunk.text.is_empty() {
        return events;
    }

    let parsed: serde_json::Value = match serde_json::from_str(&chunk.text) {
        Ok(v) => v,
        Err(_) => {
            events.push(StreamEvent::Text {
                content: chunk.text.clone(),
            });
            return events;
        }
    };

    if let Some(tool_calls) = parsed["tool_calls"].as_array() {
        for (i, tc) in tool_calls.iter().enumerate() {
            let func = &tc["function"];
            let name = func["name"].as_str().unwrap_or("").to_string();
            let arguments = func["arguments"].clone();
            let args_str = serde_json::to_string(&arguments).unwrap_or_else(|_| "{}".into());
            let id = uuid::Uuid::new_v4().to_string();

            events.push(StreamEvent::ToolCallStart {
                index: i,
                id: id.clone(),
                name,
            });
            events.push(StreamEvent::ToolCallDone {
                index: i,
                arguments: args_str,
            });
        }
        return events;
    }

    if let Some(content) = parsed["content"].as_array() {
        for item in content {
            match item["type"].as_str() {
                Some("text") => {
                    if let Some(text) = item["text"].as_str() {
                        if !text.is_empty() {
                            events.push(StreamEvent::Text {
                                content: text.to_string(),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }

    if let Some(channels) = parsed.get("channels") {
        if let Some(thought) = channels.get("thought") {
            if let Some(text) = thought.as_str() {
                if !text.is_empty() {
                    events.push(StreamEvent::Reasoning {
                        content: text.to_string(),
                    });
                }
            }
        }
    }

    events
}

#[async_trait]
impl ChatProvider for LiteRtLmProvider {
    #[cfg(test)]
    fn adapter_name(&self) -> &'static str {
        "litert_lm"
    }

    async fn stream_chat_with_tools(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        params: ChatParams,
    ) -> Result<ChatStream, ProviderError> {
        let shared = self.shared.clone();

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<StreamEvent, ProviderError>>(256);

        tokio::task::spawn_blocking(move || {
            let result = (|| -> Result<(), ProviderError> {
                let max_tokens = params.context_limit.unwrap_or(8192);
                let engine = {
                    let mut cache = ENGINE_CACHE.lock().unwrap();
                    let reuse = cache.as_ref().is_some_and(|c| {
                        c.model_path == shared.model_path
                            && c.backend == shared.backend
                            && c.max_tokens == max_tokens
                    });
                    if reuse {
                        cache.as_ref().unwrap().engine.clone()
                    } else {
                        let mut config =
                            EngineConfig::new(&shared.model_path, shared.backend);
                        config.max_tokens = max_tokens;
                        let engine = Arc::new(
                            Engine::new(&shared.runtime, &config)
                                .map_err(|e| ProviderError::Upstream(e.to_string()))?,
                        );
                        *cache = Some(CachedEngine {
                            model_path: shared.model_path.clone(),
                            backend: shared.backend,
                            max_tokens,
                            engine: engine.clone(),
                        });
                        engine
                    }
                };
                let (history, last_msg) = if messages.len() > 1 {
                    (&messages[..messages.len() - 1], &messages[messages.len() - 1])
                } else if messages.len() == 1 {
                    (&messages[..0], &messages[0])
                } else {
                    return Err(ProviderError::Upstream("no messages".into()));
                };

                let history_json = messages_to_litert_json(history);
                let last_msg_json = messages_to_litert_json(std::slice::from_ref(last_msg));
                let last_msg_str = last_msg_json[0].to_string();

                let tools_json = if tools.is_empty() {
                    None
                } else {
                    Some(tools_to_litert_json(&tools))
                };

                let config = ConversationConfig {
                    tools_json,
                    messages_json: if history.is_empty() {
                        None
                    } else {
                        Some(history_json.to_string())
                    },
                    constrained_decoding: !tools.is_empty(),
                    ..Default::default()
                };

                let conv = engine
                    .create_conversation(config)
                    .map_err(|e| ProviderError::Upstream(e.to_string()))?;

                let extra_context = if params.thinking_enabled {
                    Some(r#"{"enable_thinking":true}"#)
                } else {
                    None
                };

                let tokens_before = conv.token_count();

                conv.send_message_stream(
                    &last_msg_str,
                    extra_context,
                    |chunk| {
                        let events = parse_chunk_to_events(&chunk);
                        for event in events {
                            if tx.blocking_send(Ok(event)).is_err() {
                                return false;
                            }
                        }
                        true
                    },
                )
                .map_err(|e| ProviderError::Upstream(e.to_string()))?;

                let tokens_after = conv.token_count();
                let completion_tokens = tokens_after - tokens_before;

                let _ = tx.blocking_send(Ok(StreamEvent::Stop {
                    reason: "end_turn".into(),
                    usage: Some(TokenUsage {
                        prompt_tokens: Some(tokens_before),
                        completion_tokens: Some(completion_tokens),
                        total_tokens: Some(tokens_after),
                        cache_read_tokens: None,
                        cache_write_tokens: None,
                        billable_tool_calls: None,
                    }),
                }));

                Ok(())
            })();

            if let Err(e) = result {
                let _ = tx.blocking_send(Err(e));
            }
        });

        let stream = async_stream::stream! {
            while let Some(item) = rx.recv().await {
                yield item;
            }
        };

        Ok(Box::pin(stream))
    }

    async fn chat(
        &self,
        messages: Vec<ChatMessage>,
        params: ChatParams,
    ) -> Result<String, ProviderError> {
        let rx = self
            .stream_chat_with_tools(messages, vec![], params)
            .await?;
        use futures::StreamExt;
        let mut rx = rx;
        let mut text = String::new();
        while let Some(event) = rx.next().await {
            if let Ok(StreamEvent::Text { content }) = event {
                text.push_str(&content);
            }
        }
        Ok(text)
    }

    async fn chat_with_tools(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        params: ChatParams,
    ) -> Result<AgentResponse, ProviderError> {
        let rx = self
            .stream_chat_with_tools(messages, tools, params)
            .await?;
        use futures::StreamExt;
        let mut rx = rx;
        let mut text = String::new();
        let mut reasoning = None;
        let mut tool_calls = Vec::new();
        let mut usage = None;

        while let Some(event) = rx.next().await {
            match event {
                Ok(StreamEvent::Text { content }) => text.push_str(&content),
                Ok(StreamEvent::Reasoning { content }) => {
                    reasoning.get_or_insert_with(String::new).push_str(&content);
                }
                Ok(StreamEvent::ToolCallStart { id, name, .. }) => {
                    tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments: String::new(),
                    });
                }
                Ok(StreamEvent::ToolCallDone { arguments, .. }) => {
                    if let Some(last) = tool_calls.last_mut() {
                        last.arguments = arguments;
                    }
                }
                Ok(StreamEvent::Stop { usage: u, .. }) => {
                    usage = u;
                }
                _ => {}
            }
        }

        Ok(AgentResponse {
            text,
            reasoning_content: reasoning,
            tool_calls,
            usage,
            provider_state: None,
        })
    }
}

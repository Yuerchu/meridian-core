use super::registry::LocalInferenceBackend;
use super::{
    AgentResponse, ChatMessage, ChatParams, ChatProvider, ChatStream, ProviderError, StreamEvent, TokenUsage, ToolCall,
    ToolDefinition,
};
use async_trait::async_trait;
use litert_lm::conversation::ConversationConfig;
use litert_lm::engine::{Engine, EngineConfig};
use litert_lm::runtime::{LogSeverity, Runtime};
use litert_lm::stream::StreamChunk;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

static RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();
static ENGINE_CACHE: Mutex<Option<CachedEngine>> = Mutex::new(None);

#[derive(Debug, Clone, PartialEq, Eq)]
struct EngineCacheKey {
    revision: u64,
    config: EngineConfig,
    runtime_version: &'static str,
}

fn local_budgets(params: &ChatParams) -> Result<(i32, i32), ProviderError> {
    let context = params.context_limit.filter(|value| *value > 0).ok_or_else(|| {
        ProviderError::Upstream("Set a positive runtime context budget in Settings → Provider → Model".into())
    })?;
    let output = params
        .max_tokens
        .filter(|value| *value > 0)
        .ok_or_else(|| ProviderError::Upstream("Set a positive output budget in Settings → Provider → Model".into()))?;
    if output > context {
        return Err(ProviderError::Upstream("output budget exceeds runtime context".into()));
    }
    Ok((context, output))
}

fn same_engine(existing: &EngineCacheKey, requested: &EngineCacheKey) -> bool {
    existing == requested
}

struct CachedEngine {
    key: EngineCacheKey,
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
        return Err(format!(
            "LITERT_LM_LIB_PATH set to `{path}` but file does not exist or is a placeholder"
        ));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join(DLL_NAME);
        if is_real_dll(&candidate) {
            return Ok(candidate);
        }
    }
    #[cfg(test)]
    {
        let candidate = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../resources")
            .join(DLL_NAME);
        if is_real_dll(&candidate) {
            return Ok(candidate);
        }
    }
    Err(format!(
        "LiteRT-LM runtime not found: place `{DLL_NAME}` next to the executable or set LITERT_LM_LIB_PATH"
    ))
}

fn is_real_dll(path: &Path) -> bool {
    path.metadata().map(|m| m.len() > 1024).unwrap_or(false)
}

pub(super) fn get_or_init_runtime() -> Result<&'static Runtime, String> {
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
    get_or_init_runtime().is_ok()
}

pub struct LiteRtLmProvider {
    shared: Arc<Shared>,
}

struct Shared {
    runtime: Runtime,
    model_path: String,
    preference: LocalInferenceBackend,
}

impl LiteRtLmProvider {
    pub fn new(runtime: Runtime, model_path: &str, preference: LocalInferenceBackend) -> Self {
        runtime.set_log_level(LogSeverity::Error);
        Self {
            shared: Arc::new(Shared {
                runtime,
                model_path: model_path.to_string(),
                preference,
            }),
        }
    }

    pub fn from_model_path(model_path: &str, preference: LocalInferenceBackend) -> Result<Self, String> {
        let rt = get_or_init_runtime()?.clone();
        Ok(Self::new(rt, model_path, preference))
    }
}

fn messages_to_litert_json(messages: &[ChatMessage]) -> Result<serde_json::Value, ProviderError> {
    let mut arr = Vec::new();
    for m in messages {
        let mut msg = serde_json::Map::new();
        msg.insert("role".into(), serde_json::Value::String(m.role.clone()));

        if let Some(ref tool_calls) = m.tool_calls {
            let calls: Vec<serde_json::Value> = tool_calls
                .iter()
                .map(|tc| {
                    let args: serde_json::Value = serde_json::from_str(&tc.arguments)
                        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
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
                "response": litert_tool_text(m)?
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
    Ok(serde_json::Value::Array(arr))
}

/// A tool result as this runtime takes it: text only. A picture a tool
/// returned is a line saying so — the stored envelope, base64 and all, is not
/// something to hand a local model as its tool response.
fn litert_tool_text(m: &ChatMessage) -> Result<String, ProviderError> {
    let Some(parts) = super::tool_result_parts(m).map_err(ProviderError::Parse)? else {
        return Ok(m.content.clone());
    };
    Ok(parts
        .into_iter()
        .map(|part| match part {
            super::ToolWirePart::Text(text) => text,
            super::ToolWirePart::Image(_) => "[image omitted: this model does not accept images]".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n"))
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
        events.push(StreamEvent::Error { message: err.clone() });
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
            if item["type"].as_str() == Some("text")
                && let Some(text) = item["text"].as_str()
                && !text.is_empty()
            {
                events.push(StreamEvent::Text {
                    content: text.to_string(),
                });
            }
        }
    }

    if let Some(channels) = parsed.get("channels")
        && let Some(thought) = channels.get("thought")
        && let Some(text) = thought.as_str()
        && !text.is_empty()
    {
        events.push(StreamEvent::Reasoning {
            content: text.to_string(),
        });
    }

    events
}

#[async_trait]
impl ChatProvider for LiteRtLmProvider {
    #[cfg(test)]
    fn adapter_name(&self) -> &'static str {
        "litert_lm"
    }

    #[cfg(test)]
    fn local_backend_for_test(&self) -> Option<LocalInferenceBackend> {
        Some(self.shared.preference)
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
                let (max_tokens, max_output) = local_budgets(&params)?;
                let snapshot = super::local_model::inspect(&shared.model_path, shared.preference, false)
                    .map_err(ProviderError::Upstream)?;
                let config = super::local_model::engine_config(&snapshot, shared.preference, max_tokens)
                    .map_err(ProviderError::Upstream)?;
                let key = EngineCacheKey {
                    revision: snapshot.revision,
                    config,
                    runtime_version: litert_lm::runtime::runtime_version(),
                };
                let engine = {
                    let mut cache = ENGINE_CACHE
                        .lock()
                        .map_err(|_| ProviderError::Upstream("engine cache lock was poisoned".into()))?;
                    if let Some(cached) = cache.as_ref().filter(|cached| same_engine(&cached.key, &key)) {
                        cached.engine.clone()
                    } else {
                        // 先释放缓存自己的旧引用；活跃会话仍保有自己的 Arc。
                        *cache = None;
                        let engine = Arc::new(Engine::new(&shared.runtime, &key.config).map_err(|error| {
                            ProviderError::Upstream(format!(
                                "requested={}, resolved={}, context={max_tokens}: {error}",
                                shared.preference.as_str(),
                                key.config.backend.as_str()
                            ))
                        })?);
                        *cache = Some(CachedEngine {
                            key,
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

                let history_json = messages_to_litert_json(history)?;
                let last_msg_json = messages_to_litert_json(std::slice::from_ref(last_msg))?;
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

                conv.send_message_stream(&last_msg_str, extra_context, max_output, |chunk| {
                    let events = parse_chunk_to_events(&chunk);
                    for event in events {
                        if tx.blocking_send(Ok(event)).is_err() {
                            return false;
                        }
                    }
                    true
                })
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

    async fn chat(&self, messages: Vec<ChatMessage>, params: ChatParams) -> Result<String, ProviderError> {
        Ok(self.chat_with_tools(messages, vec![], params).await?.text)
    }

    async fn chat_with_tools(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        params: ChatParams,
    ) -> Result<AgentResponse, ProviderError> {
        collect_response(self.stream_chat_with_tools(messages, tools, params).await?).await
    }
}

async fn collect_response(mut rx: ChatStream) -> Result<AgentResponse, ProviderError> {
    use futures::StreamExt;
    let mut text = String::new();
    let mut reasoning = None;
    let mut tool_calls = Vec::new();
    let mut usage = None;
    while let Some(event) = rx.next().await {
        match event? {
            StreamEvent::Text { content } => text.push_str(&content),
            StreamEvent::Reasoning { content } => {
                reasoning.get_or_insert_with(String::new).push_str(&content);
            }
            StreamEvent::ToolCallStart { id, name, .. } => {
                tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments: String::new(),
                });
            }
            StreamEvent::ToolCallDone { arguments, .. } => {
                if let Some(last) = tool_calls.last_mut() {
                    last.arguments = arguments;
                }
            }
            StreamEvent::Stop { usage: value, .. } => {
                usage = value;
            }
            StreamEvent::Error { message } => return Err(ProviderError::Upstream(message)),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_requests_require_explicit_valid_budgets_before_loading_a_model() {
        let valid = ChatParams {
            context_limit: Some(1024),
            max_tokens: Some(32),
            ..Default::default()
        };
        assert_eq!(local_budgets(&valid).unwrap(), (1024, 32));
        for params in [
            ChatParams {
                context_limit: None,
                ..valid.clone()
            },
            ChatParams {
                max_tokens: None,
                ..valid.clone()
            },
            ChatParams {
                context_limit: Some(0),
                ..valid.clone()
            },
            ChatParams {
                max_tokens: Some(-1),
                ..valid.clone()
            },
            ChatParams {
                max_tokens: Some(1025),
                ..valid.clone()
            },
        ] {
            assert!(local_budgets(&params).is_err());
        }
    }

    #[tokio::test]
    async fn error_events_and_error_items_cannot_become_partial_success() {
        let failures = [
            Ok(StreamEvent::Error {
                message: "injected failure".into(),
            }),
            Err(ProviderError::Upstream("injected failure".into())),
        ];
        for failure in failures {
            let events = vec![
                Ok(StreamEvent::Text {
                    content: "partial".into(),
                }),
                failure,
            ];
            let stream: ChatStream = Box::pin(futures::stream::iter(events));
            let error = collect_response(stream)
                .await
                .err()
                .expect("failure must be propagated");
            assert!(error.to_string().contains("injected failure"));
        }
    }

    #[test]
    fn cache_key_includes_backend_file_context_and_runtime() {
        use litert_lm::engine::Backend;
        let original = EngineCacheKey {
            revision: 1,
            config: EngineConfig::new("a.litertlm", Backend::Cpu, 1024),
            runtime_version: "0.18.0",
        };
        assert!(same_engine(&original, &original.clone()));
        let mut changed = original.clone();
        changed.config.backend = Backend::Gpu;
        assert!(!same_engine(&original, &changed));
        let mut changed = original.clone();
        changed.config.max_tokens = 2048;
        assert!(!same_engine(&original, &changed));
        let mut changed = original.clone();
        changed.revision += 1;
        assert!(!same_engine(&original, &changed));
        let mut changed = original.clone();
        changed.config.model_path = "b.litertlm".into();
        assert!(!same_engine(&original, &changed));
        let mut changed = original.clone();
        changed.runtime_version = "future";
        assert!(!same_engine(&original, &changed));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn non_streaming_paths_propagate_model_errors_instead_of_empty_success() {
        let provider = LiteRtLmProvider::from_model_path("definitely-not-a-model", LocalInferenceBackend::Gpu).unwrap();
        let params = ChatParams {
            context_limit: Some(1024),
            max_tokens: Some(32),
            ..Default::default()
        };
        assert!(
            provider
                .chat(vec![ChatMessage::user("test")], params.clone())
                .await
                .is_err()
        );
        assert!(
            provider
                .chat_with_tools(vec![ChatMessage::user("test")], vec![], params)
                .await
                .is_err()
        );
    }

    #[test]
    fn a_tool_picture_reaches_the_runtime_as_text() {
        let parts =
            r#"[{"type":"text","text":"shot"},{"type":"image_url","image_url":{"url":"data:image/png;base64,QUJD"}}]"#;
        let wire = messages_to_litert_json(&[ChatMessage::tool_result_parts("c1", parts)]).unwrap();
        let response = wire[0]["content"][0]["response"].as_str().unwrap();
        assert_eq!(response, "shot\n[image omitted: this model does not accept images]");
    }
}

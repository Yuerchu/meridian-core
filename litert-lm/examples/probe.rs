use litert_lm::conversation::ConversationConfig;
use litert_lm::engine::{Backend, Engine, EngineConfig};
use litert_lm::model_info::Modality;
use litert_lm::runtime::{LogSeverity, Runtime};
use std::error::Error;
use std::path::Path;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 && args.len() != 6 {
        return Err("usage: probe <runtime-library> <model-file> [auto|cpu|gpu|npu <context> <output>]".into());
    }
    let runtime = Runtime::load(Path::new(&args[1]))?;
    runtime.set_log_level(LogSeverity::Error);
    let info = runtime.inspect_model(&args[2])?;
    println!("{}", serde_json::to_string_pretty(&info)?);
    if args.len() == 3 {
        return Ok(());
    }
    let requested = match args[3].as_str() {
        "auto" => None,
        "cpu" => Some(Backend::Cpu),
        "gpu" => Some(Backend::Gpu),
        "npu" => Some(Backend::Npu),
        _ => return Err("unknown backend".into()),
    };
    let backend = info.select_backend(requested)?;
    let context: i32 = args[4].parse()?;
    let output: i32 = args[5].parse()?;
    info.validate_context(context)?;
    if output <= 0 || output > context {
        return Err("output must be positive and not exceed context".into());
    }
    // 缓存只写入本次探针新建的目录，绝不放进用户的模型目录。
    let cache = std::env::temp_dir().join(format!("meridian-litert-probe-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&cache)?;
    let result = (|| -> Result<(), Box<dyn Error>> {
        let mut config = EngineConfig::new(&args[2], backend, context);
        config.vision_backend = info.backends(Modality::Vision).first().copied();
        config.audio_backend = info.backends(Modality::Audio).first().copied();
        config.cache_dir = Some(cache.to_str().ok_or("cache path is not UTF-8")?.to_string());
        let engine = Engine::new(&runtime, &config)?;
        let conversation = engine.create_conversation(ConversationConfig::default())?;
        let mut generated_chars = 0;
        let mut failure = None;
        conversation.send_message_stream(
            r#"{"role":"user","content":[{"type":"text","text":"Say OK."}]}"#,
            Some(r#"{"enable_thinking":false}"#),
            output,
            |chunk| {
                if let Some(error) = chunk.error {
                    failure = Some(error);
                    return false;
                }
                if chunk.text.is_empty() {
                    return true;
                }
                let value: serde_json::Value = match serde_json::from_str(&chunk.text) {
                    Ok(value) => value,
                    Err(error) => {
                        failure = Some(format!("invalid JSON stream chunk: {error}"));
                        return false;
                    }
                };
                if let Some(content) = value["content"].as_array() {
                    for item in content {
                        if item["type"].as_str() == Some("text")
                            && let Some(text) = item["text"].as_str()
                        {
                            generated_chars += text.chars().count();
                        }
                    }
                }
                true
            },
        )?;
        if let Some(error) = failure {
            return Err(error.into());
        }
        if generated_chars == 0 {
            return Err("the model produced no text".into());
        }
        println!(
            "backend={}, context={context}, output_budget={output}, generated_chars={generated_chars}",
            backend.as_str()
        );
        Ok(())
    })();
    std::fs::remove_dir_all(&cache)?;
    result
}

//! 把一段文字合成成语音。
//!
//! 与 `voice/`（ASR，本地模型）平级而不是放进去：那边整个是识别加模型下载，
//! 这边是一次出站 HTTP。
//!
//! **现在不抽 trait。** 已经依赖的 sherpa-onnx 自带 `OfflineTts`，零新依赖，
//! 但它要一个还没有安装的模型——`voice/` 那套 `MODEL_ID` / `MODEL_FILES` 是
//! 硬编码给一个 ASR 压缩包的，加一个 TTS 后端等于第二条下载-校验-安装链路、
//! 第二个设置区、每个音色上百 MB。更要紧的是：**一个换了嗓音的降级，比降级成
//! 文字更糟**。降级成文字是已经有的路径，一分钱不花。第二个后端真要来时，
//! 要动的只有 `synthesize` 这一个签名。

pub mod limiter;

use serde::Serialize;

/// QQ 的语音条大约一分钟。中文口语约 4–5 字/秒，60 秒 ≈ 240–300 字；200 留了
/// 余量，也留下了"一句话说得完"这个更要紧的约束。
///
/// **超了是拒绝，不是截断**：截断产生的是一段停在半句话上的录音，而拒绝会让
/// 模型自己缩短重来一次。这条限制也写在工具描述里，所以通常不会走到。
pub const MAX_TTS_CHARS: usize = 200;

/// 响应体上限。一分钟的 mp3 是几百 KB；4 MiB 是给一个跑飞的响应设的边界，
/// 不是给正常结果留的余量。
const MAX_AUDIO_BYTES: usize = 4 * 1024 * 1024;

const ENDPOINT: &str = "https://api.fish.audio/v1/tts";
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// mp3，不是 wav。
///
/// 44.1kHz/16-bit/单声道的 60 秒 wav 是 **5.3 MB**，base64 之后 7.1 MB——而这
/// 一整条要塞进一个 websocket 帧广播出去。同样长度的 mp3 是几百 KB。
const FORMAT: &str = "mp3";

/// 模型能用的情绪标记。
///
/// Fish 的 S2 把方括号里的东西当**自然语言描述**，不是一个固定标签集，所以
/// "只写在工具描述里"约束不住任何东西——模型自创一个标签，Fish 会把它原样念
/// 出来。要一个封闭清单，就得在这边解析并拒绝。
pub const CUES: &[&str] = &[
    "happy",
    "sad",
    "angry",
    "excited",
    "calm",
    "surprised",
    "curious",
    "sarcastic",
    "whispering",
    "shouting",
    "sighing",
    "laughing",
    "chuckling",
    "yawning",
    "break",
];

/// 挑出文本里所有 `[...]`，凡是不在清单上的都报出来。
///
/// 返回 `Err` 而不是悄悄删掉：删掉会让模型以为它用的标记生效了，而下一次它还
/// 会那么写。
pub fn check_cues(text: &str) -> Result<(), String> {
    let mut unknown = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let Some(close) = after.find(']') else { break };
        let cue = after[..close].trim();
        if !cue.is_empty() && !CUES.contains(&cue) {
            unknown.push(cue.to_string());
        }
        rest = &after[close + 1..];
    }
    if unknown.is_empty() {
        return Ok(());
    }
    Err(format!(
        "unknown cue(s): {}. Use only: {}",
        unknown.join(", "),
        CUES.join(", ")
    ))
}

pub struct Speech {
    pub bytes: Vec<u8>,
    /// 随结果走而不是当常量——这是留给第二个后端的唯一一个座位。
    pub format: &'static str,
}

pub struct SpeechRequest<'a> {
    pub text: &'a str,
    /// 型号可配置。免费档 `s2.1-pro-free` 的官方免费期到 2026-08-31，把它写死
    /// 成永久默认，就是给一个到期日安排一次集体失效。
    pub model: &'a str,
    /// 固定音色。机器人的嗓音是身份，不是每次调用的选项。
    pub reference_id: &'a str,
}

#[derive(Serialize)]
struct FishBody<'a> {
    text: &'a str,
    format: &'a str,
    reference_id: &'a str,
    latency: &'a str,
}

fn client() -> Result<&'static reqwest::Client, String> {
    static HTTP: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    match HTTP.get() {
        Some(c) => Ok(c),
        None => {
            // 显式 build 而不是 `Client::new()`：后者在 TLS 后端起不来时会
            // panic，而这里想要的是降级成文字。
            let client = reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .build()
                .map_err(|e| e.to_string())?;
            Ok(HTTP.get_or_init(|| client))
        }
    }
}

/// 合成一段语音。
///
/// 这里的超时是**它自己的**，和把结果发出去的那次 OneBot 调用无关——合成必须
/// 在那 10 秒之外完成，塞进去会在长文本上随机超时。
pub async fn synthesize(api_key: &str, req: SpeechRequest<'_>) -> Result<Speech, String> {
    let chars = req.text.chars().count();
    if chars > MAX_TTS_CHARS {
        return Err(format!("too long: {chars} characters, the limit is {MAX_TTS_CHARS}"));
    }
    check_cues(req.text)?;

    let response = client()?
        .post(ENDPOINT)
        .bearer_auth(api_key)
        .header("model", req.model)
        .json(&FishBody {
            text: req.text,
            format: FORMAT,
            reference_id: req.reference_id,
            latency: "normal",
        })
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = crate::util::read_body_capped(response, MAX_AUDIO_BYTES, "the speech").await?;

    if !status.is_success() {
        // 错误体是 JSON，截断之后带上——不带的话，一个 402 和一个 503 在日志里
        // 长得一模一样。**不记要合成的文本**。
        let detail: String = String::from_utf8_lossy(&body).chars().take(200).collect();
        return Err(format!("Fish Audio {status}: {detail}"));
    }
    // 内容和格式都要核。一段 HTML 错误页是 2xx 也可能发生，而把它当音频发出去
    // 的结果是对方点开一条播不了的语音。
    if !content_type.starts_with("audio/") && !looks_like_mp3(&body) {
        return Err("Fish Audio answered with something that is not audio".into());
    }
    Ok(Speech {
        bytes: body,
        format: FORMAT,
    })
}

fn looks_like_mp3(bytes: &[u8]) -> bool {
    bytes.starts_with(b"ID3") || (bytes.len() >= 2 && bytes[0] == 0xFF && (bytes[1] & 0xE0) == 0xE0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 清单是**后端**执行的。只写在工具描述里约束不住 S2——它把方括号当自然
    /// 语言，模型自创的标签会被原样念出来。
    #[test]
    fn an_invented_cue_is_refused_rather_than_spoken() {
        assert!(check_cues("[happy] 你好").is_ok());
        assert!(check_cues("[whispering][sad] 嗯").is_ok());
        assert!(check_cues("没有标记").is_ok());

        let err = check_cues("[怒吼着说] 喂").unwrap_err();
        assert!(err.contains("怒吼着说"), "要说清是哪一个不认识：{err}");
        assert!(err.contains("happy"), "并且告诉它有哪些可以用");
    }

    /// 没闭合的方括号不当成标记——那多半就是一对普通括号。
    #[test]
    fn an_unclosed_bracket_is_just_text() {
        assert!(check_cues("这里有个 [ 括号").is_ok());
    }

    #[test]
    fn mp3_is_recognised_by_its_bytes() {
        assert!(looks_like_mp3(b"ID3\x03\x00"));
        assert!(looks_like_mp3(b"\xff\xfb\x90\x00"));
        assert!(!looks_like_mp3(b"<html>"));
    }
}

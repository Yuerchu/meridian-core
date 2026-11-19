//! Media handling for OneBot messages: voice transcription, image download
//! for vision models, and OCR fallback for non-vision models.

use std::sync::Arc;
use std::time::Duration;

mod download;
mod lifecycle;
pub(super) use lifecycle::MediaGeneration;

/// Local administrator-owned exception: one exact HTTP(S) origin and one
/// fixed destination. Never constructed from adapter messages or tool input.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateMediaOrigin {
    pub origin: String,
    pub ip: std::net::IpAddr,
}

pub fn validate_private_media_origins(origins: &[PrivateMediaOrigin]) -> Result<(), String> {
    download::validate_origins(origins)
}

use super::format::{FORWARD_SENTINEL, IMAGE_SENTINEL, ParsedMessage, RECORD_SENTINEL};
use super::protocol::OneBotAction;
use super::{DirectedCallOutcome, SharedState};

pub const MAX_IMAGES: usize = 5;
pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15);
const MEDIA_API_TIMEOUT: Duration = Duration::from_secs(30);

pub struct MediaOutcome {
    /// Message text with voice/OCR results merged in.
    pub text: String,
    /// file:/// URIs of stored images (vision path only).
    pub image_uris: Vec<String>,
    /// How much of the budget this call used.
    ///
    /// **Attempts, not successes**, and the distinction is the whole reason
    /// this field exists rather than the caller counting `image_uris`. An
    /// image that fell back to OCR, or whose download failed, or whose OCR came
    /// back empty, produces no uri and still cost a fetch — so a quoted message
    /// of five images with no vision model reported nothing spent, and the
    /// turn's own images were then given the full budget again. Two calls, ten
    /// OCR requests, against a `MAX_IMAGES` of five and a doc comment promising
    /// they share one budget.
    pub spent: usize,
}

/// Process media segments of an incoming message. Must be called after the
/// session is established (needs `conversation_id` for file storage).
///
/// `record_message_id` is the id of the message the voice segment belongs to,
/// which is not always the turn's own: a reply quoting a voice note has to be
/// transcribed against the *quoted* id, and passing the reply's yields nothing.
///
/// `image_budget` is how many images this call may fetch. A turn that also
/// carries a quoted message runs this twice and the two share one budget, so
/// quoting a nine-image album cannot push the turn to eighteen downloads.
pub async fn process_media(
    state: &Arc<SharedState>,
    conn_id: u64,
    record_message_id: Option<i64>,
    parsed: &ParsedMessage,
    conversation_id: &str,
    model_override: Option<&str>,
    image_budget: usize,
) -> MediaOutcome {
    let mut text = parsed.text.clone();
    let mut image_uris = Vec::new();

    // Voice transcription runs concurrently with the image work below.
    let record_fut = async {
        if !parsed.records.is_empty()
            && let Some(mid) = record_message_id
        {
            return transcribe_record(state, conn_id, mid).await;
        }
        None
    };

    let supports_images = if parsed.images.is_empty() {
        false
    } else {
        resolve_supports_images(state, conversation_id, model_override).await
    };

    // Each image independently: vision download when supported, else OCR.
    // Returns (Option<uri>, Option<ocr_text>) so both lists rebuild in order.
    // What the budget is actually being spent on, decided by the same rule the
    // futures below use. Counted here because the answer has to survive every
    // way an attempt can come back empty.
    let spent = parsed.images.len().min(image_budget);
    let image_futs = parsed.images.iter().enumerate().map(|(i, media)| async move {
        if i >= image_budget {
            return (None, None);
        }
        let (bytes, ext) = match image_bytes(state, conn_id, media).await {
            Ok(image) => image,
            Err(_) => {
                tracing::debug!("OneBot image unavailable under media fetch policy");
                return (None, None);
            }
        };
        if supports_images {
            match store_image_bytes(&state.services, conversation_id, &bytes, &ext).await {
                Ok(uri) => return (Some(uri), None),
                Err(_) => tracing::warn!("Image storage failed, falling back to OCR"),
            }
        }
        // Never delegate an untrusted URL/path to the adapter's downloader.
        (None, ocr_image_text(state, conn_id, &bytes).await)
    });

    let (transcript, image_outcomes) = futures::future::join(record_fut, futures::future::join_all(image_futs)).await;

    if let Some(transcript) = transcript {
        text = text.replacen(RECORD_SENTINEL, &format!("[语音内容: {transcript}]"), 1);
    }

    if !parsed.images.is_empty() {
        // join_all preserves input order, so index i maps to images[i].
        let ocr_results: Vec<Option<String>> = image_outcomes.iter().map(|(_, ocr)| ocr.clone()).collect();
        for (uri, _) in &image_outcomes {
            if let Some(uri) = uri {
                image_uris.push(uri.clone());
            }
        }
        text = merge_ocr_into_text(&text, &ocr_results);
    }

    // Restore any sentinels left over (voice failed, image beyond the budget,
    // OCR empty, a forward that could not be fetched) to human-readable
    // placeholders before the model sees the text. Sticker sentinels are the
    // exception: the caller splits on them to build the content parts.
    text = text
        .replace(IMAGE_SENTINEL, "[图片]")
        .replace(RECORD_SENTINEL, "[语音]")
        .replace(FORWARD_SENTINEL, "[聊天记录]");

    MediaOutcome {
        text,
        image_uris,
        spent,
    }
}

/// Replace the i-th image sentinel with its OCR text (when present), matching
/// sentinels to images by position rather than first occurrence.
fn merge_ocr_into_text(text: &str, ocr: &[Option<String>]) -> String {
    let mut parts = text.split(IMAGE_SENTINEL);
    let mut out = String::with_capacity(text.len());
    out.push_str(parts.next().unwrap_or(""));
    for (i, part) in parts.enumerate() {
        match ocr.get(i).and_then(|o| o.as_deref()) {
            Some(t) => out.push_str(&format!("[图片内容: {t}]")),
            None => out.push_str("[图片]"),
        }
        out.push_str(part);
    }
    out
}

async fn transcribe_record(state: &Arc<SharedState>, conn_id: u64, message_id: i64) -> Option<String> {
    let echo = uuid::Uuid::new_v4().to_string();
    let action = OneBotAction::voice_msg_to_text(message_id, echo);
    match super::call_api_to_conn(state, conn_id, action, MEDIA_API_TIMEOUT).await {
        DirectedCallOutcome::AdapterAccepted(data) => data
            .get("text")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from),
        _ => {
            tracing::debug!("voice_msg_to_text unavailable");
            None
        }
    }
}

async fn ocr_image_text(state: &Arc<SharedState>, conn_id: u64, bytes: &[u8]) -> Option<String> {
    use base64::Engine;
    let image = format!("base64://{}", base64::engine::general_purpose::STANDARD.encode(bytes));
    let echo = uuid::Uuid::new_v4().to_string();
    let action = OneBotAction::ocr_image(&image, echo);
    match super::call_api_to_conn(state, conn_id, action, MEDIA_API_TIMEOUT).await {
        DirectedCallOutcome::AdapterAccepted(data) => {
            let lines: Vec<String> = data
                .get("texts")?
                .as_array()?
                .iter()
                .filter_map(|t| t.get("text").and_then(|v| v.as_str()))
                .map(String::from)
                .collect();
            Some(lines.join("\n")).filter(|s| !s.trim().is_empty())
        }
        _ => {
            tracing::debug!("ocr_image unavailable");
            None
        }
    }
}

/// Whether the effective model for this conversation supports image input.
/// Any resolution error (no provider key, missing assistant…) returns false.
async fn resolve_supports_images(
    state: &Arc<SharedState>,
    conversation_id: &str,
    model_override: Option<&str>,
) -> bool {
    let sea = &state.services.db;
    let read = sea
        .read(async |tx| {
            let effective_aid = match state.config.assistant_id.clone() {
                Some(aid) => Some(aid),
                None => match crate::db::sea::ops::conversation::get_conversation(tx, conversation_id).await? {
                    Some(conv) => conv.assistant_id,
                    None => return Ok(None),
                },
            };
            let assistant = match effective_aid {
                Some(aid) => crate::db::sea::ops::assistant::get_assistant(tx, &aid)
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            Ok::<_, crate::db::sea::DbErr>(Some(assistant))
        })
        .await;
    let Ok(Some(assistant)) = read else {
        return false;
    };
    let Ok(crate::agent::ResolvedProvider {
        provider_type,
        model: resolved_model,
        api_format,
        transport_profile,
        ..
    }) = crate::agent::resolve_provider_config(&state.services.secrets, sea, assistant.as_ref()).await
    else {
        return false;
    };
    let model = model_override
        .map(String::from)
        .or_else(|| assistant.as_ref().and_then(|a| a.model_id.clone()))
        .unwrap_or(resolved_model);
    crate::provider::registry::get_capabilities(&provider_type, &api_format, &transport_profile, &model)
        .is_ok_and(|caps| caps.reads_any_image())
}

fn image_extension(bytes: &[u8], fallback: &str) -> String {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "gif"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "webp"
    } else if bytes.starts_with(b"BM") {
        "bmp"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "jpg"
    } else {
        fallback
    }
    .to_string()
}

fn inline_image(payload: &str) -> Result<(Vec<u8>, String), String> {
    let bytes = decode_inline(payload.strip_prefix("base64://").unwrap_or(payload), MAX_IMAGE_BYTES)?;
    let extension = image_extension(&bytes, "jpg");
    Ok((bytes, extension))
}

pub(super) fn decode_inline(payload: &str, limit: u64) -> Result<Vec<u8>, String> {
    use base64::Engine;
    if payload.len() as u64 > limit.div_ceil(3) * 4 {
        return Err("inline media too large".into());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|_| "invalid inline media")?;
    if bytes.len() as u64 > limit {
        return Err("inline media too large".into());
    }
    Ok(bytes)
}

pub(super) async fn image_bytes(
    state: &Arc<SharedState>,
    conn_id: u64,
    media: &super::format::MediaRef,
) -> Result<(Vec<u8>, String), String> {
    if let Some(payload) = media.file.as_deref().and_then(|f| f.strip_prefix("base64://")) {
        return inline_image(payload);
    }
    // Do not probe get_image/get_record: OB11 returns adapter-local paths, and
    // nonstandard base64 responses have no negotiated bound (the transport's
    // message cap is smaller than MAX_IMAGE_BYTES). A future trusted bounded
    // byte capability must bind media IDs to conn_id before enabling that path.
    let _ = conn_id;
    let url = media
        .url
        .as_deref()
        .or_else(|| {
            media
                .file
                .as_deref()
                .filter(|f| f.starts_with("http://") || f.starts_with("https://"))
        })
        .ok_or("no fetchable image bytes")?;
    state
        .media_fetches
        .run(download_image(url, &state.config.private_media_origins))
        .await
}

pub(super) async fn download_bytes(
    url: &str,
    limit: u64,
    deadline: Duration,
    origins: &[PrivateMediaOrigin],
) -> Result<Vec<u8>, String> {
    Ok(download::fetch(url, limit, deadline, origins).await?.bytes)
}

pub(crate) async fn download_image(url: &str, origins: &[PrivateMediaOrigin]) -> Result<(Vec<u8>, String), String> {
    let result = download::fetch(url, MAX_IMAGE_BYTES, DOWNLOAD_TIMEOUT, origins).await?;
    let ext = result
        .content_type
        .as_deref()
        .and_then(|ct| match ct.split(';').next().unwrap_or("").trim() {
            "image/jpeg" => Some("jpg"),
            "image/png" => Some("png"),
            "image/gif" => Some("gif"),
            "image/webp" => Some("webp"),
            "image/bmp" => Some("bmp"),
            _ => None,
        })
        .unwrap_or("jpg")
        .to_string();
    let ext = image_extension(&result.bytes, &ext);
    Ok((result.bytes, ext))
}

async fn store_image_bytes(
    services: &crate::services::Services,
    conversation_id: &str,
    bytes: &[u8],
    ext: &str,
) -> Result<String, String> {
    let app_data_dir = services.paths.data_dir.clone();
    let conv_id = conversation_id.to_string();
    let ext = ext.to_string();
    let bytes = bytes.to_vec();
    tokio::task::spawn_blocking(move || {
        let (dest_path, uri) = crate::files::alloc_dest(&app_data_dir, &conv_id, &ext)?;
        std::fs::write(&dest_path, &bytes).map_err(|e| e.to_string())?;
        Ok(uri)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::super::format::IMAGE_SENTINEL;
    use super::merge_ocr_into_text;

    #[test]
    fn image_signatures_preserve_format_without_trusting_names_or_headers() {
        for (bytes, ext) in [
            (b"GIF89a".as_slice(), "gif"),
            ([137, 80, 78, 71, 13, 10, 26, 10].as_slice(), "png"),
        ] {
            use base64::Engine;
            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
            assert_eq!(super::inline_image(&encoded).unwrap().1, ext);
            assert_eq!(super::image_extension(bytes, "jpg"), ext);
        }
    }

    #[test]
    fn inline_size_is_checked_before_and_after_decoding() {
        assert_eq!(super::decode_inline("YWJj", 3).unwrap(), b"abc");
        assert!(super::decode_inline("YWJj", 2).is_err());
        assert!(super::decode_inline("YWJjZA==", 3).is_err());
        assert!(super::decode_inline("!!!!", 3).is_err());
    }

    async fn answer(
        state: &std::sync::Arc<super::SharedState>,
        rx: &mut tokio::sync::mpsc::Receiver<String>,
        data: serde_json::Value,
    ) -> serde_json::Value {
        let raw = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let action: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let echo = action["echo"].as_str().unwrap();
        let pending = state.pending_api_responses.lock().unwrap().remove(echo).unwrap();
        let response = serde_json::from_value(serde_json::json!({"retcode": 0, "data": data, "echo": echo})).unwrap();
        pending.tx.send(response).unwrap();
        action
    }

    #[tokio::test]
    async fn inline_bytes_ocr_and_quotes_use_only_the_source_connection() {
        use super::super::format::MediaRef;
        use super::super::{OneBotConfig, OneBotServer};
        let dir = tempfile::tempdir().unwrap();
        let services = crate::services::bare_services(dir.path()).await;
        let state = OneBotServer::new(services, OneBotConfig::default()).state;
        let (tx, mut source) = tokio::sync::mpsc::channel(8);
        let (other_tx, mut other) = tokio::sync::mpsc::channel(8);
        state.ws_sinks.lock().await.insert(7, tx);
        state.ws_sinks.lock().await.insert(8, other_tx);
        let media = MediaRef {
            file: Some("base64://YWJj".into()),
            url: Some("http://127.0.0.1/unused".into()),
        };
        assert_eq!(super::image_bytes(&state, 7, &media).await.unwrap().0, b"abc");
        assert!(source.try_recv().is_err());
        // Arbitrary file paths and unnegotiated handles must not reach adapter
        // file APIs, even when URL retrieval fails closed.
        for file in ["C:/do-not-read", "0123456789abcdef0123456789abcdef.png"] {
            let media = MediaRef {
                file: Some(file.into()),
                url: None,
            };
            assert!(super::image_bytes(&state, 7, &media).await.is_err());
        }
        assert!(source.try_recv().is_err());
        assert!(other.try_recv().is_err());

        let task = tokio::spawn({
            let state = state.clone();
            async move { super::ocr_image_text(&state, 7, b"abc").await }
        });
        let action = answer(
            &state,
            &mut source,
            serde_json::json!({"texts": [{"text": "synthetic OCR"}]}),
        )
        .await;
        assert_eq!(action["params"]["image"], "base64://YWJj");
        assert_eq!(task.await.unwrap().as_deref(), Some("synthetic OCR"));

        let task = tokio::spawn({
            let state = state.clone();
            async move { super::super::quote::fetch(&state, 7, 42).await }
        });
        let action = answer(
            &state,
            &mut source,
            serde_json::json!({"sender": {"user_id": 12}, "message": [{"type": "text", "data": {"text": "quoted"}}]}),
        )
        .await;
        assert_eq!(action["action"], "get_msg");
        assert_eq!(task.await.unwrap().unwrap().parsed.text, "quoted");
        assert!(other.try_recv().is_err());
        assert!(state.pending_api_responses.lock().unwrap().is_empty());
    }

    #[test]
    fn test_merge_ocr_all() {
        let out = merge_ocr_into_text(
            &format!("看 {IMAGE_SENTINEL} 和 {IMAGE_SENTINEL}"),
            &[Some("第一".into()), Some("第二".into())],
        );
        assert_eq!(out, "看 [图片内容: 第一] 和 [图片内容: 第二]");
    }

    #[test]
    fn test_merge_ocr_mixed_keeps_position() {
        // First image went through vision (placeholder kept), second fell back to OCR
        let out = merge_ocr_into_text(
            &format!("{IMAGE_SENTINEL} then {IMAGE_SENTINEL}"),
            &[None, Some("hello".into())],
        );
        assert_eq!(out, "[图片] then [图片内容: hello]");
    }

    #[test]
    fn test_merge_ocr_fewer_results_than_placeholders() {
        let out = merge_ocr_into_text(&format!("{IMAGE_SENTINEL}{IMAGE_SENTINEL}"), &[Some("a".into())]);
        assert_eq!(out, "[图片内容: a][图片]");
        assert_eq!(merge_ocr_into_text("无图", &[]), "无图");
    }
}

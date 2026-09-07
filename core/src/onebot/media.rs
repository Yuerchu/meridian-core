//! Media handling for OneBot messages: voice transcription, image download
//! for vision models, and OCR fallback for non-vision models.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use super::format::{FORWARD_SENTINEL, IMAGE_SENTINEL, ParsedMessage, RECORD_SENTINEL};
use super::protocol::OneBotAction;
use super::{SharedState, call_api_with_timeout};

pub const MAX_IMAGES: usize = 5;
pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15);
const MEDIA_API_TIMEOUT: Duration = Duration::from_secs(30);

/// Where an image can be fetched from, if anywhere.
///
/// One definition, used both to count the budget and to spend it — two copies
/// of this rule that drifted would make the count describe a different set of
/// images than the one being fetched.
fn image_url(media: &crate::onebot::format::MediaRef) -> Option<&str> {
    media
        .url
        .as_deref()
        .or_else(|| media.file.as_deref().filter(|f| f.starts_with("http")))
}

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
            return transcribe_record(state, mid).await;
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
    let spent = parsed
        .images
        .iter()
        .take(image_budget)
        .filter(|media| image_url(media).is_some())
        .count();

    let image_futs = parsed.images.iter().enumerate().map(|(i, media)| async move {
        let Some(url) = image_url(media).filter(|_| i < image_budget) else {
            return (None, None);
        };
        if supports_images {
            match fetch_and_store_image(state, conversation_id, url).await {
                Ok(uri) => return (Some(uri), None),
                Err(e) => tracing::warn!("Image download failed, falling back to OCR: {e}"),
            }
        }
        (None, ocr_image_text(state, url).await)
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

async fn transcribe_record(state: &Arc<SharedState>, message_id: i64) -> Option<String> {
    let echo = uuid::Uuid::new_v4().to_string();
    let action = OneBotAction::voice_msg_to_text(message_id, echo);
    match call_api_with_timeout(state, action, MEDIA_API_TIMEOUT).await {
        Ok(data) => data
            .get("text")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from),
        Err(e) => {
            tracing::warn!("voice_msg_to_text failed: {e}");
            None
        }
    }
}

async fn ocr_image_text(state: &Arc<SharedState>, image_url: &str) -> Option<String> {
    let echo = uuid::Uuid::new_v4().to_string();
    let action = OneBotAction::ocr_image(image_url, echo);
    match call_api_with_timeout(state, action, MEDIA_API_TIMEOUT).await {
        Ok(data) => {
            let lines: Vec<String> = data
                .get("texts")?
                .as_array()?
                .iter()
                .filter_map(|t| t.get("text").and_then(|v| v.as_str()))
                .map(String::from)
                .collect();
            Some(lines.join("\n")).filter(|s| !s.trim().is_empty())
        }
        Err(e) => {
            tracing::warn!("ocr_image failed: {e}");
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
    let pool = state.services.db.clone();
    let secrets = state.services.secrets.clone();
    let conv_id = conversation_id.to_string();
    let config_aid = state.config.assistant_id.clone();
    let override_model = model_override.map(String::from);

    tokio::task::spawn_blocking(move || {
        let mut conn = crate::util::get_conn(&pool).ok()?;
        let effective_aid = match config_aid {
            Some(aid) => Some(aid),
            None => {
                crate::db::ops::conversation::get_conversation(&mut conn, &conv_id)
                    .ok()?
                    .assistant_id
            }
        };
        let assistant = effective_aid.and_then(|aid| crate::db::ops::assistant::get_assistant(&mut conn, &aid).ok());
        drop(conn);

        let crate::agent::ResolvedProvider {
            provider_type,
            model: resolved_model,
            api_format,
            transport_profile,
            ..
        } = crate::agent::resolve_provider_config(&secrets, &pool, assistant.as_ref()).ok()?;
        let model = override_model
            .or_else(|| assistant.as_ref().and_then(|a| a.model_id.clone()))
            .unwrap_or(resolved_model);
        let caps = crate::provider::registry::get_capabilities(&provider_type, &api_format, &transport_profile, &model)
            .ok()?;
        Some(caps.supports_images)
    })
    .await
    .ok()
    .flatten()
    .unwrap_or(false)
}

async fn fetch_and_store_image(state: &Arc<SharedState>, conversation_id: &str, url: &str) -> Result<String, String> {
    let (bytes, ext) = download_image(url).await?;
    store_image_bytes(&state.services, conversation_id, bytes, &ext).await
}

static HTTP: OnceLock<reqwest::Client> = OnceLock::new();

/// Lazily-built shared client. `reqwest::Client::new()` panics if the TLS
/// backend fails to initialise; building explicitly lets that surface as an
/// error so the caller degrades to OCR/placeholder instead of aborting.
pub(super) fn http_client() -> Result<&'static reqwest::Client, String> {
    match HTTP.get() {
        Some(c) => Ok(c),
        None => {
            let client = reqwest::Client::builder().build().map_err(|e| e.to_string())?;
            Ok(HTTP.get_or_init(|| client))
        }
    }
}

pub(crate) async fn download_image(url: &str) -> Result<(Vec<u8>, String), String> {
    let resp = http_client()?
        .get(url)
        .timeout(DOWNLOAD_TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    if resp.content_length().is_some_and(|len| len > MAX_IMAGE_BYTES) {
        return Err("image too large".into());
    }

    let ext = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|ct| match ct.split(';').next().unwrap_or("").trim() {
            "image/jpeg" => Some("jpg"),
            "image/png" => Some("png"),
            "image/gif" => Some("gif"),
            "image/webp" => Some("webp"),
            "image/bmp" => Some("bmp"),
            _ => None,
        })
        .map(String::from)
        .or_else(|| {
            let path = url.split(['?', '#']).next().unwrap_or("");
            let ext = path.rsplit('.').next().unwrap_or("");
            matches!(ext, "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp").then(|| ext.to_string())
        })
        .unwrap_or_else(|| "jpg".into());

    // Stream the body so a missing/false Content-Length can't buffer unbounded data
    let mut resp = resp;
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        if (bytes.len() + chunk.len()) as u64 > MAX_IMAGE_BYTES {
            return Err("image too large".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((bytes, ext))
}

async fn store_image_bytes(
    services: &crate::services::Services,
    conversation_id: &str,
    bytes: Vec<u8>,
    ext: &str,
) -> Result<String, String> {
    let app_data_dir = services.paths.data_dir.clone();
    let conv_id = conversation_id.to_string();
    let ext = ext.to_string();
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

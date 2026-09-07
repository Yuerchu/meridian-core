use std::sync::Arc;
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

use super::SharedState;
use super::format::StickerRef;
use crate::db::models::emoji::EmojiInsert;
use crate::db::models::emoji_pack::EmojiPackInsert;

const MAX_CANDIDATES: usize = 500;
const MAX_CANDIDATE_BYTES: i64 = 500 * 1024 * 1024;

pub fn capture_in_background(state: Arc<SharedState>, self_id: i64, stickers: Vec<StickerRef>) {
    static SLOTS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let slots = SLOTS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(8))).clone();
    tokio::spawn(async move {
        let Ok(permit) = slots.acquire_owned().await else {
            return;
        };
        let _permit = permit;
        capture_stickers(&state, self_id, &stickers).await;
    });
}

fn meaningful_summary(summary: Option<&str>) -> Option<String> {
    let value = summary?.trim();
    if value.is_empty() || matches!(value, "[动画表情]" | "[商城表情]" | "[表情]" | "动画表情" | "商城表情")
    {
        return None;
    }
    Some(value.trim_matches(['[', ']']).trim().to_string()).filter(|value| !value.is_empty())
}

fn short_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest[..12].iter().map(|byte| format!("{byte:02x}")).collect()
}

fn ensure_pack(state: &SharedState, account_id: &str) -> Result<String, String> {
    let mut conn = state.services.db.get().map_err(|e| e.to_string())?;
    if let Some(pack) =
        crate::db::ops::emoji_pack::get_by_source_account(&mut conn, account_id).map_err(|e| e.to_string())?
    {
        return Ok(pack.id);
    }
    let id = uuid::Uuid::new_v4().to_string();
    let name = format!("QQ {account_id} 表情池");
    let now = crate::util::now_ms();
    match crate::db::ops::emoji_pack::create_pack(
        &mut conn,
        &EmojiPackInsert {
            id: &id,
            name: &name,
            description: Some("OneBot 自动收集；确认语义后可由助手发送"),
            cover_image: None,
            is_builtin: 0,
            sort_order: 0,
            created_at: now,
            updated_at: now,
            kind: "onebot",
            source_account_id: Some(account_id),
        },
    ) {
        Ok(_) => {}
        Err(_) => {
            return crate::db::ops::emoji_pack::get_by_source_account(&mut conn, account_id)
                .map_err(|e| e.to_string())?
                .map(|pack| pack.id)
                .ok_or_else(|| "could not create OneBot sticker pack".to_string());
        }
    }
    if let Some(assistant_id) = state.config.assistant_id.as_deref() {
        crate::db::ops::emoji_pack::assign_pack(&mut conn, assistant_id, &id, now).map_err(|e| e.to_string())?;
    }
    Ok(id)
}

pub async fn capture_stickers(state: &Arc<SharedState>, self_id: i64, stickers: &[StickerRef]) -> Vec<Option<String>> {
    if stickers.is_empty() || self_id == 0 {
        return vec![None; stickers.len()];
    }
    let account_id = self_id.to_string();
    let pack_id = match ensure_pack(state, &account_id) {
        Ok(pack) => pack,
        Err(error) => {
            tracing::warn!(%error, self_id, "could not open OneBot sticker pool");
            return vec![None; stickers.len()];
        }
    };
    let data_dir = &state.services.paths.data_dir;
    if let Err(error) = crate::emoji::ensure_pack_dir(data_dir, &pack_id) {
        tracing::warn!(%error, "could not create OneBot sticker directory");
        return vec![None; stickers.len()];
    }

    let mut captured = Vec::with_capacity(stickers.len());
    for sticker in stickers {
        let known = sticker.source_key.as_deref().and_then(|key| {
            let mut conn = state.services.db.get().ok()?;
            crate::db::ops::emoji::find_by_source_key(&mut conn, &pack_id, sticker.source, key)
                .ok()
                .flatten()
        });
        if let Some(mut known) = known {
            let url = sticker
                .url
                .as_deref()
                .or_else(|| sticker.file.as_deref().filter(|value| value.starts_with("http")));
            if known.file_name.is_empty()
                && let Some(url) = url
                && let Ok((bytes, extension)) = super::media::download_image(url).await
            {
                let file_name = format!("{}.{}", known.id, extension);
                let path = crate::emoji::emoji_path(data_dir, &pack_id, &file_name);
                if std::fs::write(path, &bytes).is_ok() {
                    let payload = serde_json::to_string(&sticker.native_payload).unwrap_or_else(|_| "{}".into());
                    if let Ok(mut conn) = state.services.db.get()
                        && let Ok(updated) = crate::db::ops::emoji::attach_captured_media(
                            &mut conn,
                            &known.id,
                            &file_name,
                            &extension,
                            bytes.len() as i64,
                            &payload,
                        )
                    {
                        known = updated;
                    }
                }
            }
            if let Ok(mut conn) = state.services.db.get() {
                let _ = crate::db::ops::emoji::mark_seen(&mut conn, &known.id, crate::util::now_ms());
            }
            captured.push(Some(known.id));
            continue;
        }

        let url = sticker
            .url
            .as_deref()
            .or_else(|| sticker.file.as_deref().filter(|value| value.starts_with("http")));
        let downloaded = match url {
            Some(url) => super::media::download_image(url).await.ok(),
            None => None,
        };
        let payload = serde_json::to_string(&sticker.native_payload).unwrap_or_else(|_| "{}".into());
        let key = sticker.source_key.clone().unwrap_or_else(|| {
            downloaded
                .as_ref()
                .map(|(bytes, _)| short_hash(bytes))
                .unwrap_or_else(|| short_hash(payload.as_bytes()))
        });
        if let Ok(mut conn) = state.services.db.get()
            && let Ok(Some(existing)) =
                crate::db::ops::emoji::find_by_source_key(&mut conn, &pack_id, sticker.source, &key)
        {
            let _ = crate::db::ops::emoji::mark_seen(&mut conn, &existing.id, crate::util::now_ms());
            captured.push(Some(existing.id));
            continue;
        }

        let id = uuid::Uuid::new_v4().to_string();
        let (file_name, file_format, file_size) = match downloaded {
            Some((bytes, extension)) => {
                let file_name = format!("{id}.{extension}");
                let path = crate::emoji::emoji_path(data_dir, &pack_id, &file_name);
                if let Err(error) = std::fs::write(&path, &bytes) {
                    tracing::warn!(%error, "could not store captured sticker");
                    (String::new(), extension, 0)
                } else {
                    (file_name, extension, bytes.len() as i64)
                }
            }
            None => (String::new(), String::new(), 0),
        };
        let hint = meaningful_summary(sticker.summary.as_deref());
        let suffix = &key[..key.len().min(8)];
        let name = match hint.as_ref() {
            Some(hint) => {
                let collides = state
                    .services
                    .db
                    .get()
                    .ok()
                    .and_then(|mut conn| crate::db::ops::emoji::list_by_pack(&mut conn, &pack_id).ok())
                    .is_some_and(|items| items.iter().any(|item| item.name == *hint));
                if collides {
                    format!("{hint}-{suffix}")
                } else {
                    hint.clone()
                }
            }
            None => format!("pending-{suffix}"),
        };
        let status = if hint.is_some() { "confirmed" } else { "pending" };
        let now = crate::util::now_ms();
        let inserted = state.services.db.get().ok().and_then(|mut conn| {
            crate::db::ops::emoji::create_emoji(
                &mut conn,
                &EmojiInsert {
                    id: &id,
                    pack_id: &pack_id,
                    name: &name,
                    tags: hint.as_deref(),
                    file_name: &file_name,
                    file_format: &file_format,
                    sort_order: 0,
                    created_at: now,
                    source: sticker.source,
                    source_key: Some(&key),
                    native_payload: Some(&payload),
                    semantic_status: status,
                    suggested_name: None,
                    suggested_tags: None,
                    file_size,
                    seen_count: 1,
                    last_seen_at: Some(now),
                },
            )
            .ok()
        });
        captured.push(inserted.map(|sticker| sticker.id));
    }

    evict_candidates(state, &pack_id, data_dir);
    captured
}

fn evict_candidates(state: &SharedState, pack_id: &str, data_dir: &std::path::Path) {
    let Ok(mut conn) = state.services.db.get() else { return };
    let Ok(mut candidates) = crate::db::ops::emoji::list_candidates(&mut conn, pack_id) else {
        return;
    };
    let mut bytes: i64 = candidates.iter().map(|sticker| sticker.file_size).sum();
    if candidates.len() <= MAX_CANDIDATES && bytes <= MAX_CANDIDATE_BYTES {
        return;
    }
    candidates.sort_by_key(|sticker| (sticker.seen_count, sticker.last_seen_at.unwrap_or(0)));
    let mut count = candidates.len();
    for sticker in candidates {
        if count <= MAX_CANDIDATES && bytes <= MAX_CANDIDATE_BYTES {
            break;
        }
        if crate::db::ops::emoji::is_referenced(&mut conn, &sticker.id).unwrap_or(true) {
            continue;
        }
        if crate::db::ops::emoji::delete_emoji(&mut conn, &sticker.id).is_ok() {
            if !sticker.file_name.is_empty() {
                crate::emoji::delete_file(data_dir, &sticker.pack_id, &sticker.file_name);
            }
            count -= 1;
            bytes -= sticker.file_size;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::meaningful_summary;

    #[test]
    fn generic_transport_labels_stay_pending() {
        for label in [None, Some(""), Some("[动画表情]"), Some("[商城表情]"), Some("[表情]")] {
            assert_eq!(meaningful_summary(label), None);
        }
    }

    #[test]
    fn descriptive_labels_can_be_confirmed_without_content_filtering() {
        assert_eq!(meaningful_summary(Some("[害羞贴贴]")), Some("害羞贴贴".into()));
    }
}

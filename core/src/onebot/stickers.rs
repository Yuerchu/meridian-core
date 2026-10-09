use std::sync::Arc;
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

use super::SharedState;
use super::format::StickerRef;
use crate::db::entity::emoji::EmojiSemanticStatus;
use crate::db::entity::emoji_pack::EmojiPackKind;
use crate::db::entity::{emoji, emoji_pack};
use crate::db::sea::ops::{emoji as emoji_ops, emoji_pack as pack_ops};
use crate::db::types::{Json, SqlBool};

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

/// The platform payload as the column stores it. A segment whose `data` is not
/// an object carries nothing a native resend could use, so it is not stored.
fn stored_payload(sticker: &StickerRef) -> Option<Json<serde_json::Map<String, serde_json::Value>>> {
    sticker.native_payload.as_object().cloned().map(Json)
}

/// The account's sticker pool, opened (and assigned to the configured
/// assistant) on first use. One write: the lookup, the insert and the
/// assignment cannot interleave with another capture opening the same pool.
async fn ensure_pack(state: &SharedState, account_id: &str) -> Result<String, String> {
    let assistant_id = state.config.assistant_id.clone();
    state
        .services
        .db
        .write(async |tx| {
            if let Some(pack) = pack_ops::get_by_source_account(tx, account_id).await? {
                return Ok(pack.id);
            }
            let now = crate::util::now_ms();
            let pack = pack_ops::create_pack(
                tx,
                emoji_pack::Model {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: format!("QQ {account_id} 表情池"),
                    description: Some("OneBot 自动收集；确认语义后可由助手发送".into()),
                    cover_image: None,
                    is_builtin: SqlBool::FALSE,
                    sort_order: 0,
                    created_at: now,
                    updated_at: now,
                    kind: EmojiPackKind::Onebot,
                    source_account_id: Some(account_id.to_owned()),
                },
            )
            .await?;
            if let Some(assistant_id) = assistant_id.as_deref() {
                pack_ops::assign_pack(tx, assistant_id, &pack.id, now).await?;
            }
            Ok(pack.id)
        })
        .await
        .map_err(|e: sea_orm::DbErr| e.to_string())
}

/// Record a sighting of a sticker that is not yet known by its key, or find
/// it if another capture recorded it first. The lookup, the name check and the
/// insert are one write, so two captures of the same new sticker leave one row.
async fn record_new(state: &SharedState, row: emoji::Model, hint: Option<&str>, suffix: &str) -> Option<String> {
    let key = row.source_key.clone().unwrap_or_default();
    state
        .services
        .db
        .write(async |tx| {
            if let Some(existing) = emoji_ops::find_by_source_key(tx, &row.pack_id, row.source, &key).await? {
                emoji_ops::mark_seen(tx, &existing.id, crate::util::now_ms()).await?;
                return Ok(existing.id);
            }
            let mut row = row;
            if let Some(hint) = hint
                && emoji_ops::name_in_use(tx, &row.pack_id, hint).await?
            {
                row.name = format!("{hint}-{suffix}");
            }
            emoji_ops::create_emoji(tx, row).await.map(|created| created.id)
        })
        .await
        .map_err(|e: sea_orm::DbErr| tracing::warn!(error = %e, "could not record a captured sticker"))
        .ok()
}

pub async fn capture_stickers(state: &Arc<SharedState>, self_id: i64, stickers: &[StickerRef]) -> Vec<Option<String>> {
    if stickers.is_empty() || self_id == 0 {
        return vec![None; stickers.len()];
    }
    let account_id = self_id.to_string();
    let pack_id = match ensure_pack(state, &account_id).await {
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
    let sea = &state.services.db;

    let mut captured = Vec::with_capacity(stickers.len());
    for sticker in stickers {
        // pool-read-before-write: this lookup only decides whether to download.
        // The download is network I/O that must not hold the write lock, and
        // `record_new` repeats the lookup inside its write before inserting.
        let known = match sticker.source_key.as_deref() {
            Some(key) => emoji_ops::find_by_source_key(sea, &pack_id, sticker.source, key)
                .await
                .ok()
                .flatten(),
            None => None,
        };
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
                    let payload = stored_payload(sticker).unwrap_or_else(|| Json(Default::default()));
                    let size = bytes.len() as i64;
                    if let Ok(updated) = sea
                        .write(async |tx| {
                            emoji_ops::attach_captured_media(tx, &known.id, &file_name, &extension, size, payload).await
                        })
                        .await
                    {
                        known = updated;
                    }
                }
            }
            let _ = sea
                .write(async |tx| emoji_ops::mark_seen(tx, &known.id, crate::util::now_ms()).await)
                .await;
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
        let key = sticker.source_key.clone().unwrap_or_else(|| {
            downloaded
                .as_ref()
                .map(|(bytes, _)| short_hash(bytes))
                .unwrap_or_else(|| {
                    short_hash(
                        serde_json::to_string(&sticker.native_payload)
                            .unwrap_or_default()
                            .as_bytes(),
                    )
                })
        });

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
        let suffix = key[..key.len().min(8)].to_string();
        let now = crate::util::now_ms();
        let row = emoji::Model {
            id,
            pack_id: pack_id.clone(),
            name: hint.clone().unwrap_or_else(|| format!("pending-{suffix}")),
            tags: hint.clone(),
            file_name,
            file_format,
            sort_order: 0,
            created_at: now,
            source: sticker.source,
            source_key: Some(key),
            native_payload: stored_payload(sticker),
            semantic_status: if hint.is_some() {
                EmojiSemanticStatus::Confirmed
            } else {
                EmojiSemanticStatus::Pending
            },
            suggested_name: None,
            suggested_tags: None,
            file_size,
            seen_count: 1,
            last_seen_at: Some(now),
        };
        captured.push(record_new(state, row, hint.as_deref(), &suffix).await);
    }

    evict_candidates(state, &pack_id, data_dir).await;
    captured
}

async fn evict_candidates(state: &SharedState, pack_id: &str, data_dir: &std::path::Path) {
    let sea = &state.services.db;
    // pool-read-before-write: eviction is best effort. Each delete is its own
    // write, and a sticker some message still shows is refused by
    // `message_stickers`' ON DELETE RESTRICT whatever this list said.
    let Ok(mut candidates) = emoji_ops::list_candidates(sea, pack_id).await else {
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
        // A referenced sticker fails here, with the key, and is kept.
        if sea
            .write(async |tx| emoji_ops::delete_emoji(tx, &sticker.id).await)
            .await
            .is_ok_and(|deleted| deleted > 0)
        {
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

    /// The capture path end to end: the same sticker captured twice is one row
    /// seen twice, under the name its summary gave it. (Whether the second
    /// capture takes the known path or the in-write lookup depends on
    /// scheduling; the race itself is the next test.)
    #[tokio::test]
    async fn two_captures_of_one_new_sticker_leave_one_row() {
        use crate::db::entity::emoji::EmojiSource;
        use crate::db::sea::ops::emoji as emoji_ops;

        let dir = tempfile::tempdir().unwrap();
        let services = crate::services::bare_services(dir.path()).await;
        let server = super::super::OneBotServer::new(services, super::super::OneBotConfig::default());
        let state = server.state.clone();
        let sticker = super::StickerRef {
            source: EmojiSource::OnebotFace,
            source_key: Some("14".into()),
            native_payload: serde_json::json!({ "id": "14" }),
            url: None,
            file: None,
            summary: Some("微笑".into()),
        };

        let first = [sticker.clone()];
        let second = [sticker];
        let (a, b) = tokio::join!(
            super::capture_stickers(&state, 42, &first),
            super::capture_stickers(&state, 42, &second),
        );
        assert!(a[0].is_some(), "{a:?}");
        assert_eq!(a, b, "both captures name the same sticker");

        let pack = crate::db::sea::ops::emoji_pack::get_by_source_account(&state.services.db, "42")
            .await
            .unwrap()
            .unwrap();
        let rows = emoji_ops::list_by_pack(&state.services.db, &pack.id).await.unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].seen_count, 2);
        assert_eq!(rows[0].name, "微笑");
    }

    /// The race itself: two captures that both missed the lookup outside the
    /// lock (as two captures arriving together do) record the same new
    /// sticker. The lookup inside the write is what lets the second find the
    /// first's row; without it the second insert hits the unique key and the
    /// capture comes back empty.
    #[tokio::test]
    async fn two_records_of_one_new_sticker_name_one_row() {
        use crate::db::entity::emoji::{EmojiSemanticStatus, EmojiSource};
        use crate::db::sea::ops::emoji as emoji_ops;

        let dir = tempfile::tempdir().unwrap();
        let services = crate::services::bare_services(dir.path()).await;
        let server = super::super::OneBotServer::new(services, super::super::OneBotConfig::default());
        let state = server.state.clone();
        let pack_id = super::ensure_pack(&state, "42").await.unwrap();
        let row = |id: &str| super::emoji::Model {
            id: id.into(),
            pack_id: pack_id.clone(),
            name: "pending-14".into(),
            tags: None,
            file_name: String::new(),
            file_format: String::new(),
            sort_order: 0,
            created_at: 1,
            source: EmojiSource::OnebotFace,
            source_key: Some("14".into()),
            native_payload: None,
            semantic_status: EmojiSemanticStatus::Pending,
            suggested_name: None,
            suggested_tags: None,
            file_size: 0,
            seen_count: 1,
            last_seen_at: Some(1),
        };

        let (a, b) = tokio::join!(
            super::record_new(&state, row("first"), None, "14"),
            super::record_new(&state, row("second"), None, "14"),
        );
        assert!(a.is_some() && b.is_some(), "{a:?} {b:?}");
        assert_eq!(a, b);
        let rows = emoji_ops::list_by_pack(&state.services.db, &pack_id).await.unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].seen_count, 2);
    }
}

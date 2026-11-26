//! Reading and writing `emoji_embeddings`.
//!
//! Vectors are stored as packed little-endian `f32` bytes so SQLite reads stay
//! compact and fast on `send_sticker` queries. Corrupt or mismatched byte
//! lengths fail explicitly rather than producing truncated vectors.

use std::collections::HashMap;

use sea_orm::sea_query::OnConflict;
use sea_orm::{ColumnTrait, DbErr, EntityTrait, IntoActiveModel, PaginatorTrait, QueryFilter};

use crate::db::entity::emoji_embedding;
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::types::EpochMs;

pub fn encode_vector(values: &[f32]) -> Result<Vec<u8>, String> {
    if values.is_empty() {
        return Err("embedding vector must not be empty".into());
    }
    let mut out = Vec::with_capacity(values.len() * 4);
    for value in values {
        if !value.is_finite() {
            return Err("embedding vector contains non-finite float".into());
        }
        out.extend_from_slice(&value.to_le_bytes());
    }
    Ok(out)
}

pub fn decode_vector(bytes: &[u8], dims: i32) -> Result<Vec<f32>, String> {
    let dims = usize::try_from(dims).map_err(|_| format!("invalid embedding dims {dims}"))?;
    if dims == 0 {
        return Err("embedding dims must be positive".into());
    }
    let expected = dims
        .checked_mul(4)
        .ok_or_else(|| format!("embedding dims {dims} overflowed byte length"))?;
    if bytes.len() != expected {
        return Err(format!(
            "embedding byte length {} does not match dims {dims} (expected {expected})",
            bytes.len()
        ));
    }
    let mut values = Vec::with_capacity(dims);
    let (chunks, _) = bytes.as_chunks::<4>();
    for &chunk in chunks {
        let value = f32::from_le_bytes(chunk);
        if !value.is_finite() {
            return Err("stored embedding vector contains non-finite float".into());
        }
        values.push(value);
    }
    Ok(values)
}

/// Insert or replace one sticker's persisted embedding.
pub async fn upsert(
    tx: &WriteTx,
    emoji_id: &str,
    model_fingerprint: &str,
    source_hash: &str,
    values: &[f32],
    updated_at: EpochMs,
) -> Result<(), DbErr> {
    let dims = i32::try_from(values.len()).map_err(|_| DbErr::Custom("embedding vector length overflow".into()))?;
    let vector = encode_vector(values).map_err(DbErr::Custom)?;
    let row = emoji_embedding::Model {
        emoji_id: emoji_id.to_owned(),
        model_fingerprint: model_fingerprint.to_owned(),
        source_hash: source_hash.to_owned(),
        dims,
        vector,
        updated_at,
    };
    emoji_embedding::Entity::insert(row.into_active_model())
        .on_conflict(
            OnConflict::column(emoji_embedding::Column::EmojiId)
                .update_columns([
                    emoji_embedding::Column::ModelFingerprint,
                    emoji_embedding::Column::SourceHash,
                    emoji_embedding::Column::Dims,
                    emoji_embedding::Column::Vector,
                    emoji_embedding::Column::UpdatedAt,
                ])
                .to_owned(),
        )
        .exec_without_returning(tx.conn()?)
        .await?;
    Ok(())
}

/// Persisted `emoji_id -> source_hash` for rows produced by `model_fingerprint`.
pub async fn list_source_hashes_for_model(
    db: &impl Read,
    model_fingerprint: &str,
) -> Result<HashMap<String, String>, DbErr> {
    let rows = emoji_embedding::Entity::find()
        .filter(emoji_embedding::Column::ModelFingerprint.eq(model_fingerprint))
        .all(db.conn()?)
        .await?;
    Ok(rows.into_iter().map(|row| (row.emoji_id, row.source_hash)).collect())
}

/// Persisted `emoji_id -> vector` for the requested stickers under
/// `model_fingerprint`. Malformed rows are logged and skipped so one bad row
/// never disables ranking for the rest of the roster.
pub async fn list_vectors_for_emojis(
    db: &impl Read,
    emoji_ids: &[String],
    model_fingerprint: &str,
) -> Result<HashMap<String, Vec<f32>>, DbErr> {
    if emoji_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = emoji_embedding::Entity::find()
        .filter(emoji_embedding::Column::EmojiId.is_in(emoji_ids.iter().map(String::as_str)))
        .filter(emoji_embedding::Column::ModelFingerprint.eq(model_fingerprint))
        .all(db.conn()?)
        .await?;
    let mut out = HashMap::with_capacity(rows.len());
    for row in rows {
        match decode_vector(&row.vector, row.dims) {
            Ok(vec) => {
                out.insert(row.emoji_id, vec);
            }
            Err(error) => {
                tracing::warn!(emoji_id = %row.emoji_id, %error, "ignoring malformed stored emoji embedding");
            }
        }
    }
    Ok(out)
}

/// Count rows indexed for `model_fingerprint`.
pub async fn count_for_model(db: &impl Read, model_fingerprint: &str) -> Result<u64, DbErr> {
    emoji_embedding::Entity::find()
        .filter(emoji_embedding::Column::ModelFingerprint.eq(model_fingerprint))
        .count(db.conn()?)
        .await
}

/// Drop stale rows produced by any previous model or vision mode.
pub async fn delete_other_models(tx: &WriteTx, keep_model_fingerprint: &str) -> Result<u64, DbErr> {
    Ok(emoji_embedding::Entity::delete_many()
        .filter(emoji_embedding::Column::ModelFingerprint.ne(keep_model_fingerprint))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::entity::emoji::EmojiSemanticStatus;
    use crate::db::entity::emoji_pack::EmojiPackKind;
    use crate::db::sea::ops::emoji::tests::{insert as insert_emoji, sticker};
    use crate::db::sea::ops::emoji_pack::tests::{insert as insert_pack, pack};
    use crate::db::sea::sea_test_db;

    #[test]
    fn vector_roundtrip_and_validation() {
        let encoded = encode_vector(&[0.25, -1.5, 3.0]).unwrap();
        assert_eq!(decode_vector(&encoded, 3).unwrap(), vec![0.25, -1.5, 3.0]);
        assert!(encode_vector(&[]).is_err());
        assert!(encode_vector(&[f32::NAN]).is_err());
        assert!(decode_vector(&encoded, 2).is_err());
        assert!(decode_vector(&encoded, 0).is_err());
    }

    #[tokio::test]
    async fn upsert_read_model_filter_and_cascade_delete() {
        let db = sea_test_db().await;
        insert_pack(&db, pack("p1", EmojiPackKind::Manual, None, 0)).await;
        insert_emoji(&db, sticker("e1", "p1", EmojiSemanticStatus::Confirmed)).await;
        insert_emoji(&db, sticker("e2", "p1", EmojiSemanticStatus::Confirmed)).await;

        db.write(async |tx| {
            upsert(tx, "e1", "m1", "h1", &[1.0, 0.0], 100).await?;
            upsert(tx, "e2", "m2", "h2", &[0.0, 1.0], 101).await
        })
        .await
        .unwrap();

        let vecs = list_vectors_for_emojis(&db, &["e1".into(), "e2".into()], "m1")
            .await
            .unwrap();
        assert_eq!(vecs.len(), 1);
        assert_eq!(vecs["e1"], vec![1.0, 0.0]);
        assert_eq!(count_for_model(&db, "m1").await.unwrap(), 1);

        // Upsert overwrites cleanly.
        db.write(async |tx| upsert(tx, "e1", "m1", "h1-updated", &[0.5, 0.5], 200).await)
            .await
            .unwrap();
        let hashes = list_source_hashes_for_model(&db, "m1").await.unwrap();
        assert_eq!(hashes.get("e1").map(String::as_str), Some("h1-updated"));

        // Deleting other models removes m2 only.
        let removed = db.write(async |tx| delete_other_models(tx, "m1").await).await.unwrap();
        assert_eq!(removed, 1);
        assert_eq!(count_for_model(&db, "m2").await.unwrap(), 0);

        // Deleting the parent emoji cascades to its embedding.
        db.write(async |tx| crate::db::sea::ops::emoji::delete(tx, "e1").await)
            .await
            .unwrap();
        assert_eq!(count_for_model(&db, "m1").await.unwrap(), 0);
    }
}

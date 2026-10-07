//! What is left of the Diesel sticker ops. Linking a sticker to a message stays
//! here because `message_stickers` references `messages`, which is still Diesel's;
//! the other three are what `turn_config::resolve`, the composer-draft read and
//! the tests still call on a Diesel connection, counted in `docs/dual-impl.md`. Everything
//! else is `db::sea::ops::emoji`.

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::emoji::{EmojiInsert, EmojiRow};
use crate::db::models::message_sticker::MessageStickerInsert;
use crate::db::schema::{emojis, message_stickers};

pub fn create_emoji(conn: &mut SqliteConnection, new: &EmojiInsert) -> QueryResult<EmojiRow> {
    diesel::insert_into(emojis::table).values(new).execute(conn)?;
    emojis::table.find(new.id).first::<EmojiRow>(conn)
}

pub fn list_confirmed_for_packs(conn: &mut SqliteConnection, pack_ids: &[String]) -> QueryResult<Vec<EmojiRow>> {
    emojis::table
        .filter(emojis::pack_id.eq_any(pack_ids))
        .filter(emojis::semantic_status.eq("confirmed"))
        .filter(emojis::file_format.ne("lottie"))
        .order((emojis::pack_id.asc(), emojis::sort_order.asc()))
        .load::<EmojiRow>(conn)
}

pub fn link_message_sticker(
    conn: &mut SqliteConnection,
    message_id: &str,
    sticker_id: &str,
    position: i32,
) -> QueryResult<()> {
    diesel::insert_or_ignore_into(message_stickers::table)
        .values(&MessageStickerInsert {
            message_id,
            sticker_id,
            position,
        })
        .execute(conn)?;
    Ok(())
}

pub fn link_stickers_in_content(conn: &mut SqliteConnection, message_id: &str, content: &str) -> QueryResult<()> {
    let Ok(parts) = serde_json::from_str::<Vec<serde_json::Value>>(content) else {
        return Ok(());
    };
    for (position, sticker_id) in parts
        .iter()
        .filter(|part| part.get("type").and_then(|v| v.as_str()) == Some("sticker"))
        .filter_map(|part| part.get("sticker_id").and_then(|v| v.as_str()))
        .enumerate()
    {
        link_message_sticker(conn, message_id, sticker_id, position as i32)?;
    }
    Ok(())
}

/// One sticker, as the SeaORM model, for the composer-draft read that still
/// runs on a Diesel connection beside its conversation lookups. Held to the
/// checks a SeaORM read makes: an unknown source or status, and a payload that
/// is not a JSON object, are errors.
pub fn get_emoji(conn: &mut SqliteConnection, id: &str) -> Result<crate::db::entity::emoji::Model, String> {
    use sea_orm::ActiveEnum;

    use crate::db::entity::emoji;
    use crate::db::entity::emoji::{EmojiSemanticStatus, EmojiSource};

    let row = emojis::table
        .find(id)
        .first::<EmojiRow>(conn)
        .map_err(|e| e.to_string())?;
    Ok(emoji::Model {
        source: EmojiSource::try_from_value(&row.source).map_err(|e| e.to_string())?,
        semantic_status: EmojiSemanticStatus::try_from_value(&row.semantic_status).map_err(|e| e.to_string())?,
        native_payload: row
            .native_payload
            .as_deref()
            .map(crate::db::types::Json::decode)
            .transpose()?,
        id: row.id,
        pack_id: row.pack_id,
        name: row.name,
        tags: row.tags,
        file_name: row.file_name,
        file_format: row.file_format,
        sort_order: row.sort_order,
        created_at: row.created_at,
        source_key: row.source_key,
        suggested_name: row.suggested_name,
        suggested_tags: row.suggested_tags,
        file_size: row.file_size,
        seen_count: row.seen_count,
        last_seen_at: row.last_seen_at,
    })
}

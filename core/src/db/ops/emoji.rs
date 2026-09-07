use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::emoji::{EmojiInsert, EmojiRow};
use crate::db::models::message_sticker::MessageStickerInsert;
use crate::db::schema::{emojis, message_stickers};

pub fn list_by_pack(conn: &mut SqliteConnection, pack_id: &str) -> QueryResult<Vec<EmojiRow>> {
    emojis::table
        .filter(emojis::pack_id.eq(pack_id))
        .order(emojis::sort_order.asc())
        .load::<EmojiRow>(conn)
}

pub fn get_emoji(conn: &mut SqliteConnection, id: &str) -> QueryResult<EmojiRow> {
    emojis::table.find(id).first::<EmojiRow>(conn)
}

pub fn create_emoji(conn: &mut SqliteConnection, new: &EmojiInsert) -> QueryResult<EmojiRow> {
    diesel::insert_into(emojis::table).values(new).execute(conn)?;
    emojis::table.find(new.id).first::<EmojiRow>(conn)
}

pub fn rename_emoji(conn: &mut SqliteConnection, id: &str, new_name: &str) -> QueryResult<EmojiRow> {
    diesel::update(emojis::table.find(id))
        .set(emojis::name.eq(new_name))
        .execute(conn)?;
    emojis::table.find(id).first::<EmojiRow>(conn)
}

pub fn delete_emoji(conn: &mut SqliteConnection, id: &str) -> QueryResult<()> {
    diesel::delete(emojis::table.find(id)).execute(conn)?;
    Ok(())
}

pub fn search_emojis(conn: &mut SqliteConnection, query: &str) -> QueryResult<Vec<EmojiRow>> {
    let pattern = format!("%{query}%");
    emojis::table
        .filter(emojis::name.like(&pattern).or(emojis::tags.like(&pattern)))
        .order(emojis::sort_order.asc())
        .limit(50)
        .load::<EmojiRow>(conn)
}

pub fn list_confirmed_for_packs(conn: &mut SqliteConnection, pack_ids: &[String]) -> QueryResult<Vec<EmojiRow>> {
    emojis::table
        .filter(emojis::pack_id.eq_any(pack_ids))
        .filter(emojis::semantic_status.eq("confirmed"))
        .filter(emojis::file_format.ne("lottie"))
        .order((emojis::pack_id.asc(), emojis::sort_order.asc()))
        .load::<EmojiRow>(conn)
}

pub fn list_candidates(conn: &mut SqliteConnection, pack_id: &str) -> QueryResult<Vec<EmojiRow>> {
    emojis::table
        .filter(emojis::pack_id.eq(pack_id))
        .filter(emojis::semantic_status.ne("confirmed"))
        .order((emojis::last_seen_at.desc(), emojis::seen_count.desc()))
        .load(conn)
}

pub fn update_suggestion(
    conn: &mut SqliteConnection,
    id: &str,
    name: &str,
    tags: Option<&str>,
) -> QueryResult<EmojiRow> {
    diesel::update(emojis::table.find(id))
        .set((
            emojis::suggested_name.eq(Some(name)),
            emojis::suggested_tags.eq(tags),
            emojis::semantic_status.eq("suggested"),
        ))
        .execute(conn)?;
    get_emoji(conn, id)
}

pub fn confirm_semantics(
    conn: &mut SqliteConnection,
    id: &str,
    name: &str,
    tags: Option<&str>,
) -> QueryResult<EmojiRow> {
    diesel::update(emojis::table.find(id))
        .set((
            emojis::name.eq(name),
            emojis::tags.eq(tags),
            emojis::suggested_name.eq::<Option<&str>>(None),
            emojis::suggested_tags.eq::<Option<&str>>(None),
            emojis::semantic_status.eq("confirmed"),
        ))
        .execute(conn)?;
    get_emoji(conn, id)
}

pub fn find_by_source_key(
    conn: &mut SqliteConnection,
    pack_id: &str,
    source: &str,
    source_key: &str,
) -> QueryResult<Option<EmojiRow>> {
    emojis::table
        .filter(emojis::pack_id.eq(pack_id))
        .filter(emojis::source.eq(source))
        .filter(emojis::source_key.eq(source_key))
        .first(conn)
        .optional()
}

pub fn mark_seen(conn: &mut SqliteConnection, id: &str, now: i64) -> QueryResult<EmojiRow> {
    diesel::update(emojis::table.find(id))
        .set((
            emojis::seen_count.eq(emojis::seen_count + 1),
            emojis::last_seen_at.eq(Some(now)),
        ))
        .execute(conn)?;
    get_emoji(conn, id)
}

pub fn attach_captured_media(
    conn: &mut SqliteConnection,
    id: &str,
    file_name: &str,
    file_format: &str,
    file_size: i64,
    native_payload: &str,
) -> QueryResult<EmojiRow> {
    diesel::update(emojis::table.find(id))
        .set((
            emojis::file_name.eq(file_name),
            emojis::file_format.eq(file_format),
            emojis::file_size.eq(file_size),
            emojis::native_payload.eq(Some(native_payload)),
        ))
        .execute(conn)?;
    get_emoji(conn, id)
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

pub fn is_referenced(conn: &mut SqliteConnection, sticker_id: &str) -> QueryResult<bool> {
    use diesel::dsl::{exists, select};
    select(exists(
        message_stickers::table.filter(message_stickers::sticker_id.eq(sticker_id)),
    ))
    .get_result(conn)
}

/// Value of the `{{emoji_list}}` template variable for an assistant. The actual
/// roster is exposed lazily through `list_stickers`, keeping large packs out of
/// the system prompt.
/// `None` when nothing is assigned, so the variable is left unresolved rather
/// than expanded into an instruction pointing at an empty set.
pub fn format_emoji_list_block(conn: &mut SqliteConnection, assistant_id: &str) -> Option<String> {
    let pack_ids = crate::db::ops::emoji_pack::list_assigned_pack_ids(conn, assistant_id).ok()?;
    if pack_ids.is_empty() {
        return None;
    }
    let emojis = list_confirmed_for_packs(conn, &pack_ids).ok()?;
    if emojis.is_empty() {
        return None;
    }
    Some(
        "You can send a sticker as a separate message part. Use list_stickers to inspect the current roster, then send_sticker with its id. Do not write [emoji:...] tags.".into(),
    )
}

pub fn count_by_pack(conn: &mut SqliteConnection, pack_id: &str) -> QueryResult<i64> {
    use diesel::dsl::count_star;
    emojis::table
        .filter(emojis::pack_id.eq(pack_id))
        .select(count_star())
        .first(conn)
}

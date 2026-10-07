//! What is left of the Diesel pack ops: the three `turn_config::resolve` and the
//! tests on a Diesel connection still call, counted in `docs/dual-impl.md`.
//! Everything else is `db::sea::ops::emoji_pack`.

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::assistant_emoji_pack::AssistantEmojiPackInsert;
use crate::db::models::emoji_pack::{EmojiPackInsert, EmojiPackRow};
use crate::db::schema::{assistant_emoji_packs, emoji_packs};

pub fn create_pack(conn: &mut SqliteConnection, new: &EmojiPackInsert) -> QueryResult<EmojiPackRow> {
    diesel::insert_into(emoji_packs::table).values(new).execute(conn)?;
    emoji_packs::table.find(new.id).first::<EmojiPackRow>(conn)
}

pub fn assign_pack(conn: &mut SqliteConnection, assistant_id: &str, pack_id: &str, now: i64) -> QueryResult<()> {
    diesel::insert_or_ignore_into(assistant_emoji_packs::table)
        .values(&AssistantEmojiPackInsert {
            assistant_id,
            pack_id,
            created_at: now,
        })
        .execute(conn)?;
    Ok(())
}

pub fn list_assigned_pack_ids(conn: &mut SqliteConnection, assistant_id: &str) -> QueryResult<Vec<String>> {
    assistant_emoji_packs::table
        .filter(assistant_emoji_packs::assistant_id.eq(assistant_id))
        .select(assistant_emoji_packs::pack_id)
        .load::<String>(conn)
}

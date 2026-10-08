//! What is left of the Diesel sticker ops. Linking a sticker to a message stays
//! here because a sticker is linked inside the transaction that writes the message,
//! which is still Diesel's; the other two are what `turn_config::resolve` and the
//! tests still call on a Diesel connection, counted in `docs/dual-impl.md`. Everything
//! else is `db::sea::ops::emoji`.

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::emoji::{EmojiInsert, EmojiRow};
use crate::db::schema::emojis;

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

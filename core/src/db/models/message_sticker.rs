use diesel::prelude::*;

use crate::db::schema::message_stickers;

#[derive(Debug, Insertable)]
#[diesel(table_name = message_stickers)]
pub struct MessageStickerInsert<'a> {
    pub message_id: &'a str,
    pub sticker_id: &'a str,
    pub position: i32,
}

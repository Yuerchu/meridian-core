use diesel::prelude::*;

use crate::db::schema::assistant_emoji_packs;

#[derive(Debug, Insertable)]
#[diesel(table_name = assistant_emoji_packs)]
pub struct AssistantEmojiPackInsert<'a> {
    pub assistant_id: &'a str,
    pub pack_id: &'a str,
    pub created_at: i64,
}

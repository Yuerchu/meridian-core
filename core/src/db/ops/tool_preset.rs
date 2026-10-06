//! What is left of the Diesel `tool_presets` ops: the two `turn_config::resolve`
//! still calls, on the connection it shares with the rest of the turn's reads.
//! Everything else is `db::sea::ops::tool_preset`; `docs/dual-impl.md` counts
//! what still holds these two.

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::tool_preset::{ToolPresetInsert, ToolPresetRow};
use crate::db::schema::tool_presets;

pub fn get_preset(conn: &mut SqliteConnection, id: &str) -> QueryResult<ToolPresetRow> {
    tool_presets::table.find(id).first::<ToolPresetRow>(conn)
}

pub fn create_preset(conn: &mut SqliteConnection, new: &ToolPresetInsert) -> QueryResult<ToolPresetRow> {
    diesel::insert_into(tool_presets::table).values(new).execute(conn)?;
    tool_presets::table.find(new.id).first::<ToolPresetRow>(conn)
}

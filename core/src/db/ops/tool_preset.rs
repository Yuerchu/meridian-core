use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::tool_preset::{ToolPresetChangeset, ToolPresetInsert, ToolPresetRow};
use crate::db::schema::tool_presets;

pub fn list_presets(conn: &mut SqliteConnection) -> QueryResult<Vec<ToolPresetRow>> {
    tool_presets::table
        .order(tool_presets::sort_order.asc())
        .load::<ToolPresetRow>(conn)
}

pub fn get_preset(conn: &mut SqliteConnection, id: &str) -> QueryResult<ToolPresetRow> {
    tool_presets::table.find(id).first::<ToolPresetRow>(conn)
}

pub fn create_preset(conn: &mut SqliteConnection, new: &ToolPresetInsert) -> QueryResult<ToolPresetRow> {
    diesel::insert_into(tool_presets::table).values(new).execute(conn)?;
    tool_presets::table.find(new.id).first::<ToolPresetRow>(conn)
}

pub fn update_preset(
    conn: &mut SqliteConnection,
    id: &str,
    changeset: &ToolPresetChangeset,
) -> QueryResult<ToolPresetRow> {
    diesel::update(tool_presets::table.find(id))
        .set(changeset)
        .execute(conn)?;
    tool_presets::table.find(id).first::<ToolPresetRow>(conn)
}

pub fn delete_preset(conn: &mut SqliteConnection, id: &str) -> QueryResult<()> {
    diesel::delete(tool_presets::table.find(id)).execute(conn)?;
    Ok(())
}

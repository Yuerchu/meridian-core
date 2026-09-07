use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::custom_tool::{CustomToolChangeset, CustomToolInsert, CustomToolRow};
use crate::db::schema::custom_tools;

pub fn list_tools(conn: &mut SqliteConnection) -> QueryResult<Vec<CustomToolRow>> {
    custom_tools::table
        .order(custom_tools::sort_order.asc())
        .load::<CustomToolRow>(conn)
}

pub fn list_enabled_tools(conn: &mut SqliteConnection) -> QueryResult<Vec<CustomToolRow>> {
    custom_tools::table
        .filter(custom_tools::is_enabled.eq(1))
        .order(custom_tools::sort_order.asc())
        .load::<CustomToolRow>(conn)
}

pub fn create_tool(conn: &mut SqliteConnection, new: &CustomToolInsert) -> QueryResult<CustomToolRow> {
    diesel::insert_into(custom_tools::table).values(new).execute(conn)?;
    custom_tools::table.find(new.id).first::<CustomToolRow>(conn)
}

pub fn update_tool(
    conn: &mut SqliteConnection,
    id: &str,
    changeset: &CustomToolChangeset,
) -> QueryResult<CustomToolRow> {
    diesel::update(custom_tools::table.find(id))
        .set(changeset)
        .execute(conn)?;
    custom_tools::table.find(id).first::<CustomToolRow>(conn)
}

pub fn delete_tool(conn: &mut SqliteConnection, id: &str) -> QueryResult<()> {
    diesel::delete(custom_tools::table.find(id)).execute(conn)?;
    Ok(())
}

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::tool_category::{ToolCategoryInsert, ToolCategoryRow};
use crate::db::schema::tool_categories;

pub fn list_categories(conn: &mut SqliteConnection) -> QueryResult<Vec<ToolCategoryRow>> {
    tool_categories::table
        .order(tool_categories::sort_order.asc())
        .load::<ToolCategoryRow>(conn)
}

pub fn create_category(conn: &mut SqliteConnection, new: &ToolCategoryInsert) -> QueryResult<ToolCategoryRow> {
    diesel::insert_into(tool_categories::table).values(new).execute(conn)?;
    tool_categories::table.find(new.id).first::<ToolCategoryRow>(conn)
}

pub fn count_categories(conn: &mut SqliteConnection) -> QueryResult<i64> {
    use diesel::dsl::count_star;
    tool_categories::table.select(count_star()).first(conn)
}

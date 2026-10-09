use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::preference::PreferenceInsert;
use crate::db::schema::preferences;

pub fn get_preference(conn: &mut SqliteConnection, key: &str) -> QueryResult<Option<String>> {
    preferences::table
        .find(key)
        .select(preferences::value)
        .first::<String>(conn)
        .optional()
}

pub fn set_preference(conn: &mut SqliteConnection, key: &str, value: &str, now: i64) -> QueryResult<()> {
    diesel::replace_into(preferences::table)
        .values(&PreferenceInsert {
            key,
            value,
            updated_at: now,
        })
        .execute(conn)?;
    Ok(())
}

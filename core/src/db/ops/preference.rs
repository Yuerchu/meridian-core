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

pub fn parse_bool_preference(key: &str, value: Option<&str>, default: bool) -> Result<bool, String> {
    match value {
        None => Ok(default),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(value) => Err(format!("preference `{key}` must be `true` or `false`, got `{value}`")),
    }
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

pub fn delete_preference(conn: &mut SqliteConnection, key: &str) -> QueryResult<()> {
    diesel::delete(preferences::table.find(key)).execute(conn)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_bool_preference;

    #[test]
    fn bool_preferences_are_closed_but_keep_the_missing_default() {
        assert!(parse_bool_preference("feature.enabled", None, true).unwrap());
        assert!(!parse_bool_preference("feature.enabled", None, false).unwrap());
        assert!(parse_bool_preference("feature.enabled", Some("true"), false).unwrap());
        assert!(!parse_bool_preference("feature.enabled", Some("false"), true).unwrap());
        assert!(parse_bool_preference("feature.enabled", Some("1"), true).is_err());
        assert!(parse_bool_preference("feature.enabled", Some("TRUE"), true).is_err());
    }
}

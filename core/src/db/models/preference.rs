use diesel::prelude::*;

use crate::db::schema::preferences;

#[derive(Debug, Insertable)]
#[diesel(table_name = preferences)]
pub struct PreferenceInsert<'a> {
    pub key: &'a str,
    pub value: &'a str,
    pub updated_at: i64,
}

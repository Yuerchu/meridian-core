use diesel::prelude::*;
use serde::Serialize;

use crate::db::schema::projects;

pub use crate::db::entity::project::ProjectSource;

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = projects)]
pub struct ProjectRow {
    pub id: String,
    pub name: String,
    pub path: Option<String>,
    pub source_type: String,
    pub source_id: Option<String>,
    pub assistant_id: Option<String>,
    pub description: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = projects)]
pub struct ProjectInsert<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub path: Option<&'a str>,
    pub source_type: &'a str,
    pub source_id: Option<&'a str>,
    pub assistant_id: Option<&'a str>,
    pub description: Option<&'a str>,
    pub created_at: i64,
    pub updated_at: i64,
}

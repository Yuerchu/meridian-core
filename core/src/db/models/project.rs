use diesel::prelude::*;
use serde::{Deserialize, Serialize};

use crate::db::schema::projects;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::EnumString, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ProjectSource {
    Local,
    OnebotPrivate,
    OnebotGroup,
}

impl ProjectSource {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value.parse().map_err(|_| format!("unknown project source `{value}`"))
    }
}

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

#[derive(Debug, Default, AsChangeset)]
#[diesel(table_name = projects)]
pub struct ProjectChangeset {
    pub name: Option<String>,
    pub path: Option<Option<String>>,
    pub assistant_id: Option<Option<String>>,
    pub description: Option<Option<String>>,
    pub updated_at: Option<i64>,
}

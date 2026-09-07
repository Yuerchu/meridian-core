use diesel::prelude::*;
use serde::Serialize;

use crate::db::schema::skills;

/// Index row for a skill directory on disk. Content lives in
/// `{app_data_dir}/skills/<dir_name>/SKILL.md`; this row exists so bindings have
/// a foreign key target and so listing does not require a disk scan.
///
/// Two name pairs on purpose: `display_*` is what the user reads in settings and
/// may be any language, `llm_*` comes from the SKILL.md frontmatter, is a slug,
/// and is what reaches the model.
#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = skills)]
pub struct SkillRow {
    pub dir_name: String,
    pub llm_name: String,
    pub llm_description: String,
    pub display_name: String,
    pub display_description: Option<String>,
    /// official | user | assistant | imported
    pub source: String,
    pub is_enabled: i32,
    pub is_builtin: i32,
    pub mtime_hash: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = skills)]
pub struct SkillInsert<'a> {
    pub dir_name: &'a str,
    pub llm_name: &'a str,
    pub llm_description: &'a str,
    pub display_name: &'a str,
    pub display_description: Option<&'a str>,
    pub source: &'a str,
    pub is_enabled: i32,
    pub is_builtin: i32,
    pub mtime_hash: Option<&'a str>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, AsChangeset, Default)]
#[diesel(table_name = skills)]
pub struct SkillChangeset {
    pub llm_name: Option<String>,
    pub llm_description: Option<String>,
    pub display_name: Option<String>,
    pub display_description: Option<Option<String>>,
    pub source: Option<String>,
    pub is_enabled: Option<i32>,
    pub mtime_hash: Option<Option<String>>,
    pub updated_at: Option<i64>,
}

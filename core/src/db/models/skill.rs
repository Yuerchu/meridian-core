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

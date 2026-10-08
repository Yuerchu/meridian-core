use diesel::prelude::*;
use serde::Serialize;

use crate::db::schema::mode_artifacts;

pub use crate::db::entity::mode_artifact::PlanStatus;

/// What a mode produced for the user to approve. Plans are the only kind today;
/// the column exists so a second mode with an approvable output does not need a
/// second table.
pub const KIND_PLAN: &str = "plan";

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = mode_artifacts)]
pub struct ModeArtifactRow {
    pub id: String,
    pub conversation_id: String,
    pub kind: String,
    pub content: String,
    pub status: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = mode_artifacts)]
pub struct ModeArtifactInsert<'a> {
    pub id: &'a str,
    pub conversation_id: &'a str,
    pub kind: &'a str,
    pub content: &'a str,
    pub status: &'a str,
    pub created_at: i64,
    pub updated_at: i64,
}

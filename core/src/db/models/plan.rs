use diesel::prelude::*;
use serde::Serialize;

use crate::db::schema::mode_artifacts;

/// What a mode produced for the user to approve. Plans are the only kind today;
/// the column exists so a second mode with an approvable output does not need a
/// second table.
pub const KIND_PLAN: &str = "plan";

/// Where an artifact stands with the user.
///
/// `Superseded` and `Done` both retire an approved artifact without deleting
/// it: the first when a newer one replaces it, the second when the work it
/// described is finished. Retiring rather than deleting keeps the history of
/// what was proposed readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, strum::IntoStaticStr, strum::EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    Approved,
    Rejected,
    Superseded,
    Done,
}

impl PlanStatus {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value.parse().map_err(|_| format!("unknown artifact status '{value}'"))
    }
}

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

//! `dream_runs`: audit history and undo log for Auto-Dream consolidation runs.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

use super::memory::MemoryScope;
use crate::db::types::{EpochMs, Json};

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    EnumIter,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
    DeriveActiveEnum,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum DreamRunStatus {
    #[sea_orm(string_value = "running")]
    Running,
    #[sea_orm(string_value = "done")]
    Done,
    #[sea_orm(string_value = "failed")]
    Failed,
    #[sea_orm(string_value = "skipped")]
    Skipped,
}

impl DreamRunStatus {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    EnumIter,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
    DeriveActiveEnum,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum DreamTrigger {
    #[sea_orm(string_value = "auto")]
    Auto,
    #[sea_orm(string_value = "manual")]
    Manual,
}

impl DreamTrigger {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "dream_runs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub scope_type: MemoryScope,
    pub scope_id: String,
    pub status: DreamRunStatus,
    pub trigger: DreamTrigger,
    pub model_id: Option<String>,
    pub started_at: EpochMs,
    pub finished_at: Option<EpochMs>,
    pub turns_reviewed: i32,
    pub memories_created: i32,
    pub memories_updated: i32,
    pub memories_pruned: i32,
    pub proposals_created: i32,
    pub pruned_memory_ids_json: Option<Json<Vec<String>>>,
    pub summary: Option<String>,
    pub error: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

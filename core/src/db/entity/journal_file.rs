//! `journal_files`: one row per path the file journal has ever seen.
//!
//! Why three tables, and why the chain invariant is the writer's job, is
//! argued in migration 42 (now part of the baseline).

use sea_orm::entity::prelude::*;

use crate::db::types::EpochMs;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "journal_files")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// Canonical, normalised, case-folded on Windows — a matching key, not a
    /// display string.
    pub norm_path: String,
    pub display_path: String,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

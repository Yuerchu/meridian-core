//! `memory_subjects`: one row per person the OneBot runner has seen, holding
//! the clock eviction reads and the flags the person or the operator set.

use sea_orm::entity::prelude::*;

use super::memory::parse_onebot_user_scope_id;
use crate::db::types::{EpochMs, SqlBool};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "memory_subjects")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub scope_id: String,
    pub display_name: Option<String>,
    pub last_seen_at: EpochMs,
    pub created_at: EpochMs,
    /// Mirrors the admin list so eviction never has to read config.
    pub is_protected: SqlBool,
    pub is_pinned: SqlBool,
    pub opted_out: SqlBool,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

impl Model {
    pub fn user_id(&self) -> Option<i64> {
        parse_onebot_user_scope_id(&self.scope_id)
    }
}

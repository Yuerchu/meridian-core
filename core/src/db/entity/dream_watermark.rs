//! `dream_watermarks`: per-scope Auto-Dream turn counter and message cursor.

use sea_orm::entity::prelude::*;

use super::memory::MemoryScope;
use crate::db::types::EpochMs;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "dream_watermarks")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub scope_type: MemoryScope,
    #[sea_orm(primary_key, auto_increment = false)]
    pub scope_id: String,
    pub last_dream_at: Option<EpochMs>,
    pub last_message_id: Option<String>,
    pub turns_since_dream: i32,
    pub updated_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

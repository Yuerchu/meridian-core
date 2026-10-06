//! `voice_sender_optouts`: "do not record me again".
//!
//! Global, not per session — a person who said no should not have to say it
//! again for every group and every bot account. Separate from deleting what
//! was already recorded (migration 39): one handles the past, the other
//! refuses the future, and merging them would make one slip either delete
//! months of data or turn a deletion into a permanent stop.

use sea_orm::entity::prelude::*;

use crate::db::types::EpochMs;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "voice_sender_optouts")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub sender_id: String,
    pub created_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

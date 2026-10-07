//! `memory_proposals`: a bot-wide memory the extraction pass wants to keep,
//! parked until the operator approves or rejects it.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

use super::memory::MemoryType;
use crate::db::types::EpochMs;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "memory_proposals")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub key: String,
    pub content: String,
    pub memory_type: MemoryType,
    pub origin_session: Option<String>,
    pub proposer_id: Option<i64>,
    pub status: ProposalStatus,
    pub created_at: EpochMs,
    pub expires_at: EpochMs,
    pub resolved_at: Option<EpochMs>,
    pub resolved_by: Option<i64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, EnumIter, Serialize, Deserialize, strum::IntoStaticStr, DeriveActiveEnum,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum ProposalStatus {
    #[sea_orm(string_value = "pending")]
    Pending,
    #[sea_orm(string_value = "approved")]
    Approved,
    #[sea_orm(string_value = "rejected")]
    Rejected,
    #[sea_orm(string_value = "expired")]
    Expired,
}

impl ProposalStatus {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

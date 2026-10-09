//! `audit_messages`: an append-only copy of every message as it was sent or
//! received, with the rates in force at the time — a fact about the past,
//! which is why the prices are snapshotted here rather than joined. No
//! foreign keys: the audit outlives the conversation it records.

use sea_orm::entity::prelude::*;

use crate::agent::pricing::BillingMode;
use crate::db::types::{EpochMs, text_enum_column};
use crate::decimal::Decimal;

text_enum_column!(BillingMode);

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "audit_messages")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub recorded_at: EpochMs,
    pub message_id: String,
    pub conversation_id: String,
    pub turn_id: Option<String>,
    pub source_type: Option<String>,
    pub source_id: Option<String>,
    pub turn_origin: Option<String>,
    pub role: String,
    pub content: String,
    pub sender_id: Option<i64>,
    pub sender_name: Option<String>,
    pub provider_id: Option<String>,
    pub provider_name: Option<String>,
    pub model_id: Option<String>,
    pub input_tokens: Option<i32>,
    pub output_tokens: Option<i32>,
    pub cache_read_tokens: Option<i32>,
    pub cache_write_tokens: Option<i32>,
    pub created_at: EpochMs,
    pub input_price: Option<Decimal>,
    pub output_price: Option<Decimal>,
    pub cache_read_price: Option<Decimal>,
    pub cache_write_price: Option<Decimal>,
    pub self_id: Option<i64>,
    pub server_tool_calls: Option<i32>,
    pub server_tool_price: Option<Decimal>,
    pub billing_mode: BillingMode,
    pub response_model_id: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

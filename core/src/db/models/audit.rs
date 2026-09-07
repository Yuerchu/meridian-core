use crate::db::schema::audit_messages;
use crate::decimal::Decimal;
use diesel::prelude::*;

/// One thing that was said, recorded where deleting a conversation cannot reach
/// it.
///
/// Self-contained on purpose: every id here names a row that may already be
/// gone, and the facts needed to read this one are copied in beside them. See
/// migration 29 for why none of it is a foreign key.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Queryable, Selectable)]
#[diesel(table_name = audit_messages)]
pub struct AuditMessageRow {
    pub id: String,
    /// When the record was written, as distinct from when the message was sent.
    pub recorded_at: i64,
    pub message_id: String,
    pub conversation_id: String,
    pub turn_id: Option<String>,
    /// `local` / `onebot_private` / `onebot_group`, and the QQ number or group
    /// number beside it. Snapshotted because the join that answers this goes
    /// through `conversations.project_id`, which is `ON DELETE SET NULL`.
    pub source_type: Option<String>,
    pub source_id: Option<String>,
    /// `desktop` or `onebot`. Otherwise only `turns` knows, and `turns` cascades.
    pub turn_origin: Option<String>,
    pub role: String,
    pub content: String,
    pub sender_id: Option<i64>,
    /// The nickname at the time. `memory_subjects` holds only the current one, so
    /// a rename would otherwise reach back through every line the person wrote.
    pub sender_name: Option<String>,
    pub provider_id: Option<String>,
    pub provider_name: Option<String>,
    pub model_id: Option<String>,
    pub input_tokens: Option<i32>,
    pub output_tokens: Option<i32>,
    /// Subsets of `input_tokens`, never additions to it — the same contract the
    /// `messages` columns carry.
    pub cache_read_tokens: Option<i32>,
    pub cache_write_tokens: Option<i32>,
    pub created_at: i64,
    /// Price per million tokens, as it stood when this row was written. NULL on
    /// anything recorded before migration 30, where the only price that exists
    /// is whatever `model_configs` says today.
    pub input_price: Option<Decimal>,
    pub output_price: Option<Decimal>,
    pub cache_read_price: Option<Decimal>,
    pub cache_write_price: Option<Decimal>,
    /// The bot account that answered. NULL for desktop traffic, which is a
    /// statement rather than a gap.
    pub self_id: Option<i64>,
    /// Billable provider-side tool invocations on this request, and what one
    /// cost per thousand — snapshotted for the same reason the token rates are.
    pub server_tool_calls: Option<i32>,
    pub server_tool_price: Option<Decimal>,
    /// Whether a per-request price is owed at all — see `agent::pricing::BillingMode`.
    ///
    /// Distinguishes "nobody has priced this model" from "this request draws on
    /// a subscription". Without it both are unpriced, and the second gets
    /// reported as cost we failed to account for.
    pub billing_mode: String,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = audit_messages)]
pub struct AuditMessageInsert<'a> {
    pub id: &'a str,
    pub recorded_at: i64,
    pub message_id: &'a str,
    pub conversation_id: &'a str,
    pub turn_id: Option<&'a str>,
    pub source_type: Option<&'a str>,
    pub source_id: Option<&'a str>,
    pub turn_origin: Option<&'a str>,
    pub role: &'a str,
    pub content: &'a str,
    pub sender_id: Option<i64>,
    pub sender_name: Option<&'a str>,
    pub provider_id: Option<&'a str>,
    pub provider_name: Option<&'a str>,
    pub model_id: Option<&'a str>,
    pub input_tokens: Option<i32>,
    pub output_tokens: Option<i32>,
    pub cache_read_tokens: Option<i32>,
    pub cache_write_tokens: Option<i32>,
    pub created_at: i64,
    pub input_price: Option<Decimal>,
    pub output_price: Option<Decimal>,
    pub cache_read_price: Option<Decimal>,
    pub cache_write_price: Option<Decimal>,
    pub self_id: Option<i64>,
    /// Billable provider-side tool invocations on this request, and what one
    /// cost per thousand — snapshotted for the same reason the token rates are.
    pub server_tool_calls: Option<i32>,
    pub server_tool_price: Option<Decimal>,
    /// Defaults to `metered` at the database level, so a caller that says
    /// nothing gets exactly today's behaviour.
    pub billing_mode: &'a str,
}

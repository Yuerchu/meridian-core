//! Message rows written through SeaORM.
//!
//! Only what a SeaORM transaction needs to write beside its own rows today: a
//! `context` row, the background a turn reads and nobody said. The general
//! `append_message` is still Diesel's (`db::ops::message`), because it also
//! files the audit copy of a user row and `audit_messages` has no entity yet —
//! a SeaORM version without it would be a second `append_message` that quietly
//! does less. A context row has no audit copy, so this one does exactly what the
//! Diesel version does for it: insert under `parent`, then move the head.

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter, Set};

use crate::db::entity::{conversation, message};
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::types::EpochMs;

/// A `context` row about to be written.
pub struct ContextRowInsert<'a> {
    pub id: &'a str,
    pub conversation_id: &'a str,
    pub content: &'a str,
    /// What wrote it, so the transcript and the exports can tell it apart.
    pub source: &'a str,
    pub turn_id: Option<&'a str>,
    pub created_at: EpochMs,
}

/// Write a `context` row under `parent` and make it the conversation's head,
/// in the caller's transaction.
///
/// `sort_order` is left 0 for `trg_messages_sort_order` to assign, as the
/// Diesel insert does.
pub async fn append_context(tx: &WriteTx, row: &ContextRowInsert<'_>, parent: Option<&str>) -> Result<(), DbErr> {
    let model = message::ActiveModel {
        id: Set(row.id.to_string()),
        conversation_id: Set(row.conversation_id.to_string()),
        role: Set("context".to_string()),
        content: Set(row.content.to_string()),
        provider_id: Set(None),
        model_id: Set(None),
        input_tokens: Set(None),
        output_tokens: Set(None),
        tool_calls: Set(None),
        tool_call_id: Set(None),
        sort_order: Set(0),
        created_at: Set(row.created_at),
        reasoning_content: Set(None),
        rating: Set(None),
        schema_version: Set(2),
        is_compact_summary: Set(false.into()),
        sender_id: Set(None),
        parent_id: Set(parent.map(str::to_string)),
        compact_anchor_id: Set(None),
        source: Set(Some(row.source.to_string())),
        turn_id: Set(row.turn_id.map(str::to_string)),
        tool_outcome: Set(None),
        cache_read_tokens: Set(None),
        cache_write_tokens: Set(None),
        provider_name: Set(None),
        provider_state: Set(None),
        auto_review: Set(None),
        server_tool_calls: Set(None),
        tool_diffs: Set(None),
        response_model_id: Set(None),
    };
    message::Entity::insert(model).exec(tx.conn()?).await?;
    conversation::Entity::update_many()
        .col_expr(conversation::Column::HeadMessageId, Expr::value(row.id.to_string()))
        .filter(conversation::Column::Id.eq(row.conversation_id))
        .exec(tx.conn()?)
        .await?;
    Ok(())
}

/// One row by id.
pub async fn get(db: &impl Read, id: &str) -> Result<Option<message::Model>, DbErr> {
    message::Entity::find_by_id(id).one(db.conn()?).await
}

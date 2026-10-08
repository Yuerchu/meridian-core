use diesel::prelude::*;
use serde::Serialize;

use crate::db::schema::queued_prompts;

pub use crate::db::entity::queued_prompt::{Delivery, QueueState};

#[derive(Debug, Clone, Queryable, Selectable, Identifiable, Serialize)]
#[diesel(table_name = queued_prompts)]
pub struct QueuedPromptRow {
    pub id: String,
    pub conversation_id: String,
    pub content: String,
    pub delivery: String,
    pub position: i32,
    pub created_at: i64,
    pub dispatched_at: Option<i64>,
    pub dispatched_turn_id: Option<String>,
    pub settled_at: Option<i64>,
    pub settled_message_id: Option<String>,
    pub held_at: Option<i64>,
    pub reported_at: Option<i64>,
}

impl QueuedPromptRow {
    pub fn delivery(&self) -> Result<Delivery, String> {
        Delivery::parse(&self.delivery)
    }

    /// Ordered so the answer cannot be ambiguous: settled outranks everything
    /// (it is finished, whatever else happened on the way), then in-doubt,
    /// which is the state that must never be mistaken for deliverable.
    pub fn state(&self) -> QueueState {
        if self.settled_at.is_some() {
            QueueState::Settled
        } else if self.dispatched_at.is_some() {
            QueueState::InDoubt
        } else if self.held_at.is_some() {
            QueueState::Held
        } else {
            QueueState::Queued
        }
    }
}

#[derive(Debug, Insertable)]
#[diesel(table_name = queued_prompts)]
pub struct QueuedPromptInsert<'a> {
    pub id: &'a str,
    pub conversation_id: &'a str,
    pub content: &'a str,
    pub delivery: &'a str,
    pub position: i32,
    pub created_at: i64,
}

/// The Diesel reads hand out the entity model; a delivery mode this build does
/// not know fails the read, as it does on the SeaORM side. Goes with them.
impl TryFrom<QueuedPromptRow> for crate::db::entity::queued_prompt::Model {
    type Error = String;

    fn try_from(row: QueuedPromptRow) -> Result<Self, String> {
        Ok(Self {
            delivery: Delivery::parse(&row.delivery)?,
            id: row.id,
            conversation_id: row.conversation_id,
            content: row.content,
            position: row.position,
            created_at: row.created_at,
            dispatched_at: row.dispatched_at,
            dispatched_turn_id: row.dispatched_turn_id,
            settled_at: row.settled_at,
            settled_message_id: row.settled_message_id,
            held_at: row.held_at,
            reported_at: row.reported_at,
        })
    }
}

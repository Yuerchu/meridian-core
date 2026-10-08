use diesel::prelude::*;
use serde::Serialize;

use crate::db::schema::queued_prompts;

pub use crate::db::entity::queued_prompt::Delivery;

/// Where one queued message has got to.
///
/// Derived from which timestamps are set rather than stored as a column,
/// because the timestamps are what the writes actually produce — a status
/// column beside them would be a second answer that could disagree after a
/// partial write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueState {
    /// Nothing has happened to it. Safe to deliver.
    Queued,
    /// Handed to a runner, and what became of it is not known. Never
    /// re-delivered; reported to the agent instead. See the migration.
    InDoubt,
    /// It became a `messages` row.
    Settled,
    /// The turn before it did not finish, so it waits for a person.
    Held,
}

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

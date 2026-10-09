//! `queued_prompts`: what a person typed while a turn was running, waiting
//! for its point in the run. Where one has got to is read from which
//! timestamps are set (`queue_state`), not from a status column.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

use crate::db::types::{EpochMs, text_enum_column};

/// When a queued message is handed to the agent.
///
/// The two are not urgency levels; they are different points in the run.
/// `FollowUp` waits for the turn to reach an ending and then starts a new one —
/// "when you have finished all that, also do this". `Interject` goes in at the
/// next point the agent accepts input, between rounds of the turn already
/// going — "stop, do it this way instead".
///
/// Claude Code offers only the second. Having only that one means every thought
/// you queue while something long runs interrupts it, which is the opposite of
/// what queueing is usually for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, strum::IntoStaticStr, strum::EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Delivery {
    FollowUp,
    Interject,
}

impl Delivery {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value
            .parse()
            .map_err(|_| format!("unknown queue delivery mode `{value}`"))
    }
}

text_enum_column!(Delivery);

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

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "queued_prompts")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub conversation_id: String,
    pub content: String,
    pub delivery: Delivery,
    pub position: i32,
    pub created_at: EpochMs,
    pub dispatched_at: Option<EpochMs>,
    pub dispatched_turn_id: Option<String>,
    pub settled_at: Option<EpochMs>,
    pub settled_message_id: Option<String>,
    pub held_at: Option<EpochMs>,
    pub reported_at: Option<EpochMs>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::conversation::Entity",
        from = "Column::ConversationId",
        to = "super::conversation::Column::Id",
        on_delete = "Cascade"
    )]
    Conversation,
}

impl Related<super::conversation::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Conversation.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

impl Model {
    /// Read from which timestamps are set: settled, then dispatched (in
    /// doubt), then held, else still queued.
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

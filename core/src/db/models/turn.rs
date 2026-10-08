use diesel::prelude::*;
use serde::Serialize;

use crate::db::schema::turns;

pub use crate::db::entity::turn::{TurnPhase, TurnStatus};

/// A turn cut short because the model kept making the same call.
///
/// A stable token rather than prose: it is matched on, and it goes in the same
/// column as a provider's error text, which is not. The loop guard stopping a
/// turn is a failure to finish, not a finish — recording it as `done` would
/// have the row claim a clean ending for a turn whose own stop event says it
/// was aborted.
pub const ERROR_LOOP_DETECTED: &str = "loop_detected";

/// A turn as stored.
#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = turns)]
pub struct TurnRow {
    pub id: String,
    pub conversation_id: String,
    pub origin: String,
    pub status: String,
    pub phase: Option<String>,
    pub phase_tool: Option<String>,
    pub error: Option<String>,
    pub started_at: i64,
    pub updated_at: i64,
    pub ended_at: Option<i64>,
    /// When a later turn actually delivered this turn's interruption to the
    /// model. `None` means it still owes the telling — which is not the same as
    /// "this turn is the most recent one", because a turn that dies before
    /// reaching a provider carries nothing and therefore consumes nothing.
    ///
    /// "Delivered" here means delivered to *this turn's own conversation*. A
    /// delegated run owes two tellings; the other one is below.
    pub reported_at: Option<i64>,
    /// When the conversation that *spawned* this turn was told about it. Only
    /// ever set on a sub-agent's turn, and kept apart from `reported_at` because
    /// the two audiences are independent: the user opening the child and saying
    /// one thing must not decide that the parent has heard about a half-written
    /// file.
    pub parent_reported_at: Option<i64>,
    /// Which bot account answered, for a turn a bot started. NULL on desktop.
    ///
    /// Beside `origin` because it is the same kind of fact about the same thing:
    /// where this turn came from. The OneBot config is a single listener today,
    /// so this is one value in practice — but it arrives on the event and is
    /// knowable nowhere else, and a second account connecting to the same port
    /// would otherwise split no history at all.
    pub self_id: Option<i64>,
    /// What set the turn going. See [`crate::turn::TurnTrigger`].
    pub trigger: String,
    /// What did the waking, for a turn nobody asked for.
    pub trigger_ref: Option<String>,
}

impl TurnRow {
    pub fn trigger(&self) -> Result<crate::turn::TurnTrigger, String> {
        crate::turn::TurnTrigger::parse(&self.trigger)
    }

    pub fn status(&self) -> Result<TurnStatus, String> {
        TurnStatus::parse(&self.status)
    }

    pub fn phase(&self) -> Result<Option<TurnPhase>, String> {
        self.phase.as_deref().map(TurnPhase::parse).transpose()
    }
}

#[derive(Debug, Insertable)]
#[diesel(table_name = turns)]
pub struct TurnInsert<'a> {
    pub id: &'a str,
    pub conversation_id: &'a str,
    pub origin: &'a str,
    pub status: &'a str,
    pub phase: Option<&'a str>,
    pub started_at: i64,
    pub updated_at: i64,
    pub self_id: Option<i64>,
    pub trigger: &'a str,
    pub trigger_ref: Option<&'a str>,
}

/// The Diesel reads hand out the entity model; a stored word this build does
/// not know fails the read, as it does on the SeaORM side. Goes with them.
impl TryFrom<TurnRow> for crate::db::entity::turn::Model {
    type Error = String;

    fn try_from(row: TurnRow) -> Result<Self, String> {
        Ok(Self {
            origin: crate::turn::TurnOrigin::parse(&row.origin)?,
            status: TurnStatus::parse(&row.status)?,
            phase: row.phase.as_deref().map(TurnPhase::parse).transpose()?,
            trigger: crate::turn::TurnTrigger::parse(&row.trigger)?,
            id: row.id,
            conversation_id: row.conversation_id,
            phase_tool: row.phase_tool,
            error: row.error,
            started_at: row.started_at,
            updated_at: row.updated_at,
            ended_at: row.ended_at,
            reported_at: row.reported_at,
            parent_reported_at: row.parent_reported_at,
            self_id: row.self_id,
            trigger_ref: row.trigger_ref,
        })
    }
}

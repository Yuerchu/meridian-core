//! `turns`: one row per turn, from the moment it starts to the moment it
//! ends — or to the startup that finds it never did.

use sea_orm::entity::prelude::*;
use serde::Serialize;

use crate::db::types::{EpochMs, text_enum_column};
use crate::turn::{TurnOrigin, TurnTrigger};

/// How a turn ended, or that it has not.
///
/// `Running` is only ever true of the process that wrote it — a turn lives on
/// the stack of the task driving it and does not survive a restart. So a
/// `Running` row read at startup is not a turn still going; it is a turn that
/// never reached its own ending, and `reconcile_interrupted` says so. That is
/// what lets nothing be written from a destructor: destructors do not run for a
/// kill, and leaving the row alone is already the truthful record.
///
/// `Cancelled` is the user pressing Stop. `Interrupted` is a turn that never
/// reached an ending. The transcript has always shown these as the same thing,
/// and they are not: one is a decision, the other is an accident that may have
/// left work half done.
///
/// Stored, `Interrupted` is only ever written by startup reconciliation, so it
/// does mean the process died. Reported — `conversation_snapshot` substitutes
/// it for a `running` row the coordinator is not holding — it means less than
/// that: a task that panicked or was dropped while the application carried on
/// looks identical. Nothing may read a cause into either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, strum::IntoStaticStr, strum::EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TurnStatus {
    Running,
    /// The model submitted a durable plan review and no task remains alive.
    /// Startup must preserve this status instead of diagnosing an interruption.
    WaitingReview,
    Done,
    Cancelled,
    Failed,
    Interrupted,
}

impl TurnStatus {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value.parse().map_err(|_| format!("unknown turn status '{value}'"))
    }
}

/// What a turn was doing when it last said anything.
///
/// Written *before* the thing it names, which is the whole point: whatever is
/// stored when the process dies is where it died. Meaningless once a turn has
/// ended normally.
///
/// `RunningTool` is the one that matters. It means a tool had started — a file
/// may already be written, a command may already have run — and nothing
/// recorded how it went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, strum::IntoStaticStr, strum::EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TurnPhase {
    Streaming,
    AwaitingApproval,
    RunningTool,
    Compacting,
}

impl TurnPhase {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value.parse().map_err(|_| format!("unknown turn phase '{value}'"))
    }
}

text_enum_column!(TurnStatus);
text_enum_column!(TurnPhase);
text_enum_column!(TurnOrigin);
text_enum_column!(TurnTrigger);

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "turns")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub conversation_id: String,
    pub origin: TurnOrigin,
    pub status: TurnStatus,
    pub phase: Option<TurnPhase>,
    pub phase_tool: Option<String>,
    pub error: Option<String>,
    pub started_at: EpochMs,
    pub updated_at: EpochMs,
    pub ended_at: Option<EpochMs>,
    /// When a later turn delivered this turn's interruption to the model;
    /// `None` while it still owes the telling.
    pub reported_at: Option<EpochMs>,
    /// When the conversation that spawned this turn was told about it. Only
    /// ever set on a sub-agent's turn.
    pub parent_reported_at: Option<EpochMs>,
    /// Which bot account answered, for a turn a bot started.
    pub self_id: Option<i64>,
    pub trigger: TurnTrigger,
    /// What did the waking, for a turn nobody asked for.
    pub trigger_ref: Option<String>,
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

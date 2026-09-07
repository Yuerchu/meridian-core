use diesel::prelude::*;
use serde::Serialize;

use crate::db::schema::turns;

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

/// A turn cut short because the model kept making the same call.
///
/// A stable token rather than prose: it is matched on, and it goes in the same
/// column as a provider's error text, which is not. The loop guard stopping a
/// turn is a failure to finish, not a finish — recording it as `done` would
/// have the row claim a clean ending for a turn whose own stop event says it
/// was aborted.
pub const ERROR_LOOP_DETECTED: &str = "loop_detected";

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
}

impl TurnRow {
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
}

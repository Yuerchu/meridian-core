//! `background_tasks`: work that outlives the turn that started it. See
//! `crate::background` for what each column decides and
//! `db::sea::migration::m0003_background_tasks` for the table.

use sea_orm::entity::prelude::*;

use crate::db::types::{EpochMs, SqlBool, checked_text_enum};

checked_text_enum!(
    /// Whose background task it is.
    BackgroundRunner {
        /// Run by this app's own `run_command`.
        Native = "native",
        /// Reported by a hosted Claude Code session.
        ClaudeCode = "claude_code",
    }
);

checked_text_enum!(
    /// What kind of work it is.
    BackgroundKind {
        Command = "command",
        Agent = "agent",
    }
);

checked_text_enum!(
    /// Where a task stands.
    BackgroundState {
        Running = "running",
        /// Exited with code 0.
        Completed = "completed",
        /// Exited non-zero, timed out, or could not be run.
        Failed = "failed",
        /// Somebody stopped it — `ended_reason` says who.
        Stopped = "stopped",
        /// Was running when the process watching it died. How it ended is
        /// unknown.
        Lost = "lost",
    }
);

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "background_tasks")]
pub struct Model {
    /// Short and model-facing: the id the model passes back to read or stop it.
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub conversation_id: String,
    pub runner: BackgroundRunner,
    pub external_id: Option<String>,
    pub spawned_turn_id: Option<String>,
    pub spawned_call_id: Option<String>,
    pub kind: BackgroundKind,
    pub command: Option<String>,
    pub description: Option<String>,
    pub cwd: Option<String>,
    /// What confined it, as `SandboxBackend` spells it. `None` for work
    /// another runner did.
    pub sandbox: Option<String>,
    pub state: BackgroundState,
    pub exit_code: Option<i32>,
    pub ended_reason: Option<String>,
    pub output_path: Option<String>,
    pub output_bytes: i64,
    pub output_truncated: SqlBool,
    pub started_at: EpochMs,
    pub ended_at: Option<EpochMs>,
    /// When the model was told this task ended. `None` on an ended task is a
    /// debt the next turn pays.
    pub notified_at: Option<EpochMs>,
    pub notified_turn_id: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    /// `CASCADE`: a conversation's tasks go with it.
    #[sea_orm(
        belongs_to = "super::conversation::Entity",
        from = "Column::ConversationId",
        to = "super::conversation::Column::Id",
        on_delete = "Cascade"
    )]
    Conversation,
}

impl ActiveModelBehavior for ActiveModel {}

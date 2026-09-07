//! The turn loop, and the ports the two runners plug into it.
//!
//! The desktop and OneBot runners were two copies of the same loop that had
//! drifted — the same retry ladder, the same stream state machine, the same row
//! writes, with the approval judgement fixed on one side and not the other. This
//! is the one copy; everything the two genuinely disagree about arrives as a
//! port rather than as a fork in the code.
//!
//! Nothing in here knows about Tauri. That is deliberate and worth keeping: it
//! is what lets the loop be tested without an app handle, and what will let a
//! sub-agent run one without owning a window.
//!
//! The rule is about the import graph, not about the text. A file with no
//! `tauri::` in it can still pull the whole framework in through one type — this
//! module imported `ApprovalDecision` from `state.rs`, two declarations below an
//! `AppHandle`, and read as clean. So what has to hold is that every `use
//! crate::` in here names `agent`, `db`, `provider`, `turn` or `util`, and that
//! those five are themselves free of it. Adding a sixth is the moment to check
//! rather than assume.

pub(crate) mod approval;
pub(crate) mod compaction;
pub(crate) mod ports;
pub(crate) mod stream;
pub(crate) mod transcript;
pub(crate) mod transitions;
pub(crate) mod turn;

pub use approval::ApprovalDecision;
pub use compaction::CompactionPolicy;
pub(crate) use ports::SteeredOrigin;
pub use ports::{
    Approvals, Commentary, Steered, Steering, Stranded, SubAgentReport, SubAgentSpec, SubAgentStatus, SubAgents,
    SurfaceTools, TurnPorts,
};
pub(crate) use stream::consume_stream;
pub(crate) use transcript::{append_steering, append_tool_result, begin_assistant, complete_assistant};
pub use transcript::{in_phase, write_steering};
pub use transitions::{PlanReadResult, PlanUpdateResult, SubmitPlanRequest, Transitions, UpdatePlanRequest};
pub use turn::{ApprovalRule, TurnOutcome, TurnProgress, TurnServices, TurnSetup, WithheldWording, run_turn};

/// Where a turn's progress goes while it is still happening.
///
/// Returns a `Result` because the two runners do not agree on what a failed
/// send means, and that disagreement is load-bearing rather than accidental:
///
/// - The desktop treats it as fatal. Its events *are* the answer — a window
///   that missed one is showing a transcript that never catches up — so a send
///   that fails takes the turn down with it and the user is told.
/// - OneBot treats it as nothing. Its answer goes out over the chat transport;
///   these events are a courtesy to a desktop window that may not even be open.
///
/// So the desktop adapter returns the real error and the OneBot one returns
/// `Ok(())` whatever happened. Folding this into `Option<&dyn Emit>` would
/// quietly give one of them the other's behaviour.
///
/// `None` is the third case, and means neither: emit nothing at all.
pub trait Emit: Send + Sync {
    fn emit(&self, channel: &str, payload: serde_json::Value) -> Result<(), String>;

    fn emit_chat(&self, event: crate::events::ChatStreamEvent) -> Result<(), String> {
        let payload = serde_json::to_value(event).map_err(|e| format!("could not serialize chat stream event: {e}"))?;
        self.emit(crate::events::CHAT_STREAM_CHANNEL, payload)
    }

    fn emit_conversation_updated(&self, conversation_id: &str) -> Result<(), String> {
        let payload = serde_json::to_value(crate::events::ConversationUpdatedEvent::new(conversation_id))
            .map_err(|e| format!("could not serialize conversation-updated event: {e}"))?;
        self.emit(crate::events::CONVERSATION_UPDATED_CHANNEL, payload)
    }

    fn emit_compact_start(&self, event: crate::events::CompactStartEvent) -> Result<(), String> {
        let payload =
            serde_json::to_value(event).map_err(|e| format!("could not serialize compact-start event: {e}"))?;
        self.emit(crate::events::COMPACT_START_CHANNEL, payload)
    }

    fn emit_compact_done(&self, event: crate::events::CompactDoneEvent) -> Result<(), String> {
        let payload =
            serde_json::to_value(event).map_err(|e| format!("could not serialize compact-done event: {e}"))?;
        self.emit(crate::events::COMPACT_DONE_CHANNEL, payload)
    }

    fn emit_plan_review_requested(&self, event: crate::events::PlanReviewEvent) -> Result<(), String> {
        let payload =
            serde_json::to_value(event).map_err(|e| format!("could not serialize plan-review-requested event: {e}"))?;
        self.emit(crate::events::PLAN_REVIEW_REQUESTED_CHANNEL, payload)
    }

    fn emit_plan_review_updated(&self, event: crate::events::PlanReviewEvent) -> Result<(), String> {
        let payload =
            serde_json::to_value(event).map_err(|e| format!("could not serialize plan-review-updated event: {e}"))?;
        self.emit(crate::events::PLAN_REVIEW_UPDATED_CHANNEL, payload)
    }
}

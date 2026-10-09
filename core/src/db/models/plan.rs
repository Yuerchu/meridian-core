pub use crate::db::entity::mode_artifact::PlanStatus;

/// What a mode produced for the user to approve. Plans are the only kind today;
/// the column exists so a second mode with an approvable output does not need a
/// second table.
pub const KIND_PLAN: &str = "plan";

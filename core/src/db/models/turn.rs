pub use crate::db::entity::turn::{TurnPhase, TurnStatus};

/// A turn cut short because the model kept making the same call.
///
/// A stable token rather than prose: it is matched on, and it goes in the same
/// column as a provider's error text, which is not. The loop guard stopping a
/// turn is a failure to finish, not a finish — recording it as `done` would
/// have the row claim a clean ending for a turn whose own stop event says it
/// was aborted.
pub const ERROR_LOOP_DETECTED: &str = "loop_detected";

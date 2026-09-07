//! Detects a model stuck repeating the same tool call with identical
//! arguments, warns it, and aborts the turn if it keeps going.
//!
//! **A run, not a set.** One fingerprint is held, so a repeat with anything in
//! between resets the count — which is right for "is the model stuck" and
//! useless for "has this already been refused". [`super::denied`] is the other
//! question and keeps its own memory.
//!
//! What the two share is [`super::call_identity`], which replaced a private
//! `DefaultHasher` into a `u64`. Nothing here needed the width or the
//! stability; what it needed was to agree with the denial memory about what
//! "the same call" means, because two answers to that would be visible as one
//! guard firing on a call the other considered different.

use super::call_identity::{Aspect, CallIdentity, identify};

/// Consecutive identical calls before a warning is injected instead of executing.
pub(crate) const LOOP_WARN_AFTER: u32 = 3;
/// Consecutive identical calls before the turn is aborted.
pub(crate) const LOOP_ABORT_AFTER: u32 = 5;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LoopVerdict {
    Proceed,
    Warn(u32),
    Abort(u32),
}

#[derive(Debug, Default)]
pub(crate) struct ToolLoopGuard {
    last: Option<CallIdentity>,
    consecutive: u32,
}

impl ToolLoopGuard {
    /// Record one tool call and judge whether the model is looping.
    ///
    /// Always [`Aspect::Ordinary`]: this counts what the *model* issued, and a
    /// sandbox escalation is the app asking again about a call the model made
    /// once. Counting it as a second identical call would have the retry push
    /// the model towards the abort threshold for something it did not repeat.
    pub(crate) fn observe(&mut self, name: &str, arguments: &str) -> LoopVerdict {
        let fingerprint = identify(name, arguments, Aspect::Ordinary);
        if self.last == Some(fingerprint) {
            self.consecutive += 1;
        } else {
            self.last = Some(fingerprint);
            self.consecutive = 1;
        }

        if self.consecutive >= LOOP_ABORT_AFTER {
            LoopVerdict::Abort(self.consecutive)
        } else if self.consecutive >= LOOP_WARN_AFTER {
            LoopVerdict::Warn(self.consecutive)
        } else {
            LoopVerdict::Proceed
        }
    }
}

/// Synthetic tool result injected instead of executing a looping call.
pub(crate) fn loop_warning_message(name: &str, count: u32) -> String {
    format!(
        "Warning: you have called `{name}` with identical arguments {count} times in a row. \
         Repeating the same call will not change the result. Try a different approach, or \
         explain to the user why you are blocked."
    )
}

/// Synthetic tool result injected when the turn is aborted.
pub(crate) fn loop_abort_message(name: &str, count: u32) -> String {
    format!(
        "Aborting: `{name}` was called with identical arguments {count} times in a row. \
         The turn has been stopped to prevent an infinite loop."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warns_then_aborts_on_identical_calls() {
        let mut guard = ToolLoopGuard::default();
        assert_eq!(guard.observe("read_file", r#"{"path":"a.txt"}"#), LoopVerdict::Proceed);
        assert_eq!(guard.observe("read_file", r#"{"path":"a.txt"}"#), LoopVerdict::Proceed);
        assert_eq!(guard.observe("read_file", r#"{"path":"a.txt"}"#), LoopVerdict::Warn(3));
        assert_eq!(guard.observe("read_file", r#"{"path":"a.txt"}"#), LoopVerdict::Warn(4));
        assert_eq!(guard.observe("read_file", r#"{"path":"a.txt"}"#), LoopVerdict::Abort(5));
    }

    #[test]
    fn resets_on_different_call() {
        let mut guard = ToolLoopGuard::default();
        guard.observe("read_file", r#"{"path":"a.txt"}"#);
        guard.observe("read_file", r#"{"path":"a.txt"}"#);
        assert_eq!(guard.observe("read_file", r#"{"path":"b.txt"}"#), LoopVerdict::Proceed);
        assert_eq!(guard.observe("read_file", r#"{"path":"a.txt"}"#), LoopVerdict::Proceed);
    }

    #[test]
    fn resets_on_different_tool_with_same_args() {
        let mut guard = ToolLoopGuard::default();
        guard.observe("read_file", r#"{"path":"a.txt"}"#);
        guard.observe("read_file", r#"{"path":"a.txt"}"#);
        assert_eq!(
            guard.observe("delete_file", r#"{"path":"a.txt"}"#),
            LoopVerdict::Proceed
        );
    }

    #[test]
    fn formatting_differences_do_not_defeat_detection() {
        let mut guard = ToolLoopGuard::default();
        guard.observe("read_file", r#"{"path":"a.txt","limit":10}"#);
        guard.observe("read_file", r#"{ "limit": 10, "path": "a.txt" }"#);
        assert_eq!(
            guard.observe("read_file", r#"{"path":"a.txt","limit":10}"#),
            LoopVerdict::Warn(3)
        );
    }

    #[test]
    fn non_json_arguments_compare_verbatim() {
        let mut guard = ToolLoopGuard::default();
        guard.observe("custom", "not json");
        guard.observe("custom", "not json");
        assert_eq!(guard.observe("custom", "not json"), LoopVerdict::Warn(3));
        assert_eq!(guard.observe("custom", "not json 2"), LoopVerdict::Proceed);
    }
}

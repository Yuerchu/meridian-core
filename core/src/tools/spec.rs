//! What a tool *is*, declared once beside the tool.
//!
//! Every built-in tool used to be classified by being listed somewhere: the
//! reviewers' whitelist, plan mode's whitelist, the Explore sub-agent's set,
//! the parallel-safe overrides — a dozen lists in two crates and a front end,
//! each a separate place to forget a new tool or to disagree with the others
//! (the read-only list had four names, the Explore list eight). Each of those
//! is now a question asked of [`ToolSpec`], which the tool's own `impl` must
//! answer.
//!
//! **No `Default`, on purpose.** Every field is a decision with a safe and an
//! unsafe answer — `reviewer: true` hands the tool to an agent that must not
//! change anything — so a new tool states every one of them or does not
//! compile. `..Default::default()` would be the way round that, and there is
//! nothing for it to call.
//!
//! The lists stay allowlists in effect: `false` is the answer for every new
//! tool until somebody writes `true` beside it.
//!
//! What is *not* here: [`super::Tool::reach`] and
//! [`super::Tool::default_permission`], which are rules about a particular call
//! or a user's setting rather than facts about the tool, and anything decided
//! per turn (the skill enum, the sub-agent models, a provider-side search
//! displacing ours).

use serde::Serialize;

/// The side effect a tool can have — what it is, not what one call does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// Reads and returns; changes nothing anywhere.
    Read,
    /// Writes, edits, moves or deletes files.
    WriteFiles,
    /// Runs a program.
    Exec,
    /// Reaches the network.
    Network,
    /// Writes this app's own state: memories, redaction rules, the checklist,
    /// the plan document.
    AppState,
    /// Sends something to somebody: a sticker, a QQ message or action.
    Messaging,
    /// Asks the person a question and waits.
    Interactive,
    /// Starts another agent.
    Delegate,
    /// Moves the conversation between modes.
    ModeTransition,
}

/// The static facts about a tool. See the module header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ToolSpec {
    pub effect: Effect,
    /// The agent loop runs it itself (it needs the turn: a question, a mode
    /// change, a sub-agent); the registry's `execute` only refuses.
    pub loop_handled: bool,
    /// Offered in plan mode. `run_command` is the deliberate soft spot — see
    /// `agent::modes`.
    pub plan_mode: bool,
    /// Given to the Explore sub-agent.
    pub explore: bool,
    /// Given to the reviewers — the hook reviewer and the automatic approval
    /// reviewer — which must not change anything and must not read opinions
    /// formed in other conversations.
    pub reviewer: bool,
    /// Safe to run beside other parallel-safe calls in one batch. Only
    /// meaningful for a call that also needs no approval.
    pub parallel: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;
    use std::collections::BTreeSet;

    fn registry() -> ToolRegistry {
        ToolRegistry::new(
            std::path::PathBuf::from("/nonexistent"),
            std::path::PathBuf::from("/nonexistent"),
            std::sync::Arc::new(crate::redaction::RedactionEngine::disabled()),
        )
    }

    fn set<'a>(names: impl IntoIterator<Item = &'a str>) -> BTreeSet<&'a str> {
        names.into_iter().collect()
    }

    /// Each set is what the hand-kept list it replaced said, name for name.
    /// Changing one is a decision about who may hold a tool, so it is made
    /// here, in review, rather than by a field flipping unnoticed.
    #[test]
    fn the_classifications_are_the_lists_they_replaced() {
        let r = registry();
        assert_eq!(
            set(r.builtin_names_where(|s| s.reviewer)),
            set(["read_file", "search_files", "glob", "list_directory"]),
            "the reviewers' whitelist"
        );
        #[cfg_attr(target_os = "android", allow(unused_mut))]
        let mut plan = set([
            "ask_user",
            "glob",
            "list_directory",
            "list_memories",
            "load_skill",
            "read_file",
            "read_plan",
            "recall_memory",
            "search_files",
            "update_todos",
            "update_plan",
            "web_search",
        ]);
        #[cfg(not(target_os = "android"))]
        plan.extend(["run_command", "read_background_output", "list_background_tasks"]);
        assert_eq!(set(r.builtin_names_where(|s| s.plan_mode)), plan, "plan mode");
        assert_eq!(
            set(r.builtin_names_where(|s| s.explore)),
            set([
                "read_file",
                "search_files",
                "glob",
                "list_directory",
                "web_search",
                "recall_memory",
                "list_memories",
                "read_app_logs",
            ]),
            "the Explore sub-agent"
        );
        #[cfg_attr(target_os = "android", allow(unused_mut))]
        let mut parallel = set([
            "read_app_logs",
            "glob",
            "list_directory",
            "recall_memory",
            "list_memories",
            "read_conversation",
            "read_file",
            "list_redaction_rules",
            "search_files",
            "load_skill",
            "web_search",
        ]);
        #[cfg(not(target_os = "android"))]
        parallel.extend(["read_background_output", "list_background_tasks"]);
        assert_eq!(set(r.builtin_names_where(|s| s.parallel)), parallel, "parallel-safe");
        assert_eq!(
            set(r.builtin_names_where(|s| s.loop_handled)),
            set([
                "ask_user",
                "enter_plan",
                "exit_plan",
                "read_plan",
                "update_plan",
                "run_agent"
            ]),
            "handled by the loop"
        );
    }

    /// What the fields must never say together, whatever a new tool claims.
    #[test]
    fn no_tool_that_changes_files_is_handed_to_a_restricted_agent() {
        let r = registry();
        for name in r.builtin_names_where(|_| true) {
            let s = r.builtin_spec(name).unwrap();
            if s.reviewer {
                assert_eq!(s.effect, Effect::Read, "{name}: a reviewer holds readers only");
            }
            if s.explore {
                assert!(
                    matches!(s.effect, Effect::Read | Effect::Network),
                    "{name}: Explore reads and searches"
                );
            }
            if s.plan_mode {
                // run_command is the one writer plan mode keeps, on purpose.
                assert!(
                    !matches!(s.effect, Effect::WriteFiles | Effect::Messaging | Effect::Delegate)
                        && (s.effect != Effect::Exec || name == "run_command"),
                    "{name}: plan mode changes nothing outside the conversation"
                );
            }
            if s.parallel {
                assert!(
                    matches!(s.effect, Effect::Read | Effect::Network),
                    "{name}: only side-effect-free tools run side by side"
                );
            }
        }
    }
}

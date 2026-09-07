//! Collaboration modes: which stage of the work the conversation is in.
//!
//! A mode narrows what the assistant may do and adds a paragraph of discipline
//! to the system prompt. That is all it does — permission policy, model choice
//! and reasoning tier stay where they are. Keeping those orthogonal is
//! deliberate: once "which stage" and "how loose are permissions" share one
//! enum, every check downstream turns into a compound condition.
//!
//! Every mode is defined in this file. Reading it should be enough to know
//! exactly what any mode does, without chasing the behaviour through the tool
//! assembly, the prompt builder and the dispatch loop.

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatMode {
    Work,
    Plan,
}

impl ChatMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Work => WORK_MODE,
            Self::Plan => PLAN_MODE,
        }
    }

    pub const fn canonical_storage(self) -> Option<&'static str> {
        match self {
            Self::Work => None,
            Self::Plan => Some(PLAN_MODE),
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            WORK_MODE => Ok(Self::Work),
            PLAN_MODE => Ok(Self::Plan),
            _ => Err(format!("unknown collaboration mode {value:?}")),
        }
    }

    pub const fn spec(self) -> &'static ModeSpec {
        match self {
            Self::Work => &WORK,
            Self::Plan => &PLAN,
        }
    }
}

/// A mode may only ever *narrow* what the assistant can already do. The one
/// exception is the pair of transition tools, which the assistant cannot have
/// enabled in advance because they are meaningless outside the modes that own
/// them. Which of them is offered follows from the declarations below rather
/// than from a hand-written list, so a mode cannot end up reachable but not
/// leavable.
pub struct ModeSpec {
    pub id: &'static str,
    /// Whitelist intersected with the assistant's own tool set. `None` means
    /// the mode does not narrow anything.
    pub tools: Option<&'static [&'static str]>,
    /// Tools owned by this mode rather than by an assistant configuration.
    /// They are injected while the mode is active and stripped everywhere
    /// else. Plan document I/O lives here so every planning assistant reaches
    /// the same durable draft regardless of its ordinary tool preset.
    pub owned_tools: &'static [&'static str],
    /// Prepended to the system prompt, ahead of every other block.
    pub instructions: Option<&'static str>,
    /// Tool the model calls to ask to *enter* this mode. Offered while in any
    /// other mode.
    pub enter_tool: Option<&'static str>,
    /// Tool the model calls to ask for this mode to end. Offered only inside it.
    pub exit_tool: Option<&'static str>,
    /// Mode to switch to once the user approves the exit.
    pub exit_to: Option<&'static str>,
}

pub const WORK_MODE: &str = "work";
pub const PLAN_MODE: &str = "plan";
pub const ENTER_PLAN_TOOL: &str = "enter_plan";
pub const EXIT_PLAN_TOOL: &str = "exit_plan";
pub const READ_PLAN_TOOL: &str = "read_plan";
pub const UPDATE_PLAN_TOOL: &str = "update_plan";

/// Tools that cannot change anything outside the conversation.
///
/// A whitelist rather than a blacklist of writers, because the set of writers
/// is not knowable: `save_memory` and `delete_memory` write to the database
/// rather than the filesystem, and custom tools and MCP tools have no declared
/// read/write property at all. Anything unrecognised is therefore excluded by
/// construction.
///
/// `run_command` is the deliberate soft spot. Without it the model cannot run
/// a test or read `git log` to check that a plan is even feasible, which is
/// most of what makes a plan worth reading. It can also write files, so the
/// mode prompt has to carry that constraint instead.
const PLAN_TOOLS: &[&str] = &[
    "ask_user",
    "glob",
    "list_directory",
    "list_memories",
    "load_skill",
    "read_file",
    READ_PLAN_TOOL,
    "recall_memory",
    "run_command",
    "search_files",
    "update_todos",
    UPDATE_PLAN_TOOL,
    "web_search",
];

const WORK: ModeSpec = ModeSpec {
    id: WORK_MODE,
    tools: None,
    owned_tools: &[],
    instructions: None,
    enter_tool: None,
    exit_tool: None,
    exit_to: None,
};

const PLAN: ModeSpec = ModeSpec {
    id: PLAN_MODE,
    tools: Some(PLAN_TOOLS),
    owned_tools: &[READ_PLAN_TOOL, UPDATE_PLAN_TOOL],
    instructions: Some(PLAN_INSTRUCTIONS),
    enter_tool: Some(ENTER_PLAN_TOOL),
    exit_tool: Some(EXIT_PLAN_TOOL),
    exit_to: Some(WORK_MODE),
};

pub const MODES: &[&ModeSpec] = &[&WORK, &PLAN];

/// The mode for a stored id. Absence is the canonical spelling of work;
/// anything present must name one of the declared modes exactly.
pub fn resolve(id: Option<&str>) -> Result<&'static ModeSpec, String> {
    match id {
        Some(id) => Ok(ChatMode::parse(id)?.spec()),
        None => Ok(&WORK),
    }
}

/// The mode a tool asks to enter, if any. Lets the agent loop handle every
/// mode's entry point without naming one.
pub fn by_enter_tool(tool: &str) -> Option<&'static ModeSpec> {
    MODES.iter().find(|m| m.enter_tool == Some(tool)).copied()
}

/// Every tool that only exists to move between modes. All of them are stripped
/// before the mode puts back the ones it actually offers, so no conversation is
/// shown a transition it cannot make.
pub fn transition_tools() -> impl Iterator<Item = &'static str> {
    MODES.iter().flat_map(|m| [m.enter_tool, m.exit_tool]).flatten()
}

/// Tools supplied by a mode rather than an assistant configuration.
pub fn owned_tools() -> impl Iterator<Item = &'static str> {
    MODES.iter().flat_map(|m| m.owned_tools.iter().copied())
}

/// Where the conversation is, and whether this turn can move it.
///
/// One value rather than a mode beside a flag, because it is the pairing that
/// would otherwise only be a comment that is dangerous: a narrowed mode with no
/// way between modes leaves the conversation holding an exit tool that reaches
/// the registry and answers with an error the model then has to read. A bool
/// lets that be written down; this does not.
///
/// Whether a turn can move modes is not a preference. It is whether the runner
/// has anywhere to put the question — the desktop shows a card and waits, and a
/// runner with no `Transitions` port has no equivalent.
#[derive(Clone, Copy)]
pub enum Modes {
    /// The runner has a transitions port. This is the mode it is in, and it may
    /// be offered the way out of it or into another.
    Switchable(&'static ModeSpec),
    /// The runner has no way between modes — OneBot today, a sub-agent later.
    /// Work mode, and no transition tool at all.
    Fixed,
}

impl Modes {
    pub fn spec(&self) -> &'static ModeSpec {
        match self {
            Self::Switchable(mode) => mode,
            Self::Fixed => &WORK,
        }
    }

    pub(crate) fn switchable(&self) -> bool {
        matches!(self, Self::Switchable(_))
    }
}

impl ModeSpec {
    /// Whether entering this mode from the given tool set would actually take
    /// anything away.
    ///
    /// An assistant that only searches the web and asks questions loses nothing
    /// by planning first, so offering it the way in is pure prompt overhead on
    /// every single turn. The check is derived rather than configured, so it
    /// keeps working for a mode added later.
    fn would_narrow(&self, current: &[String]) -> bool {
        match self.tools {
            None => false,
            Some(allowed) => current.iter().any(|name| !allowed.contains(&name.as_str())),
        }
    }

    /// Transitions offered while in this mode: the way out of it, plus the way
    /// into any other mode that would meaningfully change what is possible.
    pub fn offered_tools(&self, current_tools: &[String]) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = self.owned_tools.to_vec();
        out.extend(self.exit_tool);
        out.extend(
            MODES
                .iter()
                .filter(|m| m.id != self.id && m.would_narrow(current_tools))
                .filter_map(|m| m.enter_tool),
        );
        out
    }
}

/// Written for this codebase rather than adapted from anywhere: the tool set is
/// already narrowed by the time the model reads this, so the prompt does not
/// need to spend itself repeating that files are off limits. It covers the one
/// hole the whitelist cannot close (`run_command`) and what a finished plan
/// should contain.
const PLAN_INSTRUCTIONS: &str = "\
# Plan mode

You are planning, not building. Explore the code, settle the approach with the \
user, and hand back a plan they can approve. The tools that modify anything have \
been removed for this mode — do not describe edits as though you had made them.

- `run_command` is still available so you can check facts: run tests, read \
`git log`, inspect the build. Do not use it to write, move or delete anything, \
and do not use it to work around the missing editing tools.
- Read before you assume. A plan built on a guess about what the code currently \
does is worse than no plan.
- Ask the user when a decision is genuinely theirs — an ambiguous requirement, a \
trade-off with no clear winner. Do not ask what the code can tell you.
- Keep the plan in Meridian's private `plan.md`, not in the chat response. Call \
`read_plan` before changing it and use `update_plan` with a unified patch. The \
first draft is an `Add File: plan.md`; every revision is an `Update File` patch. \
Do not recreate or resend the whole document.
- Call `exit_plan` with no content when the saved `plan.md` is ready. It shows \
that revision to the user for approval; if they send it back with feedback, \
revise the file with another patch and call it again.
- A plan is ready when it names the files to change and what changes in each, \
points at existing code worth reusing, and says how to tell afterwards that it \
worked. Leave out anything the implementer can work out for themselves.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_missing_or_declared_mode_ids_are_accepted() {
        assert_eq!(resolve(None).unwrap().id, WORK_MODE);
        assert_eq!(resolve(Some("work")).unwrap().id, WORK_MODE);
        assert!(resolve(Some("retired-mode")).is_err());
        assert!(resolve(Some("")).is_err());
    }

    fn tools(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn work_offers_the_way_into_plan_and_plan_offers_the_way_out() {
        let editing = tools(&["read_file", "write_file"]);

        let work = resolve(None).unwrap().offered_tools(&editing);
        assert_eq!(work, [ENTER_PLAN_TOOL], "no exit from the default mode");

        let plan = resolve(Some("plan")).unwrap().offered_tools(&editing);
        assert_eq!(
            plan,
            [READ_PLAN_TOOL, UPDATE_PLAN_TOOL, EXIT_PLAN_TOOL],
            "the draft tools and no way to re-enter the active mode"
        );
    }

    /// The entry point costs prompt space on every turn, so it only appears
    /// when planning would actually restrict something.
    #[test]
    fn a_read_only_assistant_is_not_offered_planning() {
        let read_only = tools(&["read_file", "web_search", "ask_user"]);
        assert!(resolve(None).unwrap().offered_tools(&read_only).is_empty());

        let with_shell = tools(&["read_file", "run_command", "delete_file"]);
        assert_eq!(resolve(None).unwrap().offered_tools(&with_shell), [ENTER_PLAN_TOOL]);
    }

    #[test]
    fn an_assistant_with_no_tools_at_all_is_not_offered_planning() {
        assert!(resolve(None).unwrap().offered_tools(&[]).is_empty());
    }

    /// The pairing a bool would have allowed: a narrowed mode with no way out
    /// of it. `Fixed` cannot name a mode, so a runner that cannot switch is
    /// always in the one that narrows nothing.
    #[test]
    fn a_runner_that_cannot_switch_is_in_the_mode_that_narrows_nothing() {
        assert_eq!(Modes::Fixed.spec().id, WORK_MODE);
        assert!(Modes::Fixed.spec().tools.is_none());
        assert!(!Modes::Fixed.switchable());
    }

    #[test]
    fn every_mode_that_can_be_entered_can_also_be_left() {
        for m in MODES {
            if m.enter_tool.is_some() {
                assert!(m.exit_tool.is_some(), "{} can be entered but not left", m.id);
                assert!(m.exit_to.is_some(), "{} has no destination on exit", m.id);
            }
        }
    }

    #[test]
    fn transition_tools_are_recognised_by_their_owner() {
        assert_eq!(by_enter_tool(ENTER_PLAN_TOOL).map(|m| m.id), Some(PLAN_MODE));
        assert!(by_enter_tool(EXIT_PLAN_TOOL).is_none(), "leaving is not entering");
        assert!(by_enter_tool("read_file").is_none());

        let all: Vec<&str> = transition_tools().collect();
        assert!(all.contains(&ENTER_PLAN_TOOL) && all.contains(&EXIT_PLAN_TOOL));
    }

    #[test]
    fn the_read_only_set_excludes_every_writer() {
        // Database writers count: the whitelist is about side effects, not
        // about the filesystem.
        for writer in [
            "write_file",
            "edit_file",
            "apply_patch",
            "delete_file",
            "move_file",
            "save_memory",
            "delete_memory",
        ] {
            assert!(
                !PLAN_TOOLS.contains(&writer),
                "{writer} can change things and must not be in plan mode"
            );
        }
    }

    #[test]
    fn plan_can_leave_itself() {
        let plan = resolve(Some("plan")).unwrap();
        assert_eq!(plan.exit_tool, Some(EXIT_PLAN_TOOL));
        assert_eq!(plan.exit_to, Some(WORK_MODE));
        assert!(plan.offered_tools(&tools(&["read_file"])).contains(&EXIT_PLAN_TOOL));
        // Neither transition tool is in the whitelist: they are added by the
        // mode itself, since no assistant would have enabled them up front.
        assert!(!PLAN_TOOLS.contains(&EXIT_PLAN_TOOL));
        assert!(!PLAN_TOOLS.contains(&ENTER_PLAN_TOOL));
        assert!(
            MODES.iter().any(|m| Some(m.id) == plan.exit_to),
            "exit_to must name a real mode"
        );
    }

    #[test]
    fn mode_ids_are_unique() {
        let mut ids: Vec<&str> = MODES.iter().map(|m| m.id).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count);
    }
}

//! Atomic capability mixins for [`crate::agent::character::CharacterBase`].
//!
//! Each mixin bundles one orthogonal capability: the exact tools it contributes,
//! the static prompt section it appends to `base_prompt` (when its tools are
//! active), and its optional read-only downgrade for `Explore` sub-agents.
//!
//! Building turns additively from mixins eliminates the old "register everything
//! then strip with blacklists" drift that produced ghost tools (advertised in
//! the schema but unwired at runtime).

use std::collections::HashSet;

use crate::agent::hooks::{DeclaredTool, MixinResolveContext, ToolApprovalPolicy, ToolLoading};
use crate::agent::sub_agents::SubAgentCatalog;
use crate::provider::ToolDefinition;
use crate::tools::ToolRegistry;
use crate::tools::command_shell::CommandShell;

/// Built-in capability mixins provided by `meridian-core`.
///
/// External servers (such as `Meridian Server`) and plugins compose their own
/// characters from [`crate::agent::character::CharacterBase`] + any subset of
/// these built-ins + custom [`crate::agent::hooks::ToolMixin`] implementations.
#[derive(Debug, Clone)]
pub enum BuiltinMixin {
    /// Filesystem inspection and (optionally) workspace editing + conversation
    /// reference reading.
    WorkspaceFs { writable: bool },
    /// Shell command execution (`run_command`).
    ShellExec { shell: Option<CommandShell> },
    /// Background command inspection and cancellation.
    BackgroundJobs,
    /// Interactive user prompt form (`ask_user`).
    InteractiveUi,
    /// Plan mode transitions and plan document tools.
    PlanMode,
    /// Multi-step task checklist (`update_todos`).
    Todo,
    /// Local web search (`web_search`), automatically suppressed when
    /// superseded by a provider-side `server_tools` entry.
    WebSearch,
    /// Bound skills catalog (`load_skill`).
    Skills,
    /// Application log diagnostics (`read_app_logs`) with prompt-engineered
    /// guidance so the assistant adapts its reporting style to user instructions.
    Diagnostics { guidance: Option<String> },
    /// Built-in image and video generation (`generate_image`, `generate_video`).
    MediaGen,
    /// Session/global redaction rule management.
    RedactionTools,
    /// Sub-agent delegation (`run_agent`) carrying the resolved model catalog.
    SubAgent { catalog: SubAgentCatalog },
    /// External MCP tools already filtered and annotated with loading/approval
    /// policy by [`crate::mcp`].
    Mcp { tools: Vec<DeclaredTool> },
    /// Cross-turn memory tools (`recall_memory`, `list_memories`, and optionally
    /// `save_memory` / `delete_memory`).
    Memory { writable: bool },
    /// User-defined custom tools registered on [`ToolRegistry`].
    CustomTools,
}

const READ_ONLY_FS_TOOLS: &[&str] = &["read_file", "list_directory", "search_files", "glob"];

const WRITABLE_FS_TOOLS: &[&str] = &[
    "read_file",
    "list_directory",
    "search_files",
    "glob",
    "write_file",
    "edit_file",
    "apply_patch",
    "delete_file",
    "move_file",
    "read_conversation",
];

const SHELL_EXEC_TOOLS: &[&str] = &["run_command"];

const BACKGROUND_JOB_TOOLS: &[&str] = &[
    "read_background_output",
    "stop_background_task",
    "list_background_tasks",
];

const INTERACTIVE_UI_TOOLS: &[&str] = &["ask_user"];

const PLAN_MODE_TOOLS: &[&str] = &["enter_plan", "exit_plan", "read_plan", "update_plan"];

const TODO_TOOLS: &[&str] = &["update_todos"];

const WEB_SEARCH_TOOLS: &[&str] = &["web_search"];

const SKILLS_TOOLS: &[&str] = &["load_skill"];

const DIAGNOSTICS_TOOLS: &[&str] = &["read_app_logs"];

const MEDIA_GEN_TOOLS: &[&str] = &["generate_image", "generate_video"];

const REDACTION_TOOLS: &[&str] = &[
    "add_redaction_rule",
    "list_redaction_rules",
    "remove_redaction_rule",
];

const SUB_AGENT_TOOLS: &[&str] = &["run_agent"];

const READ_ONLY_MEMORY_TOOLS: &[&str] = &["recall_memory", "list_memories"];

const WRITABLE_MEMORY_TOOLS: &[&str] = &["save_memory", "recall_memory", "list_memories", "delete_memory"];

/// Default prompt-engineered guidance for [`BuiltinMixin::Diagnostics`].
///
/// Kept deliberately flexible so user instructions or assistant personas can
/// control tone, level of detail, and audience (e.g. concise operator alerts vs.
/// deep technical root-cause analysis).
pub const DEFAULT_DIAGNOSTICS_PROMPT: &str = "\
# Diagnostics\n\n\
You have `read_app_logs` to inspect Meridian's internal runtime logs when diagnosing failures \
(provider/API errors, expired credentials, MCP server failures, timeouts, or tool denials).\n\n\
- Check `read_app_logs` from evidence rather than guessing when something just failed.\n\
- Follow the user's instructions and current context when deciding how much detail to report \
(e.g. a brief actionable explanation such as asking the operator to rotate an expired credential, \
or a detailed technical breakdown when requested).\n\
- Summarize relevant findings instead of dumping raw log walls unless the user explicitly asks \
for raw entries, and never expose secrets or tokens that might appear in diagnostic fields.";

impl BuiltinMixin {
    /// Full workspace filesystem read/write + conversation reference access.
    pub fn workspace_fs() -> Self {
        Self::WorkspaceFs { writable: true }
    }

    /// Read-only workspace inspection (used by code reviewers and Explore sub-agents).
    pub fn read_only_workspace_fs() -> Self {
        Self::WorkspaceFs { writable: false }
    }

    /// Shell execution paired with the selected shell description.
    pub fn shell_exec(shell: Option<CommandShell>) -> Self {
        Self::ShellExec { shell }
    }

    /// Diagnostics mixin with default prompt-engineered log guidance.
    pub fn diagnostics() -> Self {
        Self::Diagnostics { guidance: None }
    }

    /// Diagnostics mixin with custom prompt guidance appended or overriding the default.
    pub fn diagnostics_with_guidance(guidance: impl Into<String>) -> Self {
        Self::Diagnostics {
            guidance: Some(guidance.into()),
        }
    }

    /// Full read/write memory capability.
    pub fn memory() -> Self {
        Self::Memory { writable: true }
    }

    /// Read-only memory lookup capability.
    pub fn read_only_memory() -> Self {
        Self::Memory { writable: false }
    }

    /// Wrap plain MCP definitions as eager `DeclaredTool` entries (Phase 1A
    /// helper before Phase 1B policy resolution annotates loading/approval).
    pub fn mcp_eager(defs: Vec<ToolDefinition>) -> Self {
        Self::Mcp {
            tools: defs.into_iter().map(DeclaredTool::eager).collect(),
        }
    }

    /// Stable identifier for diagnostics and deduplication.
    pub fn id(&self) -> &'static str {
        match self {
            Self::WorkspaceFs { .. } => "workspace_fs",
            Self::ShellExec { .. } => "shell_exec",
            Self::BackgroundJobs => "background_jobs",
            Self::InteractiveUi => "interactive_ui",
            Self::PlanMode => "plan_mode",
            Self::Todo => "todo",
            Self::WebSearch => "web_search",
            Self::Skills => "skills",
            Self::Diagnostics { .. } => "diagnostics",
            Self::MediaGen => "media_gen",
            Self::RedactionTools => "redaction_tools",
            Self::SubAgent { .. } => "sub_agent",
            Self::Mcp { .. } => "mcp",
            Self::Memory { .. } => "memory",
            Self::CustomTools => "custom_tools",
        }
    }

    /// Static built-in tool names declared by this mixin (empty for dynamic
    /// mixins like `Mcp` and `CustomTools`).
    pub fn static_tool_names(&self) -> &'static [&'static str] {
        match self {
            Self::WorkspaceFs { writable: true } => WRITABLE_FS_TOOLS,
            Self::WorkspaceFs { writable: false } => READ_ONLY_FS_TOOLS,
            Self::ShellExec { .. } => SHELL_EXEC_TOOLS,
            Self::BackgroundJobs => BACKGROUND_JOB_TOOLS,
            Self::InteractiveUi => INTERACTIVE_UI_TOOLS,
            Self::PlanMode => PLAN_MODE_TOOLS,
            Self::Todo => TODO_TOOLS,
            Self::WebSearch => WEB_SEARCH_TOOLS,
            Self::Skills => SKILLS_TOOLS,
            Self::Diagnostics { .. } => DIAGNOSTICS_TOOLS,
            Self::MediaGen => MEDIA_GEN_TOOLS,
            Self::RedactionTools => REDACTION_TOOLS,
            Self::SubAgent { .. } => SUB_AGENT_TOOLS,
            Self::Memory { writable: true } => WRITABLE_MEMORY_TOOLS,
            Self::Memory { writable: false } => READ_ONLY_MEMORY_TOOLS,
            Self::Mcp { .. } | Self::CustomTools => &[],
        }
    }

    /// Collect the [`DeclaredTool`]s this mixin contributes from `registry`.
    pub fn collect_declared(&self, registry: &ToolRegistry) -> Vec<DeclaredTool> {
        match self {
            Self::Mcp { tools } => tools.clone(),
            Self::CustomTools => registry
                .custom_definitions()
                .into_iter()
                .map(DeclaredTool::eager)
                .collect(),
            _ => self
                .static_tool_names()
                .iter()
                .filter_map(|name| registry.builtin_definition(name))
                .map(DeclaredTool::eager)
                .collect(),
        }
    }

    /// Optional static prompt section contributed by this mixin, gated on the
    /// final active tool list so the model is never told about a withheld tool.
    pub fn prompt_section(&self, active_tools: &[ToolDefinition], ctx: &MixinResolveContext<'_>) -> Option<String> {
        let has = |name: &str| active_tools.iter().any(|t| t.name == name);
        match self {
            Self::WorkspaceFs { .. } => {
                if has("write_file") || has("edit_file") || has("apply_patch") {
                    Some(crate::agent::base_prompt::file_editing_section(&has))
                } else {
                    None
                }
            }
            Self::Todo => has("update_todos").then(crate::agent::base_prompt::checklist_section),
            Self::ShellExec { shell } => {
                let effective_shell = shell.as_ref().or(ctx.command_shell);
                if has("run_command") {
                    effective_shell.map(|s| format!("# Shell\n\n{}", s.summary()))
                } else {
                    None
                }
            }
            Self::Diagnostics { guidance } => {
                if !has("read_app_logs") {
                    return None;
                }
                match guidance.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    Some(custom) => Some(format!("{DEFAULT_DIAGNOSTICS_PROMPT}\n\n{custom}")),
                    None => Some(DEFAULT_DIAGNOSTICS_PROMPT.to_string()),
                }
            }
            _ => None,
        }
    }

    /// Derive the read-only counterpart of this mixin for `Explore` sub-agents.
    ///
    /// Returns `None` for mixins that mutate state, interact with UI, execute
    /// shell commands, or delegate recursively. MCP tools are narrowed strictly
    /// to entries marked [`ToolApprovalPolicy::Always`].
    pub fn to_explore_readonly(&self) -> Option<Self> {
        match self {
            Self::WorkspaceFs { .. } => Some(Self::WorkspaceFs { writable: false }),
            Self::WebSearch => Some(Self::WebSearch),
            Self::Memory { .. } => Some(Self::Memory { writable: false }),
            Self::Diagnostics { guidance } => Some(Self::Diagnostics {
                guidance: guidance.clone(),
            }),
            Self::Mcp { tools } => {
                let readonly: Vec<DeclaredTool> = tools
                    .iter()
                    .filter(|t| t.approval == Some(ToolApprovalPolicy::Always))
                    .cloned()
                    .collect();
                if readonly.is_empty() {
                    None
                } else {
                    Some(Self::Mcp { tools: readonly })
                }
            }
            // Write, exec, background, UI, plan transitions, todo, skills,
            // media gen, redaction writes, recursive sub-agents, and arbitrary
            // custom scripts are never handed to read-only Explore runners.
            Self::ShellExec { .. }
            | Self::BackgroundJobs
            | Self::InteractiveUi
            | Self::PlanMode
            | Self::Todo
            | Self::Skills
            | Self::MediaGen
            | Self::RedactionTools
            | Self::SubAgent { .. }
            | Self::CustomTools => None,
        }
    }
}

/// Deduplicate declared tools in mixin order, letting later declarations
/// replace identically named earlier ones (so plugin mixins can cleanly shadow
/// or specialize built-ins they claim in `owned_names`).
pub(crate) fn merge_declared(
    declared: Vec<DeclaredTool>,
    owned_by_plugins: &HashSet<String>,
    plugin_declared: Vec<DeclaredTool>,
) -> Vec<DeclaredTool> {
    let mut out: Vec<DeclaredTool> = Vec::with_capacity(declared.len() + plugin_declared.len());
    let mut seen: HashSet<String> = HashSet::new();

    for tool in declared {
        if owned_by_plugins.contains(&tool.definition.name) {
            continue;
        }
        if seen.insert(tool.definition.name.clone()) {
            out.push(tool);
        }
    }
    for tool in plugin_declared {
        if let Some(pos) = out.iter().position(|existing| existing.definition.name == tool.definition.name) {
            out[pos] = tool;
        } else {
            seen.insert(tool.definition.name.clone());
            out.push(tool);
        }
    }
    out
}

/// Helper to split eager vs. deferred tool names from a list of [`DeclaredTool`].
pub(crate) fn deferred_tool_names(declared: &[DeclaredTool]) -> HashSet<String> {
    declared
        .iter()
        .filter(|t| t.loading == ToolLoading::Deferred)
        .map(|t| t.definition.name.clone())
        .collect()
}

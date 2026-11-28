//! Composable character assembly built additively from [`BuiltinMixin`]s and
//! external [`ToolMixin`] plugins.
//!
//! `meridian-core` deliberately defines only [`CharacterBase`] and atomic
//! capability mixins — **never** platform-specific bot classes such as
//! `BotCharacterBase` or QQ/Telegram characters. Desktop clients assemble
//! `DesktopCharacter` in `src-tauri`, and external servers (such as the
//! closed-source `Meridian Server`) inherit/compose [`CharacterBase`] in their
//! own crates.

use std::sync::Arc;

use crate::agent::hooks::ToolMixin;
use crate::agent::mixins::BuiltinMixin;
use crate::agent::modes::Modes;
use crate::agent::sub_agents::SubAgentCatalog;
use crate::provider::ServerToolKind;
use crate::tools::command_shell::CommandShell;

/// Additive builder for a turn's capability profile.
///
/// Starts completely empty — no tools, fixed mode, no shell, no sub-agents —
/// and adds only the capabilities a runner actually wires into runtime ports.
#[derive(Clone)]
pub struct CharacterBase {
    builtin_mixins: Vec<BuiltinMixin>,
    plugin_mixins: Vec<Arc<dyn ToolMixin>>,
    mode: Modes,
    supports_tools: bool,
    server_tools: Vec<ServerToolKind>,
    persona: String,
    context_blocks: Vec<String>,
}

/// Immutable capability profile consumed by [`crate::agent::turn_config::resolve`].
#[derive(Clone)]
pub struct CharacterAssembly {
    pub builtin_mixins: Vec<BuiltinMixin>,
    pub plugin_mixins: Vec<Arc<dyn ToolMixin>>,
    pub mode: Modes,
    pub supports_tools: bool,
    pub server_tools: Vec<ServerToolKind>,
    pub persona: String,
    pub context_blocks: Vec<String>,
}

impl CharacterBase {
    /// Start an empty character with the given persona prompt.
    pub fn new(persona: impl Into<String>) -> Self {
        Self {
            builtin_mixins: Vec::new(),
            plugin_mixins: Vec::new(),
            mode: Modes::Fixed,
            supports_tools: true,
            server_tools: Vec::new(),
            persona: persona.into(),
            context_blocks: Vec::new(),
        }
    }

    /// Mix in one built-in capability.
    pub fn with_builtin(mut self, mixin: BuiltinMixin) -> Self {
        self.builtin_mixins.push(mixin);
        self
    }

    /// Mix in multiple built-in capabilities in deterministic order.
    pub fn with_builtins(mut self, mixins: impl IntoIterator<Item = BuiltinMixin>) -> Self {
        self.builtin_mixins.extend(mixins);
        self
    }

    /// Mix in an external or plugin [`ToolMixin`].
    pub fn with_plugin_mixin(mut self, mixin: Arc<dyn ToolMixin>) -> Self {
        self.plugin_mixins.push(mixin);
        self
    }

    /// Mix in multiple external or plugin [`ToolMixin`]s in order.
    pub fn with_plugin_mixins(mut self, mixins: impl IntoIterator<Item = Arc<dyn ToolMixin>>) -> Self {
        self.plugin_mixins.extend(mixins);
        self
    }

    /// Set the active mode policy (`Modes::Fixed` or `Modes::Switchable`).
    ///
    /// Note: [`crate::agent::turn_config::resolve`] enforces that `Modes::Switchable`
    /// only activates when [`BuiltinMixin::PlanMode`] is present, preventing
    /// ghost `enter_plan` / `exit_plan` tools on runners without a transitions port.
    pub fn with_mode(mut self, mode: Modes) -> Self {
        self.mode = mode;
        self
    }

    /// Whether the selected model accepts a `tools` parameter at all.
    pub fn with_supports_tools(mut self, supports_tools: bool) -> Self {
        self.supports_tools = supports_tools;
        self
    }

    /// Provider-side server tools enabled for this turn; any superseded local
    /// built-in tool (such as `web_search`) is automatically omitted.
    pub fn with_server_tools(mut self, server_tools: Vec<ServerToolKind>) -> Self {
        self.server_tools = server_tools;
        self
    }

    /// Additional static context blocks appended after `persona` (e.g. project
    /// instructions, file access notes, voice context).
    pub fn with_context_blocks(mut self, context_blocks: Vec<String>) -> Self {
        self.context_blocks = context_blocks;
        self
    }

    /// Finalize into a [`CharacterAssembly`].
    pub fn build(self) -> CharacterAssembly {
        CharacterAssembly {
            builtin_mixins: self.builtin_mixins,
            plugin_mixins: self.plugin_mixins,
            mode: self.mode,
            supports_tools: self.supports_tools,
            server_tools: self.server_tools,
            persona: self.persona,
            context_blocks: self.context_blocks,
        }
    }
}

impl CharacterAssembly {
    /// Whether this assembly includes [`BuiltinMixin::PlanMode`].
    pub fn has_plan_mode(&self) -> bool {
        self.builtin_mixins.iter().any(|m| matches!(m, BuiltinMixin::PlanMode))
    }

    /// Effective mode after enforcing that switchable modes require `PlanModeMixin`.
    pub fn effective_mode(&self) -> Modes {
        if self.has_plan_mode() {
            self.mode
        } else {
            Modes::Fixed
        }
    }

    /// The resolved [`CommandShell`] from [`BuiltinMixin::ShellExec`], if mounted.
    pub fn command_shell(&self) -> Option<CommandShell> {
        self.builtin_mixins.iter().find_map(|m| match m {
            BuiltinMixin::ShellExec { shell } => *shell,
            _ => None,
        })
    }

    /// The resolved [`SubAgentCatalog`] from [`BuiltinMixin::SubAgent`], if mounted.
    pub fn sub_agent_catalog(&self) -> Option<&SubAgentCatalog> {
        self.builtin_mixins.iter().find_map(|m| match m {
            BuiltinMixin::SubAgent { catalog } => Some(catalog),
            _ => None,
        })
    }

    /// Whether [`BuiltinMixin::Skills`] is mounted.
    pub fn has_skills(&self) -> bool {
        self.builtin_mixins.iter().any(|m| matches!(m, BuiltinMixin::Skills))
    }

    /// Derive a read-only `Explore` sub-agent profile from this parent assembly.
    ///
    /// Keeps read-only counterparts of parent built-in mixins (including any
    /// MCP tools whose policy is `ToolApprovalPolicy::Always`), drops all
    /// write/exec/interactive/delegation mixins, and optionally attaches
    /// caller-supplied extra read-only plugin mixins.
    pub fn derive_explore_readonly(
        &self,
        persona: impl Into<String>,
        supports_tools: bool,
        server_tools: Vec<ServerToolKind>,
        extra_plugin_mixins: Vec<Arc<dyn ToolMixin>>,
    ) -> Self {
        let builtin_mixins = self
            .builtin_mixins
            .iter()
            .filter_map(BuiltinMixin::to_explore_readonly)
            .collect();
        let mut plugin_mixins: Vec<Arc<dyn ToolMixin>> = self
            .plugin_mixins
            .iter()
            .filter(|m| m.allow_in_explore())
            .cloned()
            .collect();
        plugin_mixins.extend(extra_plugin_mixins);
        Self {
            builtin_mixins,
            plugin_mixins,
            mode: Modes::Fixed,
            supports_tools,
            server_tools,
            persona: persona.into(),
            context_blocks: Vec::new(),
        }
    }

    /// Derive a working `Agent` sub-agent profile from this parent assembly.
    ///
    /// Inherits workspace, shell, MCP, memory, diagnostics, web search, skills,
    /// redaction, media generation, custom tools, and parent plugin mixins,
    /// while stripping [`BuiltinMixin::SubAgent`] (structural recursion
    /// prevention), [`BuiltinMixin::PlanMode`] (sub-agents run in fixed mode),
    /// [`BuiltinMixin::BackgroundJobs`] (background jobs need a foreground
    /// conversation), and [`BuiltinMixin::InteractiveUi`].
    pub fn derive_working_sub_agent(
        &self,
        persona: impl Into<String>,
        supports_tools: bool,
        server_tools: Vec<ServerToolKind>,
        command_shell: Option<CommandShell>,
        extra_plugin_mixins: Vec<Arc<dyn ToolMixin>>,
    ) -> Self {
        let builtin_mixins = self
            .builtin_mixins
            .iter()
            .filter_map(|m| match m {
                BuiltinMixin::SubAgent { .. }
                | BuiltinMixin::PlanMode
                | BuiltinMixin::BackgroundJobs
                | BuiltinMixin::InteractiveUi => None,
                BuiltinMixin::ShellExec { .. } => Some(BuiltinMixin::ShellExec { shell: command_shell }),
                other => Some(other.clone()),
            })
            .collect();
        let mut plugin_mixins = self.plugin_mixins.clone();
        plugin_mixins.extend(extra_plugin_mixins);
        Self {
            builtin_mixins,
            plugin_mixins,
            mode: Modes::Fixed,
            supports_tools,
            server_tools,
            persona: persona.into(),
            context_blocks: Vec::new(),
        }
    }
}

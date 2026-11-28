//! Ecosystem extension hooks for `meridian-core`.
//!
//! `meridian-core` intentionally provides no platform-specific bot classes
//! (such as QQ or Telegram). Instead it exposes composable, platform-neutral
//! extension traits so desktop shells, external servers, and future plugins
//! can plug in tools, prompt sections, approval rules, speaker authorization,
//! and turn lifecycle observers without forking core.

use std::any::{Any, TypeId};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::provider::ToolDefinition;
use crate::tools::{MediaOut, ToolContext, ToolOutput};

/// Whether a tool definition sits permanently in the initial prompt prefix or
/// is disclosed progressively on demand.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolLoading {
    /// Always included in the top-level `tools` prefix.
    #[default]
    Eager,
    /// Disclosed progressively via native deferred loading or the client
    /// `search_tools` / `call_tool` gateway.
    Deferred,
}

/// How execution of a tool is authorised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalPolicy {
    /// Execute without prompting (eligible for read-only Explore sub-agents).
    Always,
    /// Delegate to automatic approval review when enabled; fall back to human
    /// approval on uncertainty.
    AutoReview,
    /// Always require explicit human approval (`fail_closed`: automatic review
    /// may never auto-allow this call).
    RequestApprove,
    /// Refuse execution unconditionally.
    Never,
}

impl ToolApprovalPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::AutoReview => "auto_review",
            Self::RequestApprove => "request_approve",
            Self::Never => "never",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "always" => Ok(Self::Always),
            "auto_review" => Ok(Self::AutoReview),
            "request_approve" => Ok(Self::RequestApprove),
            "never" => Ok(Self::Never),
            other => Err(format!("unknown tool approval policy `{other}`")),
        }
    }
}

/// Visibility boundary for configurable external tools (MCP / plugins).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolScope {
    /// Visible only in local desktop/host sessions.
    LocalOnly,
    /// Visible to operators/admins across local and external channel sessions.
    OperatorOnly,
    /// Safe to expose in shared multi-party rooms and read-only sub-agents.
    SharedRoom,
}

impl ToolScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LocalOnly => "local_only",
            Self::OperatorOnly => "operator_only",
            Self::SharedRoom => "shared_room",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "local_only" => Ok(Self::LocalOnly),
            "operator_only" => Ok(Self::OperatorOnly),
            "shared_room" => Ok(Self::SharedRoom),
            other => Err(format!("unknown tool scope `{other}`")),
        }
    }

    /// Whether a tool configured with `self` may be exposed in a session whose
    /// trust boundary is `session_scope`.
    ///
    /// - `LocalOnly` tools are visible only in local desktop/host sessions.
    /// - `OperatorOnly` tools are visible in local and operator channel sessions.
    /// - `SharedRoom` tools are visible everywhere.
    pub const fn is_visible_in(self, session_scope: Self) -> bool {
        matches!(
            (self, session_scope),
            (Self::LocalOnly, Self::LocalOnly)
                | (Self::OperatorOnly, Self::LocalOnly | Self::OperatorOnly)
                | (Self::SharedRoom, _)
        )
    }
}

/// A tool contributed by a [`ToolMixin`], carrying its wire definition along
/// with its disclosure and approval metadata.
#[derive(Debug, Clone)]
pub struct DeclaredTool {
    pub definition: ToolDefinition,
    pub loading: ToolLoading,
    pub approval: Option<ToolApprovalPolicy>,
}

impl DeclaredTool {
    pub fn eager(definition: ToolDefinition) -> Self {
        Self {
            definition,
            loading: ToolLoading::Eager,
            approval: None,
        }
    }

    pub fn deferred(definition: ToolDefinition) -> Self {
        Self {
            definition,
            loading: ToolLoading::Deferred,
            approval: None,
        }
    }

    pub fn with_approval(mut self, approval: ToolApprovalPolicy) -> Self {
        self.approval = Some(approval);
        self
    }
}

/// Read-only turn metadata visible to mixins when resolving their definitions
/// and static system prompt sections.
pub struct MixinResolveContext<'a> {
    pub conversation_id: &'a str,
    pub project_id: Option<&'a str>,
    pub assistant_id: Option<&'a str>,
    pub supports_tools: bool,
    pub command_shell: Option<&'a crate::tools::command_shell::CommandShell>,
}

/// Atomic capability mixin combining tool declarations, static prompt guidance,
/// optional auto-review policy clauses, approval rules, and execution.
#[async_trait::async_trait]
pub trait ToolMixin: Send + Sync {
    /// Stable identifier for ordering and diagnostics.
    fn id(&self) -> &'static str;

    /// Every tool name claimed by this mixin (including conditionally withheld
    /// ones) so it cleanly replaces any identically named registry entry.
    fn owned_names(&self) -> Vec<String>;

    /// Tool definitions advertised for this session.
    fn declarations(&self, ctx: &MixinResolveContext<'_>) -> Vec<DeclaredTool>;

    /// Optional static section appended into `base_prompt` in mixin order.
    /// Must depend only on session-stable state so prompt caching holds.
    fn prompt_section(&self, _active_tools: &[ToolDefinition], _ctx: &MixinResolveContext<'_>) -> Option<String> {
        None
    }

    /// Optional policy clause appended to `auto_review`'s system prompt.
    fn review_policy_addendum(&self) -> Option<String> {
        None
    }

    /// Per-tool approval override for tools owned by this mixin.
    fn approval_policy(&self, _tool_name: &str) -> Option<ToolApprovalPolicy> {
        None
    }

    /// Whether this mixin's tools are strictly read-only and safe to inherit
    /// into delegated `Explore` sub-agents (defaults to `false`).
    fn allow_in_explore(&self) -> bool {
        false
    }

    /// Execute a tool owned by this mixin, returning rich text + media output.
    async fn execute_rich(&self, name: &str, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput, String>;
}

/// Dynamic speaker/session authorisation gate.
///
/// Decouples what is advertised in the session-fixed `tools` cache prefix from
/// what the current speaker is permitted to discover or invoke.
pub trait ToolAuthorizer: Send + Sync {
    /// Whether the active speaker is allowed to discover and invoke `tool_name`.
    fn allows(&self, tool_name: &str) -> bool;

    /// Optional narrowed allow-set after mid-turn steering introduces a lower
    /// privilege speaker. Intersected with `offered`, never widened.
    fn narrowed_by_steering(&self) -> Option<HashSet<String>> {
        None
    }
}

/// Default permissive authoriser used by single-user desktop sessions.
pub struct AllowAllAuthorizer;

impl ToolAuthorizer for AllowAllAuthorizer {
    fn allows(&self, _tool_name: &str) -> bool {
        true
    }
}

/// Event passed to [`TurnHooks::on_turn_start`].
pub struct TurnStartContext<'a> {
    pub conversation_id: &'a str,
    pub turn_id: &'a str,
}

/// Event passed to [`TurnHooks::on_tool_media`] whenever a tool call returns
/// generated or retrieved media assets (`generate_image`, `generate_video`,
/// MCP media blocks, etc.). External servers can hook this to forward media to
/// chat transports once their file pipeline is ready.
pub struct ToolMediaEvent<'a> {
    pub conversation_id: &'a str,
    pub turn_id: &'a str,
    pub tool_call_id: &'a str,
    pub tool_name: &'a str,
    pub media: &'a [MediaOut],
}

/// Event passed to [`TurnHooks::on_after_tool_call`].
pub struct ToolCallOutcomeEvent<'a> {
    pub conversation_id: &'a str,
    pub turn_id: &'a str,
    pub tool_call_id: &'a str,
    pub tool_name: &'a str,
    pub outcome: &'a str,
}

/// Event passed to [`TurnHooks::on_turn_end`].
pub struct TurnEndEvent<'a> {
    pub conversation_id: &'a str,
    pub turn_id: &'a str,
    pub succeeded: bool,
    pub steps: usize,
}

/// Lifecycle observer hook for runners, plugins, and background maintenance
/// (such as media delivery, analytics, and Auto-Dream trigger accounting).
#[async_trait::async_trait]
pub trait TurnHooks: Send + Sync {
    async fn on_turn_start(&self, _event: &TurnStartContext<'_>) {}
    async fn on_tool_media(&self, _event: &ToolMediaEvent<'_>) {}
    async fn on_after_tool_call(&self, _event: &ToolCallOutcomeEvent<'_>) {}
    async fn on_turn_end(&self, _event: &TurnEndEvent<'_>) {}
}

/// Type-erased extension bag mounted on [`crate::services::Services`] so
/// external servers and plugins can attach long-lived state without adding
/// platform-specific fields to `ServicesInner`.
#[derive(Default)]
pub struct ServicesExtensions {
    values: RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

impl ServicesExtensions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert<T: Send + Sync + 'static>(&self, value: Arc<T>) {
        self.values
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(TypeId::of::<T>(), value);
    }

    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.values
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&TypeId::of::<T>())
            .and_then(|v| Arc::clone(v).downcast::<T>().ok())
    }

    pub fn remove<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.values
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&TypeId::of::<T>())
            .and_then(|v| v.downcast::<T>().ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_policy_and_scope_contracts_are_closed() {
        for (raw, expected) in [
            ("always", ToolApprovalPolicy::Always),
            ("auto_review", ToolApprovalPolicy::AutoReview),
            ("request_approve", ToolApprovalPolicy::RequestApprove),
            ("never", ToolApprovalPolicy::Never),
        ] {
            assert_eq!(ToolApprovalPolicy::parse(raw).unwrap(), expected);
            assert_eq!(expected.as_str(), raw);
        }
        assert!(ToolApprovalPolicy::parse("ask_human").is_err());

        for (raw, expected) in [
            ("local_only", ToolScope::LocalOnly),
            ("operator_only", ToolScope::OperatorOnly),
            ("shared_room", ToolScope::SharedRoom),
        ] {
            assert_eq!(ToolScope::parse(raw).unwrap(), expected);
            assert_eq!(expected.as_str(), raw);
        }
        assert!(ToolScope::parse("qq_group").is_err());
    }

    #[test]
    fn services_extensions_stores_and_retrieves_typed_singletons() {
        #[derive(Debug, PartialEq, Eq)]
        struct PluginState(u32);

        let ext = ServicesExtensions::new();
        assert!(ext.get::<PluginState>().is_none());
        ext.insert(Arc::new(PluginState(42)));
        assert_eq!(*ext.get::<PluginState>().unwrap(), PluginState(42));
        assert_eq!(*ext.remove::<PluginState>().unwrap(), PluginState(42));
        assert!(ext.get::<PluginState>().is_none());
    }
}

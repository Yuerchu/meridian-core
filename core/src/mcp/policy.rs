//! Three-tier MCP tool policy resolution (`global default -> server override -> tool override`)
//! with supply-chain drift protection for `always` approval.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::agent::hooks::{ToolApprovalPolicy, ToolLoading, ToolScope};
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::sea::ops::preference;
use crate::db::types::EpochMs;

/// Preference key holding the JSON-encoded global default [`McpToolPolicy`].
pub const MCP_DEFAULT_POLICY_PREF: &str = "mcp.default_policy";

/// Effective three-axis policy for an MCP tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpToolPolicy {
    pub scope: ToolScope,
    pub loading: ToolLoading,
    pub approval: ToolApprovalPolicy,
}

impl Default for McpToolPolicy {
    fn default() -> Self {
        Self {
            scope: ToolScope::LocalOnly,
            loading: ToolLoading::Deferred,
            approval: ToolApprovalPolicy::RequestApprove,
        }
    }
}

/// Sparse override applied at the server or per-tool tier.
///
/// Any `None` field inherits from the layer above (`tool -> server -> global`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpToolPolicyOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<ToolScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loading: Option<ToolLoading>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ToolApprovalPolicy>,
}

impl McpToolPolicyOverride {
    pub fn is_empty(&self) -> bool {
        self.scope.is_none() && self.loading.is_none() && self.approval.is_none()
    }
}

/// Map from server-native tool name to its per-tool override.
pub type McpToolPolicyMap = BTreeMap<String, McpToolPolicyOverride>;

/// Resolved policy for one MCP tool after merging all three tiers and applying
/// supply-chain drift protection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedMcpToolPolicy {
    pub scope: ToolScope,
    pub loading: ToolLoading,
    pub approval: ToolApprovalPolicy,
    /// True when the merged server/global policy requested `always`, but this
    /// tool was not present in the server's `approved_always_tools_json`
    /// snapshot (e.g. a newly added tool after an upstream MCP upgrade). The
    /// effective `approval` is automatically downgraded to `request_approve`
    /// until the user explicitly acknowledges the new tool.
    pub needs_always_reapproval: bool,
}

/// Resolve one MCP tool's effective policy across the three tiers.
pub fn resolve_tool_policy(
    global_default: McpToolPolicy,
    server_override: Option<&McpToolPolicyOverride>,
    tool_override: Option<&McpToolPolicyOverride>,
    approved_always_tools: Option<&[String]>,
    tool_name: &str,
) -> ResolvedMcpToolPolicy {
    let scope = tool_override
        .and_then(|o| o.scope)
        .or_else(|| server_override.and_then(|o| o.scope))
        .unwrap_or(global_default.scope);

    let loading = tool_override
        .and_then(|o| o.loading)
        .or_else(|| server_override.and_then(|o| o.loading))
        .unwrap_or(global_default.loading);

    let explicit_tool_approval = tool_override.and_then(|o| o.approval);
    let merged_approval = explicit_tool_approval
        .or_else(|| server_override.and_then(|o| o.approval))
        .unwrap_or(global_default.approval);

    // Supply-chain drift guard: an inherited `always` (from global default or
    // server default) only authorizes tool names that were explicitly reviewed
    // and recorded in `approved_always_tools_json`. An explicit per-tool
    // `always` override already names the exact tool and passes directly.
    if merged_approval == ToolApprovalPolicy::Always && explicit_tool_approval != Some(ToolApprovalPolicy::Always) {
        let acknowledged = approved_always_tools
            .is_some_and(|list| list.iter().any(|name| name == tool_name));
        if !acknowledged {
            return ResolvedMcpToolPolicy {
                scope,
                loading,
                approval: ToolApprovalPolicy::RequestApprove,
                needs_always_reapproval: true,
            };
        }
    }

    ResolvedMcpToolPolicy {
        scope,
        loading,
        approval: merged_approval,
        needs_always_reapproval: false,
    }
}

/// Read the global default [`McpToolPolicy`] from `preferences`.
///
/// Absence returns [`McpToolPolicy::default()`] (`local_only` / `deferred` /
/// `request_approve`); malformed JSON fails closed instead of silently
/// downgrading security defaults.
pub async fn read_global_default(db: &impl Read) -> Result<McpToolPolicy, String> {
    let raw = preference::get(db, MCP_DEFAULT_POLICY_PREF)
        .await
        .map_err(|e| format!("failed to read `{MCP_DEFAULT_POLICY_PREF}`: {e}"))?;
    match raw {
        None => Ok(McpToolPolicy::default()),
        Some(text) => serde_json::from_str::<McpToolPolicy>(&text)
            .map_err(|e| format!("invalid `{MCP_DEFAULT_POLICY_PREF}` JSON: {e}")),
    }
}

/// Persist the global default [`McpToolPolicy`] into `preferences`.
pub async fn write_global_default(tx: &WriteTx, policy: McpToolPolicy, now: EpochMs) -> Result<(), String> {
    let encoded = serde_json::to_string(&policy)
        .map_err(|e| format!("failed to encode `{MCP_DEFAULT_POLICY_PREF}`: {e}"))?;
    preference::set(tx, MCP_DEFAULT_POLICY_PREF, &encoded, now)
        .await
        .map_err(|e| format!("failed to write `{MCP_DEFAULT_POLICY_PREF}`: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::sea_test_db;

    #[test]
    fn default_policy_is_conservative() {
        let policy = McpToolPolicy::default();
        assert_eq!(policy.scope, ToolScope::LocalOnly);
        assert_eq!(policy.loading, ToolLoading::Deferred);
        assert_eq!(policy.approval, ToolApprovalPolicy::RequestApprove);
    }

    #[test]
    fn three_tier_precedence_tool_beats_server_beats_global() {
        let global = McpToolPolicy {
            scope: ToolScope::LocalOnly,
            loading: ToolLoading::Deferred,
            approval: ToolApprovalPolicy::RequestApprove,
        };
        let server = McpToolPolicyOverride {
            scope: Some(ToolScope::SharedRoom),
            loading: Some(ToolLoading::Eager),
            approval: Some(ToolApprovalPolicy::AutoReview),
        };
        let tool = McpToolPolicyOverride {
            scope: Some(ToolScope::OperatorOnly),
            loading: None,
            approval: Some(ToolApprovalPolicy::Never),
        };

        let resolved = resolve_tool_policy(global, Some(&server), Some(&tool), None, "place_order");
        assert_eq!(resolved.scope, ToolScope::OperatorOnly);
        assert_eq!(resolved.loading, ToolLoading::Eager);
        assert_eq!(resolved.approval, ToolApprovalPolicy::Never);
        assert!(!resolved.needs_always_reapproval);
    }

    #[test]
    fn server_always_downgrades_unseen_tools_to_request_approve() {
        let global = McpToolPolicy::default();
        let server = McpToolPolicyOverride {
            scope: Some(ToolScope::SharedRoom),
            loading: None,
            approval: Some(ToolApprovalPolicy::Always),
        };
        let approved = vec!["fetch_url".to_string()];

        let seen = resolve_tool_policy(global, Some(&server), None, Some(&approved), "fetch_url");
        assert_eq!(seen.approval, ToolApprovalPolicy::Always);
        assert!(!seen.needs_always_reapproval);

        // Upstream MCP server update silently adds a new tool: must NOT auto-run.
        let drifted = resolve_tool_policy(global, Some(&server), None, Some(&approved), "exec_shell");
        assert_eq!(drifted.approval, ToolApprovalPolicy::RequestApprove);
        assert!(drifted.needs_always_reapproval);
    }

    #[test]
    fn explicit_per_tool_always_does_not_require_server_snapshot() {
        let global = McpToolPolicy::default();
        let tool = McpToolPolicyOverride {
            scope: None,
            loading: None,
            approval: Some(ToolApprovalPolicy::Always),
        };
        let resolved = resolve_tool_policy(global, None, Some(&tool), None, "fetch_url");
        assert_eq!(resolved.approval, ToolApprovalPolicy::Always);
        assert!(!resolved.needs_always_reapproval);
    }

    #[test]
    fn policy_contracts_reject_unknown_fields() {
        assert!(serde_json::from_str::<McpToolPolicy>(r#"{"scope":"local_only","loading":"deferred","approval":"request_approve","extra":1}"#).is_err());
        assert!(serde_json::from_str::<McpToolPolicyOverride>(r#"{"approval":"ask_human"}"#).is_err());
        assert!(serde_json::from_str::<McpToolPolicyOverride>(r#"{"approval":"request_approve"}"#).is_ok());
    }

    #[tokio::test]
    async fn global_default_round_trips_through_preferences() {
        let db = sea_test_db().await;
        assert_eq!(read_global_default(&db).await.unwrap(), McpToolPolicy::default());

        let custom = McpToolPolicy {
            scope: ToolScope::OperatorOnly,
            loading: ToolLoading::Eager,
            approval: ToolApprovalPolicy::AutoReview,
        };
        db.write(async |tx| {
            write_global_default(tx, custom, 100).await.unwrap();
            Ok::<_, sea_orm::DbErr>(())
        })
        .await
        .unwrap();
        assert_eq!(read_global_default(&db).await.unwrap(), custom);
    }
}

//! Partitioning tool calls into parallel and serial segments.
//!
//! A parallel segment groups consecutive calls that are safe to execute
//! concurrently: they need no approval, the tool declares itself parallel-safe,
//! and none of them is a special-cased tool (plan mode, `ask_user`,
//! `run_agent`, surface tools, MCP tools). A serial segment is a single call
//! that must run alone — either because it writes, needs a prompt, or belongs
//! to a dispatch path the turn loop handles specially.
//!
//! The partitioning runs **after** `plan_batch_order` and preserves whatever
//! ordering it imposed. Calls within a parallel segment are dispatched
//! concurrently but their results are collected in wire order.

use std::collections::HashSet;

use crate::agent::modes::ModeSpec;
use crate::provider::ToolCall;
use crate::tools::{self, ToolContext, ToolRegistry};

use super::ports::{SubAgents, SurfaceTools};
use super::turn::ApprovalRule;

/// One segment of a partitioned tool-call batch.
#[derive(Debug)]
pub(crate) enum Segment<'a> {
    /// Multiple calls that can run concurrently.
    Parallel(Vec<&'a ToolCall>),
    /// A single call that must be dispatched alone.
    Serial(&'a ToolCall),
}

/// Whether a single call is eligible for parallel dispatch.
fn is_parallel_eligible(
    tc: &ToolCall,
    tools: &ToolRegistry,
    offered: &HashSet<String>,
    tool_context: &ToolContext,
    approval_rule: &ApprovalRule,
    surface_tools: Option<&dyn SurfaceTools>,
    mode: &'static ModeSpec,
    has_sub_agents: bool,
) -> bool {
    // Must be in the offered set.
    if !offered.contains(&tc.name) {
        return false;
    }

    // Special-cased dispatch paths that are always serial.
    if tc.name == "ask_user" || tc.name.starts_with("mcp__") {
        return false;
    }

    // run_agent is parallel-eligible when the sub_agents port is available.
    // Each sub-agent has its own conversation and context — they are
    // independent by design.
    if tc.name == crate::agent::sub_agents::RUN_AGENT_TOOL {
        return has_sub_agents;
    }

    // Surface tools have their own dispatch path.
    if surface_tools.is_some_and(|s| s.owns(&tc.name)) {
        return false;
    }

    // Mode transition tools (enter/exit/read/update plan) are always serial.
    if mode.enter_tool == Some(&tc.name)
        || mode.exit_tool.is_some_and(|exit| exit == tc.name)
        || mode.owned_tools.contains(&tc.name.as_str())
    {
        return false;
    }

    // Must resolve to a registry tool.
    let Some(tool) = tools.get(&tc.name) else {
        return false;
    };

    // The tool must declare itself parallel-safe.
    if !tool.supports_parallel() {
        return false;
    }

    // Must not require approval for this particular call.
    let args = match serde_json::from_str::<serde_json::Value>(&tc.arguments) {
        Ok(v) if v.is_object() => v,
        _ => return false,
    };
    let permission = tool.default_permission();
    let must_ask = match approval_rule {
        ApprovalRule::ByReach { accept_edits } => {
            tools::reach::needs_approval(permission, tool.reach(&args, tool_context), *accept_edits)
        }
        ApprovalRule::ByPermission => permission == tools::Permission::Ask,
    };

    !must_ask
}

/// Partition an ordered slice of tool calls into alternating parallel and
/// serial segments.
///
/// Consecutive parallel-eligible calls are merged into a single `Parallel`
/// segment. A single eligible call is still wrapped in `Parallel` — the
/// executor treats it the same way, and the overhead of spawning one task is
/// negligible.
pub(crate) fn partition_tool_calls<'a>(
    calls: &[&'a ToolCall],
    tools: &ToolRegistry,
    offered: &HashSet<String>,
    tool_context: &ToolContext,
    approval_rule: &ApprovalRule,
    surface_tools: Option<&dyn SurfaceTools>,
    mode: &'static ModeSpec,
    sub_agents: Option<&dyn SubAgents>,
) -> Vec<Segment<'a>> {
    let has_sub_agents = sub_agents.is_some();
    let mut segments: Vec<Segment<'a>> = Vec::new();
    let mut parallel_batch: Vec<&'a ToolCall> = Vec::new();

    for &tc in calls {
        if is_parallel_eligible(
            tc,
            tools,
            offered,
            tool_context,
            approval_rule,
            surface_tools,
            mode,
            has_sub_agents,
        ) {
            parallel_batch.push(tc);
        } else {
            if !parallel_batch.is_empty() {
                segments.push(Segment::Parallel(std::mem::take(&mut parallel_batch)));
            }
            segments.push(Segment::Serial(tc));
        }
    }

    if !parallel_batch.is_empty() {
        segments.push(Segment::Parallel(parallel_batch));
    }

    segments
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::modes;
    use crate::tools::{FileAccess, ShellType};

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: format!("call_{name}"),
            name: name.to_string(),
            arguments: "{}".to_string(),
        }
    }

    fn test_context() -> ToolContext {
        ToolContext {
            working_directory: Some("/project".into()),
            shell: ShellType::Bash,
            file_access: FileAccess::Unrestricted,
            project_id: None,
            conversation_id: Some("c1".into()),
            turn_id: Some("t1".into()),
            assistant_id: None,
            db_pool: None,
            #[cfg(not(target_os = "android"))]
            sandbox_policy: None,
            tool_secrets: Default::default(),
            cancel: tokio_util::sync::CancellationToken::new(),
            journal: None,
        }
    }

    fn default_offered(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn default_mode() -> &'static ModeSpec {
        modes::resolve(None).unwrap()
    }

    fn registry() -> ToolRegistry {
        ToolRegistry::new(std::path::PathBuf::from("/skills"), std::path::PathBuf::from("/logs"))
    }

    // Permission::Always tools for parallel testing (no path resolution needed).
    // recall_memory and list_memories have supports_parallel = true.
    // save_memory and delete_memory have supports_parallel = false (default).

    #[test]
    fn always_parallel_tools_form_one_segment() {
        let calls = [call("recall_memory"), call("list_memories"), call("read_app_logs")];
        let refs: Vec<&ToolCall> = calls.iter().collect();
        let reg = registry();
        let ctx = test_context();
        let offered = default_offered(&["recall_memory", "list_memories", "read_app_logs"]);
        let rule = ApprovalRule::ByReach { accept_edits: false };

        let segments = partition_tool_calls(&refs, &reg, &offered, &ctx, &rule, None, default_mode(), None);

        assert_eq!(segments.len(), 1);
        assert!(matches!(&segments[0], Segment::Parallel(v) if v.len() == 3));
    }

    #[test]
    fn non_parallel_tool_breaks_segment() {
        let calls = [
            call("recall_memory"),
            call("list_memories"),
            call("save_memory"),
            call("recall_memory"),
        ];
        let refs: Vec<&ToolCall> = calls.iter().collect();
        let reg = registry();
        let ctx = test_context();
        let offered = default_offered(&["recall_memory", "list_memories", "save_memory"]);
        let rule = ApprovalRule::ByReach { accept_edits: false };

        let segments = partition_tool_calls(&refs, &reg, &offered, &ctx, &rule, None, default_mode(), None);

        // [recall, list] -> Parallel, [save] -> Serial, [recall] -> Parallel
        assert_eq!(segments.len(), 3);
        assert!(matches!(&segments[0], Segment::Parallel(v) if v.len() == 2));
        assert!(matches!(&segments[1], Segment::Serial(tc) if tc.name == "save_memory"));
        assert!(matches!(&segments[2], Segment::Parallel(v) if v.len() == 1));
    }

    #[test]
    fn all_non_parallel_all_serial() {
        let calls = [call("save_memory"), call("delete_memory")];
        let refs: Vec<&ToolCall> = calls.iter().collect();
        let reg = registry();
        let ctx = test_context();
        let offered = default_offered(&["save_memory", "delete_memory"]);
        let rule = ApprovalRule::ByReach { accept_edits: false };

        let segments = partition_tool_calls(&refs, &reg, &offered, &ctx, &rule, None, default_mode(), None);

        assert_eq!(segments.len(), 2);
        assert!(matches!(&segments[0], Segment::Serial(_)));
        assert!(matches!(&segments[1], Segment::Serial(_)));
    }

    #[test]
    fn mcp_tools_are_serial() {
        let calls = [call("mcp__server__tool"), call("recall_memory")];
        let refs: Vec<&ToolCall> = calls.iter().collect();
        let reg = registry();
        let ctx = test_context();
        let offered = default_offered(&["mcp__server__tool", "recall_memory"]);
        let rule = ApprovalRule::ByReach { accept_edits: false };

        let segments = partition_tool_calls(&refs, &reg, &offered, &ctx, &rule, None, default_mode(), None);

        assert_eq!(segments.len(), 2);
        assert!(matches!(&segments[0], Segment::Serial(tc) if tc.name == "mcp__server__tool"));
        assert!(matches!(&segments[1], Segment::Parallel(v) if v.len() == 1));
    }

    #[test]
    fn ask_user_is_serial() {
        let calls = [call("recall_memory"), call("ask_user"), call("list_memories")];
        let refs: Vec<&ToolCall> = calls.iter().collect();
        let reg = registry();
        let ctx = test_context();
        let offered = default_offered(&["recall_memory", "ask_user", "list_memories"]);
        let rule = ApprovalRule::ByReach { accept_edits: false };

        let segments = partition_tool_calls(&refs, &reg, &offered, &ctx, &rule, None, default_mode(), None);

        assert_eq!(segments.len(), 3);
        assert!(matches!(&segments[0], Segment::Parallel(_)));
        assert!(matches!(&segments[1], Segment::Serial(tc) if tc.name == "ask_user"));
        assert!(matches!(&segments[2], Segment::Parallel(_)));
    }

    #[test]
    fn not_offered_is_serial() {
        let calls = [call("recall_memory"), call("list_memories")];
        let refs: Vec<&ToolCall> = calls.iter().collect();
        let reg = registry();
        let ctx = test_context();
        let offered = default_offered(&["recall_memory"]); // list_memories not offered
        let rule = ApprovalRule::ByReach { accept_edits: false };

        let segments = partition_tool_calls(&refs, &reg, &offered, &ctx, &rule, None, default_mode(), None);

        assert_eq!(segments.len(), 2);
        assert!(matches!(&segments[0], Segment::Parallel(v) if v.len() == 1));
        assert!(matches!(&segments[1], Segment::Serial(tc) if tc.name == "list_memories"));
    }

    #[test]
    fn run_agent_serial_without_port() {
        let calls = [call("recall_memory"), call("run_agent")];
        let refs: Vec<&ToolCall> = calls.iter().collect();
        let reg = registry();
        let ctx = test_context();
        let offered = default_offered(&["recall_memory", "run_agent"]);
        let rule = ApprovalRule::ByReach { accept_edits: false };

        let segments = partition_tool_calls(&refs, &reg, &offered, &ctx, &rule, None, default_mode(), None);

        assert_eq!(segments.len(), 2);
        assert!(matches!(&segments[0], Segment::Parallel(v) if v.len() == 1));
        assert!(matches!(&segments[1], Segment::Serial(tc) if tc.name == "run_agent"));
    }

    struct FakeSubAgents;

    #[async_trait::async_trait]
    impl super::SubAgents for FakeSubAgents {
        async fn run(
            &self,
            _spec: super::super::ports::SubAgentSpec,
        ) -> Result<super::super::ports::SubAgentReport, String> {
            unreachable!("partition never calls run")
        }
    }

    #[test]
    fn run_agent_parallel_with_port() {
        let calls = [call("run_agent"), call("run_agent"), call("recall_memory")];
        let refs: Vec<&ToolCall> = calls.iter().collect();
        let reg = registry();
        let ctx = test_context();
        let offered = default_offered(&["run_agent", "recall_memory"]);
        let rule = ApprovalRule::ByReach { accept_edits: false };
        let fake = FakeSubAgents;

        let segments = partition_tool_calls(&refs, &reg, &offered, &ctx, &rule, None, default_mode(), Some(&fake));

        // All three should be in one Parallel segment.
        assert_eq!(segments.len(), 1);
        assert!(matches!(&segments[0], Segment::Parallel(v) if v.len() == 3));
    }

    #[test]
    fn empty_calls_empty_segments() {
        let calls: Vec<&ToolCall> = vec![];
        let reg = registry();
        let ctx = test_context();
        let offered = default_offered(&[]);
        let rule = ApprovalRule::ByReach { accept_edits: false };

        let segments = partition_tool_calls(&calls, &reg, &offered, &ctx, &rule, None, default_mode(), None);

        assert!(segments.is_empty());
    }

    #[test]
    fn single_parallel_tool_still_wraps_in_parallel() {
        let calls = [call("recall_memory")];
        let refs: Vec<&ToolCall> = calls.iter().collect();
        let reg = registry();
        let ctx = test_context();
        let offered = default_offered(&["recall_memory"]);
        let rule = ApprovalRule::ByReach { accept_edits: false };

        let segments = partition_tool_calls(&refs, &reg, &offered, &ctx, &rule, None, default_mode(), None);

        assert_eq!(segments.len(), 1);
        assert!(matches!(&segments[0], Segment::Parallel(v) if v.len() == 1));
    }
}

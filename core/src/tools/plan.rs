use async_trait::async_trait;

use super::{Permission, Tool, ToolContext};

/// Asks the user for permission to stop building and plan instead.
///
/// The counterpart to the toolbar switch: the user can always put a
/// conversation into plan mode themselves, and this lets the model raise its
/// hand when it can tell that the approach is not settled. Like `exit_plan` it
/// is a shell — the agent loop intercepts the call, because switching mode and
/// rebuilding the tool set needs the loop's state.
pub struct EnterPlanTool;

#[async_trait]
impl Tool for EnterPlanTool {
    fn name(&self) -> &str {
        "enter_plan"
    }

    fn description(&self) -> &str {
        "Ask the user to switch this conversation into plan mode, where you explore and design \
         instead of building. Use it when the request is open enough that guessing wrong would \
         waste real work — several defensible approaches, unclear requirements, or a change whose \
         shape depends on code you have not read yet. Do not use it for work whose approach is \
         already clear, however long that work is: a plan nobody needed is just a delay. If the \
         user approves, the editing tools go away for the rest of the planning."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "One sentence on what is unsettled and why planning first is \
                                    worth the user's time. Shown to them as-is."
                }
            },
            "required": ["reason"]
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Always
    }

    async fn execute(&self, _args: serde_json::Value, _context: &ToolContext) -> Result<String, String> {
        Err("enter_plan must be handled by the agent loop".to_string())
    }
}

/// Hands a finished plan to the user for approval and, if they accept, ends
/// plan mode.
///
/// Like `ask_user` this is a shell: it is registered so the manual and the
/// tool schema know it exists, but the agent loop intercepts the call. The work
/// — waiting on the user, recording the plan, switching the conversation's mode
/// and rebuilding the tool set so the same turn can start implementing — needs
/// the loop's state and cannot happen inside `execute`.
pub struct ExitPlanTool;

#[async_trait]
impl Tool for ExitPlanTool {
    fn name(&self) -> &str {
        "exit_plan"
    }

    fn description(&self) -> &str {
        "Submit the current saved plan.md for review. This call takes no plan content: use \
         read_plan and update_plan first, then call exit_plan only when the saved revision is \
         complete. The turn ends while the user reviews it. If they request changes, a later \
         continuation gives you their edits and comments and you revise plan.md with another patch."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Always
    }

    async fn execute(&self, _args: serde_json::Value, _context: &ToolContext) -> Result<String, String> {
        Err("exit_plan must be handled by the agent loop".to_string())
    }
}

/// Read the one private document plan mode owns. Like the transition tools this
/// is a registry shell: the desktop adapter has the app-data root and performs
/// the durable read, while runners with no plan-mode port never see it.
pub struct ReadPlanTool;

#[async_trait]
impl Tool for ReadPlanTool {
    fn name(&self) -> &str {
        "read_plan"
    }

    fn description(&self) -> &str {
        "Read the current private plan.md and its optimistic-concurrency token. Call this before \
         every update. It returns the complete saved Markdown, generation, SHA-256, and file-sync \
         state; it never reads a plan.md from the project working tree."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Always
    }

    async fn execute(&self, _args: serde_json::Value, _context: &ToolContext) -> Result<String, String> {
        Err("read_plan must be handled by the agent loop".to_string())
    }
}

/// Apply one patch to the private plan document. It cannot name a project file
/// and therefore needs no ordinary filesystem approval.
pub struct UpdatePlanTool;

#[async_trait]
impl Tool for UpdatePlanTool {
    fn name(&self) -> &str {
        "update_plan"
    }

    fn description(&self) -> &str {
        "Apply one unified or Codex-style patch to the private plan.md. The first draft must add \
         plan.md; later calls must update it. Pass the generation and SHA-256 returned by \
         read_plan. Stale, multi-file, delete, move, empty, and no-op patches are rejected."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "base_generation": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Generation returned by the most recent read_plan call"
                },
                "base_sha256": {
                    "type": "string",
                    "description": "SHA-256 returned by the most recent read_plan call"
                },
                "patch": {
                    "type": "string",
                    "description": "One patch whose only logical path is plan.md"
                }
            },
            "required": ["base_generation", "base_sha256", "patch"],
            "additionalProperties": false
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Always
    }

    async fn execute(&self, _args: serde_json::Value, _context: &ToolContext) -> Result<String, String> {
        Err("update_plan must be handled by the agent loop".to_string())
    }
}

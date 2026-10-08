//! Reading, stopping and listing the commands `run_command` left running.
//!
//! None of these asks permission. Each is scoped to the conversation it runs
//! in — another conversation's task reads as no task at all — and each touches
//! only what this conversation's own approved command produced or is doing.
//! Stopping is a way to do *less*; it never starts anything.

use async_trait::async_trait;
use std::time::Duration;

use super::{Permission, Tool, ToolContext};
use crate::background::{Launcher, StoppedBy};
use crate::db::entity::background_task;

/// Background commands exist only where a [`Launcher`] does — see
/// `ToolContext::background`.
fn launcher(context: &ToolContext) -> Result<(&Launcher, &str), String> {
    match (&context.background, context.conversation_id.as_deref()) {
        (Some(launcher), Some(conversation)) => Ok((launcher, conversation)),
        _ => Err("background commands are not available in this session".into()),
    }
}

fn task_id(args: &serde_json::Value) -> Result<&str, String> {
    args["task_id"]
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| "missing 'task_id' argument".to_string())
}

/// One line saying where a task stands.
pub fn describe(row: &background_task::Model) -> String {
    let mut line = format!("{} [{}]", row.id, row.state.as_str());
    if let Some(code) = row.exit_code {
        line.push_str(&format!(" exit code {code}"));
    }
    if let Some(reason) = &row.ended_reason {
        line.push_str(&format!(" ({reason})"));
    }
    if let Some(command) = &row.command {
        line.push_str(&format!(": {command}"));
    }
    line
}

pub struct ReadBackgroundOutputTool;

#[async_trait]
impl Tool for ReadBackgroundOutputTool {
    fn spec(&self) -> crate::tools::spec::ToolSpec {
        crate::tools::spec::ToolSpec {
            effect: crate::tools::spec::Effect::Read,
            loop_handled: false,
            // Plan mode may run commands (see `agent::modes`), so what it starts
            // in the background it may also read.
            plan_mode: true,
            explore: false,
            reviewer: false,
            parallel: true,
        }
    }

    fn name(&self) -> &str {
        "read_background_output"
    }

    fn description(&self) -> &str {
        "Read the output of a command started with run_command and run_in_background, from a byte offset. \
         Returns the output so far, the offset to read from next, and whether the command is still running. \
         Set wait_ms to wait up to that long (at most 60000) for new output when there is none yet. \
         Do not poll in a loop: you are told when the command finishes."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "The id run_command returned" },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Byte offset to read from. Omit to read from the start."
                },
                "max_bytes": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": crate::background::READ_MAX,
                    "description": "Most bytes to return. Defaults to the maximum, 262144."
                },
                "wait_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 60000,
                    "description": "How long to wait for new output if there is none past offset yet. Defaults to 0."
                }
            },
            "required": ["task_id"]
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Always
    }

    async fn execute(&self, args: serde_json::Value, context: &ToolContext) -> Result<String, String> {
        let (launcher, conversation) = launcher(context)?;
        let id = task_id(&args)?;
        let offset = args["offset"].as_u64().unwrap_or(0);
        // domain-default: the schema documents an absent max_bytes as the most one read returns
        let max_bytes = args["max_bytes"]
            .as_u64()
            .map_or(crate::background::READ_MAX, |n| n as usize);
        let wait = Duration::from_millis(args["wait_ms"].as_u64().unwrap_or(0));
        let output = launcher.read(conversation, id, offset, max_bytes, wait).await?;

        let mut out = format!("{}\n", describe(&output.row));
        out.push_str(&format!(
            "bytes {}..{} of {}",
            output.offset, output.next_offset, output.total
        ));
        if output.row.output_truncated.get() {
            out.push_str(" (the log stopped at its size cap)");
        }
        out.push('\n');
        if output.next_offset < output.total {
            out.push_str(&format!("more output: read again from offset {}\n", output.next_offset));
        }
        out.push_str("---\n");
        if output.text.is_empty() {
            out.push_str("(no new output)");
        } else {
            out.push_str(&output.text);
        }
        Ok(out)
    }
}

pub struct StopBackgroundTaskTool;

#[async_trait]
impl Tool for StopBackgroundTaskTool {
    fn spec(&self) -> crate::tools::spec::ToolSpec {
        crate::tools::spec::ToolSpec {
            effect: crate::tools::spec::Effect::Exec,
            loop_handled: false,
            // Not in plan mode: `run_command` is the one program plan mode may
            // run (`tools::spec`), and stopping one is acting on the world too.
            plan_mode: false,
            explore: false,
            reviewer: false,
            parallel: false,
        }
    }

    fn name(&self) -> &str {
        "stop_background_task"
    }

    fn description(&self) -> &str {
        "Stop a command started with run_command and run_in_background, killing its whole process tree. \
         Its output so far stays readable."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "The id run_command returned" }
            },
            "required": ["task_id"]
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Always
    }

    async fn execute(&self, args: serde_json::Value, context: &ToolContext) -> Result<String, String> {
        let (launcher, conversation) = launcher(context)?;
        let id = task_id(&args)?;
        let row = launcher.stop(conversation, id, StoppedBy::Model).await?;
        Ok(describe(&row))
    }
}

pub struct ListBackgroundTasksTool;

#[async_trait]
impl Tool for ListBackgroundTasksTool {
    fn spec(&self) -> crate::tools::spec::ToolSpec {
        crate::tools::spec::ToolSpec {
            effect: crate::tools::spec::Effect::Read,
            loop_handled: false,
            // Plan mode may run commands (see `agent::modes`), so what it starts
            // in the background it may also list.
            plan_mode: true,
            explore: false,
            reviewer: false,
            parallel: true,
        }
    }

    fn name(&self) -> &str {
        "list_background_tasks"
    }

    fn description(&self) -> &str {
        "List the commands this conversation has run in the background, running and finished, oldest first."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    fn default_permission(&self) -> Permission {
        Permission::Always
    }

    async fn execute(&self, _args: serde_json::Value, context: &ToolContext) -> Result<String, String> {
        let (launcher, conversation) = launcher(context)?;
        let rows = launcher.list(conversation).await?;
        if rows.is_empty() {
            return Ok("(no background commands in this conversation)".into());
        }
        Ok(rows.iter().map(describe).collect::<Vec<_>>().join("\n"))
    }
}

use async_trait::async_trait;

use super::{Permission, Tool, ToolContext};

/// Delegation, as the model sees it.
///
/// Handled by the agent loop, so `execute` refuses the same way `ask_user` and
/// the mode transitions do. The registry entry exists so that one description
/// and one schema have a single home, and so `apply_mode` can pull it back in
/// like any other tool.
pub struct RunAgentTool;

#[async_trait]
impl Tool for RunAgentTool {
    fn name(&self) -> &str {
        crate::agent::sub_agents::RUN_AGENT_TOOL
    }

    /// The guidance about *when* to delegate is the part that matters. A model
    /// given a delegation tool and no policy delegates reflexively, and a
    /// sub-agent that reads three files to answer something already on screen
    /// costs more than doing it directly. The decision rules below follow the
    /// ones OpenAI's Codex CLI ships with its own spawn tool (Apache-2.0).
    fn description(&self) -> &str {
        "Hand a self-contained piece of work to a separate agent and wait for its answer.\n\
         \n\
         The sub-agent does NOT see this conversation. It starts from nothing but the \
         `prompt` you write, so brief it the way you would brief a colleague who just \
         walked in: what to look at, what question to answer, what shape the answer \
         should take. A prompt that says \"continue\" or refers to \"the file above\" \
         produces a wasted run.\n\
         \n\
         Delegate when the work is wide rather than deep — searching a codebase you have \
         not read, checking several independent things, summarising a lot of material \
         you do not need in full. Do it yourself when you could finish in two or three \
         steps, when you already have the context, or when each step depends on what the \
         last one returned; a round trip through another agent is slower and costs more \
         than the steps it replaces.\n\
         \n\
         The sub-agent's own steps do not enter this conversation. You get its final \
         answer and nothing else, so ask for what you actually need stated back to you."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "enum": ["explore", "agent"],
                    "description": "\"explore\" is read-only: it can read, search and look things up, and cannot change anything. \"agent\" has the same tools you do, including the ones that write. Prefer \"explore\" unless the task is to change something."
                },
                "description": {
                    "type": "string",
                    "description": "Three to five words naming the errand, shown to the user while it runs."
                },
                "prompt": {
                    "type": "string",
                    "description": "The full briefing. Self-contained: the sub-agent cannot see this conversation."
                },
                "model": {
                    "type": "string",
                    "description": "Which model to run it on. Omit to use the configured default for this kind of agent."
                }
            },
            "required": ["agent", "description", "prompt"]
        })
    }

    /// Nothing dangerous happens here — the tools the sub-agent reaches for ask
    /// on their own behalf, and its approvals surface on this turn's card.
    fn default_permission(&self) -> Permission {
        Permission::Always
    }

    async fn execute(&self, _args: serde_json::Value, _context: &ToolContext) -> Result<String, String> {
        Err("run_agent must be handled by the agent loop".to_string())
    }
}

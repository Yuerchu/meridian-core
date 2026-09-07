use super::{Permission, Tool, ToolContext};
use async_trait::async_trait;

pub struct AskUserTool;

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Ask the user one or more questions and wait for their responses. Each question can optionally provide 2-4 choices for the user to select from. The user may also skip a question, add supplementary notes to their selection, or type a free-form answer. Put the recommended option first and suffix its label with \"(Recommended)\". An \"Other\" free-form option is added automatically when options are provided."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {
                                "type": "string",
                                "description": "Stable snake_case identifier for mapping answers"
                            },
                            "question": {
                                "type": "string",
                                "description": "The question to ask the user"
                            },
                            "options": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {
                                            "type": "string",
                                            "description": "User-facing label (1-5 words)"
                                        },
                                        "description": {
                                            "type": "string",
                                            "description": "One short sentence explaining impact or tradeoff"
                                        }
                                    },
                                    "required": ["label"]
                                },
                                "description": "2-4 choices. Omit for free-form text input."
                            },
                            "multi_select": {
                                "type": "boolean",
                                "description": "If true, user can select multiple options. Default false."
                            }
                        },
                        "required": ["id", "question"]
                    },
                    "description": "1-4 questions to show the user. Prefer fewer questions."
                }
            },
            "required": ["questions"]
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Always
    }

    async fn execute(&self, _args: serde_json::Value, _context: &ToolContext) -> Result<String, String> {
        Err("ask_user must be handled by the agent loop".to_string())
    }
}

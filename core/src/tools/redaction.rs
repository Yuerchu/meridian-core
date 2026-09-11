use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use super::{Permission, Tool, ToolContext};
use crate::db::models::redaction_rule::*;
use crate::redaction::RedactionEngine;

// ── AddRedactionRuleTool ────────────────────────────────────────────────────

pub struct AddRedactionRuleTool {
    engine: Arc<RedactionEngine>,
}

impl AddRedactionRuleTool {
    pub fn new(engine: Arc<RedactionEngine>) -> Self {
        Self { engine }
    }
}

#[async_trait]
impl Tool for AddRedactionRuleTool {
    fn name(&self) -> &str {
        "add_redaction_rule"
    }

    fn description(&self) -> &str {
        "Add a regex rule so matching text is replaced with a placeholder before anything is sent \
         upstream. Call it the moment you notice a credential or identifier that no existing rule \
         caught, giving synthetic examples of the same shape — never the real value."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["name", "description", "pattern", "examples"],
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Slug like `acme_live_key`; becomes the placeholder [REDACTED:<name>]"
                },
                "description": {
                    "type": "string",
                    "description": "One line saying what the pattern catches"
                },
                "pattern": {
                    "type": "string",
                    "description": "Rust regex (no lookaround). Use (?P<secret>...) to redact only that part"
                },
                "examples": {
                    "type": "array",
                    "minItems": 2,
                    "items": {
                        "type": "object",
                        "required": ["text", "should_match"],
                        "properties": {
                            "text": { "type": "string" },
                            "should_match": { "type": "boolean" }
                        }
                    },
                    "description": "At least one positive and one negative. Never paste a real secret; invent one of the same shape"
                },
                "scope": {
                    "type": "string",
                    "enum": ["global", "project"],
                    "description": "Defaults to global"
                },
                "category": {
                    "type": "string",
                    "enum": ["secret", "pii"],
                    "description": "Defaults to secret"
                }
            }
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Always
    }

    async fn execute(&self, args: serde_json::Value, context: &ToolContext) -> Result<String, String> {
        let name = args.get("name").and_then(|v| v.as_str()).ok_or("missing `name`")?;
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .ok_or("missing `description`")?;
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or("missing `pattern`")?;

        let examples_val = args.get("examples").ok_or("missing `examples`")?;
        let examples: Vec<RedactionExample> =
            serde_json::from_value(examples_val.clone()).map_err(|e| format!("invalid examples: {e}"))?;

        let scope_str = args.get("scope").and_then(|v| v.as_str()).unwrap_or("global");
        let scope = RuleScope::parse(scope_str)?;

        let category_str = args.get("category").and_then(|v| v.as_str()).unwrap_or("secret");
        let category = RuleCategory::parse(category_str)?;

        if scope == RuleScope::Project && context.project_id.is_none() {
            return Err("scope `project` requires this conversation to belong to a project".into());
        }

        let scope_id = match scope {
            RuleScope::Global => GLOBAL_SCOPE_ID.to_string(),
            RuleScope::Project => context.project_id.clone().unwrap(),
        };

        let spec = crate::redaction::rule::RuleSpec {
            name: name.to_string(),
            description: description.to_string(),
            pattern: pattern.to_string(),
            category,
            examples: examples.clone(),
        };

        crate::redaction::rule::validate_spec(&spec)?;

        let pool = context.db_pool.clone().ok_or("no database available")?;
        let engine = self.engine.clone();
        let scope_type_str = scope.as_str().to_string();
        let scope_id_owned = scope_id.clone();
        let conversation_id = context.conversation_id.clone();

        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let mut conn = pool.get().map_err(|e| e.to_string())?;

            crate::db::ops::redaction_rule::validate_new_rule(&mut conn, &scope_type_str, &scope_id_owned, &spec)?;

            let id = uuid::Uuid::new_v4().to_string();
            let now = crate::util::now_ms();
            let examples_json = encode_examples(&examples)?;

            crate::db::ops::redaction_rule::create_rule(
                &mut conn,
                &RedactionRuleInsert {
                    id: &id,
                    scope_type: &scope_type_str,
                    scope_id: &scope_id_owned,
                    name: &spec.name,
                    description: &spec.description,
                    pattern: &spec.pattern,
                    category: category.as_str(),
                    examples: &examples_json,
                    origin: RuleOrigin::Model.as_str(),
                    source_conversation_id: conversation_id.as_deref(),
                    is_enabled: 1,
                    created_at: now,
                    updated_at: now,
                },
            )
            .map_err(|e| e.to_string())?;

            engine.reload(&mut conn)?;

            Ok(format!(
                "Rule `{}` added ({}). It applies from the next request; \
                 what was already sent cannot be recalled. \
                 Do not repeat the value in your reply.",
                spec.name, scope_type_str
            ))
        })
        .await
        .map_err(|e| e.to_string())??;

        Ok(result)
    }
}

// ── ListRedactionRulesTool ──────────────────────────────────────────────────

pub struct ListRedactionRulesTool {
    engine: Arc<RedactionEngine>,
}

impl ListRedactionRulesTool {
    pub fn new(engine: Arc<RedactionEngine>) -> Self {
        Self { engine }
    }
}

#[async_trait]
impl Tool for ListRedactionRulesTool {
    fn name(&self) -> &str {
        "list_redaction_rules"
    }

    fn description(&self) -> &str {
        "List all active redaction rules (built-in and custom) that filter text before it reaches the model provider."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({ "type": "object", "properties": {} })
    }

    fn default_permission(&self) -> Permission {
        Permission::Always
    }

    fn supports_parallel(&self) -> bool {
        true
    }

    async fn execute(&self, _args: serde_json::Value, context: &ToolContext) -> Result<String, String> {
        let rules = self.engine.active_rules(context.project_id.as_deref());
        if rules.is_empty() {
            return Ok("No redaction rules active (mode may be off).".into());
        }

        let mut out = String::new();
        for r in &rules {
            let status = if r.active { "" } else { " (inactive in current mode)" };
            out.push_str(&format!(
                "- [{}:{}] {}: {}{}\n",
                r.kind, r.category, r.name, r.description, status
            ));
        }
        Ok(out)
    }
}

// ── RemoveRedactionRuleTool ─────────────────────────────────────────────────

pub struct RemoveRedactionRuleTool {
    engine: Arc<RedactionEngine>,
}

impl RemoveRedactionRuleTool {
    pub fn new(engine: Arc<RedactionEngine>) -> Self {
        Self { engine }
    }
}

#[async_trait]
impl Tool for RemoveRedactionRuleTool {
    fn name(&self) -> &str {
        "remove_redaction_rule"
    }

    fn description(&self) -> &str {
        "Remove a model-added redaction rule by name. Cannot remove user-created or built-in rules."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The rule name to remove"
                },
                "scope": {
                    "type": "string",
                    "enum": ["global", "project"],
                    "description": "Defaults to global"
                }
            }
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Ask
    }

    async fn execute(&self, args: serde_json::Value, context: &ToolContext) -> Result<String, String> {
        let name = args.get("name").and_then(|v| v.as_str()).ok_or("missing `name`")?;

        let scope_str = args.get("scope").and_then(|v| v.as_str()).unwrap_or("global");
        let scope = RuleScope::parse(scope_str)?;

        if scope == RuleScope::Project && context.project_id.is_none() {
            return Err("scope `project` requires this conversation to belong to a project".into());
        }

        let scope_id = match scope {
            RuleScope::Global => GLOBAL_SCOPE_ID.to_string(),
            RuleScope::Project => context.project_id.clone().unwrap(),
        };

        use crate::redaction::builtin::BUILTIN_RULES;
        if BUILTIN_RULES.iter().any(|b| b.name == name) {
            return Err(format!(
                "`{name}` is a built-in rule. Change the redaction mode in Settings to disable it."
            ));
        }

        let pool = context.db_pool.clone().ok_or("no database available")?;
        let engine = self.engine.clone();
        let name_owned = name.to_string();
        let scope_type_str = scope.as_str().to_string();

        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let mut conn = pool.get().map_err(|e| e.to_string())?;

            let row =
                crate::db::ops::redaction_rule::get_rule_by_name(&mut conn, &scope_type_str, &scope_id, &name_owned)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("no rule named `{name_owned}` found in {scope_type_str} scope"))?;

            let origin = row.origin()?;
            if origin == RuleOrigin::User {
                return Err(format!(
                    "`{name_owned}` was created by the user; ask them to change it in Settings"
                ));
            }

            crate::db::ops::redaction_rule::delete_rule(&mut conn, &row.id).map_err(|e| e.to_string())?;
            engine.reload(&mut conn)?;

            Ok(format!("Rule `{name_owned}` removed."))
        })
        .await
        .map_err(|e| e.to_string())??;

        Ok(result)
    }
}

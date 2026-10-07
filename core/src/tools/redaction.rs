use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use super::{Permission, Tool, ToolContext};
use crate::db::entity::redaction_rule;
use crate::db::entity::redaction_rule::{GLOBAL_SCOPE_ID, RedactionExample, RuleCategory, RuleOrigin, RuleScope};
use crate::db::sea::ops::redaction_rule as rule_ops;
use crate::db::types::{Json, SqlBool};
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
    fn spec(&self) -> crate::tools::spec::ToolSpec {
        crate::tools::spec::ToolSpec {
            effect: crate::tools::spec::Effect::AppState,
            loop_handled: false,
            plan_mode: false,
            explore: false,
            reviewer: false,
            parallel: false,
        }
    }

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

        let db = context.sea.as_ref().ok_or("no database available")?;
        let now = crate::util::now_ms();
        let row = redaction_rule::Model {
            id: uuid::Uuid::new_v4().to_string(),
            scope_type: scope,
            scope_id,
            name: spec.name.clone(),
            description: spec.description.clone(),
            pattern: spec.pattern.clone(),
            category,
            examples: Json(examples),
            origin: RuleOrigin::Model,
            source_conversation_id: context.conversation_id.clone(),
            is_enabled: SqlBool::TRUE,
            created_at: now,
            updated_at: now,
        };
        // The checks and the insert share one write lock, so two concurrent
        // adds cannot both pass the name check or the per-scope count.
        db.write(async |tx| {
            if let Err(refused) = rule_ops::validate_new_rule(tx, scope, &row.scope_id, &spec).await {
                return Ok(Err(refused));
            }
            rule_ops::create_rule(tx, row).await.map(Ok)
        })
        .await
        .map_err(|e| e.to_string())??;
        self.engine.reload(db).await?;

        Ok(format!(
            "Rule `{}` added ({}). It applies from the next request; \
             what was already sent cannot be recalled. \
             Do not repeat the value in your reply.",
            spec.name,
            scope.as_str()
        ))
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
    fn spec(&self) -> crate::tools::spec::ToolSpec {
        crate::tools::spec::ToolSpec {
            effect: crate::tools::spec::Effect::Read,
            loop_handled: false,
            plan_mode: false,
            explore: false,
            reviewer: false,
            parallel: true,
        }
    }

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
    fn spec(&self) -> crate::tools::spec::ToolSpec {
        crate::tools::spec::ToolSpec {
            effect: crate::tools::spec::Effect::AppState,
            loop_handled: false,
            plan_mode: false,
            explore: false,
            reviewer: false,
            parallel: false,
        }
    }

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

        let db = context.sea.as_ref().ok_or("no database available")?;
        let refused = db
            .write(async |tx| {
                let Some(row) = rule_ops::get_rule_by_name(tx, scope, &scope_id, name).await? else {
                    return Ok(Some(format!(
                        "no rule named `{name}` found in {} scope",
                        scope.as_str()
                    )));
                };
                if row.origin == RuleOrigin::User {
                    return Ok(Some(format!(
                        "`{name}` was created by the user; ask them to change it in Settings"
                    )));
                }
                rule_ops::delete_rule(tx, &row.id).await?;
                Ok::<_, sea_orm::DbErr>(None)
            })
            .await
            .map_err(|e| e.to_string())?;
        if let Some(refused) = refused {
            return Err(refused);
        }
        self.engine.reload(db).await?;

        Ok(format!("Rule `{name}` removed."))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::db::entity::redaction_rule::MAX_RULES_PER_SCOPE;
    use crate::db::sea::cap::Db;
    use crate::db::sea::sea_test_db;

    fn context(db: &Db) -> ToolContext {
        ToolContext {
            working_directory: None,
            shell: crate::tools::ShellType::default_for_platform(),
            file_access: crate::tools::FileAccess::Roots(vec![]),
            project_id: None,
            conversation_id: Some("c1".into()),
            turn_id: None,
            assistant_id: None,
            db_pool: None,
            sea: Some(db.clone()),
            #[cfg(not(target_os = "android"))]
            sandbox_policy: crate::sandbox::CommandSandbox::UNCONFINED,
            tool_secrets: HashMap::new(),
            cancel: tokio_util::sync::CancellationToken::new(),
            journal: None,
        }
    }

    fn add_args(name: &str) -> serde_json::Value {
        json!({
            "name": name,
            "description": "test token",
            "pattern": "zq_[0-9]{6}",
            "examples": [
                {"text": "zq_123456", "should_match": true},
                {"text": "zq_12", "should_match": false},
            ],
        })
    }

    #[tokio::test]
    async fn an_added_rule_applies_and_a_removed_one_stops() {
        let db = sea_test_db().await;
        let engine = Arc::new(RedactionEngine::disabled());
        let add = AddRedactionRuleTool::new(engine.clone());
        let remove = RemoveRedactionRuleTool::new(engine.clone());

        add.execute(add_args("zq_token"), &context(&db)).await.unwrap();
        assert!(engine.redact("zq_123456", None).text.contains("[REDACTED:zq_token]"));
        let again = add.execute(add_args("zq_token"), &context(&db)).await.unwrap_err();
        assert!(again.contains("already exists"), "{again}");

        remove
            .execute(json!({"name": "zq_token"}), &context(&db))
            .await
            .unwrap();
        assert_eq!(engine.redact("zq_123456", None).text, "zq_123456");
        let gone = remove
            .execute(json!({"name": "zq_token"}), &context(&db))
            .await
            .unwrap_err();
        assert!(gone.contains("no rule named"), "{gone}");
    }

    /// The name check, the per-scope count and the insert share one write
    /// lock. With one slot left, of several adds racing for it exactly one
    /// gets in; checked outside the lock, each would count the same 199.
    #[tokio::test]
    async fn concurrent_adds_cannot_overfill_a_scope() {
        let db = sea_test_db().await;
        for i in 0..MAX_RULES_PER_SCOPE - 1 {
            let row = redaction_rule::Model {
                id: format!("seed{i}"),
                scope_type: RuleScope::Global,
                scope_id: GLOBAL_SCOPE_ID.into(),
                name: format!("seed{i}"),
                description: "d".into(),
                pattern: "x".into(),
                category: RuleCategory::Secret,
                examples: Json(Vec::new()),
                origin: RuleOrigin::User,
                source_conversation_id: None,
                is_enabled: SqlBool::TRUE,
                created_at: 1,
                updated_at: 1,
            };
            db.write(async |tx| rule_ops::create_rule(tx, row).await).await.unwrap();
        }
        let engine = Arc::new(RedactionEngine::disabled());
        let add = AddRedactionRuleTool::new(engine);
        let ctx = context(&db);
        let attempts = (0..8).map(|i| add.execute(add_args(&format!("racer{i}")), &ctx));
        let results = futures::future::join_all(attempts).await;
        let admitted = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(admitted, 1, "{results:?}");
    }
}

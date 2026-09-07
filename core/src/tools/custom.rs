use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use std::time::Duration;

use super::{Permission, Tool, ToolContext};

pub struct CustomToolExecutor {
    tool_name: String,
    tool_description: String,
    schema: Value,
    command: String,
    args_template: Option<String>,
    tool_working_directory: Option<String>,
    timeout: Duration,
    perm: Permission,
}

impl CustomToolExecutor {
    pub fn from_db(tool: &crate::db::models::custom_tool::CustomToolRow) -> Result<Self, String> {
        let schema: Value = serde_json::from_str(&tool.parameters_schema)
            .map_err(|error| format!("custom tool {} has invalid parameters_schema JSON: {error}", tool.name))?;
        if !schema.is_object() {
            return Err(format!(
                "custom tool {} parameters_schema must be a JSON object",
                tool.name
            ));
        }
        let perm = Permission::parse(&tool.permission)?;
        Ok(Self {
            tool_name: tool.name.clone(),
            tool_description: tool.description.clone(),
            schema,
            command: tool.command.clone(),
            args_template: tool.args_template.clone(),
            tool_working_directory: tool.working_directory.clone(),
            timeout: Duration::from_millis(tool.timeout_ms.unwrap_or(30000) as u64),
            perm,
        })
    }
}

/// POSIX single-quote escaping. Model-supplied argument values must reach the
/// shell as literal strings, never as syntax — the command template itself is
/// author-defined and trusted, the values are not.
fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[async_trait]
impl Tool for CustomToolExecutor {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        &self.tool_description
    }

    fn parameters_schema(&self) -> Value {
        self.schema.clone()
    }

    fn default_permission(&self) -> Permission {
        self.perm
    }

    #[cfg(target_os = "android")]
    async fn execute(&self, _args: Value, _context: &ToolContext) -> Result<String, String> {
        Err("Custom command tools are not supported on Android.".to_string())
    }

    #[cfg(not(target_os = "android"))]
    async fn execute(&self, args: Value, context: &ToolContext) -> Result<String, String> {
        let args_obj = args.as_object().cloned().unwrap_or_default();

        let final_args = if let Some(ref template) = self.args_template {
            let re = Regex::new(r"\{\{(\w+)\}\}").unwrap();
            let resolved = re.replace_all(template, |caps: &regex::Captures| {
                let key = &caps[1];
                let raw = args_obj
                    .get(key)
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_else(|| args_obj.get(key).map(|v| v.to_string()).unwrap_or_default());
                shell_escape(&raw)
            });
            resolved.into_owned()
        } else {
            shell_escape(&serde_json::to_string(&args).unwrap_or_default())
        };

        let wd = self
            .tool_working_directory
            .as_deref()
            .or(context.working_directory.as_deref())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

        let shell_cmd = if final_args.is_empty() {
            self.command.clone()
        } else {
            format!("{} {}", self.command, final_args)
        };

        // A containered command resolves its argv inside the container, where
        // the host's Git Bash path means nothing — same rule as `run_command`.
        let containered = context
            .sandbox_policy
            .as_ref()
            .is_some_and(|p| p.backend == crate::sandbox::SandboxBackend::Container);
        let argv: Vec<String> = if !containered && cfg!(target_os = "windows") {
            // Reuse run_command's Git Bash discovery instead of a bare "bash"
            // that depends on PATH.
            vec![super::run_command::find_bash().to_string(), "-c".into(), shell_cmd]
        } else {
            vec!["sh".into(), "-c".into(), shell_cmd]
        };

        // **The turn's own policy, not `None`.** These used to be handed
        // nothing and so always ran on the host, which was a defensible
        // position while the only sandbox narrowed a command on this machine
        // anyway. It stops being one the moment a conversation can place its
        // commands somewhere else: `run_command` inside a container and a
        // user's own command tool outside it, in the same turn, is not a
        // session sandbox — it is a sandbox with a documented way round it.
        //
        // The cost is real and belongs to the user rather than to this code: a
        // custom tool written against the host's toolchain will not find it
        // inside a container. That is a thing to say in the settings, not a
        // reason to leave the hole open.
        // Bracketed like `run_command` and for the same reason: a user's own
        // command tool changes files through no primitive this crate owns, and
        // without the bracket that work is `external` — real changes with
        // nobody's name on them. Settled on every exit path.
        let bracket = match &context.journal {
            Some(j) => j.command_bracket().await,
            None => None,
        };
        let res = crate::sandbox::execute(
            &argv,
            &wd,
            context.sandbox_policy.as_ref(),
            self.timeout,
            &context.cancel,
        )
        .await
        .map_err(|e| e.to_string());
        if let (Some(j), Some(b)) = (&context.journal, bracket) {
            j.settle_command_bracket(b, &self.tool_name).await;
        }
        let res = res?;

        if res.timed_out {
            return Err(format!("Command timed out after {}s", self.timeout.as_secs()));
        }

        let stdout = String::from_utf8_lossy(&res.stdout);
        let stderr = String::from_utf8_lossy(&res.stderr);

        if res.exit_code == 0 {
            Ok(if stdout.is_empty() {
                "(no output)".to_string()
            } else {
                stdout.into_owned()
            })
        } else {
            Err(format!(
                "Command exited with code {}.\nstdout: {}\nstderr: {}",
                res.exit_code, stdout, stderr,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::custom_tool::CustomToolRow;

    fn row(schema: &str) -> CustomToolRow {
        CustomToolRow {
            id: "t1".into(),
            name: "strict_tool".into(),
            description: "test".into(),
            category_id: None,
            parameters_schema: schema.into(),
            command: "true".into(),
            args_template: None,
            working_directory: None,
            timeout_ms: None,
            permission: "ask".into(),
            is_enabled: 1,
            sort_order: 0,
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn stored_parameter_schema_must_be_valid_json() {
        let Err(error) = CustomToolExecutor::from_db(&row("not json")) else {
            panic!("malformed schema must fail");
        };
        assert!(error.contains("invalid parameters_schema JSON"), "{error}");
    }

    #[test]
    fn stored_parameter_schema_must_be_an_object() {
        let Err(error) = CustomToolExecutor::from_db(&row("[]")) else {
            panic!("non-object schema must fail");
        };
        assert!(error.contains("must be a JSON object"), "{error}");
    }

    #[test]
    fn stored_permission_must_be_declared() {
        let mut stored = row(r#"{"type":"object"}"#);
        stored.permission = "future".into();
        let Err(error) = CustomToolExecutor::from_db(&stored) else {
            panic!("unknown permission must fail");
        };
        assert!(error.contains("unknown tool permission"), "{error}");
    }
}

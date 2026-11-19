use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

use super::{Permission, Tool, ToolContext};
use crate::db::entity::custom_tool;

/// Author-controlled execution source. Runtime values are only argv entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CustomToolExecution {
    Executable {
        program: String,
        args: Vec<CustomToolArgument>,
    },
    Script {
        script: String,
        args: Vec<CustomToolArgument>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CustomToolArgument {
    Literal { value: String },
    Parameter { name: String },
    Json {},
}

impl CustomToolExecution {
    pub fn validate(&self) -> Result<(), String> {
        let (source, args) = match self {
            Self::Executable { program, args } => (program, args),
            Self::Script { script, args } => (script, args),
        };
        if source.trim().is_empty() || source.contains('\0') {
            return Err("Custom tool executable/script must be nonempty and contain no NUL".into());
        }
        for arg in args {
            match arg {
                CustomToolArgument::Literal { value } if value.contains('\0') => {
                    return Err("Custom tool literal argument contains NUL".into());
                }
                CustomToolArgument::Parameter { name } if name.trim().is_empty() || name.contains('\0') => {
                    return Err("Custom tool parameter name must be nonempty and contain no NUL".into());
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn argv(&self, values: &Value, containered: bool) -> Result<Vec<String>, String> {
        self.validate()?;
        let object = values.as_object().ok_or("Custom tool arguments must be an object")?;
        let (mut argv, args) = match self {
            Self::Executable { program, args } => {
                // Windows batch launchers implicitly invoke cmd.exe, which is not
                // a literal-argv boundary. Authors must use the fixed script mode.
                if cfg!(windows) && !containered {
                    let lower = program.to_ascii_lowercase();
                    if lower.ends_with(".cmd") || lower.ends_with(".bat") {
                        return Err("Batch launchers are not supported as custom tool executables".into());
                    }
                }
                (vec![program.clone()], args)
            }
            Self::Script { script, args } => {
                let shell = if cfg!(windows) && !containered {
                    super::command_shell::find_bash().to_owned()
                } else {
                    "sh".to_owned()
                };
                (
                    vec![shell, "-c".into(), script.clone(), "meridian-custom-tool".into()],
                    args,
                )
            }
        };
        for arg in args {
            let value = match arg {
                CustomToolArgument::Literal { value } => value.clone(),
                CustomToolArgument::Parameter { name } => {
                    let value = object
                        .get(name)
                        .ok_or_else(|| format!("Missing custom tool parameter: {name}"))?;
                    match value {
                        Value::String(value) => value.clone(),
                        value => serde_json::to_string(value).map_err(|e| e.to_string())?,
                    }
                }
                CustomToolArgument::Json {} => serde_json::to_string(values).map_err(|e| e.to_string())?,
            };
            if value.contains('\0') {
                return Err("Custom tool argument contains NUL".into());
            }
            argv.push(value);
        }
        Ok(argv)
    }
}

pub struct CustomToolExecutor {
    tool_name: String,
    tool_description: String,
    schema: Value,
    execution: Option<CustomToolExecution>,
    tool_working_directory: Option<String>,
    timeout: Duration,
    perm: Permission,
}

impl CustomToolExecutor {
    /// Infallible: a row whose schema is not a JSON object, or whose permission
    /// is not one of the three, failed to decode at the read and never became a
    /// `Model` (see `db::sea::ops::custom_tool`'s malformed-row test).
    pub fn from_db(tool: &custom_tool::Model) -> Self {
        Self {
            tool_name: tool.name.clone(),
            tool_description: tool.description.clone(),
            schema: Value::Object(tool.parameters_schema.0.clone()),
            execution: tool.execution.as_ref().map(|value| value.0.clone()),
            tool_working_directory: tool.working_directory.clone(),
            // domain-default: a user's custom tool with no timeout configured gets this app's own thirty-second ceiling
            timeout: Duration::from_millis(tool.timeout_ms.unwrap_or(30000) as u64),
            perm: tool.permission,
        }
    }
}

#[async_trait]
impl Tool for CustomToolExecutor {
    fn spec(&self) -> crate::tools::spec::ToolSpec {
        crate::tools::spec::ToolSpec {
            effect: crate::tools::spec::Effect::Exec,
            loop_handled: false,
            plan_mode: false,
            explore: false,
            reviewer: false,
            parallel: false,
        }
    }

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
        let wd = self
            .tool_working_directory
            .as_deref()
            .or(context.working_directory.as_deref())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

        // A containered command resolves its argv inside the container, where
        // the host's Git Bash path means nothing — same rule as `run_command`.
        // The same refusal `run_command` gives, and the same way past it: with
        // the settings unread there is no answer to where this should run, so
        // the turn asks the user whether it may run outside the sandbox.
        let policy = match context.sandbox_policy.policy() {
            Ok(policy) => policy,
            Err(error) => return Err(super::encode_settings_unreadable(error)),
        };
        let execution = self
            .execution
            .as_ref()
            .ok_or("Legacy custom tool is disabled: configure an executable or fixed script with typed arguments")?;
        let argv = execution.argv(&args, context.sandbox_policy.is_container())?;

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
        let res = crate::sandbox::execute(&argv, &wd, policy, self.timeout, &context.cancel)
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
    use crate::db::types::{Json, SqlBool};

    #[test]
    fn execution_contract_is_strict_and_missing_parameters_fail() {
        for raw in [
            r#"{"kind":"executable","program":"echo","args":[],"extra":true}"#,
            r#"{"kind":"script","script":"echo","args":[{"kind":"json","extra":true}]}"#,
            r#"{"kind":"script","script":"echo","args":[{"kind":"parameter","name":"x","extra":true}]}"#,
            r#"{"kind":"template","command":"echo {{x}}"}"#,
        ] {
            assert!(serde_json::from_str::<CustomToolExecution>(raw).is_err(), "{raw}");
        }
        let execution = CustomToolExecution::Executable {
            program: "echo".into(),
            args: vec![CustomToolArgument::Parameter { name: "x".into() }],
        };
        assert!(execution.argv(&serde_json::json!({}), false).is_err());
        assert!(execution.argv(&serde_json::json!([]), false).is_err());
        assert!(execution.argv(&serde_json::json!({"x":"a\0b"}), false).is_err());
        assert_eq!(
            execution.argv(&serde_json::json!({"x":false}), false).unwrap()[1],
            "false"
        );
        assert_eq!(
            execution.argv(&serde_json::json!({"x":null}), false).unwrap()[1],
            "null"
        );
    }

    #[cfg(not(target_os = "android"))]
    #[tokio::test]
    async fn synthetic_arguments_roundtrip_through_executable_and_script() {
        argument_roundtrip(false).await;
    }

    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "requires Git Bash/MSYS shared mappings accessible to the Windows restricted token"]
    async fn restricted_msys_arguments_roundtrip() {
        argument_roundtrip(true).await;
    }

    #[cfg(not(target_os = "android"))]
    async fn argument_roundtrip(include_restricted_shells: bool) {
        let dir = tempfile::tempdir().unwrap();
        let values = [
            "",
            "two words",
            "'single' \"double\"",
            "line\nnext",
            "\\",
            "ends\\",
            "雪☃",
            "$(touch INJECTED)",
            "`touch INJECTED`",
            "; touch INJECTED",
            "&|<>^%!",
            "--option",
            "{{other}}",
            "*",
            "[a-z]?",
        ];
        let arguments: Vec<_> = values
            .iter()
            .enumerate()
            .map(|(i, _)| CustomToolArgument::Parameter { name: format!("p{i}") })
            .collect();
        let object = Value::Object(
            values
                .iter()
                .enumerate()
                .map(|(i, v)| (format!("p{i}"), Value::String((*v).into())))
                .collect(),
        );
        let shell = if cfg!(windows) {
            super::super::command_shell::find_bash().to_owned()
        } else {
            "sh".into()
        };
        let script = "printf '%s\\0' \"$@\"";
        let mut direct_args = vec![
            CustomToolArgument::Literal { value: "-c".into() },
            CustomToolArgument::Literal { value: script.into() },
            CustomToolArgument::Literal { value: "probe".into() },
        ];
        direct_args.extend(arguments.clone());
        let source = dir.path().join("argv_probe.rs");
        let binary = dir
            .path()
            .join(if cfg!(windows) { "argv_probe.exe" } else { "argv_probe" });
        std::fs::write(
            &source,
            r#"use std::io::Write; fn main() {
            let mut out = std::io::stdout().lock();
            for arg in std::env::args().skip(1) {
                out.write_all(arg.as_bytes()).unwrap(); out.write_all(&[0]).unwrap();
            }
        }"#,
        )
        .unwrap();
        let compiled = std::process::Command::new("rustc")
            .arg(&source)
            .arg("-o")
            .arg(&binary)
            .output()
            .unwrap();
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let executions = [
            CustomToolExecution::Executable {
                program: binary.to_string_lossy().into_owned(),
                args: arguments.clone(),
            },
            CustomToolExecution::Executable {
                program: shell,
                args: direct_args,
            },
            CustomToolExecution::Script {
                script: script.into(),
                args: arguments,
            },
        ];
        let policies = vec![None];
        #[cfg(windows)]
        let policies = {
            let mut policies = policies;
            policies.push(Some(crate::sandbox::SandboxPolicy {
                backend: crate::sandbox::SandboxBackend::WindowsRestrictedToken,
                project_dir: Some(dir.path().to_path_buf()),
                ..Default::default()
            }));
            policies
        };
        for policy in &policies {
            #[cfg(windows)]
            {
                let result = crate::sandbox::execute(
                    &["cmd.exe".into(), "/C".into(), "echo daybreak-cmd-smoke".into()],
                    dir.path(),
                    policy.as_ref(),
                    Duration::from_secs(10),
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await
                .unwrap();
                assert_eq!(result.exit_code, 0, "{}", String::from_utf8_lossy(&result.stderr));
                assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "daybreak-cmd-smoke");
            }
            for (index, execution) in executions.iter().enumerate() {
                if policy.is_some() && index > 0 && !include_restricted_shells {
                    continue;
                }
                let argv = execution.argv(&object, false).unwrap();
                let result = crate::sandbox::execute(
                    &argv,
                    dir.path(),
                    policy.as_ref(),
                    Duration::from_secs(10),
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await
                .unwrap();
                assert_eq!(result.exit_code, 0, "{}", String::from_utf8_lossy(&result.stderr));
                let mut expected = values.join("\0").into_bytes();
                expected.push(0);
                assert_eq!(result.stdout, expected);
                assert!(!dir.path().join("INJECTED").exists());
            }
        }
    }

    #[test]
    fn fixed_script_and_json_arguments_are_never_interpolated() {
        let execution = CustomToolExecution::Script {
            script: "printf '%s' \"$1\"".into(),
            args: vec![CustomToolArgument::Json {}],
        };
        let values = serde_json::json!({"x":"$(touch INJECTED)", "n":42});
        let argv = execution.argv(&values, true).unwrap();
        assert_eq!(&argv[..4], ["sh", "-c", "printf '%s' \"$1\"", "meridian-custom-tool"]);
        assert_eq!(serde_json::from_str::<Value>(&argv[4]).unwrap(), values);
    }

    fn row() -> custom_tool::Model {
        custom_tool::Model {
            id: "t1".into(),
            name: "strict_tool".into(),
            description: "test".into(),
            category_id: None,
            parameters_schema: Json(serde_json::Map::from_iter([("type".to_owned(), Value::from("object"))])),
            command: "true".into(),
            args_template: None,
            execution: Some(Json(CustomToolExecution::Executable {
                program: "true".into(),
                args: vec![],
            })),
            working_directory: None,
            timeout_ms: None,
            permission: Permission::Ask,
            is_enabled: SqlBool::TRUE,
            sort_order: 0,
            created_at: 1,
            updated_at: 1,
        }
    }

    /// A user's own command tool is a command like any other: with the
    /// settings unread it runs nowhere and asks, through the same escalation
    /// `run_command` uses. Otherwise it is the documented way round the rule.
    #[cfg(not(target_os = "android"))]
    #[tokio::test]
    async fn unreadable_settings_run_no_custom_command() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let mut stored = row();
        stored.command = format!("echo ran > \"{}\"", marker.display().to_string().replace('\\', "/"));
        stored.execution = Some(Json(CustomToolExecution::Script {
            script: stored.command.clone(),
            args: vec![],
        }));
        let tool = CustomToolExecutor::from_db(&stored);
        let mut context = ToolContext {
            working_directory: Some(dir.path().display().to_string()),
            shell: crate::tools::ShellType::default_for_platform(),
            file_access: crate::tools::FileAccess::Unrestricted,
            project_id: None,
            conversation_id: Some("c-1".into()),
            turn_id: None,
            assistant_id: None,
            db: None,
            sandbox_policy: crate::sandbox::CommandSandbox::Unreadable("database is locked".into()),
            #[cfg(not(target_os = "android"))]
            background: None,
            tool_secrets: Default::default(),
            cancel: tokio_util::sync::CancellationToken::new(),
            journal: None,
        };

        let err = tool.execute(serde_json::json!({}), &context).await.unwrap_err();
        let escalation = crate::tools::decode_escalation(&err).expect("an escalation, not a plain error");
        assert_eq!(escalation.kind, crate::events::ApprovalRetryKind::SettingsUnreadable);
        assert!(
            !marker.exists(),
            "the custom command ran with nothing known about where it should"
        );
        stored.execution = None;
        context.sandbox_policy = crate::sandbox::CommandSandbox::UNCONFINED;
        let legacy_error = CustomToolExecutor::from_db(&stored)
            .execute(serde_json::json!({}), &context)
            .await
            .unwrap_err();
        assert!(
            legacy_error.contains("Legacy custom tool is disabled"),
            "{legacy_error}"
        );
        assert!(
            !marker.exists(),
            "enabled legacy row must never execute its preserved command"
        );
    }
}

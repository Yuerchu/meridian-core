use super::{Permission, ShellType, Tool, ToolContext};
use crate::sandbox::{ExecError, ExecResult, SandboxBackend};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub struct RunCommandTool;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// The facts a caller needs to render or persist one command result.
///
/// `run_command` used to collapse these into one display string inside the
/// tool.  The composer shell needs the same execution path, but it also needs
/// to tell stdout from stderr and to preserve timeout/truncation/sandbox facts
/// without scraping prose.  Keeping the structured result here makes the two
/// entry points share the security boundary instead of reimplementing it in a
/// Tauri command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandExecution {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
    pub truncated: bool,
    pub sandbox: SandboxBackend,
    pub duration_ms: u64,
}

impl CommandExecution {
    /// The legacy tool result shown to the model.  Kept byte-for-byte compatible
    /// with the old formatter while the direct composer path uses the fields.
    pub fn formatted(&self) -> String {
        let mut result = String::new();
        if !self.stdout.is_empty() {
            result.push_str(&self.stdout);
        }
        if !self.stderr.is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str("[stderr] ");
            result.push_str(&self.stderr);
        }
        if self.exit_code != 0 && !self.timed_out {
            result.push_str(&format!("\n[exit code: {}]", self.exit_code));
        }
        if self.timed_out {
            result.push_str("\n[timed out; process tree killed]");
        }
        if self.truncated {
            result.push_str("\n[output truncated at 256KB]");
        }
        if result.is_empty() {
            result = "(no output)".to_string();
        }
        result
    }
}

/// A sandbox refusal is not an ordinary failed command: it is the only state
/// from which a caller may offer the explicit host retry.  Cancellation is
/// separate as well so a direct user command can persist the truthful ending
/// rather than presenting it as a spawn failure.
#[derive(Debug)]
pub enum CommandExecutionError {
    SandboxDenied(CommandExecution),
    Cancelled,
    Execution(String),
}

impl std::fmt::Display for CommandExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SandboxDenied(_) => write!(f, "command blocked by the sandbox"),
            Self::Cancelled => write!(f, "command cancelled"),
            Self::Execution(message) => write!(f, "{message}"),
        }
    }
}

#[async_trait]
impl Tool for RunCommandTool {
    fn name(&self) -> &str {
        "run_command"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return its output (stdout and stderr). The command runs in the project's working directory. Output is truncated at 256KB. Timeout: 120 seconds."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "description": super::description_property(),
            },
            "required": ["command"]
        })
    }

    fn default_permission(&self) -> Permission {
        Permission::Ask
    }

    async fn execute(&self, args: serde_json::Value, context: &ToolContext) -> Result<String, String> {
        let command = args["command"].as_str().ok_or("missing 'command' argument")?;

        match execute_command(command, context).await {
            Ok(result) => Ok(result.formatted()),
            Err(CommandExecutionError::SandboxDenied(result)) => Err(super::encode_sandbox_denied(&result.formatted())),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// Execute one command through the exact shell, sandbox/container, timeout and
/// cancellation path used by the model tool.
///
/// This function deliberately performs no permission check. `Tool::execute`
/// is reached only after the agent approval layer has granted it; the direct
/// composer command is itself an explicit user action.  Removing a sandbox is
/// *not* implied by that action — callers must invoke this with
/// `ToolContext::without_sandbox()` only after a second, explicit confirmation.
///
/// The journal cannot see what a shell does, so it brackets it here — at the
/// one point both the model tool and the direct composer command pass through:
/// tracked files are snapshotted before, compared after, and the differences
/// recorded as inferred. Settled on every exit path, because a failed or
/// timed-out command has usually still changed files.
pub async fn execute_command(command: &str, context: &ToolContext) -> Result<CommandExecution, CommandExecutionError> {
    let bracket = match &context.journal {
        Some(j) => j.command_bracket().await,
        None => None,
    };
    let result = execute_command_inner(command, context).await;
    if let (Some(j), Some(b)) = (&context.journal, bracket) {
        j.settle_command_bracket(b, "run_command").await;
    }
    result
}

async fn execute_command_inner(
    command: &str,
    context: &ToolContext,
) -> Result<CommandExecution, CommandExecutionError> {
    if command.trim().is_empty() {
        return Err(CommandExecutionError::Execution("command cannot be empty".into()));
    }

    let cwd = context.working_dir_or_current();

    // `context.shell` describes this machine, and a container is not this
    // machine: its argv resolves *inside*, where neither `C:\Program
    // Files\Git\bin\bash.exe` nor necessarily `/bin/bash` exists — the
    // default image is Alpine, which ships `sh` alone. So a containered
    // command gets the one shell the POSIX image contract promises, and the
    // host shell selection applies only where the command actually runs.
    let containered = context
        .sandbox_policy
        .as_ref()
        .is_some_and(|p| p.backend == SandboxBackend::Container);
    let shell_argv: Vec<String> = if containered {
        vec!["sh".into(), "-c".into(), command.into()]
    } else {
        match context.shell {
            ShellType::Cmd => vec!["cmd".into(), "/C".into(), command.into()],
            ShellType::PowerShell => {
                vec![
                    find_powershell().into(),
                    "-NoProfile".into(),
                    "-Command".into(),
                    command.into(),
                ]
            }
            ShellType::Bash => {
                vec![find_bash().into(), "-c".into(), command.into()]
            }
        }
    };

    let timeout = context
        .sandbox_policy
        .as_ref()
        .map(|p| p.timeout)
        .unwrap_or(COMMAND_TIMEOUT);

    let sandboxed = context.sandbox_policy.is_some();
    let started = std::time::Instant::now();
    let res = crate::sandbox::execute(
        &shell_argv,
        &cwd,
        context.sandbox_policy.as_ref(),
        timeout,
        &context.cancel,
    )
    .await
    .map_err(|e| {
        if matches!(e, ExecError::Cancelled) {
            return CommandExecutionError::Cancelled;
        }
        // The command string is never logged: it is model-generated and
        // routinely contains exported tokens and passwords.
        tracing::warn!(
            tool = "run_command",
            sandboxed,
            timeout_secs = timeout.as_secs(),
            error = %e,
            "run_command could not be executed"
        );
        CommandExecutionError::Execution(e.to_string())
    })?;

    let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    let result = structured_output(&res, duration_ms);

    // Two conditions, and the second is not redundant. The heuristic
    // already only fires for the one backend whose escalation is safe;
    // asking the backend as well means a new backend cannot be added to
    // that heuristic and silently inherit the host-retry card. See
    // `SandboxBackend::may_retry_on_host`.
    if is_sandbox_denied(&res) && res.ran_under.may_retry_on_host() {
        // The user is about to get an "allow this without the sandbox?"
        // prompt. Without this line there is nothing recording what was
        // blocked or why they were asked.
        tracing::warn!(
            tool = "run_command",
            exit_code = res.exit_code,
            stdout_len = res.stdout.len(),
            stderr_len = res.stderr.len(),
            duration_ms = started.elapsed().as_millis() as u64,
            "command blocked by the sandbox; asking whether to retry without it"
        );
        return Err(CommandExecutionError::SandboxDenied(result));
    }

    if res.timed_out {
        tracing::warn!(
            tool = "run_command",
            timeout_secs = timeout.as_secs(),
            sandboxed,
            "run_command timed out"
        );
    } else if res.exit_code != 0 {
        // Below info on purpose: a non-zero exit is an ordinary outcome the
        // model sees and handles, not something worth a line in the file.
        tracing::debug!(
            tool = "run_command",
            exit_code = res.exit_code,
            duration_ms,
            "run_command exited non-zero"
        );
    }

    Ok(result)
}

/// Heuristic ported from codex-rs/sandboxing/src/denial.rs, adjusted for
/// Windows: a non-zero exit alone is not a denial — the output must show an
/// access failure the restricted token would produce.
///
/// **Matched against the backend that ran the command, not against "a sandbox
/// ran it".** These strings are what a Windows restricted token produces;
/// another confinement refuses in its own words, and running its output past
/// this list would either miss every refusal or, worse, match one of these by
/// coincidence and offer to rerun the command outside a sandbox it was never
/// in.
fn is_sandbox_denied(res: &ExecResult) -> bool {
    if res.ran_under != SandboxBackend::WindowsRestrictedToken || res.timed_out || res.exit_code == 0 {
        return false;
    }
    let hay = format!(
        "{}\n{}",
        String::from_utf8_lossy(&res.stdout),
        String::from_utf8_lossy(&res.stderr),
    )
    .to_lowercase();
    const NEEDLES: &[&str] = &[
        "access is denied",
        "拒绝访问",
        "permission denied",
        "unauthorizedaccess",
        "operation not permitted",
        "read-only file system",
        "(os error 5)",
        // MSYS2/Cygwin (Git Bash) can't create its shared-memory section under
        // a restricted token and dies with "CreateFileMapping ... Win32 error 5"
        // before running anything; escalation is the only way forward.
        "win32 error 5",
        "createfilemapping",
    ];
    NEEDLES.iter().any(|n| hay.contains(n))
}

fn structured_output(res: &ExecResult, duration_ms: u64) -> CommandExecution {
    CommandExecution {
        stdout: String::from_utf8_lossy(&res.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&res.stderr).into_owned(),
        exit_code: res.exit_code,
        timed_out: res.timed_out,
        truncated: res.truncated,
        sandbox: res.ran_under,
        duration_ms,
    }
}

fn find_powershell() -> &'static str {
    if cfg!(target_os = "windows") {
        if std::path::Path::new("C:\\Program Files\\PowerShell\\7\\pwsh.exe").exists() {
            "C:\\Program Files\\PowerShell\\7\\pwsh.exe"
        } else {
            "powershell"
        }
    } else {
        "pwsh"
    }
}

pub(crate) fn find_bash() -> &'static str {
    if cfg!(target_os = "windows") {
        let git_bash = "C:\\Program Files\\Git\\bin\\bash.exe";
        if std::path::Path::new(git_bash).exists() {
            return git_bash;
        }
        let git_bash_x86 = "C:\\Program Files (x86)\\Git\\bin\\bash.exe";
        if std::path::Path::new(git_bash_x86).exists() {
            return git_bash_x86;
        }
        "bash"
    } else {
        "/bin/bash"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connector that records the argv it was handed and answers success.
    #[derive(Debug, Default)]
    struct ArgvRecorder {
        argv: std::sync::Mutex<Option<Vec<String>>>,
    }

    #[async_trait]
    impl crate::container::CommandConnector for ArgvRecorder {
        fn backend(&self) -> SandboxBackend {
            SandboxBackend::Container
        }
        async fn execute(
            &self,
            argv: &[String],
            _: &std::path::Path,
            _: &std::path::Path,
            _: &str,
            _: Duration,
            _: &tokio_util::sync::CancellationToken,
        ) -> Result<ExecResult, crate::sandbox::ExecError> {
            *self.argv.lock().unwrap() = Some(argv.to_vec());
            Ok(ExecResult {
                exit_code: 0,
                stdout: b"ok".to_vec(),
                stderr: Vec::new(),
                timed_out: false,
                truncated: false,
                ran_under: SandboxBackend::Container,
            })
        }
    }

    /// **The shell is the container's, not this machine's.** `context.shell`
    /// selected the argv unconditionally, so a Windows host sent `C:\Program
    /// Files\Git\bin\bash.exe` — or PowerShell — into `docker exec`, where the
    /// default image ships `sh` alone, and every command failed before running.
    #[tokio::test]
    async fn a_containered_command_gets_the_containers_shell_not_this_machines() {
        let recorder = std::sync::Arc::new(ArgvRecorder::default());
        let context = ToolContext {
            working_directory: Some("/the/project".into()),
            // The most host-bound choice there is: proof the selection is
            // ignored where the command does not run on the host.
            shell: ShellType::PowerShell,
            file_access: crate::tools::FileAccess::Unrestricted,
            project_id: None,
            conversation_id: Some("c-1".into()),
            turn_id: None,
            assistant_id: None,
            db_pool: None,
            sandbox_policy: Some(crate::sandbox::SandboxPolicy {
                project_dir: Some(std::path::PathBuf::from("/the/project")),
                backend: SandboxBackend::Container,
                connector: Some(recorder.clone()),
                conversation_id: Some("c-1".into()),
                ..Default::default()
            }),
            tool_secrets: Default::default(),
            cancel: tokio_util::sync::CancellationToken::new(),
            journal: None,
        };

        RunCommandTool
            .execute(serde_json::json!({"command": "echo hi"}), &context)
            .await
            .unwrap();

        let argv = recorder.argv.lock().unwrap().clone().expect("the connector ran");
        assert_eq!(&argv[..2], &["sh".to_string(), "-c".to_string()], "{argv:?}");
        assert_eq!(argv[2], "echo hi");
    }

    fn exec_res(exit_code: i32, stderr: &str, sandboxed: bool, timed_out: bool) -> ExecResult {
        ExecResult {
            exit_code,
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
            timed_out,
            truncated: false,
            ran_under: if sandboxed {
                SandboxBackend::WindowsRestrictedToken
            } else {
                SandboxBackend::Host
            },
        }
    }

    #[test]
    fn denied_on_access_keywords() {
        assert!(is_sandbox_denied(&exec_res(1, "拒绝访问。", true, false)));
        assert!(is_sandbox_denied(&exec_res(1, "Access is denied.", true, false)));
        assert!(is_sandbox_denied(&exec_res(
            1,
            "mkdir: cannot create directory: Permission denied",
            true,
            false
        )));
        // Git Bash dying at startup under the restricted token
        assert!(is_sandbox_denied(&exec_res(
            256,
            "0 [main] bash (123) bash.exe: *** fatal error - CreateFileMapping S-1-5-21-x.1, Win32 error 5.  Terminating.",
            true,
            false,
        )));
    }

    #[test]
    fn not_denied_without_keywords_or_sandbox() {
        assert!(!is_sandbox_denied(&exec_res(
            127,
            "bash: foo: command not found",
            true,
            false
        )));
        assert!(!is_sandbox_denied(&exec_res(0, "", true, false)));
        assert!(!is_sandbox_denied(&exec_res(1, "Access is denied.", false, false)));
        assert!(!is_sandbox_denied(&exec_res(1, "Access is denied.", true, true)));
        assert!(!is_sandbox_denied(&exec_res(1, "some other failure", true, false)));
    }

    /// **The escalation gate.** The card this produces says "retry without the
    /// sandbox", and what that means is decided entirely by which backend
    /// refused: for a restricted token it is the same command on the same
    /// machine with the token removed, and for anything that confines a command
    /// *elsewhere* it is a different and much larger action than the one the
    /// user is being asked about.
    ///
    /// Two things stand between a backend and that card and both are asserted
    /// here, because either alone is one edit away from being bypassed: the
    /// denial heuristic only recognises the backend whose words these are, and
    /// `may_retry_on_host` only answers for the backend whose escalation is
    /// safe.
    #[test]
    fn only_the_restricted_token_can_reach_the_host_retry_card() {
        // The words of a Windows refusal, produced by something else. Both
        // gates say no, so no card is offered.
        let elsewhere = ExecResult {
            exit_code: 1,
            stdout: Vec::new(),
            stderr: b"mkdir: cannot create directory: Permission denied".to_vec(),
            timed_out: false,
            truncated: false,
            ran_under: SandboxBackend::Host,
        };
        assert!(!is_sandbox_denied(&elsewhere));
        assert!(!elsewhere.ran_under.may_retry_on_host());

        // And the one that may.
        assert!(SandboxBackend::WindowsRestrictedToken.may_retry_on_host());
        assert!(is_sandbox_denied(&exec_res(1, "Access is denied.", true, false)));
    }

    /// `ran_under` is what happened, not what was asked for. A caller reading
    /// the *request* would call an unconfined command sandboxed on any platform
    /// where the backend it named does not exist.
    #[test]
    fn a_result_reports_what_confined_it() {
        assert!(!exec_res(0, "", false, false).was_sandboxed());
        assert!(exec_res(0, "", true, false).was_sandboxed());
    }

    #[test]
    fn sandbox_denied_marker_roundtrip() {
        let encoded = crate::tools::encode_sandbox_denied("blocked output");
        assert_eq!(crate::tools::decode_sandbox_denied(&encoded), Some("blocked output"));
        assert_eq!(crate::tools::decode_sandbox_denied("plain error"), None);
    }

    #[test]
    fn structured_result_preserves_the_model_tools_text_contract() {
        let result = CommandExecution {
            stdout: "out".into(),
            stderr: "err".into(),
            exit_code: 7,
            timed_out: false,
            truncated: true,
            sandbox: SandboxBackend::Host,
            duration_ms: 12,
        };
        assert_eq!(
            result.formatted(),
            "out\n[stderr] err\n[exit code: 7]\n[output truncated at 256KB]"
        );

        let timed_out = CommandExecution {
            timed_out: true,
            ..result
        };
        assert_eq!(
            timed_out.formatted(),
            "out\n[stderr] err\n[timed out; process tree killed]\n[output truncated at 256KB]"
        );
    }
}

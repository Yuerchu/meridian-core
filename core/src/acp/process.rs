//! Starting the adapter, and keeping its complaints.
//!
//! Everything here is the half of [`crate::mcp::stdio`] that is about running a
//! child process rather than about talking to one, and it is copied on purpose:
//! each line of it was a bug. stderr is drained continuously because a full pipe
//! blocks the child; the last few lines are kept because a process that fails to
//! start explains itself there and nowhere else; `kill_on_drop` covers every
//! path that abandons a session without shutting it down; `CREATE_NO_WINDOW`
//! stops a console flashing on Windows.
//!
//! What is *not* copied is the request/response pairing. That lives in
//! [`super::peer`], which needs the streams whole.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::io::BufReader;
use tokio::process::{Child, ChildStdin, ChildStdout};

/// Lines of the adapter's stderr kept for a failure report.
///
/// The same exception to "log a length, never the content" that MCP makes, for
/// the same reason: `command not found`, a missing module, a node stack trace.
/// Without these, "the session won't start" has no diagnosable cause. They go
/// through the same redaction as everything else.
const STDERR_TAIL_LINES: usize = 8;
const STDERR_LINE_CHARS: usize = 400;

/// Set on the adapter, and inherited by everything the agent runs.
///
/// The agent inside loads the user's own Claude Code configuration, which on
/// this machine includes the `meridian-plan-gate` plugin — so without this a
/// hosted turn ends by asking *this* app to review it over the loopback hook
/// endpoint. That gate exists for sessions Meridian did not start: it fetches a
/// diff, runs a second model for minutes while the turn waits on the hook, and
/// files the result as another conversation in the sidebar. Reviewing a session
/// this app is already hosting, with a transcript it already has, is the same
/// work done twice and a turn that appears to hang at the end.
///
/// The plugin reads this and stands down. It lives in another repository
/// (`~/.claude/plugins/local/meridian-plan-gate`), so the name is a contract
/// between the two and changing it needs both.
///
/// **Setting it on the child is not enough once the child is a container
/// launcher.** Measured: `docker run` does not forward the client's
/// environment, so an adapter configured as `docker run … claude-agent-acp` —
/// which `acp.command` has always permitted, and which is how somebody
/// containerises a hosted session today — gets an agent that cannot see this.
/// The plugin then does not stand down, and every hosted turn ends by asking
/// this app to review a transcript it already has: a second model for minutes,
/// and another conversation in the sidebar. Nothing fails; it just quietly
/// costs twice. [`forward_marker_into_container`] is what closes that.
pub const HOSTED_MARKER: &str = "MERIDIAN_ACP_HOSTED";

/// Container launchers whose `run` takes `-e` the way Docker's does.
///
/// A short list rather than a guess at any command that might start a
/// container: what this does is *rewrite somebody's configured command*, and
/// the bar for that is knowing exactly what the flag means to the thing being
/// rewritten. Anything not on it is left alone.
pub(super) const CONTAINER_LAUNCHERS: &[&str] = &["docker", "podman", "nerdctl"];

/// Add `-e MERIDIAN_ACP_HOSTED` to a `run`, so the marker reaches the agent.
///
/// **Rewriting a user's command is intrusive, and the alternative is worse.**
/// The marker is a contract with another repository; breaking it silently
/// produces no error, only a second model running for minutes at the end of
/// every hosted turn. So the one case that is unambiguous — a known launcher,
/// a `run` subcommand, no forwarding already present — is repaired, and
/// everything else is left exactly as written.
///
/// The bare `-e NAME` form is used rather than `-e NAME=1`: it forwards
/// whatever this process has, which is the value `spawn` just set, so there is
/// one place the value is decided.
///
/// The flag goes immediately after the subcommand. Anywhere later risks landing
/// after the image name, where it would be an argument to the *agent* rather
/// than to the launcher.
/// Whether this command starts the adapter inside a container: a known
/// launcher with a `run` subcommand. The same reading `forward_marker_into_container`
/// repairs and `mounts::MountMap` translates against — and what the bridge
/// asks before advertising an endpoint, since `127.0.0.1` inside a container
/// is the container.
pub(super) fn launches_in_container(command: &str, args: &[String]) -> bool {
    let program = std::path::Path::new(command)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(command)
        .to_ascii_lowercase();
    CONTAINER_LAUNCHERS.contains(&program.as_str()) && args.iter().any(|a| a == "run")
}

fn forward_marker_into_container(command: &str, args: &[String]) -> Vec<String> {
    let program = std::path::Path::new(command)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(command)
        .to_ascii_lowercase();
    if !CONTAINER_LAUNCHERS.contains(&program.as_str()) {
        return args.to_vec();
    }
    // Already forwarded, in either spelling. Adding a second is not harmful and
    // does read as this app not knowing what is in the command it is editing.
    if args
        .iter()
        .any(|a| a == HOSTED_MARKER || a.starts_with(&format!("{HOSTED_MARKER}=")))
    {
        return args.to_vec();
    }
    let Some(run_at) = args.iter().position(|a| a == "run") else {
        // `exec`, `start`, or something else. `-e` is not universally accepted
        // there and the container it would enter is not one this app made.
        return args.to_vec();
    };

    let mut out = args.to_vec();
    out.splice(run_at + 1..run_at + 1, ["-e".to_string(), HOSTED_MARKER.to_string()]);
    tracing::debug!(
        program = %program,
        "forwarded the hosted marker into the adapter's container"
    );
    out
}

/// What `command` actually names on this system, when the OS will not work it
/// out for itself.
///
/// `CreateProcessW` tries the literal name and the name plus `.exe`, and stops
/// — it never consults `PATHEXT`. Every node tool ships as a `.cmd` shim, so
/// `npx` on disk is `npx.cmd`, and the default configuration fails with
/// "program not found" while the identical command works in any shell.
/// Measured on a machine with node installed: `npx` fails to spawn, `npx.cmd`
/// starts, `node` starts because it happens to be a real `.exe`.
///
/// Returns `None` when there is nothing to correct, and the caller spawns what
/// it was handed. An extension already present is left alone rather than
/// second-guessed: it is either right or it is the user telling us something we
/// should not override.
#[cfg(target_os = "windows")]
fn resolve_on_path(command: &str) -> Option<std::path::PathBuf> {
    use std::path::{Path, PathBuf};

    let given = Path::new(command);
    if given.extension().is_some() {
        return None;
    }
    let stem = given.file_name()?.to_str()?.to_string();

    let from_env: Vec<String> = std::env::var("PATHEXT")
        .map(|raw| {
            raw.split(';')
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let extensions = if from_env.is_empty() {
        [".COM", ".EXE", ".BAT", ".CMD"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    } else {
        from_env
    };

    let first_hit = |dir: &Path| -> Option<PathBuf> {
        extensions.iter().find_map(|ext| {
            let candidate = dir.join(format!("{stem}{ext}"));
            candidate.is_file().then_some(candidate)
        })
    };

    // Anything carrying a separator is a location, not a name to look up.
    match given.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => first_hit(dir),
        _ => std::env::split_paths(&std::env::var_os("PATH")?).find_map(|dir| first_hit(&dir)),
    }
}

/// The program to hand `Command`, once the platform's own resolution has been
/// helped along.
fn program_for(command: &str) -> std::ffi::OsString {
    #[cfg(target_os = "windows")]
    {
        if let Some(found) = resolve_on_path(command) {
            return found.into_os_string();
        }
    }
    std::ffi::OsString::from(command)
}

/// A running adapter, taken apart into the pieces the peer needs.
pub struct AdapterProcess {
    pub child: Child,
    pub stdin: ChildStdin,
    pub stdout: BufReader<ChildStdout>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
}

impl AdapterProcess {
    /// Start `command` with `args`, wired for JSON-RPC over stdio.
    ///
    /// `cwd` is not set here even though every session has one: the adapter is
    /// told the working directory in `session/new`, and one process is meant to
    /// carry several sessions in different repositories. Launching it inside one
    /// of them would make the first session's directory silently special.
    pub async fn spawn(command: &str, args: &[String]) -> Result<Self, String> {
        // Set on the child, and — when the child is a container launcher —
        // forwarded past it. `.env` alone stops at the launcher; see
        // [`HOSTED_MARKER`].
        let args = forward_marker_into_container(command, args);
        let mut cmd = tokio::process::Command::new(program_for(command));
        cmd.args(&args)
            .env(HOSTED_MARKER, "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        // CREATE_NO_WINDOW: without it every adapter launch flashes a console.
        // `tokio::process::Command` carries this itself, so the std extension
        // trait is not needed.
        #[cfg(target_os = "windows")]
        cmd.creation_flags(0x08000000);

        let mut child = cmd.spawn().map_err(|e| {
            tracing::error!(
                command = %command,
                arg_count = args.len(),
                error = %e,
                "failed to spawn the ACP adapter"
            );
            // Named explicitly, and with where to look: the usual cause is the
            // command not being on PATH, and an error that does not say what it
            // tried to run sends the user looking in the wrong place. Worth
            // spelling out for an installed build in particular, which inherits
            // the PATH the session was logged in with — so a node installed
            // after that is invisible until the next sign-in.
            format!("could not start `{command}`: {e}. Check it is installed and on PATH.")
        })?;

        let stdin = child.stdin.take().ok_or("the adapter has no stdin")?;
        let stdout = child.stdout.take().ok_or("the adapter has no stdout")?;

        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES)));
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            let command = command.to_string();
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let line = crate::secrets::sanitizer::redact_secrets(line);
                    let line = crate::util::take_bytes_at_char_boundary(&line, STDERR_LINE_CHARS).to_string();
                    tracing::debug!(command = %command, "acp stderr: {line}");
                    if let Ok(mut tail) = tail.lock() {
                        if tail.len() == STDERR_TAIL_LINES {
                            tail.pop_front();
                        }
                        tail.push_back(line);
                    }
                }
            });
        }

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr_tail,
        })
    }

    /// A handle to the stderr tail that outlives this struct being taken apart.
    ///
    /// The peer splits the process into a reader task and a writer task, and by
    /// the time either of them discovers the adapter is gone, neither owns this
    /// value any more. Both still have to be able to say why.
    pub fn stderr_handle(&self) -> StderrTail {
        StderrTail(self.stderr_tail.clone())
    }
}

/// What the adapter last said on stderr. Cheap to clone, shared with the task
/// draining the pipe.
#[derive(Clone)]
pub struct StderrTail(Arc<Mutex<VecDeque<String>>>);

impl StderrTail {
    pub fn get(&self) -> String {
        self.0
            .lock()
            .map(|tail| tail.iter().cloned().collect::<Vec<_>>().join(" | "))
            .unwrap_or_default()
    }

    /// The tail as a suffix for an error message, or nothing at all when the
    /// adapter died silently. `format!("{e}{}", tail.suffix())` reads correctly
    /// either way.
    pub fn suffix(&self) -> String {
        let tail = self.get();
        if tail.is_empty() {
            String::new()
        } else {
            format!(" (adapter said: {tail})")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    /// What the bridge asks before advertising its endpoint: a `run` on a known
    /// launcher means the adapter's `127.0.0.1` is not this machine. An `exec`
    /// enters a container this app did not start and is left alone here for
    /// the same reason the marker rewrite leaves it alone — but it still
    /// counts as "not reachable" being false only because nothing is known
    /// about it, so only the unambiguous `run` answers yes.
    #[test]
    fn a_container_launch_is_recognised_and_nothing_else_is() {
        assert!(launches_in_container("docker", &argv(&["run", "-i", "--rm", "img"])));
        assert!(launches_in_container("podman", &argv(&["run", "img"])));
        assert!(launches_in_container(
            "C:\\Program Files\\Docker\\docker.exe",
            &argv(&["run", "img"])
        ));
        assert!(!launches_in_container("docker", &argv(&["exec", "some-container"])));
        assert!(!launches_in_container(
            "npx",
            &argv(&["-y", "@agentclientprotocol/claude-agent-acp"])
        ));
    }

    /// The case the whole thing exists for. Measured: `docker run` does not
    /// forward the client's environment, so `.env()` alone leaves the agent
    /// unable to see the marker — and the failure is silent, costing a second
    /// model at the end of every hosted turn rather than an error.
    #[test]
    fn the_marker_is_forwarded_past_a_container_launcher() {
        let out = forward_marker_into_container(
            "docker",
            &argv(&["run", "-i", "--rm", "node:22", "npx", "claude-agent-acp"]),
        );
        assert_eq!(
            out,
            argv(&[
                "run",
                "-e",
                "MERIDIAN_ACP_HOSTED",
                "-i",
                "--rm",
                "node:22",
                "npx",
                "claude-agent-acp"
            ])
        );
    }

    /// Immediately after the subcommand, never appended. Anywhere after the
    /// image name it stops being a flag to the launcher and becomes an argument
    /// to the agent.
    #[test]
    fn the_flag_lands_before_the_image_and_not_after_it() {
        let out = forward_marker_into_container("docker", &argv(&["run", "alpine", "sh"]));
        let image = out.iter().position(|a| a == "alpine").unwrap();
        let flag = out.iter().position(|a| a == "-e").unwrap();
        assert!(flag < image, "{out:?}");
    }

    /// Everything not unambiguously a container `run` is left exactly as
    /// written — the bar for rewriting somebody's configured command is knowing
    /// what the flag means to the thing being rewritten.
    #[test]
    fn anything_else_is_left_alone() {
        // The ordinary default.
        let npx = argv(&["-y", "@agentclientprotocol/claude-agent-acp"]);
        assert_eq!(forward_marker_into_container("npx", &npx), npx);
        // A launcher, but not a `run`: `-e` is not universally accepted there,
        // and the container it enters is not one this app made.
        let exec = argv(&["exec", "-i", "some-container", "claude-agent-acp"]);
        assert_eq!(forward_marker_into_container("docker", &exec), exec);
        // A command that merely mentions one.
        let mentions = argv(&["run", "--docker-ish"]);
        assert_eq!(forward_marker_into_container("my-wrapper", &mentions), mentions);
    }

    /// A path, and a `.exe`, are the same launcher. The configured command is
    /// whatever made `docker` reachable on this machine.
    #[test]
    fn a_launcher_is_recognised_however_it_was_written() {
        for command in [
            "docker",
            "docker.exe",
            "/usr/bin/docker",
            "C:\\Program Files\\Docker\\docker.exe",
            "PODMAN",
        ] {
            let out = forward_marker_into_container(command, &argv(&["run", "alpine"]));
            assert!(out.contains(&"-e".to_string()), "{command} was not recognised: {out:?}");
        }
    }

    /// Somebody who forwarded it themselves gets no second copy — harmless, and
    /// it reads as this app not knowing what is in the command it just edited.
    #[test]
    fn a_marker_already_forwarded_is_not_forwarded_twice() {
        for existing in [
            argv(&["run", "-e", "MERIDIAN_ACP_HOSTED", "alpine"]),
            argv(&["run", "-e", "MERIDIAN_ACP_HOSTED=1", "alpine"]),
        ] {
            let out = forward_marker_into_container("docker", &existing);
            assert_eq!(out, existing);
            assert_eq!(out.iter().filter(|a| *a == "-e").count(), 1, "{out:?}");
        }
    }

    #[tokio::test]
    async fn a_command_that_does_not_exist_names_itself_in_the_error() {
        let Err(err) = AdapterProcess::spawn("meridian-no-such-adapter-binary", &[]).await else {
            panic!("spawning a binary that does not exist must fail");
        };
        assert!(
            err.contains("meridian-no-such-adapter-binary"),
            "the error must name what it tried to run, got: {err}"
        );
    }

    /// The default configuration has to work on the platform most people will
    /// run it on.
    ///
    /// `npx` is the shipped default and on Windows it exists only as `npx.cmd`
    /// — plus an extensionless shell script that `CreateProcessW` cannot
    /// execute. Without resolution the first thing a user sees is "program not
    /// found" for a command that runs fine if they paste it into a terminal.
    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn the_default_command_starts_on_windows() {
        if resolve_on_path("node").is_none() {
            eprintln!("skipping: node is not installed");
            return;
        }

        let found = resolve_on_path("npx").expect("npx ships with node");
        assert_eq!(
            found.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase),
            Some("cmd".to_string()),
            "npx resolves to its shim, not to the extensionless shell script: {found:?}"
        );

        // And it is the whole point that this now starts.
        assert!(
            AdapterProcess::spawn("npx", &["--version".into()]).await.is_ok(),
            "the shipped default must be spawnable"
        );
    }

    /// A name the caller spelled out is not second-guessed, and neither is a
    /// path — the search is only ever for an extension the platform would have
    /// found for itself anywhere else.
    #[cfg(target_os = "windows")]
    #[test]
    fn an_explicit_extension_is_left_alone() {
        assert!(resolve_on_path("npx.cmd").is_none());
        assert!(resolve_on_path(r"C:\tools\adapter.exe").is_none());
        assert!(
            resolve_on_path("meridian-no-such-command-anywhere").is_none(),
            "nothing to find means nothing to correct"
        );
    }

    /// The tail is read by tasks that no longer own the process, so it has to
    /// survive being handed out — and read as nothing when there is nothing.
    #[test]
    fn an_empty_tail_contributes_no_suffix() {
        let tail = StderrTail(Arc::new(Mutex::new(VecDeque::new())));
        assert_eq!(tail.get(), "");
        assert_eq!(tail.suffix(), "");
    }

    #[test]
    fn a_tail_reads_oldest_first_and_is_bounded() {
        let shared = Arc::new(Mutex::new(VecDeque::new()));
        let tail = StderrTail(shared.clone());
        {
            let mut t = shared.lock().unwrap();
            for i in 0..STDERR_TAIL_LINES + 3 {
                if t.len() == STDERR_TAIL_LINES {
                    t.pop_front();
                }
                t.push_back(format!("line {i}"));
            }
        }
        let got = tail.get();
        assert!(!got.contains("line 0"), "the oldest lines are dropped");
        assert!(got.contains(&format!("line {}", STDERR_TAIL_LINES + 2)));
        assert!(tail.suffix().starts_with(" (adapter said: "));
    }
}

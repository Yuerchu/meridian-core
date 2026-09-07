//! Running a command somewhere other than this machine.
//!
//! One container per *conversation*, which is what makes a session's filesystem
//! continuous: something `pip install`ed by one command is still there for the
//! next. Everything below follows from that and from what Docker was measured
//! to actually do — `tests/docker_probe.rs` is the measurement, and it should
//! be re-run before any of this is trusted on a daemon it has not seen.
//!
//! # Cancelling is not killing the client
//!
//! **Measured: killing the `docker exec` process leaves the command running
//! inside.** So this cannot reuse the host path's process-tree kill, and a
//! cancelled command would otherwise keep writing to the workspace while the
//! turn believes it stopped — with the next command entering a container that
//! has a predecessor loose in it.
//!
//! The mechanism has to reach inside, and the obvious way to do that does not
//! work: `pkill -f <pattern>` from a second exec kills its own shell, because
//! its own argv contains the pattern it is searching for. Measured, it exits
//! 143. So each exec carries an id in its *environment*, and the killer only
//! ever *names* that id on its command line — matching on `/proc/*/environ`,
//! which the killer does not have. A pid would be simpler and is not available:
//! the client is never told one.
//!
//! **When the kill cannot be confirmed, the container goes.** Not a "poisoned"
//! flag for later commands to consult: the whole value of a session container
//! is that its filesystem is continuous, and a container with an unknown
//! process still running in it does not have that property in any useful sense.
//! Removing it costs the writable layer, which is the honest price of not
//! knowing what is still in there, and the next command starts clean.
//!
//! # What it does not promise
//!
//! **No continuous working directory.** Measured: `cd /tmp` in one exec leaves
//! the next at `/`. Each exec is given its directory explicitly, exactly as
//! `working_dir_or_current()` already resolves one per call. Parsing `cd` out
//! of shell commands to maintain a logical cwd is an approximation that can
//! never be made to agree with what the shell did, so it is not attempted.
//!
//! **Nothing outside the workspace is reachable.** A path that does not resolve
//! under the mounted root is an error rather than a silent fallback to the
//! host, which is the one outcome a sandbox must not have.
//!
//! # Ownership
//!
//! Every container carries [`OWNER_LABEL`], so everything this app started can
//! be found without remembering names — which is what reclaim after a crash
//! needs, and measured, a label is enough. Names are derived from the
//! conversation so a restart finds its own containers rather than making
//! second ones beside them.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::sandbox::{ExecError, ExecResult, SandboxBackend};

/// On every container this app creates. The value is the schema version of
/// what the label means, so a future layout can tell its own containers from
/// this one's rather than adopting them.
pub const OWNER_LABEL: &str = "cn.yuxiaoqiu.meridian.owner";
pub const OWNER_LABEL_VALUE: &str = "1";

/// Where a conversation's project is mounted inside its container.
///
/// Fixed rather than mirroring the host path: a Windows path has no meaning
/// inside a Linux container, and a mount point that varied per conversation
/// would put the host's directory layout into every command the model writes.
pub const WORKSPACE_MOUNT: &str = "/workspace";

/// How long to wait for a `docker` invocation that is *not* the command
/// itself — creating, inspecting, killing. The command's own timeout is the
/// caller's.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

/// Where a command runs.
///
/// A trait rather than a match inside `sandbox::execute`, because an
/// implementation needs a lifetime longer than one call — a conversation's
/// container is created once and entered many times — and because the thing
/// that owns it is `Services`, which knows nothing about executing commands.
/// `Debug` is required rather than merely useful: `SandboxPolicy` derives it,
/// and a policy that cannot be printed is one that cannot be logged when a turn
/// fails — which is exactly when somebody wants to know where the command was
/// going to run.
#[async_trait::async_trait]
pub trait CommandConnector: Send + Sync + std::fmt::Debug {
    /// Which backend this reports itself as. Read into `ExecResult::ran_under`,
    /// so a refusal is classified against what actually confined it.
    fn backend(&self) -> SandboxBackend;

    /// Run one command to completion, or until it is cancelled or times out.
    ///
    /// `cwd` is a **host** path. Translating it is the connector's job: only it
    /// knows where the workspace is mounted, and a caller that translated would
    /// have to know too.
    ///
    /// `workspace` is what gets mounted, and it is the *policy's* project
    /// directory rather than the command's cwd. The two used to be one
    /// parameter, and that was a boundary defect: a custom tool sets its own
    /// working directory, and taking that as the mount source meant the first
    /// command of a conversation decided what the sandbox contains — `/`, if
    /// that was its cwd. A cwd outside `workspace` is refused, not mounted.
    async fn execute(
        &self,
        argv: &[String],
        cwd: &Path,
        workspace: &Path,
        conversation_id: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<ExecResult, ExecError>;
}

#[derive(Debug, Clone)]
pub struct ContainerConfig {
    /// Resolved to a digest when a container is created, so that "the same
    /// conversation" keeps meaning the same environment even if the tag moves.
    pub image: String,
    /// The client to invoke. A setting because `docker` is not always on PATH
    /// under the environment a GUI process inherits.
    pub binary: String,
}

impl Default for ContainerConfig {
    fn default() -> Self {
        Self {
            image: "alpine:3.20".into(),
            binary: "docker".into(),
        }
    }
}

/// One container per conversation, entered per command.
#[derive(Debug)]
pub struct DockerConnector {
    config: ContainerConfig,
    /// Serialises creation. Held only across the create-or-reuse round trip,
    /// never across a command: two commands in one conversation would otherwise
    /// both find no container and both try to make one, and the loser's error
    /// is indistinguishable from the daemon being down.
    ///
    /// One lock for every conversation rather than one each, because creation
    /// is rare, takes about a second, and is not on the path a running command
    /// takes.
    creating: tokio::sync::Mutex<HashMap<String, String>>,
}

impl DockerConnector {
    pub fn new(config: ContainerConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            creating: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    async fn docker(&self, args: &[&str]) -> Result<Output, ExecError> {
        let run = Command::new(&self.config.binary)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output();

        let out = tokio::time::timeout(CONTROL_TIMEOUT, run)
            .await
            .map_err(|_| ExecError::Internal(format!("`{} {}` did not answer", self.config.binary, args[0])))?
            .map_err(|e| {
                // Named explicitly: the usual cause is the binary not being on
                // the PATH a GUI process inherits, and an error that does not
                // say what it tried to run sends people to the wrong place.
                ExecError::Spawn(format!(
                    "could not run `{}`: {e}. Check Docker is installed and running.",
                    self.config.binary
                ))
            })?;

        Ok(Output {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }

    /// The container for this conversation, creating it if there is not one.
    async fn ensure(&self, conversation_id: &str, workspace: &Path) -> Result<String, ExecError> {
        let name = container_name(conversation_id);
        let mut known = self.creating.lock().await;

        // Asked of the daemon rather than trusted from the map: a container can
        // be removed by anything with a Docker client, and a cached name that
        // no longer exists produces "no such container" on the *command*, which
        // reads to the user as their command failing.
        let running = self.docker(&["inspect", "-f", "{{.State.Running}}", &name]).await?;
        if running.status == 0 && running.stdout == "true" {
            known.insert(conversation_id.to_string(), name.clone());
            return Ok(name);
        }
        if running.status == 0 {
            // It exists and is stopped — the app was restarted, or somebody
            // stopped it. Starting is what keeps the writable layer.
            let started = self.docker(&["start", &name]).await?;
            if started.status == 0 {
                known.insert(conversation_id.to_string(), name.clone());
                return Ok(name);
            }
            // It would not start. Remove it rather than leaving a name that
            // blocks every later attempt.
            let _ = self.docker(&["rm", "-f", &name]).await;
        }

        let mount = mount_argument(workspace)?;
        let label = format!("{OWNER_LABEL}={OWNER_LABEL_VALUE}");
        let created = self
            .docker(&[
                "run",
                "-d",
                "--name",
                &name,
                "--label",
                &label,
                "-v",
                &mount,
                "-w",
                WORKSPACE_MOUNT,
                // The container has to outlive each exec, which is the whole
                // difference between `docker run` and `docker exec`.
                &self.config.image,
                "sleep",
                "infinity",
            ])
            .await?;

        if created.status != 0 {
            return Err(ExecError::Spawn(format!(
                "could not create a container for this conversation: {}",
                created.stderr
            )));
        }
        known.insert(conversation_id.to_string(), name.clone());
        Ok(name)
    }

    /// Stop trying to kill and take the whole container instead.
    ///
    /// See the module note: a container with an unknown process still running
    /// in it has lost the one property a session container is for.
    async fn discard(&self, name: &str, conversation_id: &str) {
        let _ = self.docker(&["rm", "-f", name]).await;
        self.creating.lock().await.remove(conversation_id);
    }

    /// Signal an exec by the id it carries, and say whether it is gone.
    async fn kill_exec(&self, name: &str, exec_id: &str) -> bool {
        let script = killer_script(exec_id);
        let killed = self.docker(&["exec", name, "sh", "-c", &script]).await;
        matches!(killed, Ok(out) if out.status == 0 && out.stdout == "gone")
    }

    /// The names of every container this app owns, whether or not running.
    ///
    /// Names rather than ids, because a name is derived from a conversation
    /// (`container_name`) and can therefore be compared against the rows that
    /// still exist — which is what telling an orphan from a survivor takes.
    async fn owned(&self) -> Vec<String> {
        let label = format!("label={OWNER_LABEL}={OWNER_LABEL_VALUE}");
        match self
            .docker(&["ps", "-a", "--filter", &label, "--format", "{{.Names}}"])
            .await
        {
            Ok(out) if out.status == 0 => out
                .stdout
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Settle what an earlier run left behind, against the conversations that
    /// still exist.
    ///
    /// For startup. A container whose conversation is gone is removed — nothing
    /// can ever enter it again, and by-label discovery is what finds it without
    /// a record this app would have had to survive a crash to keep. A container
    /// whose conversation *does* still exist is stopped, not removed: its
    /// writable layer is the conversation's continuity across a restart, and
    /// `ensure` starts it again on the next command. **Removing everything**,
    /// which this used to do, contradicted that reuse branch — the two were
    /// written against each other and whichever ran first won.
    pub async fn reconcile(&self, live_conversation_ids: &[String]) -> (usize, usize) {
        let live: std::collections::HashSet<String> =
            live_conversation_ids.iter().map(|id| container_name(id)).collect();
        let (mut removed, mut stopped) = (0, 0);
        for name in self.owned().await {
            if live.contains(&name) {
                // Cheap on an already-stopped container, and `-t 1` because the
                // only thing inside between commands is `sleep`.
                if self
                    .docker(&["stop", "-t", "1", &name])
                    .await
                    .is_ok_and(|o| o.status == 0)
                {
                    stopped += 1;
                }
            } else if self.docker(&["rm", "-f", &name]).await.is_ok_and(|o| o.status == 0) {
                removed += 1;
            }
        }
        if removed > 0 || stopped > 0 {
            tracing::info!(removed, stopped, "settled containers left behind by an earlier run");
        }
        (removed, stopped)
    }

    /// Stop every owned container that is still running.
    ///
    /// For shutdown: `sleep infinity` keeps a conversation's container running
    /// for as long as the daemon does, and an app that has exited is not coming
    /// back for it until the next launch. Stopped, not removed — the writable
    /// layer is what a restart resumes. `reconcile` covers the exits this never
    /// sees, which is every crash.
    pub async fn stop_owned(&self) -> usize {
        let mut stopped = 0;
        for name in self.owned().await {
            if self
                .docker(&["stop", "-t", "1", &name])
                .await
                .is_ok_and(|o| o.status == 0)
            {
                stopped += 1;
            }
        }
        stopped
    }

    /// End one conversation's container.
    pub async fn close(&self, conversation_id: &str) {
        let name = container_name(conversation_id);
        // `stop` before `rm`: measured, it returns with the container already
        // stopped, which is the same invariant `acp::peer` holds for the
        // adapter — so what follows cannot race a container still shutting down.
        let _ = self.docker(&["stop", "-t", "5", &name]).await;
        let _ = self.docker(&["rm", "-f", &name]).await;
        self.creating.lock().await.remove(conversation_id);
    }
}

struct Output {
    status: i32,
    stdout: String,
    stderr: String,
}

#[async_trait::async_trait]
impl CommandConnector for DockerConnector {
    fn backend(&self) -> SandboxBackend {
        SandboxBackend::Container
    }

    async fn execute(
        &self,
        argv: &[String],
        cwd: &Path,
        workspace: &Path,
        conversation_id: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<ExecResult, ExecError> {
        if argv.is_empty() {
            return Err(ExecError::Spawn("empty command".into()));
        }
        // The workspace decides the mount; the cwd is merely resolved against
        // it, and `container_path` refusing a cwd outside it is the boundary.
        let name = self.ensure(conversation_id, workspace).await?;
        let inside = container_path(cwd, workspace)?;
        let exec_id = uuid::Uuid::new_v4().simple().to_string();

        let mut args: Vec<String> = vec![
            "exec".into(),
            "-w".into(),
            inside,
            "-e".into(),
            // The marker the killer looks for. On the *target*, never on the
            // killer, which is what stops it matching itself.
            format!("MERIDIAN_EXEC_ID={exec_id}"),
            name.clone(),
        ];
        args.extend_from_slice(argv);

        // The client is an ordinary child process, so the host path's bounded
        // capture, timeout and cancellation all apply to *it*. What they do not
        // do is reach the command inside, which is what the rest of this is for.
        let outcome = crate::sandbox::run_client(&self.config.binary, &args, timeout, cancel).await;

        match outcome {
            Ok(mut res) => {
                if res.timed_out {
                    self.stop_inside(&name, &exec_id, conversation_id).await;
                }
                res.ran_under = SandboxBackend::Container;
                Ok(res)
            }
            Err(ExecError::Cancelled) => {
                self.stop_inside(&name, &exec_id, conversation_id).await;
                Err(ExecError::Cancelled)
            }
            Err(e) => Err(e),
        }
    }
}

impl DockerConnector {
    /// Make sure the command really stopped, and take the container if it did
    /// not.
    async fn stop_inside(&self, name: &str, exec_id: &str, conversation_id: &str) {
        if self.kill_exec(name, exec_id).await {
            return;
        }
        tracing::warn!(
            conversation_id,
            "could not confirm a cancelled command had stopped; discarding the container"
        );
        self.discard(name, conversation_id).await;
    }
}

/// The container name for a conversation.
///
/// Derived rather than random so a restart finds its own containers instead of
/// making second ones beside them, and prefixed so that everything this app
/// owns is recognisable even to somebody reading `docker ps` by hand.
///
/// Conversation ids are uuids; anything not `[a-zA-Z0-9_.-]` is replaced,
/// because Docker's own name rule is narrower than a uuid needs but not
/// narrower than an id from an import might be.
pub fn container_name(conversation_id: &str) -> String {
    let safe: String = conversation_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("meridian-{safe}")
}

/// `-v host:container` for the workspace.
///
/// Absolute on both sides. Docker takes a Windows path on the left and the
/// daemon translates it, which is why nothing here rewrites the host half.
fn mount_argument(workspace: &Path) -> Result<String, ExecError> {
    let host = workspace
        .to_str()
        .ok_or_else(|| ExecError::Spawn("the project path is not valid UTF-8".into()))?;
    Ok(format!("{host}:{WORKSPACE_MOUNT}"))
}

/// Where a host path is inside the container.
///
/// **A path outside the workspace is an error, not a fallback.** Answering with
/// the host path would have the command run against a directory the container
/// does not have — or, worse, one it does, meaning something entirely different.
/// The only safe answer to "that is not in the sandbox" is to say so.
fn container_path(path: &Path, workspace: &Path) -> Result<String, ExecError> {
    let relative = path.strip_prefix(workspace).map_err(|_| {
        ExecError::Spawn(format!(
            "`{}` is outside this conversation's workspace, which is the only directory the \
             container can see",
            path.display()
        ))
    })?;

    let mut out = String::from(WORKSPACE_MOUNT);
    for part in relative.components() {
        let std::path::Component::Normal(part) = part else {
            // `..` cannot appear — `strip_prefix` on a normalised path does not
            // produce one — and a root or prefix component here would mean the
            // path was not actually under the workspace.
            return Err(ExecError::Spawn(format!(
                "`{}` cannot be resolved inside the container",
                path.display()
            )));
        };
        out.push('/');
        out.push_str(&part.to_string_lossy());
    }
    Ok(out)
}

/// Signal whatever carries `exec_id`, then say whether anything still does.
///
/// Matches on the environment rather than the command line, because the
/// killer's own command line contains `exec_id` — measured, `pkill -f` on a
/// pattern the killer itself carries exits 143, having killed its own shell.
/// Its environment does not carry it, so `/proc/*/environ` cannot match it.
fn killer_script(exec_id: &str) -> String {
    format!(
        "found=0; \
         for p in /proc/[0-9]*; do \
           tr '\\0' '\\n' < $p/environ 2>/dev/null | grep -qx 'MERIDIAN_EXEC_ID={exec_id}' || continue; \
           found=1; kill -TERM ${{p#/proc/}} 2>/dev/null; \
         done; \
         [ $found -eq 0 ] && echo gone && exit 0; \
         sleep 1; \
         for p in /proc/[0-9]*; do \
           tr '\\0' '\\n' < $p/environ 2>/dev/null | grep -qx 'MERIDIAN_EXEC_ID={exec_id}' || continue; \
           kill -KILL ${{p#/proc/}} 2>/dev/null; \
         done; \
         sleep 0.3; \
         for p in /proc/[0-9]*; do \
           tr '\\0' '\\n' < $p/environ 2>/dev/null | grep -qx 'MERIDIAN_EXEC_ID={exec_id}' && exit 1; \
         done; \
         echo gone"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_container_is_named_after_its_conversation() {
        let name = container_name("0198fd2a-4b6c-7000-8000-abcdef012345");
        assert_eq!(name, "meridian-0198fd2a-4b6c-7000-8000-abcdef012345");
        // Derived, so a restart finds the same one.
        assert_eq!(name, container_name("0198fd2a-4b6c-7000-8000-abcdef012345"));
    }

    /// Docker's name rule is narrower than what an id might carry, and a
    /// rejected name is a conversation that can never run a command.
    #[test]
    fn a_name_is_reduced_to_what_docker_accepts() {
        let name = container_name("a b/c:d");
        assert_eq!(name, "meridian-a-b-c-d");
        assert!(
            name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "{name}"
        );
    }

    #[test]
    fn a_path_inside_the_workspace_maps_onto_the_mount() {
        let workspace = Path::new("/home/me/project");
        assert_eq!(container_path(workspace, workspace).unwrap(), "/workspace");
        assert_eq!(
            container_path(Path::new("/home/me/project/src/lib"), workspace).unwrap(),
            "/workspace/src/lib"
        );
    }

    /// **The one that must not be a fallback.** Answering with the host path
    /// would run the command against a directory the container does not have,
    /// or against one it does that means something else entirely.
    #[test]
    fn a_path_outside_the_workspace_is_refused() {
        let workspace = Path::new("/home/me/project");
        let err = container_path(Path::new("/etc"), workspace).unwrap_err();
        assert!(matches!(err, ExecError::Spawn(_)));
        assert!(
            err.to_string().contains("outside this conversation's workspace"),
            "{err}"
        );
    }

    /// The killer names the id on its command line and matches on the
    /// environment. If it ever matched on `cmdline` it would find itself —
    /// measured, that is an exit 143 and a race over whether the target died.
    #[test]
    fn the_killer_matches_the_environment_and_not_the_command_line() {
        let script = killer_script("abc123");
        assert!(script.contains("/environ"), "{script}");
        assert!(!script.contains("cmdline"), "{script}");
        assert!(!script.contains("pkill"), "{script}");
        // TERM first, KILL only if it is still there.
        assert!(script.find("kill -TERM") < script.find("kill -KILL"), "{script}");
        // And it reports rather than assuming: "gone" is the only success.
        assert!(script.contains("echo gone"), "{script}");
    }

    #[test]
    fn the_mount_puts_the_workspace_where_commands_expect_it() {
        let mount = mount_argument(Path::new("/home/me/project")).unwrap();
        assert_eq!(mount, "/home/me/project:/workspace");
    }

    // ------------------------------------------------------ against a daemon
    //
    // Everything above is a pure function. None of it can reach the three
    // things this module is actually for — that a container is reused, that a
    // cancelled command really stops, and that a workspace is writable — and
    // those are exactly the ones that would fail silently.

    /// A conversation id nothing else will use, so a crashed run cannot make
    /// the next one flaky.
    fn probe_conversation(tag: &str) -> String {
        format!("probe-{tag}-1111")
    }

    async fn connector() -> Arc<DockerConnector> {
        DockerConnector::new(ContainerConfig::default())
    }

    /// The filesystem is continuous, which is the whole reason a container
    /// belongs to a conversation rather than to a command.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a Docker daemon"]
    async fn a_conversations_container_is_reused_and_keeps_what_was_written() {
        let docker = connector().await;
        let conversation = probe_conversation("reuse");
        let workspace = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();

        let run = |cmd: &str| {
            let docker = docker.clone();
            let conversation = conversation.clone();
            let dir = workspace.path().to_path_buf();
            let cancel = cancel.clone();
            let cmd = cmd.to_string();
            async move {
                docker
                    .execute(
                        &["sh".into(), "-c".into(), cmd],
                        &dir,
                        &dir,
                        &conversation,
                        Duration::from_secs(30),
                        &cancel,
                    )
                    .await
            }
        };

        let first = run("echo installed > /opt/marker; echo ok")
            .await
            .expect("first command");
        assert_eq!(first.exit_code, 0, "{}", String::from_utf8_lossy(&first.stderr));
        assert_eq!(first.ran_under, SandboxBackend::Container);

        let second = run("cat /opt/marker").await.expect("second command");
        assert_eq!(
            String::from_utf8_lossy(&second.stdout).trim(),
            "installed",
            "the second command entered a different container"
        );

        // And the workspace really is the project directory, writable, and the
        // same one the host can see.
        let wrote = run("echo from-inside > ./written-by-container").await.unwrap();
        assert_eq!(wrote.exit_code, 0, "{}", String::from_utf8_lossy(&wrote.stderr));
        let on_host = std::fs::read_to_string(workspace.path().join("written-by-container")).unwrap();
        assert_eq!(on_host.trim(), "from-inside");

        docker.close(&conversation).await;
    }

    /// **The one the whole cancellation mechanism exists for.** Measured
    /// separately in `tests/docker_probe.rs`: killing the client leaves the
    /// command running. So cancelling has to be seen to work here, against a
    /// real daemon, or "cancelled" means only that this app stopped watching.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a Docker daemon"]
    async fn a_cancelled_command_really_stops_inside_the_container() {
        let docker = connector().await;
        let conversation = probe_conversation("cancel");
        let workspace = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();

        let running = {
            let docker = docker.clone();
            let conversation = conversation.clone();
            let dir = workspace.path().to_path_buf();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                docker
                    .execute(
                        &["sh".into(), "-c".into(), "touch /tmp/probe-started; sleep 300".into()],
                        &dir,
                        &dir,
                        &conversation,
                        Duration::from_secs(120),
                        &cancel,
                    )
                    .await
            })
        };

        // Wait for it to really be running rather than sleeping a fixed time.
        let name = container_name(&conversation);
        let mut started = false;
        for _ in 0..100 {
            let seen = docker
                .docker(&["exec", &name, "test", "-f", "/tmp/probe-started"])
                .await;
            if matches!(seen, Ok(ref o) if o.status == 0) {
                started = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(started, "the command never started, so this test measured nothing");

        cancel.cancel();
        let outcome = running.await.expect("the task panicked");
        assert!(matches!(outcome, Err(ExecError::Cancelled)), "{outcome:?}");

        // The point: not that the client returned, but that nothing is left.
        let left = docker
            .docker(&["exec", &name, "sh", "-c", "ps -o args= | grep -c '[s]leep 300' || true"])
            .await
            .expect("could not look inside");
        assert_eq!(
            left.stdout, "0",
            "the command survived its own cancellation, which is what killing the client alone does"
        );

        docker.close(&conversation).await;
    }

    /// A crash leaves containers with nobody holding them. They are found by
    /// label, because a record kept in this process would have had to survive
    /// the crash to be of any use.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a Docker daemon"]
    async fn containers_left_behind_are_found_by_label_and_removed() {
        let docker = connector().await;
        let conversation = probe_conversation("orphan");
        let workspace = tempfile::tempdir().unwrap();

        docker
            .execute(
                &["true".into()],
                workspace.path(),
                workspace.path(),
                &conversation,
                Duration::from_secs(30),
                &CancellationToken::new(),
            )
            .await
            .expect("could not start a container to orphan");

        // A second connector, as a restarted app would be: it has no memory of
        // the container above and has to find it.
        let restarted = connector().await;
        assert!(
            !restarted.owned().await.is_empty(),
            "a container this app owns was not findable by its label"
        );
        // While its conversation still exists, a restart stops it — the
        // writable layer is the conversation's continuity — and does not
        // remove it.
        let (removed, stopped) = restarted.reconcile(std::slice::from_ref(&conversation)).await;
        assert_eq!(removed, 0, "a live conversation's container was removed");
        assert!(stopped >= 1);
        assert!(
            !restarted.owned().await.is_empty(),
            "stopping must keep the container findable"
        );
        // Once the conversation is gone, the same pass removes it.
        let (removed, _) = restarted.reconcile(&[]).await;
        assert!(removed >= 1);
        assert!(restarted.owned().await.is_empty(), "reconcile left containers behind");
    }
}

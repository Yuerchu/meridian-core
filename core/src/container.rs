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
//! Cancellation and timeout stop the entire verified session container by its
//! immutable ID, then inspect that ID to confirm a stopped state. The writable
//! layer is retained. Environment markers are not containment: descendants can
//! erase them. Uncertain failures and dropped execution futures quarantine the
//! session until verified maintenance recovers it. Per-session locks serialize
//! execution, reuse, and close.
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
//! Names and labels select candidates, not authorization. Structured inspection
//! checks owner/version, exact session, configured image tag and digest, and
//! the sole writable workspace bind before reuse or mutation. Mismatches and
//! legacy containers lacking workspace labels are left untouched. The image
//! must be available locally. Cleanup never deletes an unknown target.
//!
//! Reconciliation stops verified live sessions and removes verified orphans;
//! shutdown stops without removing. Both serialize against active executions.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::sandbox::{ExecError, ExecResult, OutputTee, SandboxBackend};

/// On every container this app creates. The value is the schema version of
/// what the label means, so a future layout can tell its own containers from
/// this one's rather than adopting them.
pub const OWNER_LABEL: &str = "cn.yuxiaoqiu.meridian.owner";
pub const OWNER_LABEL_VALUE: &str = "1";

const SESSION_LABEL: &str = "cn.yuxiaoqiu.meridian.conversation";
const SOURCE_LABEL: &str = "cn.yuxiaoqiu.meridian.workspace";

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Inspect {
    id: String,
    name: String,
    image: String,
    config: Config,
    mounts: Vec<Mount>,
    state: State,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Config {
    image: String,
    labels: HashMap<String, String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Mount {
    #[serde(rename = "Type")]
    kind: String,
    source: String,
    destination: String,
    #[serde(rename = "RW")]
    writable: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct State {
    status: String,
    running: bool,
    #[serde(default)]
    restarting: bool,
    #[serde(default)]
    paused: bool,
    #[serde(default)]
    dead: bool,
}

fn refused() -> ExecError {
    ExecError::Internal("session container identity or lifecycle does not match policy".into())
}

fn full_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Inspect {
    fn verify(
        &self,
        conversation: &str,
        workspace: Option<&str>,
        image: &str,
        image_id: &str,
    ) -> Result<(), ExecError> {
        let [mount] = self.mounts.as_slice() else {
            return Err(refused());
        };
        if !full_id(&self.id)
            || !image_id.strip_prefix("sha256:").is_some_and(full_id)
            || self.image != image_id
            || self.config.image != image
            || self.name != format!("/{}", container_name(conversation))
            || conversation.is_empty()
            || self.config.labels.get(OWNER_LABEL).map(String::as_str) != Some(OWNER_LABEL_VALUE)
            || self.config.labels.get(SESSION_LABEL).map(String::as_str) != Some(conversation)
            || self
                .config
                .labels
                .get(SOURCE_LABEL)
                .is_none_or(|s| normalized_source(s) != normalized_source(&mount.source))
            || !absolute_source(&mount.source)
            || workspace.is_some_and(|w| normalized_source(w) != normalized_source(&mount.source))
            || mount.kind != "bind"
            || !mount.writable
            || mount.destination != WORKSPACE_MOUNT
        {
            return Err(refused());
        }
        Ok(())
    }
    fn stopped(&self) -> bool {
        !self.state.running
            && !self.state.restarting
            && !self.state.paused
            && !self.state.dead
            && matches!(self.state.status.as_str(), "exited" | "created")
    }
}

#[cfg(test)]
mod identity_tests;

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
    shutting_down: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    driver: Option<Arc<lifecycle::tests::FakeDocker>>,
}

impl DockerConnector {
    pub fn new(config: ContainerConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            driver: None,
        })
    }

    async fn docker(&self, args: &[&str]) -> Result<Output, ExecError> {
        #[cfg(test)]
        if let Some(driver) = &self.driver {
            return driver.control(args);
        }
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
}

mod lifecycle;

#[derive(Debug)]
struct Output {
    status: i32,
    stdout: String,
    stderr: String,
}

fn absolute_source(source: &str) -> bool {
    source.starts_with('/')
        || (cfg!(windows)
            && source.as_bytes().get(1) == Some(&b':')
            && source.as_bytes().get(2).is_some_and(|b| matches!(b, b'/' | b'\\')))
}

fn normalized_source(source: &str) -> String {
    #[cfg(windows)]
    {
        let normalized = source.replace('\\', "/");
        let normalized = normalized.strip_prefix("//?/").unwrap_or(&normalized);
        for prefix in ["/run/desktop/mnt/host/", "/host_mnt/"] {
            if let Some(rest) = normalized.strip_prefix(prefix)
                && rest.as_bytes().get(1) == Some(&b'/')
                && rest.as_bytes()[0].is_ascii_alphabetic()
            {
                return format!("{}:{}", rest[..1].to_ascii_lowercase(), &rest[1..]);
            }
        }
        if normalized.as_bytes().get(1) == Some(&b':') {
            return format!("{}{}", normalized[..1].to_ascii_lowercase(), &normalized[1..]);
        }
        normalized.into()
    }
    #[cfg(not(windows))]
    source.into()
}

fn mount_source(workspace: &Path) -> Result<String, ExecError> {
    let source = workspace
        .to_str()
        .ok_or_else(|| ExecError::Spawn("project path is not valid UTF-8".into()))?;
    if !absolute_source(source) {
        return Err(ExecError::Spawn("container workspace must be absolute".into()));
    }
    Ok(source.into())
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

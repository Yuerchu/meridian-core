use super::*;
use std::sync::atomic::Ordering;

#[derive(Default)]
struct Session {
    quarantined: bool,
    closed: bool,
    known_id: Option<String>,
}

type Sessions = std::sync::Mutex<HashMap<(String, String), Arc<tokio::sync::Mutex<Session>>>>;
fn session_lock(binary: &str, conversation: &str) -> Arc<tokio::sync::Mutex<Session>> {
    static SESSIONS: std::sync::OnceLock<Sessions> = std::sync::OnceLock::new();
    SESSIONS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry((binary.into(), container_name(conversation)))
        .or_default()
        .clone()
}

// Maintenance is exclusive with executions, but executions for different
// sessions run concurrently. Shared across connector/configuration instances.
static LIFECYCLE: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

impl DockerConnector {
    async fn inspect(&self, id: &str) -> Result<Inspect, ExecError> {
        let out = self.docker(&["container", "inspect", id]).await?;
        if out.status != 0 {
            return Err(ExecError::Internal(format!(
                "container inspection failed: {}",
                out.stderr
            )));
        }
        let mut rows: Vec<Inspect> = serde_json::from_str(&out.stdout)
            .map_err(|e| ExecError::Internal(format!("invalid Docker inspect response: {e}")))?;
        if rows.len() != 1 {
            return Err(ExecError::Internal("expected one inspected container".into()));
        }
        Ok(rows.remove(0))
    }

    async fn image_id(&self) -> Result<String, ExecError> {
        let out = self
            .docker(&["image", "inspect", "--format", "{{.Id}}", &self.config.image])
            .await?;
        if out.status != 0 || !out.stdout.starts_with("sha256:") {
            return Err(ExecError::Internal(format!(
                "configured container image must be available locally: {}",
                out.stderr
            )));
        }
        Ok(out.stdout)
    }

    async fn find(&self, conversation: &str) -> Result<Option<Inspect>, ExecError> {
        let filter = format!("name=^/{}$", container_name(conversation));
        // A failed inspect is not evidence of absence. List successfully first.
        let out = self.docker(&["ps", "-aq", "--no-trunc", "--filter", &filter]).await?;
        if out.status != 0 {
            return Err(ExecError::Internal(format!(
                "container discovery failed: {}",
                out.stderr
            )));
        }
        let ids: Vec<_> = out.stdout.lines().filter(|s| !s.is_empty()).collect();
        match ids.as_slice() {
            [] => Ok(None),
            [id] => Ok(Some(self.inspect(id).await?)),
            _ => Err(ExecError::Internal("ambiguous session container discovery".into())),
        }
    }

    async fn stop_confirmed(&self, inspected: &Inspect) -> Result<(), ExecError> {
        // The caller has verified identity; never address a mutable name here.
        let stopped = self.docker(&["stop", "--time", "2", &inspected.id]).await;
        let after = self.inspect(&inspected.id).await?;
        if after.id != inspected.id || !after.stopped() {
            return Err(ExecError::Internal(
                "session stop could not be confirmed; session quarantined".into(),
            ));
        }
        // An error response can race a successful daemon-side stop. Inspection
        // of the same immutable ID is the authoritative result.
        let _ = stopped;
        Ok(())
    }

    async fn ensure_locked(
        &self,
        conversation: &str,
        workspace: &Path,
        state: &mut Session,
    ) -> Result<Inspect, ExecError> {
        let source = mount_source(workspace)?;
        let image_id = self.image_id().await?;
        let inspected = match self.find(conversation).await? {
            Some(row) => row,
            None => {
                let owner = format!("{OWNER_LABEL}={OWNER_LABEL_VALUE}");
                let session = format!("{SESSION_LABEL}={conversation}");
                let workspace_label = format!("{SOURCE_LABEL}={source}");
                let mount = mount_argument(workspace)?;
                let name = container_name(conversation);
                let created = self
                    .docker(&[
                        "create",
                        "--name",
                        &name,
                        "--label",
                        &owner,
                        "--label",
                        &session,
                        "--label",
                        &workspace_label,
                        "-v",
                        &mount,
                        "-w",
                        WORKSPACE_MOUNT,
                        &self.config.image,
                        "sleep",
                        "infinity",
                    ])
                    .await?;
                if created.status != 0 {
                    return Err(ExecError::Spawn(format!(
                        "could not create session container: {}",
                        created.stderr
                    )));
                }
                self.inspect(&created.stdout).await?
            }
        };
        inspected.verify(conversation, Some(&source), &self.config.image, &image_id)?;
        // A new connector/process must not inherit a running command from an
        // earlier lifetime, even when all identity fields match.
        if !inspected.stopped() && state.known_id.as_deref() != Some(&inspected.id) {
            self.stop_confirmed(&inspected).await?;
        }
        if inspected.stopped() || state.known_id.as_deref() != Some(&inspected.id) {
            let started = self.docker(&["start", &inspected.id]).await?;
            if started.status != 0 {
                return Err(ExecError::Internal(format!(
                    "session start failed; layer preserved: {}",
                    started.stderr
                )));
            }
        }
        let after = self.inspect(&inspected.id).await?;
        after.verify(conversation, Some(&source), &self.config.image, &image_id)?;
        if after.id != inspected.id
            || !after.state.running
            || after.state.restarting
            || after.state.paused
            || after.state.dead
        {
            return Err(ExecError::Internal(
                "session did not reach a stable running state".into(),
            ));
        }
        state.known_id = Some(after.id.clone());
        Ok(after)
    }

    // Discovery labels are candidates, never authorization. Every candidate is
    // inspected and verified before any stop/remove operation.
    pub async fn owned(&self) -> Vec<String> {
        let label = format!("label={OWNER_LABEL}={OWNER_LABEL_VALUE}");
        match self.docker(&["ps", "-aq", "--no-trunc", "--filter", &label]).await {
            Ok(out) if out.status == 0 => out
                .stdout
                .lines()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
            other => {
                tracing::warn!(?other, "could not discover session containers");
                Vec::new()
            }
        }
    }

    async fn maintenance(&self, active: Option<&[String]>) -> (usize, usize) {
        let _lifecycle = LIFECYCLE.write().await;
        let image_id = match self.image_id().await {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(error = %e, "container maintenance refused");
                return (0, 0);
            }
        };
        let mut removed = 0;
        let mut stopped = 0;
        for id in self.owned().await {
            let inspected = match self.inspect(&id).await {
                Ok(row) => row,
                Err(_) => continue,
            };
            let Some(conversation) = inspected.config.labels.get(SESSION_LABEL) else {
                continue;
            };
            let lock = session_lock(&self.config.binary, conversation);
            let mut state = lock.lock().await;
            if inspected
                .verify(conversation, None, &self.config.image, &image_id)
                .is_err()
                || inspected.id != id
            {
                // Never delete a collision, a legacy container, or another image.
                continue;
            }
            state.quarantined = true;
            if self.stop_confirmed(&inspected).await.is_err() {
                continue;
            }
            state.known_id = Some(id.clone());
            if active.is_some_and(|ids| !ids.contains(conversation)) {
                state.closed = true;
                match self.docker(&["rm", &id]).await {
                    Ok(out) if out.status == 0 => {
                        removed += 1;
                        state.quarantined = false;
                    }
                    _ => {} // Unknown remove result stays quarantined.
                }
            } else {
                state.quarantined = false;
                stopped += 1;
            }
        }
        (removed, stopped)
    }

    pub async fn reconcile(&self, active: &[String]) -> (usize, usize) {
        self.maintenance(Some(active)).await
    }

    pub async fn stop_owned(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.maintenance(None).await;
    }

    pub async fn close(&self, conversation: &str) {
        let _lifecycle = LIFECYCLE.read().await;
        let lock = session_lock(&self.config.binary, conversation);
        let mut state = lock.lock().await;
        state.closed = true;
        state.quarantined = true;
        let result: Result<(), ExecError> = async {
            let Some(row) = self.find(conversation).await? else {
                return Ok(());
            };
            let image_id = self.image_id().await?;
            row.verify(conversation, None, &self.config.image, &image_id)?;
            self.stop_confirmed(&row).await?;
            let removed = self.docker(&["rm", &row.id]).await?;
            if removed.status != 0 {
                return Err(ExecError::Internal("session removal failed".into()));
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            tracing::warn!(error = %e, conversation, "session close refused or uncertain");
        } else {
            state.quarantined = false;
        }
    }
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
        conversation: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<ExecResult, ExecError> {
        self.execute_teed(argv, cwd, workspace, conversation, timeout, cancel, None)
            .await
    }
}

impl DockerConnector {
    async fn execute_teed(
        &self,
        argv: &[String],
        cwd: &Path,
        workspace: &Path,
        conversation: &str,
        timeout: Duration,
        cancel: &CancellationToken,
        tee: Option<OutputTee>,
    ) -> Result<ExecResult, ExecError> {
        if argv.is_empty() {
            return Err(ExecError::Spawn("empty command".into()));
        }
        let workdir = container_path(cwd, workspace)?;
        let _lifecycle = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ExecError::Cancelled),
            guard = LIFECYCLE.read() => guard,
        };
        let lock = session_lock(&self.config.binary, conversation);
        let mut state = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ExecError::Cancelled),
            guard = lock.lock() => guard,
        };
        if self.shutting_down.load(Ordering::SeqCst) || state.closed || state.quarantined {
            return Err(ExecError::Internal(
                "session is closed, shutting down, or quarantined; command not started".into(),
            ));
        }
        // Arm BEFORE the first control await: dropping this future at any point
        // leaves a persistent in-process quarantine, not a reusable unknown exec.
        state.quarantined = true;
        let inspected = self.ensure_locked(conversation, workspace, &mut state).await?;
        if cancel.is_cancelled() {
            self.stop_confirmed(&inspected).await?;
            state.quarantined = false;
            return Err(ExecError::Cancelled);
        }
        let mut args = vec![
            self.config.binary.clone(),
            "exec".into(),
            "-w".into(),
            workdir,
            inspected.id.clone(),
        ];
        args.extend_from_slice(argv);
        #[cfg(test)]
        let outcome = if let Some(driver) = &self.driver {
            driver.exec(&args, cancel).await
        } else {
            crate::sandbox::execute_teed(&args, cwd, None, timeout, cancel, tee).await
        };
        #[cfg(not(test))]
        let outcome = crate::sandbox::execute_teed(&args, cwd, None, timeout, cancel, tee).await;
        match outcome {
            Ok(mut result) => {
                if result.timed_out || matches!(result.exit_code, -1 | 125 | 126 | 127) {
                    self.stop_confirmed(&inspected).await?;
                }
                state.quarantined = false;
                result.ran_under = SandboxBackend::Container;
                Ok(result)
            }
            Err(error) => {
                // Even a client spawn/wait error can leave uncertain daemon
                // state. Confirm whole-container termination before release.
                self.stop_confirmed(&inspected).await?;
                state.quarantined = false;
                Err(error)
            }
        }
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
pub(super) mod tests;

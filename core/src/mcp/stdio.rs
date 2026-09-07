use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin, ChildStdout};

use super::protocol::{JsonRpcRequest, JsonRpcResponse};
use super::{McpTransport, TransportError};

/// Lines of the server's stderr kept for the failure report.
///
/// Why keep any at all, when the rule is to log lengths rather than content: an
/// MCP server that fails to start says why on stderr and nowhere else —
/// `command not found`, a missing module, a Python traceback. Without these
/// lines "the server won't connect" has no diagnosable cause at all. They go
/// through the same redaction as everything else, and only the last few are
/// kept.
const STDERR_TAIL_LINES: usize = 5;
const STDERR_LINE_CHARS: usize = 400;

pub struct StdioTransport {
    child: Child,
    writer: BufWriter<ChildStdin>,
    reader: BufReader<ChildStdout>,
    next_id: AtomicU64,
    /// Shared with the task draining stderr.
    stderr_tail: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
}

impl StdioTransport {
    pub async fn spawn(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        cwd: Option<&str>,
    ) -> Result<Self, String> {
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // Was Stdio::null(), which discarded the only explanation a failing
            // server ever gives.
            .stderr(std::process::Stdio::piped())
            // Backstop for every path that drops a transport without going
            // through shutdown: a superseded connect, an actor that stopped on
            // a dead transport, a panic. Not a replacement for the shutdown on
            // exit — `std::process::exit` runs no destructors — but it covers
            // the cases that shutdown never hears about.
            .kill_on_drop(true)
            .envs(env);

        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }

        // CREATE_NO_WINDOW: without it every stdio server flashes a console.
        // `tokio::process::Command` carries this itself on Windows, so the
        // std extension trait is not needed.
        #[cfg(target_os = "windows")]
        cmd.creation_flags(0x08000000);

        let mut child = cmd.spawn().map_err(|e| {
            // env is deliberately absent: MCP server environments routinely hold
            // tokens.
            tracing::error!(
                command = %command,
                arg_count = args.len(),
                env_key_count = env.len(),
                error = %e,
                "failed to spawn MCP server"
            );
            format!("failed to spawn MCP server: {e}")
        })?;

        let stdin = child.stdin.take().ok_or("failed to get stdin")?;
        let stdout = child.stdout.take().ok_or("failed to get stdout")?;

        let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::with_capacity(
            STDERR_TAIL_LINES,
        )));
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            let command = command.to_string();
            // Drained continuously rather than read on failure: a full stderr
            // pipe blocks the child, which would turn a noisy server into a hung
            // one.
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let line = crate::secrets::sanitizer::redact_secrets(line);
                    let line = crate::util::take_bytes_at_char_boundary(&line, STDERR_LINE_CHARS).to_string();
                    tracing::debug!(command = %command, "mcp stderr: {line}");
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
            writer: BufWriter::new(stdin),
            reader: BufReader::new(stdout),
            next_id: AtomicU64::new(1),
            stderr_tail,
        })
    }

    /// What the server last said on stderr, for a failure report.
    fn stderr_tail(&self) -> String {
        self.stderr_tail
            .lock()
            .map(|tail| tail.iter().cloned().collect::<Vec<_>>().join(" | "))
            .unwrap_or_default()
    }

    // MCP stdio transport (2024-11-05) frames messages as newline-delimited
    // JSON-RPC — not LSP-style Content-Length headers.
    //
    // No timeout of its own. A write that is abandoned part way leaves half a
    // frame in the pipe, and there is no way to find the boundary again — so
    // the only safe thing to interrupt this is something that is also going to
    // destroy the transport. That is the actor's deadline, and it does exactly
    // that.
    async fn send_raw(&mut self, body: &str) -> Result<(), String> {
        self.writer
            .write_all(body.as_bytes())
            .await
            .map_err(|e| format!("write body: {e}"))?;
        self.writer
            .write_all(b"\n")
            .await
            .map_err(|e| format!("write newline: {e}"))?;
        self.writer.flush().await.map_err(|e| format!("flush: {e}"))?;
        Ok(())
    }

    /// `Err` here is always fatal: every path out of it means the stream can no
    /// longer be read in step. A JSON-RPC error *response* is a success as far
    /// as framing goes and is reported separately.
    async fn read_response(&mut self, expected_id: u64) -> Result<Result<serde_json::Value, String>, String> {
        loop {
            let mut line = String::new();
            let n = self
                .reader
                .read_line(&mut line)
                .await
                .map_err(|e| format!("read line: {e}"))?;
            if n == 0 {
                // The child is gone. Its stderr is the only account of why, and
                // this is the last moment anyone will ask.
                let exit_code = self.child.try_wait().ok().flatten().and_then(|s| s.code());
                tracing::warn!(
                    exit_code,
                    stderr_tail = %self.stderr_tail(),
                    "MCP server closed stdout; the process is gone"
                );
                return Err("MCP server closed stdout".into());
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Only accept the response matching *this* request id; skip
            // notifications, server-initiated requests, stray output, and stale
            // responses left over from a prior (e.g. timed-out) request.
            let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                continue;
            };
            if value.get("id").and_then(|v| v.as_u64()) != Some(expected_id) {
                continue;
            }
            if value.get("result").is_none() && value.get("error").is_none() {
                continue;
            }

            let resp: JsonRpcResponse = serde_json::from_value(value).map_err(|e| format!("parse response: {e}"))?;

            // Framing held; the server simply said no.
            if let Some(err) = resp.error {
                return Ok(Err(format!("MCP error {}: {}", err.code, err.message)));
            }

            return Ok(resp.result.ok_or_else(|| "empty result".to_string()));
        }
    }
}

#[async_trait::async_trait]
impl McpTransport for StdioTransport {
    async fn request(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, TransportError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = JsonRpcRequest::new(id, method, params);
        // Serialising our own request cannot fail for any reason the server is
        // responsible for, but nothing has been written yet either — the pipe
        // is still in step.
        let body = serde_json::to_string(&req).map_err(|e| TransportError::Rpc(e.to_string()))?;

        // From here on every failure is fatal: a write that got part way, a
        // read that stopped mid-frame, a closed pipe. There is no way to find
        // the boundary again, so the transport does not get to be reused.
        //
        // No timeout of its own any more. The one that used to be here wrapped
        // `read_response`, and `read_line` is not cancel-safe: expiring it threw
        // away however much of a line had already been consumed, and every
        // later response was read against the wrong request. Bounding this is
        // the actor's job, because the actor also destroys the transport when
        // its deadline expires.
        self.send_raw(&body).await.map_err(|e| {
            tracing::warn!(method, stderr_tail = %self.stderr_tail(), error = %e, "MCP write failed");
            TransportError::Broken(e)
        })?;

        match self.read_response(id).await {
            Ok(answer) => answer.map_err(TransportError::Rpc),
            Err(e) => {
                tracing::warn!(method, stderr_tail = %self.stderr_tail(), error = %e, "MCP read failed");
                Err(TransportError::Broken(e))
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Option<serde_json::Value>) -> Result<(), String> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params.unwrap_or(serde_json::Value::Null),
        });
        self.send_raw(&body.to_string()).await
    }

    async fn shutdown(&mut self) {
        let _ = self.child.kill().await;
    }
}

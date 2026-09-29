//! What does the real adapter send when Claude Code carries on by itself?
//!
//! Ignored by default: it needs `node`, a signed-in `claude`, and it spends
//! real quota.
//!
//! ```
//! cargo test -p meridian-core --test acp_autonomous_probe -- --ignored --nocapture --test-threads=1
//! ```
//!
//! A background command that finishes after the prompt which started it has
//! already been answered makes the agent run a turn of its own: the CLI feeds
//! the completion back in as a task notification and the model answers it,
//! with nobody's `session/prompt` open. `acp::session` has to recognise that
//! stretch, give it a turn, and close the turn again — and which frames mark
//! its two ends, in what order, is a question the adapter's source answers
//! only by inference. So this records the whole timeline of one such episode,
//! frame by frame, with the client declaring exactly what the real client
//! declares.
//!
//! The raw JSON-RPC is on purpose, as in `mcp_bridge_probe`: going through
//! `acp::peer` would record what our own code makes of the traffic.
//!
//! **Measured against 0.84.0 on Windows, 2026-09-29** (the timeline is written
//! to `%TEMP%/acp_autonomous_probe.jsonl`):
//!
//! ```text
//!     7.5s  session/prompt sent
//!     8.9s  sdk session_state_changed running
//!    10.1s  tool_call Bash (the background command)
//!    11.6s  sdk requires_action → request_permission → sdk running
//!    14.6s  agent_message_chunk "started"
//!    14.9s  reply to session/prompt (end_turn)
//!    14.9s  sdk idle                               ← the prompted turn's idle, after its reply
//!    27.9s  sdk task_notification bg273fiw8 completed
//!    27.9s  sdk running
//!    28.8s  agent_message_chunk …, tool_call Bash (echo), …
//!    31.8s  usage_update with _claude/origin { kind: task-notification }
//!    31.8s  sdk idle                               ← the unprompted turn's end
//! ```
//!
//! Three things `acp::session` rests on. The completion is announced *before*
//! the cycle that answers it, so it can name that turn. `idle` follows every
//! cycle, prompted or not — and a prompted turn's arrives after its reply, when
//! `finish` already owns it. And nothing about the unprompted cycle's updates
//! themselves says they are unprompted: only the absence of an open prompt does.
//! Re-run before moving `ADAPTER_PACKAGE`.

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Long enough for `npx` to fetch the adapter on a cold machine, for the model
/// to start the command, for the command to run, and for the follow-up cycle.
const PROBE_TIMEOUT: Duration = Duration::from_secs(420);
/// How long to keep listening once the prompt has been answered.
const AFTER_REPLY: Duration = Duration::from_secs(240);
/// What the adapter should forward as `_claude/sdkMessage`.
fn sdk_filter() -> Value {
    json!([
        { "type": "system", "subtype": "session_state_changed" },
        { "type": "system", "subtype": "task_notification" },
    ])
}

struct Adapter {
    _child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    next_id: u64,
    started: Instant,
    /// One line per frame, in arrival order.
    timeline: Vec<Value>,
}

impl Adapter {
    async fn spawn() -> Adapter {
        let mut command = tokio::process::Command::new(if cfg!(windows) { "npx.cmd" } else { "npx" });
        command
            .args(["-y", "@agentclientprotocol/claude-agent-acp@0.84.0"])
            .env(meridian_core::acp::process::HOSTED_MARKER, "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("could not start the adapter — is node on PATH?");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    eprintln!("[adapter] {line}");
                }
            });
        }
        Adapter {
            _child: child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            next_id: 1,
            started: Instant::now(),
            timeline: Vec::new(),
        }
    }

    async fn send(&mut self, message: Value) {
        let line = format!("{message}\n");
        self.stdin.write_all(line.as_bytes()).await.expect("adapter stdin");
        self.stdin.flush().await.expect("adapter stdin");
    }

    fn record(&mut self, what: &str, message: &Value) {
        let at = self.started.elapsed().as_millis();
        let summary = summarise(what, message);
        println!("{at:>7}ms {summary}");
        self.timeline
            .push(json!({ "at_ms": at, "what": what, "summary": summary, "frame": message }));
    }

    /// Read one frame, answering requests inline. Returns the frame's id when
    /// it is a reply, so the caller can tell whether it was waiting for it.
    async fn pump(&mut self) -> Option<(Option<u64>, Value)> {
        let line = match self.lines.next_line().await {
            Ok(Some(line)) => line,
            _ => return None,
        };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            eprintln!("[adapter non-JSON] {line}");
            return Some((None, Value::Null));
        };
        let method = message.get("method").and_then(Value::as_str).map(str::to_string);
        let id = message.get("id").cloned();
        match (id, method) {
            (Some(id), None) => {
                self.record("reply", &message);
                return Some((id.as_u64(), message));
            }
            (Some(id), Some(method)) => {
                self.record("request", &message);
                let reply = answer(&method, id, message.get("params").unwrap_or(&Value::Null));
                self.send(reply).await;
            }
            (None, Some(_)) => self.record("notification", &message),
            _ => {}
        }
        Some((None, message))
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.record("sent", &request);
        self.send(request).await;
        loop {
            match self.pump().await {
                Some((Some(got), message)) if got == id => return message,
                Some(_) => {}
                None => panic!("the adapter closed its output while waiting for {method}"),
            }
        }
    }
}

/// Allow every permission once, and decline anything else.
fn answer(method: &str, id: Value, params: &Value) -> Value {
    if method == "session/request_permission" {
        let option = params
            .get("options")
            .and_then(Value::as_array)
            .and_then(|options| {
                options
                    .iter()
                    .find(|o| o.get("kind").and_then(Value::as_str) == Some("allow_once"))
            })
            .and_then(|o| o.get("optionId"))
            .cloned()
            .unwrap_or_else(|| json!("allow"));
        return json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "outcome": { "outcome": "selected", "optionId": option } },
        });
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32601, "message": format!("this probe does not implement {method}") },
    })
}

/// One readable line per frame.
fn summarise(what: &str, message: &Value) -> String {
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "session/update" => {
            let update = &params["update"];
            let kind = update["sessionUpdate"].as_str().unwrap_or("?");
            let detail = match kind {
                "agent_message_chunk" | "agent_thought_chunk" => {
                    format!("{:?}", update["content"]["text"].as_str().unwrap_or(""))
                }
                "tool_call" | "tool_call_update" => format!(
                    "{} {} {}",
                    update["toolCallId"].as_str().unwrap_or(""),
                    update["_meta"]["claudeCode"]["toolName"].as_str().unwrap_or(""),
                    update["status"].as_str().unwrap_or("")
                ),
                _ => clip(update.to_string(), 200),
            };
            format!("{what} update {kind} {detail}")
        }
        "_claude/sdkMessage" => {
            let m = &params["message"];
            format!(
                "{what} sdk {}/{} state={} task={} status={}",
                m["type"].as_str().unwrap_or(""),
                m["subtype"].as_str().unwrap_or(""),
                m["state"].as_str().unwrap_or("-"),
                m["task_id"].as_str().unwrap_or("-"),
                m["status"].as_str().unwrap_or("-"),
            )
        }
        "" => {
            let s = clip(
                message
                    .get("result")
                    .or(message.get("error"))
                    .cloned()
                    .unwrap_or(Value::Null)
                    .to_string(),
                200,
            );
            format!("{what} {s}")
        }
        other => {
            let s = clip(params.to_string(), 160);
            format!("{what} {other} {s}")
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs node, a signed-in claude, and spends quota"]
async fn a_background_command_finishing_after_its_turn() {
    let outcome = tokio::time::timeout(PROBE_TIMEOUT, probe()).await;
    assert!(outcome.is_ok(), "the probe timed out after {PROBE_TIMEOUT:?}");
}

async fn probe() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let cwd = workspace.path().to_string_lossy().to_string();
    let mut adapter = Adapter::spawn().await;

    adapter
        .call(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities":
                    serde_json::to_value(meridian_core::acp::protocol::ClientCapabilities::default()).unwrap(),
                "clientInfo": { "name": "meridian-autonomous-probe", "version": "0" },
            }),
        )
        .await;

    let session = adapter
        .call(
            "session/new",
            json!({
                "cwd": cwd,
                "mcpServers": [],
                "_meta": { "claudeCode": { "emitRawSDKMessages": sdk_filter() } },
            }),
        )
        .await;
    let session_id = session["result"]["sessionId"]
        .as_str()
        .expect("session/new returned no id")
        .to_string();

    let command = if cfg!(windows) {
        "ping -n 15 127.0.0.1"
    } else {
        "sleep 15"
    };
    let text = format!(
        "Run exactly this shell command in the background, using the Bash tool with run_in_background set to true: \
         `{command}`. Do not wait for it and do not check on it. Reply with the single word `started` and end your \
         turn. Later, when you are notified that it finished, reply with one short sentence and then run the shell \
         command `echo followup` (not in the background)."
    );
    let reply = adapter
        .call(
            "session/prompt",
            json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": text }] }),
        )
        .await;
    println!("=== the prompt was answered: {} ===", reply["result"]);

    let deadline = Instant::now() + AFTER_REPLY;
    let mut idles_after_reply = 0;
    while Instant::now() < deadline {
        let next = tokio::time::timeout(deadline - Instant::now(), adapter.pump()).await;
        let Ok(Some((_, message))) = next else { break };
        if message["method"] == "_claude/sdkMessage" && message["params"]["message"]["state"] == "idle" {
            idles_after_reply += 1;
            // The autonomous cycle's own idle is the one after its output.
            if idles_after_reply >= 1
                && adapter
                    .timeline
                    .iter()
                    .rev()
                    .take(200)
                    .any(|f| f["summary"].as_str().unwrap_or("").contains("followup"))
            {
                break;
            }
        }
    }

    let out = std::env::temp_dir().join("acp_autonomous_probe.jsonl");
    let body = adapter
        .timeline
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&out, body).expect("write the timeline");
    println!("=== timeline written to {} ===", out.display());
}

/// At most `n` characters, cut on a character boundary.
fn clip(s: String, n: usize) -> String {
    s.chars().take(n).collect()
}

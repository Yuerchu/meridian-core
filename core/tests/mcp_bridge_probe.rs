//! What does the real adapter do with an MCP server we hand it?
//!
//! Ignored by default: it needs `node`, a signed-in `claude`, and it spends
//! real quota.
//!
//! ```
//! cargo test -p meridian-core --test mcp_bridge_probe -- --ignored --nocapture --test-threads=1
//! ```
//!
//! This exists because the bridge design rested on two claims that were
//! *inferred* rather than measured, and everything downstream hung off them.
//! Both have now been run. Measured against `@agentclientprotocol/claude-agent-acp`
//! **0.70.0**, protocol version 1, MCP `2025-11-25`, on Windows.
//!
//! **1. "Answering with plain JSON is enough" — yes, and a stateless server is
//! enough too.** The client sends `Accept: application/json, text/event-stream`
//! and is content with the first: `initialize`, `tools/list` and `tools/call`
//! all completed against a server that never speaks SSE. It opens a `GET` for
//! the notification stream once, takes `405` for an answer, and carries on. It
//! neither sends nor requires `Mcp-Session-Id`, and a notification answered
//! `202` with no body is accepted. So the bridge can be exactly as small as it
//! hoped: POST only, one route, pure JSON.
//!
//! Two details that would break a server written from the guess alone. Every
//! request *except the opening `initialize`* carries `MCP-Protocol-Version:
//! 2025-11-25` — requiring that header unconditionally rejects the handshake.
//! And `tools/call` arrives with `_meta` carrying `claudecode/toolUseId` and a
//! `progressToken`; ignoring them is fine, refusing unknown members is not.
//!
//! **2. "Claude Code asks permission before calling an MCP tool" — yes, and it
//! still must not be leaned on.** A `session/request_permission` arrives before
//! the call, offering `reject_once` / `allow_once` / `allow_always` in that
//! order, with the tool namespaced as `mcp__<server>__<tool>` (the bare name is
//! what reaches `tools/call`). So the ask is real and would land on
//! `acp::approvals::ask()` for free.
//!
//! It is still not a boundary this app may build on, and the same run says why
//! rather than leaving it a worry: `session/load` reports a `mode` config option
//! whose values include `bypassPermissions` ("Bypass all permission checks"),
//! `dontAsk` and `auto` ("use a model classifier to approve/deny"). Any of the
//! three is the user's to set, none of them is visible from here, and the first
//! removes the ask entirely. The plan's answer — read-only tools first, scoped
//! by a wrapper, writes waiting for the bridge's own approval — is unchanged,
//! but it is now a decision about a knob that was seen rather than about one
//! that was suspected.
//!
//! **3. The descriptor can be replaced, but only by reopening the session.**
//! `session/load` with a *different* `mcpServers` list moves the session wholly
//! onto the new endpoint: the second server is initialized, listed and called,
//! and the first receives `notifications/cancelled`. So a per-turn URL — the
//! thing that would let the bridge tell a delayed call from an earlier turn
//! apart from a current one — is possible in principle and costs a full load per
//! turn, which recites the whole history (measured elsewhere at ~37s on a large
//! session). That price is not worth paying for the guarantee it buys, so
//! capability identity stays per-session and the limitation is stated rather
//! than papered over.
//!
//! **Both halves are deliberately raw.** The HTTP server is a socket and a
//! parser rather than hyper, and the JSON-RPC is lines on a pipe rather than
//! [`meridian_core::acp::peer`]. Going through either would answer a different
//! question — what our own code makes of the traffic — when what is wanted is
//! the bytes the adapter chose to send. `hooks::http`'s socket test made the
//! same call for the same reason.
//!
//! The server here is the *most minimal thing the MCP spec permits*, on
//! purpose: no SSE, no `Mcp-Session-Id`, no GET stream, no DELETE. That is what
//! makes a pass here mean something — the bridge may be exactly this small, and
//! anything the adapter had insisted on would have shown up as a failure naming
//! it. Re-run it against a new adapter version before trusting the findings
//! above; the package is deliberately unpinned, so they can go stale with no
//! commit in this repository.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// Long enough for `npx` to fetch the adapter on a cold machine and for a model
/// to answer, short enough that a wedged probe does not sit there for ever.
const PROBE_TIMEOUT: Duration = Duration::from_secs(300);

/// The one tool the fake server offers. The name is deliberately unlike
/// anything Claude Code has of its own, so "did it call ours" has an
/// unambiguous answer.
const TOOL_NAME: &str = "meridian_probe_echo";

/// Asked for in the prompt and returned by the tool, so the transcript proves
/// the round trip completed rather than merely started.
const SECRET: &str = "kingfisher";

// =========================================================================
// The fake MCP server
// =========================================================================

/// One HTTP request, as it came off the socket.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The JSON-RPC method, for a request that carries one.
    fn rpc(&self) -> Option<String> {
        serde_json::from_str::<Value>(&self.body)
            .ok()?
            .get("method")?
            .as_str()
            .map(str::to_string)
    }
}

#[derive(Default)]
struct Log {
    seen: Vec<Seen>,
    /// Whatever the adapter used as a JSON-RPC id, described rather than
    /// stored: the bridge has to echo all of these back unchanged, and
    /// `mcp/protocol.rs` types the field as `u64` today.
    id_shapes: BTreeSet<String>,
    /// The arguments a `tools/call` arrived with, so the probe can say whether
    /// the model's arguments survive the trip.
    calls: Vec<Value>,
}

struct Server {
    log: Arc<Mutex<Log>>,
    token: String,
    /// The capability half of the URL — a path nobody can guess.
    path: String,
    port: u16,
    /// Named so the second probe can tell its two servers apart in the report.
    label: &'static str,
}

impl Server {
    fn url(&self) -> String {
        format!("http://127.0.0.1:{}{}", self.port, self.path)
    }

    fn entries(&self) -> Vec<Seen> {
        self.log.lock().unwrap().seen.clone()
    }

    fn saw_rpc(&self, method: &str) -> bool {
        self.entries().iter().any(|s| s.rpc().as_deref() == Some(method))
    }
}

/// Bind, and answer on a task for as long as the test holds the handle.
async fn start_server(label: &'static str) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("could not bind");
    let port = listener.local_addr().unwrap().port();
    let log = Arc::new(Mutex::new(Log::default()));
    let token = uuid::Uuid::new_v4().simple().to_string();
    let path = format!("/mcp/{}", uuid::Uuid::new_v4().simple());

    let state = (log.clone(), token.clone(), path.clone());
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let state = state.clone();
            tokio::spawn(async move {
                serve_connection(stream, state.0, state.1, state.2).await;
            });
        }
    });

    Server {
        log,
        token,
        path,
        port,
        label,
    }
}

/// Read requests off one connection until the peer stops asking.
///
/// Keep-alive is honoured rather than refused: whether the adapter's client
/// reuses a connection is itself one of the things worth seeing, and answering
/// `Connection: close` every time would hide it.
async fn serve_connection(mut stream: TcpStream, log: Arc<Mutex<Log>>, token: String, path: String) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let Some(request) = read_request(&mut stream, &mut buf).await else {
            return;
        };
        {
            let mut log = log.lock().unwrap();
            if let Ok(parsed) = serde_json::from_str::<Value>(&request.body) {
                log.id_shapes.insert(describe_id(parsed.get("id")));
                if parsed.get("method").and_then(Value::as_str) == Some("tools/call") {
                    log.calls.push(parsed.get("params").cloned().unwrap_or(Value::Null));
                }
            }
            log.seen.push(request.clone());
        }
        let response = answer(&request, &token, &path);
        if stream.write_all(&response).await.is_err() {
            return;
        }
    }
}

/// How the bridge would have to be able to echo this id back.
fn describe_id(id: Option<&Value>) -> String {
    match id {
        None => "absent (notification)".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Number(n)) => format!("number ({n})"),
        Some(Value::String(_)) => "string".into(),
        Some(other) => format!("other ({other})"),
    }
}

/// Enough HTTP/1.1 to read one request. Returns `None` when the peer hangs up.
async fn read_request(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Option<Seen> {
    let head_end = loop {
        if let Some(pos) = find(buf, b"\r\n\r\n") {
            break pos;
        }
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let target = request_line.next()?.to_string();

    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();

    let length: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);

    let body_start = head_end + 4;
    while buf.len() < body_start + length {
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..read]);
    }

    let body = String::from_utf8_lossy(&buf[body_start..body_start + length]).to_string();
    buf.drain(..body_start + length);

    // The query string is dropped from the recorded path but kept out of the
    // comparison below on purpose: a client appending one must not 404.
    let path = target.split('?').next().unwrap_or(&target).to_string();
    Some(Seen {
        method,
        path,
        headers,
        body,
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn http(status: &str, body: Option<&str>) -> Vec<u8> {
    match body {
        Some(body) => format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes(),
        None => format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").into_bytes(),
    }
}

fn rpc_result(id: Option<&Value>, result: Value) -> Vec<u8> {
    let body = json!({ "jsonrpc": "2.0", "id": id.cloned().unwrap_or(Value::Null), "result": result });
    http("200 OK", Some(&body.to_string()))
}

/// The whole server, and it is meant to look this small.
fn answer(request: &Seen, token: &str, path: &str) -> Vec<u8> {
    // A GET is the SSE stream and a DELETE ends a session. Refusing both is how
    // the probe finds out whether a stateless server is acceptable — if the
    // adapter needs either, it will say so by failing after this.
    if request.method != "POST" {
        return http("405 Method Not Allowed", Some(r#"{"error":"probe is POST-only"}"#));
    }
    if request.path != path {
        return http("404 Not Found", Some(r#"{"error":"no such endpoint"}"#));
    }
    let presented = request
        .header("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if presented != token {
        return http("401 Unauthorized", Some(r#"{"error":"bad or missing token"}"#));
    }

    let Ok(message) = serde_json::from_str::<Value>(&request.body) else {
        return http("400 Bad Request", Some(r#"{"error":"not JSON"}"#));
    };
    let id = message.get("id");
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");

    match method {
        "initialize" => {
            // Echoed rather than asserted: a server that answers with a version
            // the client did not ask for is the client's problem to resolve, and
            // what this probe wants to know is which one gets proposed.
            let version = message
                .get("params")
                .and_then(|p| p.get("protocolVersion"))
                .cloned()
                .unwrap_or_else(|| json!("2025-06-18"));
            rpc_result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "meridian-bridge-probe", "version": "0" },
                }),
            )
        }
        // 202 with no body is what the spec prescribes for a notification. If
        // the adapter dislikes it, the session dies here and the report says so.
        _ if id.is_none() => http("202 Accepted", None),
        "tools/list" => rpc_result(
            id,
            json!({
                "tools": [{
                    "name": TOOL_NAME,
                    "description": format!(
                        "Returns a secret word. Call this with word=\"{SECRET}\" when asked to."
                    ),
                    "inputSchema": {
                        "type": "object",
                        "properties": { "word": { "type": "string", "description": "The word to echo." } },
                        "required": ["word"],
                    },
                }],
            }),
        ),
        "tools/call" => {
            let word = message
                .get("params")
                .and_then(|p| p.get("arguments"))
                .and_then(|a| a.get("word"))
                .and_then(Value::as_str)
                .unwrap_or("<none>");
            rpc_result(
                id,
                json!({
                    "content": [{ "type": "text", "text": format!("the probe received: {word}") }],
                    "isError": false,
                }),
            )
        }
        other => {
            let body = json!({
                "jsonrpc": "2.0",
                "id": id.cloned().unwrap_or(Value::Null),
                "error": { "code": -32601, "message": format!("no such method: {other}") },
            });
            http("200 OK", Some(&body.to_string()))
        }
    }
}

// =========================================================================
// The adapter, over a raw pipe
// =========================================================================

/// What the adapter did that this probe is here to record.
#[derive(Default)]
struct AcpLog {
    /// The finding the design hangs on: was permission asked for *our* tool?
    permission_requests: Vec<Value>,
    /// Every inbound method, so a capability we declared unsupported showing up
    /// anyway does not go unnoticed.
    inbound: BTreeSet<String>,
    /// Tool-call updates naming our tool, which is the transcript's own account
    /// of the call the fake server saw.
    tool_updates: Vec<Value>,
    text: String,
}

struct Adapter {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    next_id: u64,
    log: AcpLog,
}

impl Adapter {
    async fn spawn() -> Adapter {
        let mut command = tokio::process::Command::new(if cfg!(windows) { "npx.cmd" } else { "npx" });
        command
            .args(["-y", "@agentclientprotocol/claude-agent-acp"])
            // The same marker `AdapterProcess::spawn` sets. Without it the
            // user's own plan-gate plugin reviews this probe's turn — minutes of
            // a second model, and a conversation in the sidebar for a test.
            .env(meridian_core::acp::process::HOSTED_MARKER, "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn().expect("could not start the adapter — is node on PATH?");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        // Drained rather than ignored: a full stderr pipe blocks the child, and
        // when the handshake fails this is the only place that says why.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    eprintln!("[adapter] {line}");
                }
            });
        }

        Adapter {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            next_id: 1,
            log: AcpLog::default(),
        }
    }

    async fn send(&mut self, message: Value) {
        let line = format!("{message}\n");
        self.stdin.write_all(line.as_bytes()).await.expect("adapter stdin");
        self.stdin.flush().await.expect("adapter stdin");
    }

    /// Send a request and pump the pipe until its reply lands.
    ///
    /// Everything arriving in the meantime is answered inline. That is fine
    /// *here* and would not be in the real client — `peer.rs` spawns inbound
    /// requests precisely because a permission is answered by a person minutes
    /// later, and this probe answers instantly.
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;

        loop {
            let line = match self.lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => return Err(format!("the adapter closed its output while waiting for {method}")),
                Err(e) => return Err(format!("reading the adapter failed: {e}")),
            };
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                eprintln!("[adapter non-JSON] {line}");
                continue;
            };

            let inbound_method = message.get("method").and_then(Value::as_str);
            match (message.get("id").and_then(Value::as_u64), inbound_method) {
                // Our reply.
                (Some(got), None) if got == id => {
                    if let Some(error) = message.get("error") {
                        return Err(error.to_string());
                    }
                    return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                }
                // A request from the agent.
                (Some(_), Some(name)) => {
                    let reply = self.handle_inbound(name, &message);
                    self.send(reply).await;
                }
                // A notification.
                (None, Some(name)) => self.absorb(name, &message),
                _ => {}
            }
        }
    }

    fn handle_inbound(&mut self, method: &str, message: &Value) -> Value {
        self.log.inbound.insert(method.to_string());
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let params = message.get("params").cloned().unwrap_or(Value::Null);

        if method == "session/request_permission" {
            self.log.permission_requests.push(params.clone());
            // Chosen by `kind`, never by position. Measured: the agent lists
            // `reject_once` *first*, so taking `options[0]` denies the very call
            // the probe exists to make — which then reads as "the model declined
            // to use the tool" and proves nothing at all.
            let options = params
                .get("options")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let option = options
                .iter()
                .find(|o| o.get("kind").and_then(Value::as_str) == Some("allow_once"))
                .or_else(|| {
                    options
                        .iter()
                        .find(|o| o.get("kind").and_then(Value::as_str) == Some("allow_always"))
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

    fn absorb(&mut self, method: &str, message: &Value) {
        self.log.inbound.insert(method.to_string());
        if method != "session/update" {
            return;
        }
        let Some(update) = message.get("params").and_then(|p| p.get("update")) else {
            return;
        };
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("agent_message_chunk") => {
                if let Some(text) = update
                    .get("content")
                    .and_then(|c| c.get("text"))
                    .and_then(Value::as_str)
                {
                    self.log.text.push_str(text);
                }
            }
            Some("tool_call") | Some("tool_call_update") if update.to_string().contains(TOOL_NAME) => {
                self.log.tool_updates.push(update.clone());
            }
            _ => {}
        }
    }

    /// Declared exactly as the real client declares them, so the probe cannot
    /// accidentally learn something that depends on a capability we do not ship.
    async fn initialize(&mut self) -> Value {
        self.call(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false },
                    "terminal": false,
                    "elicitation": { "form": {} },
                },
                "clientInfo": { "name": "meridian-bridge-probe", "version": "0" },
            }),
        )
        .await
        .expect("the adapter refused to initialize")
    }
}

/// The descriptor shape being probed: ACP's HTTP MCP server.
///
/// Written out here rather than built from a type, because what the right type
/// *is* is the question. The alternative encodings the probe falls back to are
/// below it.
fn http_descriptor(server: &Server) -> Value {
    json!({
        "type": "http",
        "name": "meridian",
        "url": server.url(),
        "headers": [{ "name": "Authorization", "value": format!("Bearer {}", server.token) }],
    })
}

fn prompt(text: &str) -> Value {
    json!([{ "type": "text", "text": text }])
}

// =========================================================================
// Probe 1: does a minimal server work, and is permission asked?
// =========================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs node, a signed-in claude, and spends quota"]
async fn a_minimal_http_mcp_server_is_reachable_from_a_hosted_session() {
    let outcome = tokio::time::timeout(PROBE_TIMEOUT, probe_one()).await;
    assert!(outcome.is_ok(), "the probe timed out after {PROBE_TIMEOUT:?}");
}

async fn probe_one() {
    let server = start_server("only").await;
    let workspace = tempfile::tempdir().expect("tempdir");
    let cwd = workspace.path().to_string_lossy().to_string();

    let mut adapter = Adapter::spawn().await;
    let init = adapter.initialize().await;
    println!("\n=== initialize ===\n{}", pretty(&init));

    let session = adapter
        .call(
            "session/new",
            json!({ "cwd": cwd, "mcpServers": [http_descriptor(&server)] }),
        )
        .await;

    let session = match session {
        Ok(value) => value,
        Err(e) => {
            // The descriptor being refused is itself a finding, and the most
            // useful one this probe can produce — it names the schema.
            println!("\n!!! session/new refused the HTTP descriptor:\n{e}");
            println!("descriptor sent:\n{}", pretty(&http_descriptor(&server)));
            report(&adapter, &[&server]);
            panic!("the adapter would not take the MCP descriptor — see above for its own words");
        }
    };
    let session_id = session
        .get("sessionId")
        .and_then(Value::as_str)
        .expect("no sessionId in the reply")
        .to_string();
    println!("session: {session_id}");

    let answer = adapter
        .call(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": prompt(&format!(
                    "Call the `{TOOL_NAME}` tool with word=\"{SECRET}\", then tell me exactly \
                     what it returned. Do not use any other tool and do not read any files."
                )),
            }),
        )
        .await
        .expect("the prompt failed");
    println!("stop reason: {}", pretty(&answer));

    report(&adapter, &[&server]);

    // Only two assertions, and both are about the probe having produced
    // evidence rather than about which way the evidence went. A model that
    // declines to call the tool leaves every finding below unproven, and that
    // is a failed measurement, not a failed hypothesis.
    assert!(
        server.saw_rpc("tools/list"),
        "the adapter never listed our tools — the descriptor was accepted but never used"
    );
    assert!(
        server.saw_rpc("tools/call"),
        "the model never called the tool, so this run proves nothing about the transport"
    );

    let _ = adapter.child.kill().await;
}

// =========================================================================
// Probe 2: can the tool set change without starting over?
// =========================================================================

/// The turn-scoped capability idea needs a way to hand the agent a *different*
/// descriptor part-way through a conversation. ACP sends `mcpServers` once, at
/// `session/new` and at `session/load` — so the question is whether a load
/// swaps them, and what that costs.
///
/// Whichever way it comes out, it decides how strong a promise the bridge may
/// make. A "no" is what the design already assumes; a "yes" is what would let
/// the per-turn URL become real.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs node, a signed-in claude, and spends quota"]
async fn whether_a_reload_can_replace_the_tool_set() {
    let outcome = tokio::time::timeout(PROBE_TIMEOUT, probe_two()).await;
    assert!(outcome.is_ok(), "the probe timed out after {PROBE_TIMEOUT:?}");
}

async fn probe_two() {
    let first = start_server("first").await;
    let second = start_server("second").await;
    let workspace = tempfile::tempdir().expect("tempdir");
    let cwd = workspace.path().to_string_lossy().to_string();

    let mut adapter = Adapter::spawn().await;
    let init = adapter.initialize().await;
    let lists_sessions = init
        .get("agentCapabilities")
        .and_then(|c| c.get("loadSession"))
        .cloned()
        .unwrap_or(Value::Null);
    println!("loadSession capability: {lists_sessions}");

    let session = adapter
        .call(
            "session/new",
            json!({ "cwd": cwd, "mcpServers": [http_descriptor(&first)] }),
        )
        .await
        .expect("session/new failed");
    let session_id = session.get("sessionId").and_then(Value::as_str).unwrap().to_string();

    // One turn against the first server, so the session is real and has a
    // history for the load to recite.
    let _ = adapter
        .call(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": prompt(&format!("Call `{TOOL_NAME}` with word=\"{SECRET}\" and say what it returned.")),
            }),
        )
        .await;

    println!("first server saw tools/list: {}", first.saw_rpc("tools/list"));

    // Now reopen the same session pointed at a different endpoint.
    let loaded = adapter
        .call(
            "session/load",
            json!({ "sessionId": session_id, "cwd": cwd, "mcpServers": [http_descriptor(&second)] }),
        )
        .await;
    match &loaded {
        Ok(value) => println!("session/load: {}", pretty(value)),
        Err(e) => println!("session/load refused: {e}"),
    }

    if loaded.is_ok() {
        let _ = adapter
            .call(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": prompt(&format!("Call `{TOOL_NAME}` once more with word=\"{SECRET}\".")),
                }),
            )
            .await;
    }

    report(&adapter, &[&first, &second]);

    println!(
        "\n--- the finding ---\n\
         a reload {} the tool set: the second endpoint was {} after the load.\n\
         If it did, a per-turn descriptor is possible but costs a full session/load \
         (measured elsewhere at ~37s on a large session, and it recites the history).\n\
         If it did not, capability identity stays per-session and the bridge cannot \
         distinguish a delayed call from an earlier turn.",
        if second.saw_rpc("tools/list") {
            "CAN replace"
        } else {
            "CANNOT replace"
        },
        if second.saw_rpc("tools/list") {
            "contacted"
        } else {
            "never contacted"
        },
    );

    let _ = adapter.child.kill().await;
}

// =========================================================================
// Reporting
// =========================================================================

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// Everything the two halves saw, laid out as the questions the design asked.
fn report(adapter: &Adapter, servers: &[&Server]) {
    println!("\n================ PROBE FINDINGS ================");

    for server in servers {
        let entries = server.entries();
        println!("\n--- server `{}` : {} requests ---", server.label, entries.len());
        for seen in &entries {
            let rpc = seen.rpc().unwrap_or_else(|| "-".into());
            println!(
                "  {} {} [{rpc}]  accept={:?} content-type={:?} mcp-session-id={:?} protocol={:?}",
                seen.method,
                seen.path,
                seen.header("accept"),
                seen.header("content-type"),
                seen.header("mcp-session-id"),
                seen.header("mcp-protocol-version"),
            );
        }

        let log = server.log.lock().unwrap();
        println!("  JSON-RPC id shapes used: {:?}", log.id_shapes);
        println!(
            "  non-POST attempts: {}",
            entries.iter().filter(|s| s.method != "POST").count()
        );
        println!(
            "  sent us an Mcp-Session-Id: {}",
            entries.iter().any(|s| s.header("mcp-session-id").is_some())
        );
        for call in &log.calls {
            println!("  tools/call params: {}", call);
        }
    }

    println!("\n--- the adapter ---");
    println!("  inbound methods: {:?}", adapter.log.inbound);
    println!(
        "  session/request_permission for a bridge tool: {}",
        if adapter.log.permission_requests.is_empty() {
            "NO — the bridge cannot rely on Claude Code asking".to_string()
        } else {
            format!("YES ({})", adapter.log.permission_requests.len())
        }
    );
    for request in &adapter.log.permission_requests {
        let kinds: Vec<&str> = request
            .get("options")
            .and_then(Value::as_array)
            .map(|o| o.iter().filter_map(|o| o.get("kind").and_then(Value::as_str)).collect())
            .unwrap_or_default();
        println!("  permission option kinds, in the order offered: {kinds:?}");
        println!("  permission params: {}", pretty(request));
    }
    println!(
        "  tool_call updates naming our tool: {}",
        adapter.log.tool_updates.len()
    );
    let text = adapter.log.text.trim();
    println!("  the answer mentions the secret: {}", text.contains(SECRET));
    println!("================================================\n");
}

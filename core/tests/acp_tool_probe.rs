//! What does the real adapter send for each tool, to a client that declares
//! what Meridian declares?
//!
//! Ignored by default: it needs `node`, a signed-in `claude`, and it spends
//! real quota.
//!
//! ```
//! cargo test -p meridian-core --test acp_tool_probe -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Written because several facts about hosted tool calls were *read out of the
//! adapter's source* rather than seen: that declaring the AIR envelope makes it
//! drop the text of a successful `Read`, and of a `Grep` or `Glob` given a
//! `path`; that a `Bash` result arrives fenced as ```` ```console ```` with no
//! exit code; that a refused call comes back as `failed`, distinguishable only
//! by `_meta.claudeCode`; that `TodoWrite` is reported as a `plan` update and
//! never as a tool call. The front end's tool cards are being rebuilt on those
//! facts, so they are measured first.
//!
//! **It speaks exactly what the real client speaks.** `initialize` is built
//! from [`meridian_core::acp::protocol::InitializeParams`] with
//! `ClientCapabilities::default()` — the AIR `_meta` included, which is the
//! whole question — and the adapter is the pinned
//! [`meridian_core::acp::ADAPTER_PACKAGE`]. The JSON-RPC is raw lines rather than
//! `acp::peer`, for the reason `mcp_bridge_probe` gives: what is wanted is the
//! bytes the adapter chose to send, not what our code makes of them.
//!
//! **What it writes.** Every line in both directions goes to
//! `tests/fixtures/acp/tool-probe.jsonl`, scrubbed of the workspace path, the
//! home directory, e-mail addresses and the `_auth/` traffic — this repository
//! is public. The mapping's replay tests read that file; re-run this when the
//! pinned adapter moves, and review the diff of the fixture as the change log.
//!
//! Permissions are granted with `allow_once`, except for a call whose input
//! names `deny-me`, which is refused — so the fixture holds one of each.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PROBE_TIMEOUT: Duration = Duration::from_secs(900);

/// The steps, in the words the model is given. Each exercises one fact above.
const SCRIPT: &str = "\
This is a test of tool reporting. Do exactly these steps, in order, one tool call each, \
and do not explain or summarise between them:
1. Read the file notes.txt.
2. Read notes.txt again with offset 2 and limit 2.
3. Grep for the word needle across the project, with no path.
4. Grep for the word needle with path src.
5. Glob for **/*.rs with no path.
6. Glob for *.rs with path src.
7. Run the shell command: echo hello
8. Run the shell command: exit 3
9. Run a shell command that prints nothing: true
10. Edit notes.txt, replacing the word alpha with beta.
11. Write a new file new.txt containing the single line x.
12. Write a file deny-me.txt containing the single line no. (Permission will be refused; that is expected — carry on.)
13. Use TodoWrite to record two todos: first step (completed) and second step (in progress).
14. Use WebSearch for: Agent Client Protocol.
Then reply with the single word done.";

struct Adapter {
    _child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    next_id: u64,
    /// Every line, in order, tagged with its direction.
    transcript: Vec<Value>,
}

impl Adapter {
    async fn spawn() -> Adapter {
        let mut command = tokio::process::Command::new(if cfg!(windows) { "npx.cmd" } else { "npx" });
        command
            .args(["-y", meridian_core::acp::ADAPTER_PACKAGE])
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
            transcript: Vec::new(),
        }
    }

    async fn send(&mut self, message: Value) {
        self.transcript.push(json!({ "dir": "out", "msg": message }));
        let line = format!("{message}\n");
        self.stdin.write_all(line.as_bytes()).await.expect("adapter stdin");
        self.stdin.flush().await.expect("adapter stdin");
    }

    /// Send a request and pump the pipe until its reply lands, answering the
    /// agent's own requests inline.
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
            self.transcript.push(json!({ "dir": "in", "msg": message.clone() }));
            let inbound = message.get("method").and_then(Value::as_str).map(str::to_string);
            match (message.get("id").and_then(Value::as_u64), inbound) {
                (Some(got), None) if got == id => {
                    return match message.get("error") {
                        Some(error) => Err(error.to_string()),
                        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                    };
                }
                (Some(_), Some(name)) => {
                    let reply = answer(&name, &message);
                    self.send(reply).await;
                }
                _ => {}
            }
        }
    }
}

/// `allow_once` for everything but a call naming `deny-me`, which gets
/// `reject_once`. Chosen by `kind`, never by position: the agent lists the
/// rejection first.
fn answer(method: &str, message: &Value) -> Value {
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    if method != "session/request_permission" {
        return json!({ "jsonrpc": "2.0", "id": id,
            "error": { "code": -32601, "message": format!("this probe does not implement {method}") } });
    }
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let refuse = params.to_string().contains("deny-me");
    let wanted = if refuse { "reject_once" } else { "allow_once" };
    let option = params
        .get("options")
        .and_then(Value::as_array)
        .and_then(|o| o.iter().find(|o| o.get("kind").and_then(Value::as_str) == Some(wanted)))
        .and_then(|o| o.get("optionId"))
        .cloned()
        .unwrap_or_else(|| json!(wanted));
    json!({ "jsonrpc": "2.0", "id": id,
        "result": { "outcome": { "outcome": "selected", "optionId": option } } })
}

/// The workspace the model works in: a file to read and edit, and a needle in
/// two places so the two Greps differ.
fn seed() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("notes.txt"),
        "one alpha\ntwo\nthree\nfour needle\nfive\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src").join("lib.rs"), "// needle\npub fn f() {}\n").unwrap();
    std::fs::write(dir.path().join("src").join("main.rs"), "fn main() {}\n").unwrap();
    dir
}

/// Paths, the home directory and anything that looks like an e-mail address
/// out; the `_auth/` notifications dropped. The fixture is committed to a
/// public repository.
fn scrub(transcript: &[Value], workspace: &std::path::Path) -> String {
    let mut needles: Vec<(String, &str)> = Vec::new();
    for path in [workspace.to_path_buf(), dunce_like(workspace)] {
        let plain = path.to_string_lossy().into_owned();
        needles.push((
            serde_json::to_string(&plain).unwrap().trim_matches('"').to_string(),
            "<WORKSPACE>",
        ));
        needles.push((plain.replace('\\', "/"), "<WORKSPACE>"));
    }
    if let Some(home) = dirs::home_dir() {
        let plain = home.to_string_lossy().into_owned();
        needles.push((
            serde_json::to_string(&plain).unwrap().trim_matches('"').to_string(),
            "<HOME>",
        ));
        needles.push((plain.replace('\\', "/"), "<HOME>"));
    }
    needles.sort_by_key(|(n, _)| std::cmp::Reverse(n.len()));
    let email = regex::Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}").unwrap();
    let mut out = String::new();
    for entry in transcript {
        let method = entry["msg"].get("method").and_then(Value::as_str).unwrap_or("");
        if method.starts_with("_auth/") {
            continue;
        }
        // The adapter announces every skill and slash command the signed-in
        // user has installed, descriptions included — private, and nothing to
        // do with tools. The list is emptied; the update itself stays.
        let mut entry = entry.clone();
        if let Some(update) = entry
            .pointer_mut("/msg/params/update")
            .filter(|u| u["sessionUpdate"] == "available_commands_update")
        {
            update["availableCommands"] = json!([]);
        }
        let mut line = entry.to_string();
        for (needle, replacement) in &needles {
            if !needle.is_empty() {
                line = line.replace(needle.as_str(), replacement);
            }
        }
        out.push_str(&email.replace_all(&line, "<EMAIL>"));
        out.push('\n');
    }
    out
}

/// The canonical spelling, which is what the adapter echoes back.
fn dunce_like(path: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(path)
        .map(|p| std::path::PathBuf::from(p.to_string_lossy().trim_start_matches(r"\\?\")))
        .unwrap_or_else(|_| path.to_path_buf())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs node, a signed-in claude, and spends quota"]
async fn record_what_the_adapter_sends_for_each_tool() {
    let outcome = tokio::time::timeout(PROBE_TIMEOUT, probe(Air::Declared, "tool-probe.jsonl")).await;
    assert!(outcome.is_ok(), "the probe timed out after {PROBE_TIMEOUT:?}");
}

/// The same script for a client that does not declare the AIR envelope — the
/// upstream contract Zed gets. Declaring `_meta.jetbrains.air` at all makes the
/// adapter treat the client as JetBrains AIR and apply AIR's display rules
/// (a read shown as a list of viewed files, so its text is dropped), while
/// Meridian declared it only for `sessionFailure`. This is the other half of
/// that comparison.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs node, a signed-in claude, and spends quota"]
async fn record_the_same_without_the_air_envelope() {
    let outcome = tokio::time::timeout(PROBE_TIMEOUT, probe(Air::Undeclared, "tool-probe-no-air.jsonl")).await;
    assert!(outcome.is_ok(), "the probe timed out after {PROBE_TIMEOUT:?}");
}

#[derive(Clone, Copy, PartialEq)]
enum Air {
    Declared,
    Undeclared,
}

async fn probe(air: Air, fixture_name: &str) {
    use meridian_core::acp::protocol;
    let workspace = seed();
    let mut adapter = Adapter::spawn().await;
    let init = serde_json::to_value(protocol::InitializeParams {
        protocol_version: protocol::PROTOCOL_VERSION,
        client_capabilities: protocol::ClientCapabilities::default(),
        client_info: protocol::Implementation {
            name: "meridian-tool-probe".into(),
            title: None,
            version: "0".into(),
        },
    })
    .unwrap();
    let mut init = init;
    if air == Air::Undeclared {
        init["clientCapabilities"]
            .as_object_mut()
            .expect("capabilities are an object")
            .remove("_meta");
    }
    adapter
        .call("initialize", init)
        .await
        .expect("the adapter refused to initialize");
    let cwd = dunce_like(workspace.path()).to_string_lossy().into_owned();
    let session = adapter
        .call("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
        .await
        .expect("session/new refused");
    let session_id = session["sessionId"].as_str().expect("a session id").to_string();
    let reply = adapter
        .call(
            "session/prompt",
            json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": SCRIPT }] }),
        )
        .await;
    println!("\n=== session/prompt replied ===\n{reply:?}");

    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/acp")
        .join(fixture_name);
    std::fs::create_dir_all(fixture.parent().unwrap()).unwrap();
    std::fs::write(&fixture, scrub(&adapter.transcript, workspace.path())).unwrap();
    println!("wrote {} lines to {}", adapter.transcript.len(), fixture.display());
}

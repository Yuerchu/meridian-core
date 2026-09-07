//! Lending a hosted agent the things only Meridian knows.
//!
//! One loopback MCP server per ACP session. The agent inside already has files,
//! a shell and a web fetcher of its own; what it cannot reach is this app's
//! memories, its log and its ledger. So this offers those and nothing else —
//! duplicating Claude Code's own tools would be two routes to one effect, with
//! only one of them going through this app's approvals.
//!
//! The whole endpoint is three facts: a port the OS picked, a path nobody can
//! guess, and a bearer token. All three travel together in the `mcpServers`
//! descriptor handed to `session/new`, and none of them is written to disk.
//!
//! **The server is as small as the MCP spec permits, and that is measured
//! rather than hoped.** `tests/mcp_bridge_probe.rs` ran a stateless JSON-only
//! server against the real adapter: POST only, no SSE, no `Mcp-Session-Id`, no
//! GET stream, no DELETE. The client asks for a notification stream once, takes
//! `405`, and carries on. Read that file before adding anything here — and
//! re-run it, since the adapter is deliberately unpinned.
//!
//! # The two lifetimes
//!
//! **The server lives as long as the session; what it may *do* lives as long as
//! the turn.** An ACP session outlives many turns and sits idle between them,
//! where `current_turn_id()` is `None`. Building a `ToolContext` with
//! `turn_id: None` would break four things at once: per-turn limits stop
//! counting, the cancel token belongs to no prompt, a call arriving after its
//! turn ended still runs, and todo and audit rows are filed under nothing.
//!
//! So a turn *installs* a [`TurnSnapshot`] and clears it on the way out, and a
//! `tools/call` with no snapshot is an error rather than a call with a hole in
//! it. `tools/list` answers regardless: it describes capability and executes
//! nothing, and an idle session that claimed to have no tools would teach the
//! model they do not exist.
//!
//! The snapshot is taken **once** per call and re-checked before executing —
//! not read field by field out of the session, which would leave "which turn is
//! this call part of" without a single answer.
//!
//! **What this cannot do is reject a delayed call from an earlier turn, and
//! that is a property of the protocol rather than a gap here.** The client
//! sends no turn identity, so "issued during A, arrived during B" and "issued
//! during B, arrived during B" are the same bytes. Telling them apart needs a
//! per-turn URL, and `mcpServers` is sent once per session — measured, a
//! `session/load` *can* replace the descriptor, so the price of that guarantee
//! is a full reload per turn, which recites the entire history. Not worth it.
//! Capability identity is therefore per-session, which is the other reason the
//! first tools here are read-only, bounded and idempotent.
//!
//! # Scope is the wrapper's job, not the whitelist's
//!
//! A list of tool names says which tools; it says nothing about how much each
//! one can see, and all three of these default outward:
//!
//! - `recall_memory` and `list_memories` fall back to the *client-global* scope
//!   when a conversation has no project — so a hosted session, which often has
//!   none, would read every memory in the app.
//! - `read_app_logs` has `this_conversation` defaulting to **false**, so the
//!   model omitting one field reads the whole application log.
//! - usage had no scope field at all until `tools::usage` was written for this.
//!
//! So the bridge does not hold names and look them up. It *builds* its tools,
//! each already carrying the scope it may not leave, and `tools/call` resolves
//! against that same list — which is the "check what `tools/list` actually
//! returned, not what a constant says" rule with the two made structurally
//! identical. The wrappers overwrite the scope on the context rather than
//! checking it, so a mistake in how the context is built cannot widen them.
//!
//! None of it changes what these tools do on the desktop.
//!
//! # Permission
//!
//! Measured: the adapter *does* send `session/request_permission` before an MCP
//! tool call, which lands on `acp::approvals::ask` with no code here at all.
//! That is a bonus and **not the boundary**. The same probe found the session's
//! `mode` option offers `bypassPermissions`, `dontAsk` and `auto` — the user's
//! to set, invisible from here, and the first removes the ask entirely. So
//! every tool on this bridge has to be one that needs no permission, which is
//! what read-only, scoped and idempotent buys. A writing tool waits for the
//! bridge to ask on its own behalf.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming as IncomingBody;
use hyper::header::{AUTHORIZATION, CONTENT_TYPE};
use hyper::http::request::Parts;
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::listen_guard::{constant_time_eq, generate_token};
use crate::mcp::protocol::{INVALID_REQUEST, Inbound, Incoming, METHOD_NOT_FOUND, Outgoing};
use crate::services::Services;
use crate::tools::{FileAccess, ShellType, Tool, ToolContext};

/// Tool arguments, which are JSON a model wrote. Anything approaching this is
/// not a call.
const MAX_BODY: usize = 256 * 1024;
/// A whole request has this long to arrive — about a stalled socket, not about
/// a slow tool.
const HEADER_TIMEOUT_SECS: u64 = 15;
/// What one tool result may return. Each tool bounds its own output already;
/// this is the backstop, and it is generous because the tightest of them
/// (`read_app_logs`) sits at 12 KiB.
const MAX_RESULT_BYTES: usize = 64 * 1024;

/// The name the agent sees. Tools arrive at the model as
/// `mcp__meridian__<tool>` and appear that way in its permission prompts, so it
/// wants to read as the app rather than as a mechanism.
const SERVER_NAME: &str = "meridian";

/// The version this client speaks. Echoed from the request when the client
/// proposes one, which is what the probe measured happening.
const FALLBACK_PROTOCOL_VERSION: &str = "2025-06-18";

// ============================================================== the turn scope

/// Everything a call needs that belongs to the turn rather than the session.
///
/// Conversation, project and directory are session-level and live on the
/// [`Bridge`]; only these change from turn to turn.
#[derive(Clone)]
struct TurnSnapshot {
    /// Which turn this window belongs to, and the whole of its identity.
    ///
    /// Guards the ABA: a turn ending late must not shut the window of the turn
    /// that started after it, so `end_turn` compares rather than clearing.
    ///
    /// A monotonic counter handed back by `begin_turn` would serve equally well
    /// as an identity and worse as an API — it is a token the caller has to
    /// carry to wherever the turn ends, and `AcpSession::finish` has a path
    /// where the turn's own state is already gone. The id is known to both
    /// sides everywhere, so there is nothing to carry and nowhere to drop it.
    turn_id: String,
    assistant_id: Option<String>,
    cancel: CancellationToken,
}

// ============================================================== scoped tools

/// A memory tool pinned to one project.
///
/// `memory::get_pool_and_scope` reads `context.project_id` and falls back to
/// `ClientGlobal` when it is absent — right for a desktop conversation, where
/// losing a memory is worse than filing it broadly, and wrong here, where the
/// fallback is every memory in the app.
///
/// It *overwrites* the field rather than asserting on it. An assertion turns a
/// mis-built context into a refusal, which is better than a leak; overwriting
/// makes the leak unreachable regardless of how the context was built, which is
/// better than either.
struct ProjectScoped {
    inner: Arc<dyn Tool>,
    project_id: String,
}

#[async_trait::async_trait]
impl Tool for ProjectScoped {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> Value {
        self.inner.parameters_schema()
    }

    fn default_permission(&self) -> crate::tools::Permission {
        self.inner.default_permission()
    }

    async fn execute(&self, args: Value, context: &ToolContext) -> Result<String, String> {
        let mut scoped = context.clone();
        scoped.project_id = Some(self.project_id.clone());
        self.inner.execute(args, &scoped).await
    }
}

/// The log reader, pinned to one conversation.
///
/// Two separate holes, and closing either alone leaves the other open.
/// `this_conversation` defaults to **false**, so a model that omits it — or
/// passes it — reads the whole application log. And the tool resolves "this
/// one" from `context.conversation_id`, which is `None` on a context built
/// without one; `this_conversation: true` then produces *no filter at all*,
/// which is the same unrestricted read arrived at from the other side. So the
/// argument is overwritten **and** the context is.
///
/// The bounds are tightened too. A hosted agent asking why something failed
/// wants the recent past, and the desktop's ceilings are sized for a person
/// scrolling.
struct ConversationScopedLogs {
    inner: Arc<dyn Tool>,
    conversation_id: String,
}

/// Tighter than `app_logs`'s own 200 and one week.
const BRIDGE_LOG_LIMIT: u64 = 50;
const BRIDGE_LOG_MINUTES: i64 = 24 * 60;

#[async_trait::async_trait]
impl Tool for ConversationScopedLogs {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        "Read Meridian's own log for THIS conversation, to find out why something in the app \
         failed — a provider error, a request timing out, an MCP server refusing to connect. \
         Records outside this conversation are not visible. This is the app's runtime log, not \
         the user's files and not the transcript."
    }

    /// The two fields this tool decides are removed rather than left to be
    /// ignored. A parameter the model can set and that has no effect is a
    /// parameter it will spend tokens reasoning about.
    fn parameters_schema(&self) -> Value {
        let mut schema = self.inner.parameters_schema();
        if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
            properties.remove("this_conversation");
            properties.remove("since_minutes");
            if let Some(limit) = properties.get_mut("limit") {
                *limit = json!({
                    "type": "integer",
                    "minimum": 1,
                    "maximum": BRIDGE_LOG_LIMIT,
                    "description": "Maximum records to return, newest first. Defaults to 50.",
                });
            }
        }
        schema
    }

    fn default_permission(&self) -> crate::tools::Permission {
        self.inner.default_permission()
    }

    async fn execute(&self, args: Value, context: &ToolContext) -> Result<String, String> {
        let mut args = match args {
            Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        args.insert("this_conversation".into(), Value::Bool(true));
        args.insert("since_minutes".into(), json!(BRIDGE_LOG_MINUTES));
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(BRIDGE_LOG_LIMIT)
            .clamp(1, BRIDGE_LOG_LIMIT);
        args.insert("limit".into(), json!(limit));

        let mut scoped = context.clone();
        scoped.conversation_id = Some(self.conversation_id.clone());
        self.inner.execute(Value::Object(args), &scoped).await
    }
}

/// What this session lends, already scoped.
///
/// Built rather than filtered. A whitelist of names would need a second step to
/// wrap each one, and the failure mode of forgetting that step is a tool that
/// works — and reads everything.
///
/// The memory pair is present only when the conversation belongs to a project.
/// Hiding them beats offering them and refusing: project membership cannot
/// change under a session, so the list is stable, and a model that cannot see a
/// tool will not keep trying it or tell the user about a capability it does not
/// have.
fn tools_for(conversation_id: &str, project_id: Option<&str>, logs_dir: std::path::PathBuf) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ConversationScopedLogs {
            inner: Arc::new(crate::tools::app_logs::ReadAppLogsTool::new(logs_dir)),
            conversation_id: conversation_id.to_string(),
        }),
        Arc::new(crate::tools::usage::ConversationUsageTool::new(
            conversation_id.to_string(),
        )),
        // Read-only, bounded, idempotent — the bar every tool here has to
        // clear. Its narrowing is its own grant set (only conversations the
        // user attached to this one may be read), and the pinned constructor
        // is what stops a context mix-up from widening whose grants those are.
        Arc::new(crate::tools::read_conversation::ReadConversationTool::pinned(
            conversation_id.to_string(),
        )),
    ];
    if let Some(project_id) = project_id {
        for inner in [
            Arc::new(crate::tools::memory::RecallMemoryTool) as Arc<dyn Tool>,
            Arc::new(crate::tools::memory::ListMemoriesTool) as Arc<dyn Tool>,
        ] {
            tools.push(Arc::new(ProjectScoped {
                inner,
                project_id: project_id.to_string(),
            }));
        }
    }
    tools
}

// ================================================================= the server

pub struct Bridge {
    services: Services,
    conversation_id: String,
    tools: Vec<Arc<dyn Tool>>,
    /// `None` between turns. See the module note on the two lifetimes.
    turn: RwLock<Option<TurnSnapshot>>,
    url: String,
    token: String,
    path: String,
    shutdown: watch::Sender<bool>,
}

impl Bridge {
    /// Bind, start answering, and hand back something the session can describe
    /// to the agent.
    ///
    /// Port zero: the OS picks, and `local_addr` reads it back. A fixed port
    /// would be one more thing two Meridians on a machine could collide over,
    /// and there is no handshake file here for anyone to find it in — the URL
    /// travels in the descriptor and nowhere else.
    pub async fn start(
        services: Services,
        conversation_id: &str,
        project_id: Option<&str>,
        logs_dir: std::path::PathBuf,
    ) -> Result<Arc<Self>, String> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("could not bind the tool bridge: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("the tool bridge has no address: {e}"))?
            .port();

        let token = generate_token();
        let path = format!("/mcp/{}", uuid::Uuid::new_v4().simple());
        let (shutdown, mut shutdown_rx) = watch::channel(false);

        let bridge = Arc::new(Self {
            services,
            conversation_id: conversation_id.to_string(),
            tools: tools_for(conversation_id, project_id, logs_dir),
            turn: RwLock::new(None),
            url: format!("http://127.0.0.1:{port}{path}"),
            token,
            path,
            shutdown,
        });

        let serving = bridge.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = shutdown_rx.changed() => {
                        // `Err` means every sender is gone, which is this
                        // session being dropped. Same meaning as `true`, and
                        // not treating it so spins on a closed channel.
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => serve(stream, serving.clone()),
                        Err(e) => tracing::warn!(error = %e, "the tool bridge failed to accept"),
                    },
                }
            }
            tracing::debug!(conversation_id = %serving.conversation_id, "the tool bridge stopped");
        });

        tracing::info!(
            conversation_id = %conversation_id,
            port,
            tools = bridge.tools.len(),
            scoped_to_project = project_id.is_some(),
            "the tool bridge is listening"
        );
        Ok(bridge)
    }

    /// What goes into `mcpServers`.
    ///
    /// The token is a header rather than part of the URL: a URL is the thing
    /// that ends up in logs and error messages, and the path already carries
    /// enough entropy to be unguessable without being a credential anyone would
    /// think to redact.
    pub fn descriptor(&self) -> Value {
        json!({
            "type": "http",
            "name": SERVER_NAME,
            "url": self.url,
            "headers": [{ "name": "Authorization", "value": format!("Bearer {}", self.token) }],
        })
    }

    /// Open the window a call may execute in.
    pub fn begin_turn(&self, turn_id: &str, assistant_id: Option<&str>, cancel: CancellationToken) {
        if let Ok(mut slot) = self.turn.write() {
            *slot = Some(TurnSnapshot {
                turn_id: turn_id.to_string(),
                assistant_id: assistant_id.map(str::to_string),
                cancel,
            });
        }
    }

    /// Close it, but only if it is still this turn's.
    ///
    /// A turn ending after the next one has started must not take the new one's
    /// window with it, which is why this compares rather than being a bare
    /// `clear()`. Cheap to call for a turn that never opened one.
    pub fn end_turn(&self, turn_id: &str) {
        if let Ok(mut slot) = self.turn.write()
            && slot.as_ref().is_some_and(|t| t.turn_id == turn_id)
        {
            *slot = None;
        }
    }

    pub fn stop(&self) {
        let _ = self.shutdown.send(true);
    }

    fn snapshot(&self) -> Option<TurnSnapshot> {
        self.turn.read().ok().and_then(|slot| slot.clone())
    }

    fn find(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.iter().find(|tool| tool.name() == name)
    }

    /// The `tools/list` payload, straight off the trait.
    ///
    /// `parameters_schema()` already *is* an MCP `inputSchema`, which is why
    /// there is no mapping layer and no second description of a tool to keep in
    /// step with the first.
    fn tool_definitions(&self) -> Value {
        json!({
            "tools": self.tools.iter().map(|tool| json!({
                "name": tool.name(),
                "description": tool.description(),
                "inputSchema": tool.parameters_schema(),
            })).collect::<Vec<_>>(),
        })
    }

    /// Run one call, if there is a turn to run it in.
    async fn call(&self, params: &Value) -> Value {
        let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
        let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

        // Resolved against the tools this session actually offers, which is the
        // same list `tools/list` renders. There is no separate constant that
        // could disagree with it.
        let Some(tool) = self.find(name) else {
            return tool_error(format!(
                "`{name}` is not one of this bridge's tools. Call tools/list to see what is."
            ));
        };

        // Taken once. Read field by field off the session instead, a call would
        // have no single answer to "which turn am I part of".
        let Some(turn) = self.snapshot() else {
            return tool_error(
                "There is no turn running in this conversation, so this tool cannot be used right now. \
                 It works while you are answering.",
            );
        };
        if let Some(closed) = self.window_closed(&turn) {
            return closed;
        }

        let context = self.context(&turn).await;

        // Checked again, because building the context awaits and a turn can end
        // across an await.
        if let Some(closed) = self.window_closed(&turn) {
            return closed;
        }

        match tool.execute(args, &context).await {
            Ok(output) => {
                // And once more. The execution is the long await, and a result
                // belonging to a turn that has since ended is one the agent
                // will read as belonging to the turn it is in now.
                if let Some(closed) = self.window_closed(&turn) {
                    return closed;
                }
                let output = crate::util::take_bytes_at_char_boundary(&output, MAX_RESULT_BYTES).to_string();
                json!({ "content": [{ "type": "text", "text": output }], "isError": false })
            }
            Err(e) => tool_error(e),
        }
    }

    /// Whether the window this call was admitted through has since shut, and
    /// what to say if it has.
    ///
    /// One function rather than a condition written out at each of the three
    /// points, because the three differ only in which await they follow — and a
    /// check that drifted from its siblings would be the kind of hole that
    /// still passes every test aimed at the other two.
    ///
    /// The turn id is what makes "still the same turn" answerable. A non-empty
    /// snapshot is not the same question: the *next* turn's snapshot is
    /// non-empty too, and admitting on that basis is how a call issued under
    /// one turn comes to execute under another.
    fn window_closed(&self, turn: &TurnSnapshot) -> Option<Value> {
        if turn.cancel.is_cancelled() {
            return Some(tool_error(
                "This turn was stopped, so the tool did not run to completion.",
            ));
        }
        let current = self
            .turn
            .read()
            .ok()
            .and_then(|slot| slot.as_ref().map(|t| t.turn_id.clone()));
        (current.as_deref() != Some(turn.turn_id.as_str()))
            .then(|| tool_error("The turn ended, so this tool call was not completed."))
    }

    /// The same no-window construction `hooks::review` makes, with every field
    /// coming off the session or the snapshot rather than being read live.
    ///
    /// `FileAccess` and the sandbox are the restrictive answers rather than the
    /// convenient ones. None of the tools here touches a file or runs a
    /// command, so both are inert — and if one ever does, the default it meets
    /// should be the closed one.
    async fn context(&self, turn: &TurnSnapshot) -> ToolContext {
        let tool_secrets = {
            let pool = self.services.db.clone();
            let secrets = self.services.secrets.clone();
            tokio::task::spawn_blocking(move || crate::agent::build_tool_secrets(&secrets, &pool))
                .await
                .unwrap_or_default()
        };

        ToolContext {
            working_directory: None,
            shell: ShellType::default_for_platform(),
            file_access: FileAccess::Roots(Vec::new()),
            // Overwritten by `ProjectScoped` anyway. Left `None` so that a tool
            // added here without a wrapper fails closed rather than inheriting
            // a scope it was never granted.
            project_id: None,
            conversation_id: Some(self.conversation_id.clone()),
            turn_id: Some(turn.turn_id.clone()),
            assistant_id: turn.assistant_id.clone(),
            db_pool: Some(self.services.db.clone()),
            #[cfg(not(target_os = "android"))]
            sandbox_policy: None,
            tool_secrets,
            cancel: turn.cancel.clone(),
            journal: None,
        }
    }
}

/// An MCP tool failure, which is a *successful* JSON-RPC reply carrying
/// `isError`. A JSON-RPC error means the call could not be dispatched; this
/// means it was dispatched and said no, and the model is meant to read it.
fn tool_error(message: impl Into<String>) -> Value {
    json!({ "content": [{ "type": "text", "text": message.into() }], "isError": true })
}

// =================================================================== the wire

fn connection_builder() -> hyper::server::conn::http1::Builder {
    let mut builder = hyper::server::conn::http1::Builder::new();
    // `hooks::http` learned this the expensive way: hyper 1.x carries no
    // default timer, and `header_read_timeout` without one panics on every
    // connection rather than timing out.
    builder
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(Duration::from_secs(HEADER_TIMEOUT_SECS));
    builder
}

fn serve(stream: tokio::net::TcpStream, bridge: Arc<Bridge>) {
    tokio::spawn(async move {
        let io = hyper_util::rt::TokioIo::new(stream);
        let service = hyper::service::service_fn(move |req| {
            let bridge = bridge.clone();
            async move { Ok::<_, std::convert::Infallible>(route(req, bridge).await) }
        });
        if let Err(e) = connection_builder().serve_connection(io, service).await {
            tracing::debug!(error = %e, "a tool bridge connection ended");
        }
    });
}

fn json(status: StatusCode, body: &Value) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from_static(b"{}"))))
}

fn refuse(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    tracing::debug!(%status, message, "the tool bridge refused a request");
    json(status, &json!({ "error": message }))
}

async fn route(req: Request<IncomingBody>, bridge: Arc<Bridge>) -> Response<Full<Bytes>> {
    let (parts, body) = req.into_parts();

    // A `GET` is the notification stream and a `DELETE` ends a session. This
    // server has neither, and answering `405` is measured to be fine: the
    // adapter opens the stream once, is refused, and carries on. Nothing here
    // holds state between requests for a `DELETE` to release.
    if parts.method != Method::POST {
        return refuse(StatusCode::METHOD_NOT_ALLOWED, "this endpoint only takes POST");
    }
    // The path is half the credential, so a wrong one is not a routing miss.
    if parts.uri.path() != bridge.path {
        return refuse(StatusCode::NOT_FOUND, "no such endpoint");
    }
    if let Err((status, message)) = guard(&parts, &bridge.token) {
        return refuse(status, message);
    }

    let bytes = match Limited::new(body, MAX_BODY).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => return refuse(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
    };

    let message: Incoming = match serde_json::from_slice(&bytes) {
        Ok(message) => message,
        Err(e) => {
            return json(
                StatusCode::OK,
                &serde_json::to_value(Outgoing::error(
                    Value::Null,
                    INVALID_REQUEST,
                    format!("could not read the request: {e}"),
                ))
                .unwrap_or_else(|_| json!({})),
            );
        }
    };

    match message.classify() {
        Inbound::Request { id, method, params } => {
            let reply = match dispatch(&bridge, &method, &params).await {
                Ok(result) => Outgoing::result(id, result),
                Err(message) => Outgoing::error(id, METHOD_NOT_FOUND, message),
            };
            json(
                StatusCode::OK,
                &serde_json::to_value(reply).unwrap_or_else(|_| json!({})),
            )
        }
        // `202` with no body is what the spec prescribes and what the adapter
        // was measured to accept. A notification owes no reply, and inventing
        // one would put an unmatched id on the wire.
        Inbound::Notification { method } => {
            tracing::debug!(method, "the tool bridge received a notification");
            Response::builder()
                .status(StatusCode::ACCEPTED)
                .body(Full::new(Bytes::new()))
                .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
        }
        // A null id is not a legal request, and JSON-RPC's own answer to one it
        // cannot attribute is an error whose id is null.
        Inbound::Malformed => json(
            StatusCode::OK,
            &serde_json::to_value(Outgoing::error(
                Value::Null,
                INVALID_REQUEST,
                "a request id must not be null",
            ))
            .unwrap_or_else(|_| json!({})),
        ),
    }
}

async fn dispatch(bridge: &Arc<Bridge>, method: &str, params: &Value) -> Result<Value, String> {
    match method {
        "initialize" => {
            // The client's own proposal is echoed. Naming a version it did not
            // ask for is how a negotiation becomes a disagreement, and this
            // server has nothing version-dependent in it to disagree about.
            let version = params
                .get("protocolVersion")
                .cloned()
                .unwrap_or_else(|| json!(FALLBACK_PROTOCOL_VERSION));
            Ok(json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
            }))
        }
        "tools/list" => Ok(bridge.tool_definitions()),
        "tools/call" => Ok(bridge.call(params).await),
        // `ping` is in the spec and costs nothing to answer.
        "ping" => Ok(json!({})),
        other => Err(format!("no such method: {other}")),
    }
}

/// Whether this request is allowed to reach a tool.
///
/// The same four checks as `hooks::http::guard` and for the same reason: the
/// only legitimate caller is a local process this app started, so anything
/// carrying browser provenance is not it. Do **not** copy `remote/`'s guard
/// here — that one answers preflights because its caller really is a page on
/// another device, and the two are inverses.
fn guard(parts: &Parts, token: &str) -> Result<(), (StatusCode, &'static str)> {
    // A page in the user's browser can POST to loopback. It cannot read the
    // reply, but a tool that has already run does not need to be read.
    // Requiring JSON forces a preflight, which is never answered, and refusing
    // a cross-site fetch closes what is left.
    if parts.headers.contains_key("origin") {
        return Err((StatusCode::FORBIDDEN, "cross-origin request refused"));
    }
    if let Some(site) = parts.headers.get("sec-fetch-site").and_then(|v| v.to_str().ok())
        && site != "none"
        && site != "same-origin"
    {
        return Err((StatusCode::FORBIDDEN, "cross-site request refused"));
    }
    let content_type = parts
        .headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !content_type.starts_with("application/json") {
        return Err((StatusCode::FORBIDDEN, "expected application/json"));
    }

    let presented = parts
        .headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !constant_time_eq(token.as_bytes(), presented.as_bytes()) {
        return Err((StatusCode::UNAUTHORIZED, "bad or missing token"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn parts(headers: &[(&str, &str)]) -> Parts {
        let mut builder = Request::builder().method(Method::POST).uri("/mcp/abc");
        for (key, value) in headers {
            builder = builder.header(*key, *value);
        }
        builder.body(()).unwrap().into_parts().0
    }

    fn authed() -> Vec<(&'static str, &'static str)> {
        vec![("content-type", "application/json"), ("authorization", "Bearer t0ken")]
    }

    #[test]
    fn a_well_formed_local_request_is_allowed() {
        assert!(guard(&parts(&authed()), "t0ken").is_ok());
    }

    #[test]
    fn a_browser_origin_is_refused() {
        let mut headers = authed();
        headers.push(("origin", "https://evil.example"));
        assert_eq!(guard(&parts(&headers), "t0ken").unwrap_err().0, StatusCode::FORBIDDEN);
    }

    #[test]
    fn a_cross_site_fetch_is_refused() {
        let mut headers = authed();
        headers.push(("sec-fetch-site", "cross-site"));
        assert_eq!(guard(&parts(&headers), "t0ken").unwrap_err().0, StatusCode::FORBIDDEN);
    }

    /// curl and the address bar send `none`; our own page sends `same-origin`.
    /// Neither is the attack, and the adapter sends the header at all.
    #[test]
    fn a_direct_request_passes() {
        for site in ["none", "same-origin"] {
            let mut headers = authed();
            headers.push(("sec-fetch-site", site));
            assert!(guard(&parts(&headers), "t0ken").is_ok(), "{site}");
        }
    }

    #[test]
    fn a_form_post_is_refused_before_the_token_is_read() {
        let err = guard(&parts(&[("content-type", "text/plain")]), "t0ken").unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }

    #[test]
    fn a_wrong_or_missing_token_is_unauthorized() {
        let wrong = vec![("content-type", "application/json"), ("authorization", "Bearer nope")];
        assert_eq!(guard(&parts(&wrong), "t0ken").unwrap_err().0, StatusCode::UNAUTHORIZED);
        let none = vec![("content-type", "application/json")];
        assert_eq!(guard(&parts(&none), "t0ken").unwrap_err().0, StatusCode::UNAUTHORIZED);
    }

    // ------------------------------------------------------------ the scope

    fn names(tools: &[Arc<dyn Tool>]) -> Vec<&str> {
        tools.iter().map(|t| t.name()).collect()
    }

    /// The decision the module note calls hiding rather than refusing: with no
    /// project there is no project scope, and the client-global fallback is not
    /// an acceptable substitute.
    #[test]
    fn memory_is_absent_from_a_conversation_with_no_project() {
        let without = tools_for("c-1", None, "logs".into());
        assert!(!names(&without).contains(&"recall_memory"), "{:?}", names(&without));
        assert!(!names(&without).contains(&"list_memories"));

        let with = tools_for("c-1", Some("p-1"), "logs".into());
        assert!(names(&with).contains(&"recall_memory"), "{:?}", names(&with));
        assert!(names(&with).contains(&"list_memories"));
    }

    /// Nothing that writes, runs a command or touches a file. The list is
    /// asserted whole rather than by exclusion, so adding a tool has to come
    /// past this test.
    #[test]
    fn the_bridge_lends_five_read_only_tools_and_no_others() {
        let tools = tools_for("c-1", Some("p-1"), "logs".into());
        let mut found = names(&tools);
        found.sort_unstable();
        assert_eq!(
            found,
            [
                "conversation_usage",
                "list_memories",
                "read_app_logs",
                "read_conversation",
                "recall_memory"
            ]
        );
        for tool in &tools {
            assert!(
                matches!(tool.default_permission(), crate::tools::Permission::Always),
                "`{}` needs permission, which this bridge cannot ask for",
                tool.name()
            );
        }
    }

    /// The fields the wrapper decides are gone from the schema, so the model
    /// has nothing to set and nothing to reason about.
    #[test]
    fn the_log_wrapper_removes_the_parameters_it_overrides() {
        let tools = tools_for("c-1", None, "logs".into());
        let logs = tools.iter().find(|t| t.name() == "read_app_logs").unwrap();
        let schema = logs.parameters_schema();
        let properties = schema.get("properties").and_then(Value::as_object).unwrap();
        assert!(!properties.contains_key("this_conversation"), "{properties:?}");
        assert!(!properties.contains_key("since_minutes"), "{properties:?}");
        assert_eq!(properties["limit"]["maximum"], json!(BRIDGE_LOG_LIMIT));
    }

    /// The underlying tool still has both. A wrapper for the bridge must not
    /// have changed what the desktop offers.
    #[test]
    fn the_desktop_log_tool_is_untouched() {
        let bare = crate::tools::app_logs::ReadAppLogsTool::new("logs".into());
        let schema = bare.parameters_schema();
        let properties = schema.get("properties").and_then(Value::as_object).unwrap();
        assert!(properties.contains_key("this_conversation"));
        assert!(properties.contains_key("since_minutes"));
    }

    // ------------------------------------------------- the argument override

    /// What a wrapper actually handed the tool underneath it.
    struct Seen {
        args: Value,
        conversation_id: Option<String>,
    }

    /// A recording stand-in, so the override can be observed rather than
    /// inferred from a database.
    struct Spy(Arc<Mutex<Vec<Seen>>>);

    #[async_trait::async_trait]
    impl Tool for Spy {
        fn name(&self) -> &str {
            "read_app_logs"
        }
        fn description(&self) -> &str {
            "spy"
        }
        fn parameters_schema(&self) -> Value {
            json!({ "type": "object", "properties": {
                "this_conversation": {}, "since_minutes": {}, "limit": {}
            }})
        }
        fn default_permission(&self) -> crate::tools::Permission {
            crate::tools::Permission::Always
        }
        async fn execute(&self, args: Value, context: &ToolContext) -> Result<String, String> {
            self.0.lock().unwrap().push(Seen {
                args,
                conversation_id: context.conversation_id.clone(),
            });
            Ok(String::new())
        }
    }

    fn blank_context() -> ToolContext {
        ToolContext {
            working_directory: None,
            shell: ShellType::default_for_platform(),
            file_access: FileAccess::Roots(Vec::new()),
            project_id: None,
            // Deliberately absent, which is the second of the two holes: the
            // tool resolves "this conversation" from here, and `None` means no
            // filter at all.
            conversation_id: None,
            turn_id: Some("t-1".into()),
            assistant_id: None,
            db_pool: None,
            #[cfg(not(target_os = "android"))]
            sandbox_policy: None,
            tool_secrets: Default::default(),
            cancel: CancellationToken::new(),
            journal: None,
        }
    }

    /// Both halves of the log hole, in the one case that has both: the model
    /// asking for the whole log *and* a context that could not have narrowed it
    /// anyway.
    #[tokio::test]
    async fn the_log_wrapper_overrides_the_argument_and_the_context() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let wrapper = ConversationScopedLogs {
            inner: Arc::new(Spy(seen.clone())),
            conversation_id: "c-1".into(),
        };

        wrapper
            .execute(
                json!({ "this_conversation": false, "since_minutes": 100_000, "limit": 10_000 }),
                &blank_context(),
            )
            .await
            .unwrap();

        let calls = seen.lock().unwrap();
        let Seen { args, conversation_id } = &calls[0];
        assert_eq!(args["this_conversation"], json!(true), "the model kept the whole log");
        assert_eq!(args["since_minutes"], json!(BRIDGE_LOG_MINUTES));
        assert_eq!(args["limit"], json!(BRIDGE_LOG_LIMIT));
        assert_eq!(
            conversation_id.as_deref(),
            Some("c-1"),
            "a context with no conversation would have meant no filter"
        );
    }

    /// Arguments that are not an object at all must not become a way past the
    /// override.
    #[tokio::test]
    async fn the_log_wrapper_survives_arguments_that_are_not_an_object() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let wrapper = ConversationScopedLogs {
            inner: Arc::new(Spy(seen.clone())),
            conversation_id: "c-1".into(),
        };
        wrapper.execute(json!("not an object"), &blank_context()).await.unwrap();
        let calls = seen.lock().unwrap();
        assert_eq!(calls[0].args["this_conversation"], json!(true));
    }

    struct ProjectSpy(Arc<Mutex<Option<String>>>);

    #[async_trait::async_trait]
    impl Tool for ProjectSpy {
        fn name(&self) -> &str {
            "recall_memory"
        }
        fn description(&self) -> &str {
            "spy"
        }
        fn parameters_schema(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        fn default_permission(&self) -> crate::tools::Permission {
            crate::tools::Permission::Always
        }
        async fn execute(&self, _args: Value, context: &ToolContext) -> Result<String, String> {
            *self.0.lock().unwrap() = context.project_id.clone();
            Ok(String::new())
        }
    }

    /// Overwriting rather than asserting: a context built without a project —
    /// which is what the bridge deliberately hands over — still reaches the
    /// tool with one, so the client-global fallback is unreachable.
    #[tokio::test]
    async fn the_memory_wrapper_pins_the_project_whatever_the_context_says() {
        let seen = Arc::new(Mutex::new(None));
        let wrapper = ProjectScoped {
            inner: Arc::new(ProjectSpy(seen.clone())),
            project_id: "p-1".into(),
        };

        let mut context = blank_context();
        context.project_id = Some("somebody-elses-project".into());
        wrapper.execute(json!({}), &context).await.unwrap();

        assert_eq!(seen.lock().unwrap().as_deref(), Some("p-1"));
    }

    // ------------------------------------------------------- the turn window

    /// Records that it ran, and optionally moves the session on while running.
    ///
    /// The second half is what makes the "ended during execute" case reachable
    /// without a race: the turn genuinely changes inside the await that a real
    /// slow tool would be sitting in.
    struct Ran {
        ran: Arc<Mutex<usize>>,
        cancelled_when_called: Arc<Mutex<Option<bool>>>,
        during: Option<MidCall>,
    }

    /// Something for the test to do while the tool is "running", which is what
    /// makes the turn genuinely change inside the await a slow tool would be
    /// sitting in.
    type MidCall = Box<dyn Fn() + Send + Sync>;

    #[async_trait::async_trait]
    impl Tool for Ran {
        fn name(&self) -> &str {
            "conversation_usage"
        }
        fn description(&self) -> &str {
            "spy"
        }
        fn parameters_schema(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        fn default_permission(&self) -> crate::tools::Permission {
            crate::tools::Permission::Always
        }
        async fn execute(&self, _args: Value, context: &ToolContext) -> Result<String, String> {
            *self.ran.lock().unwrap() += 1;
            *self.cancelled_when_called.lock().unwrap() = Some(context.cancel.is_cancelled());
            if let Some(during) = &self.during {
                during();
            }
            Ok("the tool ran".into())
        }
    }

    /// The shape `generate_token` produces — a simple uuid — rather than a
    /// letter, so "this string is not in that one" means something.
    const TEST_TOKEN: &str = "9f2c4b7e1a6d40538c2e7b91a4f6d3e0";

    /// A bridge holding one spy and no socket. `start` binds a port and spawns
    /// an accept loop, neither of which any of these need.
    fn bridge_with(tool: Arc<dyn Tool>, dir: &std::path::Path) -> Arc<Bridge> {
        Arc::new(Bridge {
            services: crate::services::bare_services(dir),
            conversation_id: "c-1".into(),
            tools: vec![tool],
            turn: RwLock::new(None),
            // A token that looks like a real one, because a one-letter stand-in
            // occurs by accident in "http" and would make "the token is not in
            // the URL" impossible to assert.
            url: "http://127.0.0.1:0/mcp/x".into(),
            token: TEST_TOKEN.into(),
            path: "/mcp/x".into(),
            shutdown: watch::channel(false).0,
        })
    }

    /// A tool, and the two things a test wants to know about it afterwards.
    struct Watched {
        ran: Arc<Mutex<usize>>,
        cancelled_when_called: Arc<Mutex<Option<bool>>>,
        tool: Arc<dyn Tool>,
    }

    fn spy() -> Watched {
        let ran = Arc::new(Mutex::new(0));
        let cancelled_when_called = Arc::new(Mutex::new(None));
        let tool = Arc::new(Ran {
            ran: ran.clone(),
            cancelled_when_called: cancelled_when_called.clone(),
            during: None,
        });
        Watched {
            ran,
            cancelled_when_called,
            tool,
        }
    }

    fn is_error(reply: &Value) -> bool {
        reply["isError"] == json!(true)
    }

    fn text(reply: &Value) -> String {
        reply["content"][0]["text"].as_str().unwrap_or_default().to_string()
    }

    /// The case the module note is about: a session outlives its turns, and a
    /// call landing between them has no turn to belong to. Refusing beats
    /// running with `turn_id: None`.
    #[tokio::test]
    async fn a_call_on_an_idle_session_is_refused_and_the_tool_does_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let Watched { ran, tool, .. } = spy();
        let bridge = bridge_with(tool, dir.path());

        let reply = bridge.call(&json!({ "name": "conversation_usage" })).await;
        assert!(is_error(&reply), "{reply}");
        assert!(text(&reply).contains("no turn running"), "{}", text(&reply));
        assert_eq!(*ran.lock().unwrap(), 0, "the tool ran without a turn behind it");
    }

    /// `tools/list` is the opposite decision, and it matters: an idle session
    /// that reported no tools would teach the model they do not exist, and it
    /// would stop asking for them once a turn was running.
    #[tokio::test]
    async fn an_idle_session_still_lists_its_tools() {
        let dir = tempfile::tempdir().unwrap();
        let Watched { tool, .. } = spy();
        let bridge = bridge_with(tool, dir.path());
        assert!(bridge.snapshot().is_none());
        assert_eq!(bridge.tool_definitions()["tools"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_call_inside_a_turn_runs() {
        let dir = tempfile::tempdir().unwrap();
        let Watched { ran, tool, .. } = spy();
        let bridge = bridge_with(tool, dir.path());
        bridge.begin_turn("t-1", Some("m-1"), CancellationToken::new());

        let reply = bridge.call(&json!({ "name": "conversation_usage" })).await;
        assert!(!is_error(&reply), "{reply}");
        assert_eq!(text(&reply), "the tool ran");
        assert_eq!(*ran.lock().unwrap(), 1);
    }

    /// The turn's own token reaches the tool, so stopping the prompt stops
    /// whatever the bridge started rather than leaving it running against a
    /// turn that has gone.
    #[tokio::test]
    async fn a_running_tool_holds_the_turns_own_cancellation_token() {
        let dir = tempfile::tempdir().unwrap();
        let Watched {
            cancelled_when_called,
            tool,
            ..
        } = spy();
        let bridge = bridge_with(tool, dir.path());
        let cancel = CancellationToken::new();
        bridge.begin_turn("t-1", None, cancel.clone());

        bridge.call(&json!({ "name": "conversation_usage" })).await;
        assert_eq!(*cancelled_when_called.lock().unwrap(), Some(false));

        // And once the turn is stopped the same token refuses the next call
        // before the tool is reached at all.
        cancel.cancel();
        let reply = bridge.call(&json!({ "name": "conversation_usage" })).await;
        assert!(is_error(&reply) && text(&reply).contains("stopped"), "{reply}");
    }

    /// The last of the three checks, and the one that needs a real await to be
    /// reachable: the tool finishes, and by then the turn it belonged to is
    /// over. Handing the result back would file work from turn A against turn
    /// B, which is what the agent reads it as.
    #[tokio::test]
    async fn a_result_is_discarded_when_the_turn_ended_while_the_tool_ran() {
        let dir = tempfile::tempdir().unwrap();
        let ran = Arc::new(Mutex::new(0));
        let ended: Arc<Mutex<Option<Arc<Bridge>>>> = Arc::new(Mutex::new(None));

        let handle = ended.clone();
        let tool = Arc::new(Ran {
            ran: ran.clone(),
            cancelled_when_called: Arc::new(Mutex::new(None)),
            during: Some(Box::new(move || {
                if let Some(bridge) = handle.lock().unwrap().as_ref() {
                    bridge.end_turn("t-1");
                }
            })),
        });
        let bridge = bridge_with(tool, dir.path());
        *ended.lock().unwrap() = Some(bridge.clone());

        bridge.begin_turn("t-1", None, CancellationToken::new());
        let reply = bridge.call(&json!({ "name": "conversation_usage" })).await;

        assert_eq!(*ran.lock().unwrap(), 1, "the tool should have run");
        assert!(is_error(&reply), "its result was handed back anyway: {reply}");
        assert!(text(&reply).contains("turn ended"), "{}", text(&reply));
    }

    /// The ABA the generation exists for. A turn ending late must not shut the
    /// window of the turn that started after it — with a bare `clear()` it
    /// would, and every call in the new turn would then be refused for a reason
    /// nobody could reconstruct.
    #[tokio::test]
    async fn a_late_ending_turn_does_not_close_the_next_ones_window() {
        let dir = tempfile::tempdir().unwrap();
        let Watched { ran, tool, .. } = spy();
        let bridge = bridge_with(tool, dir.path());

        bridge.begin_turn("t-1", None, CancellationToken::new());
        bridge.begin_turn("t-2", None, CancellationToken::new());

        bridge.end_turn("t-1");

        let reply = bridge.call(&json!({ "name": "conversation_usage" })).await;
        assert!(!is_error(&reply), "the second turn lost its window: {reply}");
        assert_eq!(*ran.lock().unwrap(), 1);

        bridge.end_turn("t-2");
        assert!(is_error(&bridge.call(&json!({ "name": "conversation_usage" })).await));
    }

    /// A name outside this session's list is refused, and the list it is
    /// checked against is the one `tools/list` renders — not a constant that
    /// could drift from it.
    #[tokio::test]
    async fn a_tool_this_session_does_not_offer_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let Watched { tool, .. } = spy();
        let bridge = bridge_with(tool, dir.path());
        bridge.begin_turn("t-1", None, CancellationToken::new());

        let reply = bridge
            .call(&json!({ "name": "run_command", "arguments": { "command": "rm -rf /" } }))
            .await;
        assert!(is_error(&reply), "{reply}");
        assert!(text(&reply).contains("not one of this bridge's tools"), "{reply}");
    }

    /// **A limitation, pinned so nobody later mistakes it for a guarantee.**
    ///
    /// The protocol carries no turn identity, so a call issued during turn A
    /// and arriving during turn B is byte-identical to one issued during B.
    /// Both are admitted. What the window *does* stop is a call arriving when
    /// there is no turn at all, and a result outliving the turn that asked for
    /// it — which is why the tools here are read-only and idempotent.
    ///
    /// Delete this test only alongside a per-turn descriptor, which
    /// `mcp_bridge_probe` measured to cost a full `session/load` per turn.
    #[tokio::test]
    async fn a_delayed_call_from_an_earlier_turn_cannot_be_told_apart() {
        let dir = tempfile::tempdir().unwrap();
        let Watched { ran, tool, .. } = spy();
        let bridge = bridge_with(tool, dir.path());

        bridge.begin_turn("t-1", None, CancellationToken::new());
        // The whole of what turn A's caller sent. There is nothing else in it.
        let issued_during_a = json!({ "name": "conversation_usage" });

        bridge.end_turn("t-1");
        bridge.begin_turn("t-2", None, CancellationToken::new());

        let reply = bridge.call(&issued_during_a).await;
        assert!(
            !is_error(&reply),
            "if this now refuses, the protocol grew a turn identity — say so here"
        );
        assert_eq!(*ran.lock().unwrap(), 1);
    }

    /// The descriptor is what the agent is handed, and the probe's fixture is
    /// what the adapter was measured accepting.
    #[tokio::test]
    async fn the_descriptor_is_the_shape_the_adapter_takes() {
        let dir = tempfile::tempdir().unwrap();
        let Watched { tool, .. } = spy();
        let bridge = bridge_with(tool, dir.path());

        let descriptor = bridge.descriptor();
        assert_eq!(descriptor["type"], json!("http"));
        assert_eq!(descriptor["name"], json!(SERVER_NAME));
        assert_eq!(descriptor["headers"][0]["name"], json!("Authorization"));
        assert_eq!(descriptor["headers"][0]["value"], json!(format!("Bearer {TEST_TOKEN}")));
        // The token is a header and not part of the address, because the
        // address is the half that ends up in logs and error messages.
        assert!(
            !descriptor["url"].as_str().unwrap().contains(TEST_TOKEN),
            "the token leaked into the URL: {}",
            descriptor["url"]
        );
    }
}

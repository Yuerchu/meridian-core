pub mod actor;
pub mod protocol;
pub mod stdio;
pub mod streamable_http;

#[cfg(test)]
mod tests;

use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::db::models::mcp_server::{McpServerRow, McpTransport as McpTransportKind};
use crate::provider::ToolDefinition;
use actor::{ActorHandle, ActorObituary};
use protocol::{McpCallToolResult, McpToolsListResult};
use stdio::StdioTransport;
use streamable_http::StreamableHttpTransport;

/// Ceiling on the whole connect sequence — spawn, handshake, tools/list.
///
/// The individual steps have their own timeouts, but not all of them: a stdio
/// server that never reads its stdin can block the write side indefinitely, and
/// spawning is unbounded on both platforms. Without this the settings page's
/// "connect" button could hang for as long as the server felt like.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// Why a request did not produce a result.
///
/// The distinction is the whole point: one of these leaves the connection
/// usable and the other does not, and treating them alike is what let a
/// desynchronised stdio stream carry on answering the wrong questions.
#[derive(Debug)]
pub enum TransportError {
    /// The server answered, and the answer was an error. Framing is intact and
    /// the next request will be understood.
    Rpc(String),
    /// The transport itself failed — a closed pipe, an I/O error, a frame that
    /// could not be completed. Nothing further may be sent on it.
    Broken(String),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::Rpc(m) | TransportError::Broken(m) => write!(f, "{m}"),
        }
    }
}

#[async_trait::async_trait]
pub trait McpTransport: Send {
    async fn request(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, TransportError>;
    async fn notify(&mut self, method: &str, params: Option<serde_json::Value>) -> Result<(), String>;
    async fn shutdown(&mut self);
}

#[derive(Debug, Clone, Serialize)]
pub struct McpToolDef {
    pub server_id: String,
    pub server_name: String,
    pub name: String,
    pub qualified_name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// What the settings page shows, as opposed to what it used to infer from the
/// tool list. A server that connects and exposes nothing is connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionState {
    #[allow(dead_code)] // the front end infers this from a missing entry
    Disconnected,
    Connecting,
    Connected,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpConnectionStatus {
    pub server_id: String,
    pub state: ConnectionState,
    pub tool_count: usize,
}

/// The outcome of one connect attempt, shared with everyone who asked for the
/// same one.
type ConnectResult = Option<Result<(), String>>;

enum Slot {
    Connecting {
        generation: u64,
        cancel: CancellationToken,
        done: watch::Receiver<ConnectResult>,
    },
    Connected {
        generation: u64,
        handle: ActorHandle,
    },
}

impl Slot {
    fn generation(&self) -> u64 {
        match self {
            Slot::Connecting { generation, .. } | Slot::Connected { generation, .. } => *generation,
        }
    }
}

fn sanitize_name(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
        .collect()
}

fn build_qualified_name(server_name: &str, tool_name: &str) -> String {
    format!("mcp__{}__{}", sanitize_name(server_name), sanitize_name(tool_name))
}

/// Owns every MCP connection and the tool list derived from them.
///
/// Interior mutability rather than an outer mutex, because the outer mutex was
/// the bug: it was held across whole tool calls, so one server's slow response
/// stopped every conversation in the app from assembling its tool set, and the
/// settings page with them.
///
/// Two locks, never held together for long:
/// - `servers` guards the connection state machine. Only map operations happen
///   under it; connecting and shutting down are done outside.
/// - `snapshot` holds the published tool list. Readers clone an `Arc` and are
///   never blocked by a server that is busy or hanging.
pub struct McpRegistry {
    servers: Mutex<HashMap<String, Slot>>,
    snapshot: RwLock<Snapshot>,
    generations: AtomicU64,
    /// Transports handed to the next `dial` calls instead of ones built from
    /// the server's configuration, newest first. The only way to exercise the
    /// state machine without spawning real processes.
    #[cfg(test)]
    staged_transports: Mutex<Vec<Box<dyn McpTransport>>>,
    /// Shortened so a test does not have to wait out the real one.
    #[cfg(test)]
    test_deadline: std::sync::OnceLock<std::time::Duration>,
}

/// Published together so a reader can never see tools from one moment and
/// definitions from another.
#[derive(Default)]
struct Snapshot {
    tools: Arc<Vec<McpToolDef>>,
    definitions: Arc<Vec<ToolDefinition>>,
}

impl McpRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            servers: Mutex::new(HashMap::new()),
            snapshot: RwLock::new(Snapshot::default()),
            generations: AtomicU64::new(1),
            #[cfg(test)]
            staged_transports: Mutex::new(Vec::new()),
            #[cfg(test)]
            test_deadline: std::sync::OnceLock::new(),
        })
    }

    /// Poisoning is recovered from rather than propagated: every critical
    /// section is a map or vector operation, so a panic elsewhere cannot leave
    /// one half-applied, and refusing the lock would take every MCP server in
    /// the app down with whichever call panicked.
    fn servers(&self) -> std::sync::MutexGuard<'_, HashMap<String, Slot>> {
        self.servers.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn next_generation(&self) -> u64 {
        self.generations.fetch_add(1, Ordering::Relaxed)
    }

    /// The tool definitions handed to a model. Reading this does not touch the
    /// connection lock at all, which is what keeps a hung server from stopping
    /// other conversations from starting.
    pub fn tool_definitions(&self) -> Arc<Vec<ToolDefinition>> {
        Arc::clone(&self.snapshot.read().unwrap_or_else(|e| e.into_inner()).definitions)
    }

    pub fn tools(&self) -> Arc<Vec<McpToolDef>> {
        Arc::clone(&self.snapshot.read().unwrap_or_else(|e| e.into_inner()).tools)
    }

    pub fn tools_for_server(&self, server_id: &str) -> Vec<McpToolDef> {
        self.tools()
            .iter()
            .filter(|t| t.server_id == server_id)
            .cloned()
            .collect()
    }

    /// Only servers with an entry appear. Anything absent is disconnected, and
    /// the front end already knows the full list from the database — reporting
    /// a row per known-but-unconnected server here would just be the same
    /// information from a worse source.
    pub fn all_connection_statuses(&self) -> Vec<McpConnectionStatus> {
        let states: Vec<(String, ConnectionState)> = self
            .servers()
            .iter()
            .map(|(id, slot)| {
                (
                    id.clone(),
                    match slot {
                        Slot::Connected { .. } => ConnectionState::Connected,
                        Slot::Connecting { .. } => ConnectionState::Connecting,
                    },
                )
            })
            .collect();
        let tools = self.tools();
        states
            .into_iter()
            .map(|(server_id, state)| McpConnectionStatus {
                tool_count: tools.iter().filter(|t| t.server_id == server_id).count(),
                server_id,
                state,
            })
            .collect()
    }

    /// Republish the tool list from `tools`, which the caller has already
    /// brought up to date. Called from inside the `servers` critical section so
    /// the connection state and the published tools cannot disagree — including
    /// on the uniqueness of qualified names, which is decided against the whole
    /// list.
    /// Drop this server's tools from the published list, from inside the
    /// connection critical section.
    ///
    /// The guard is taken by reference purely to make that impossible to get
    /// wrong. Removing tools after releasing the lock is a race: a stale
    /// obituary can pass its generation check, release, let a fresh connection
    /// publish, and then delete the new connection's tools on its way out.
    fn forget_tools_locked(&self, _guard: &std::sync::MutexGuard<'_, HashMap<String, Slot>>, server_id: &str) {
        let remaining: Vec<McpToolDef> = self
            .tools()
            .iter()
            .filter(|t| t.server_id != server_id)
            .cloned()
            .collect();
        self.publish(remaining);
    }

    fn publish(&self, tools: Vec<McpToolDef>) {
        let definitions = tools
            .iter()
            .map(|t| ToolDefinition {
                name: t.qualified_name.clone(),
                description: t.description.clone(),
                parameters: t.input_schema.clone(),
            })
            .collect();
        let mut snapshot = self.snapshot.write().unwrap_or_else(|e| e.into_inner());
        snapshot.tools = Arc::new(tools);
        snapshot.definitions = Arc::new(definitions);
    }

    /// Connect, handshake and list tools.
    ///
    /// Concurrent calls for the same server share one attempt: the second
    /// waits on the first rather than starting a second process. Returning
    /// `Ok(())` immediately instead would tell the settings page a connection
    /// was established while the handshake was still running.
    pub async fn connect(self: &Arc<Self>, server: &McpServerRow) -> Result<(), String> {
        enum Start {
            /// Someone else is already doing this; wait for their answer.
            Join(watch::Receiver<ConnectResult>),
            /// Ours to run.
            Own {
                generation: u64,
                cancel: CancellationToken,
                done: watch::Sender<ConnectResult>,
                /// The connection being replaced, to be shut down off-lock.
                replaced: Option<ActorHandle>,
            },
        }

        let start = {
            let mut servers = self.servers();
            match servers.get(&server.id) {
                Some(Slot::Connecting { done, .. }) => Start::Join(done.clone()),
                existing => {
                    // Swapped for a fresh `Connecting` inside this one critical
                    // section. Releasing the lock to disconnect first would let
                    // a second caller find the slot empty and start its own
                    // connect alongside ours.
                    let replaced = match existing {
                        Some(Slot::Connected { handle, .. }) => Some(handle.clone()),
                        _ => None,
                    };
                    let generation = self.next_generation();
                    let cancel = CancellationToken::new();
                    let (tx, rx) = watch::channel(None);
                    servers.insert(
                        server.id.clone(),
                        Slot::Connecting {
                            generation,
                            cancel: cancel.clone(),
                            done: rx,
                        },
                    );
                    // The old connection is about to be stopped, so its tools
                    // stop being offered now rather than when the new one
                    // arrives. Leaving them up would mean handing the model a
                    // tool with nothing behind it — and if the new dial fails,
                    // leaving them there for good.
                    self.forget_tools_locked(&servers, &server.id);
                    Start::Own {
                        generation,
                        cancel,
                        done: tx,
                        replaced,
                    }
                }
            }
        };

        let (generation, cancel, done, replaced) = match start {
            Start::Join(mut rx) => {
                // The initial `None` means "still running"; wait for it to be
                // replaced by the outcome.
                loop {
                    if let Some(result) = rx.borrow().clone() {
                        return result;
                    }
                    if rx.changed().await.is_err() {
                        return Err("the MCP connection attempt ended without a result".into());
                    }
                }
            }
            Start::Own {
                generation,
                cancel,
                done,
                replaced,
            } => (generation, cancel, done, replaced),
        };

        if let Some(old) = replaced {
            self.retire(&server.id, old).await;
        }

        let outcome = tokio::select! {
            _ = cancel.cancelled() => Err("the MCP connection attempt was cancelled".into()),
            r = tokio::time::timeout(CONNECT_TIMEOUT, self.dial(server)) => match r {
                Ok(inner) => inner,
                Err(_) => Err(format!(
                    "connecting to the MCP server timed out after {}s",
                    CONNECT_TIMEOUT.as_secs()
                )),
            },
        };

        let result = match outcome {
            Ok((transport, tools)) => self.commit(server, generation, transport, tools),
            Err(e) => {
                self.abandon(&server.id, generation);
                Err(e)
            }
        };

        // Everyone who joined this attempt gets the same answer.
        let _ = done.send(Some(result.clone()));
        result
    }

    /// Build the transport and complete the handshake. Runs with no lock held.
    async fn dial(&self, server: &McpServerRow) -> Result<(Box<dyn McpTransport>, Vec<protocol::McpToolInfo>), String> {
        let started = std::time::Instant::now();
        let fail = |stage: &'static str, error: String| -> String {
            tracing::warn!(
                server_id = %server.id,
                server_name = %server.name,
                transport = %server.transport_type,
                stage,
                error = %error,
                "MCP server connection failed"
            );
            error
        };

        #[cfg(test)]
        let staged = self.staged_transports.lock().unwrap_or_else(|e| e.into_inner()).pop();
        #[cfg(not(test))]
        let staged: Option<Box<dyn McpTransport>> = None;

        let transport_type = McpTransportKind::parse(&server.transport_type).map_err(|error| fail("config", error))?;
        let mut transport: Box<dyn McpTransport> = match staged {
            Some(t) => t,
            None => match transport_type {
                McpTransportKind::Stdio => {
                    let command = server
                        .command
                        .as_deref()
                        .ok_or_else(|| fail("config", "missing command".into()))?;
                    let args = parse_config_field::<Vec<String>>(server.args.as_deref(), "args")
                        .map_err(|error| fail("config", error))?;
                    // A malformed env is the worst of the three: the server starts
                    // without its token and fails every call with a 401 that looks
                    // like the user's key is wrong.
                    let env = parse_config_field::<HashMap<String, String>>(server.env.as_deref(), "env")
                        .map_err(|error| fail("config", error))?;
                    Box::new(
                        StdioTransport::spawn(command, &args, &env, None)
                            .await
                            .map_err(|e| fail("spawn", e))?,
                    )
                }
                McpTransportKind::StreamableHttp => {
                    let url = server
                        .url
                        .as_deref()
                        .ok_or_else(|| fail("config", "missing URL".into()))?;
                    let headers = parse_config_field::<HashMap<String, String>>(server.headers.as_deref(), "headers")
                        .map_err(|error| fail("config", error))?;
                    Box::new(StreamableHttpTransport::new(url, &headers).map_err(|e| fail("connect", e))?)
                }
            },
        };

        transport
            .request(
                "initialize",
                Some(serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "meridian", "version": "0.1.0" }
                })),
            )
            .await
            .map_err(|e| fail("initialize", e.to_string()))?;

        if let Err(e) = transport.notify("notifications/initialized", None).await {
            // Some servers refuse later requests without it, so a failure here
            // explains an otherwise baffling timeout further down.
            tracing::debug!(server_id = %server.id, error = %e, "MCP initialized notification failed");
        }

        let result = transport
            .request("tools/list", None)
            .await
            .map_err(|e| fail("tools_list", e.to_string()))?;
        let tools_result: McpToolsListResult =
            serde_json::from_value(result).map_err(|e| fail("parse_tools", format!("parse tools/list: {e}")))?;

        tracing::debug!(
            server_id = %server.id,
            tool_count = tools_result.tools.len(),
            duration_ms = started.elapsed().as_millis() as u64,
            "MCP handshake finished"
        );
        Ok((transport, tools_result.tools))
    }

    /// Take the connection live, but only if the slot is still the one we
    /// started. Anything else means a disconnect, a delete or another connect
    /// happened while we were dialling, and this transport is already stale.
    fn commit(
        self: &Arc<Self>,
        server: &McpServerRow,
        generation: u64,
        transport: Box<dyn McpTransport>,
        tools: Vec<protocol::McpToolInfo>,
    ) -> Result<(), String> {
        let mut servers = self.servers();
        let still_ours = servers
            .get(&server.id)
            .is_some_and(|s| matches!(s, Slot::Connecting { .. }) && s.generation() == generation);
        if !still_ours {
            drop(servers);
            // Not an error the user needs to see — their more recent
            // instruction is the one being honoured.
            tracing::debug!(
                server_id = %server.id,
                generation,
                "discarding an MCP connection that was superseded while it was being established"
            );
            let mut transport = transport;
            tokio::spawn(async move { transport.shutdown().await });
            return Err("this MCP connection was superseded before it finished".into());
        }

        #[cfg(test)]
        let handle = actor::spawn_with_deadline(
            server.id.clone(),
            generation,
            transport,
            Arc::clone(self) as Arc<dyn ActorObituary>,
            self.test_deadline
                .get()
                .copied()
                .unwrap_or(std::time::Duration::from_secs(90)),
        );
        #[cfg(not(test))]
        let handle = actor::spawn(
            server.id.clone(),
            generation,
            transport,
            Arc::clone(self) as Arc<dyn ActorObituary>,
        );
        servers.insert(server.id.clone(), Slot::Connected { generation, handle });

        // Rebuilt from the published list under the same lock, so two servers
        // registering at once cannot both decide a qualified name is free.
        let mut all: Vec<McpToolDef> = self
            .tools()
            .iter()
            .filter(|t| t.server_id != server.id)
            .cloned()
            .collect();
        for tool in tools {
            let mut qualified_name = build_qualified_name(&server.name, &tool.name);
            // sanitize_name folds distinct names onto the same string (e.g.
            // "foo-bar" and "foo_bar"). If another server already owns this
            // qualified name, disambiguate with the server id so a call can't
            // route to the wrong server.
            if all.iter().any(|t| t.qualified_name == qualified_name) {
                qualified_name = format!("{qualified_name}__{}", sanitize_name(&server.id));
                tracing::warn!("MCP tool name collision on '{qualified_name}'; disambiguated by server id");
            }
            all.push(McpToolDef {
                server_id: server.id.clone(),
                server_name: server.name.clone(),
                name: tool.name.clone(),
                qualified_name,
                description: tool.description.unwrap_or_default(),
                input_schema: tool.input_schema.unwrap_or(serde_json::json!({"type": "object"})),
            });
        }
        let tool_count = all.iter().filter(|t| t.server_id == server.id).count();
        self.publish(all);
        drop(servers);

        // Connecting is a user-initiated, low-frequency state change, and the
        // tool count is what distinguishes "connected" from "connected but
        // useless".
        tracing::info!(
            server_id = %server.id,
            server_name = %server.name,
            transport = %server.transport_type,
            tool_count,
            "MCP server connected"
        );
        Ok(())
    }

    /// Drop a `Connecting` slot that failed, unless it has already been
    /// replaced by a newer attempt.
    fn abandon(&self, server_id: &str, generation: u64) {
        let mut servers = self.servers();
        if servers.get(server_id).is_some_and(|s| s.generation() == generation) {
            servers.remove(server_id);
        }
    }

    /// Stop talking to a server. Removing the entry and shutting the actor down
    /// are separate steps: the first must be immediate and under the lock, the
    /// second waits on I/O and must not be.
    pub async fn disconnect(&self, server_id: &str) {
        let handle = {
            let mut servers = self.servers();
            let previous = servers.remove(server_id);
            // In the same critical section as the removal. Done afterwards it
            // races a reconnect: the new connection publishes, then this
            // deletes what it published.
            self.forget_tools_locked(&servers, server_id);
            // A connect still dialling has no actor to stop; cancelling it is
            // what keeps a slow `npx` download from coming back to life later.
            match previous {
                Some(Slot::Connecting { cancel, .. }) => {
                    cancel.cancel();
                    None
                }
                Some(Slot::Connected { handle, .. }) => Some(handle),
                None => None,
            }
        };
        if let Some(handle) = handle {
            self.retire(server_id, handle).await;
        }
        tracing::info!(server_id = %server_id, "MCP server disconnected");
    }

    /// Stop an actor and wait until its transport is really gone. Called with
    /// no lock held.
    ///
    /// Dropping the handle would not do: a call in flight holds a clone, so the
    /// channel stays open, and the task is parked on the request anyway. It has
    /// to be told.
    async fn retire(&self, server_id: &str, handle: ActorHandle) {
        handle.stop().await;
        tracing::debug!(server_id = %server_id, "MCP actor stopped");
    }

    /// Call a tool by its qualified name.
    ///
    /// The connection lock is held only long enough to find the handle. The
    /// request itself is awaited with nothing locked, which is the whole point
    /// of the rewrite: a server taking thirty seconds no longer stops every
    /// other conversation from assembling its tools.
    pub async fn call_tool(&self, qualified_name: &str, args: serde_json::Value) -> Result<String, String> {
        let (server_id, original_name) = {
            let tools = self.tools();
            let tool_def = tools
                .iter()
                .find(|t| t.qualified_name == qualified_name)
                .ok_or_else(|| format!("MCP tool not found: {qualified_name}"))?;
            (tool_def.server_id.clone(), tool_def.name.clone())
        };

        let handle = {
            let servers = self.servers();
            match servers.get(&server_id) {
                Some(Slot::Connected { handle, .. }) => handle.clone(),
                _ => {
                    // The tool list and the connection map disagree — a bug
                    // rather than a configuration problem, and the model just
                    // sees "MCP error".
                    tracing::error!(
                        server_id = %server_id,
                        qualified_name,
                        "MCP tool is listed but its server has no live connection"
                    );
                    return Err("MCP server not connected".into());
                }
            }
        };

        let result = handle
            .request(
                "tools/call",
                Some(serde_json::json!({
                    "name": original_name,
                    "arguments": args,
                })),
            )
            .await
            .map_err(|e| {
                tracing::warn!(
                    server_id = %server_id,
                    tool = %original_name,
                    fatal = e.is_fatal(),
                    error = %e,
                    "MCP tool call failed"
                );
                e.to_string()
            })?;

        let call_result: McpCallToolResult = serde_json::from_value(result).map_err(|e| {
            tracing::warn!(
                server_id = %server_id,
                tool = %original_name,
                error = %e,
                "MCP tool result did not match the expected shape"
            );
            format!("parse tools/call result: {e}")
        })?;

        let text = call_result
            .content
            .iter()
            .filter(|c| c.content_type == "text")
            .filter_map(|c| c.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n");

        if call_result.is_error {
            // Length only: the text is tool output and goes to the model, not
            // into a file the user may export.
            tracing::warn!(
                server_id = %server_id,
                tool = %original_name,
                result_len = text.len(),
                "MCP tool reported an error"
            );
            Err(text)
        } else {
            Ok(if text.is_empty() {
                "(no output)".to_string()
            } else {
                text
            })
        }
    }

    /// Close every connection. Servers are shut down concurrently and under a
    /// total deadline: a handful of HTTP transports, each allowed its own five
    /// second DELETE, would otherwise hold up the whole exit.
    pub async fn shutdown_all(&self, budget: std::time::Duration) {
        /// A connection on its way out, at whichever stage it had reached.
        enum Closing {
            Live(String, ActorHandle),
            /// Still dialling. Cancelling is not the same as being finished
            /// with it: the task has to be scheduled before it can notice, and
            /// it may already hold a child process it has not dropped yet. Its
            /// result arriving is the signal that it has.
            Dialling(String, watch::Receiver<ConnectResult>),
        }

        let pending: Vec<Closing> = {
            let mut servers = self.servers();
            let taken = servers
                .drain()
                .map(|(id, slot)| match slot {
                    Slot::Connected { handle, .. } => Closing::Live(id, handle),
                    Slot::Connecting { cancel, done, .. } => {
                        cancel.cancel();
                        Closing::Dialling(id, done)
                    }
                })
                .collect();
            // Emptied here, with the map, rather than after the waiting below:
            // nothing is connected any more the moment the entries are gone,
            // and a late publish would race an obituary from one of them.
            self.publish(Vec::new());
            taken
        };
        if pending.is_empty() {
            return;
        }
        let count = pending.len();
        let closing = futures::future::join_all(pending.into_iter().map(|entry| async move {
            match entry {
                Closing::Live(id, handle) => {
                    handle.stop().await;
                    tracing::debug!(server_id = %id, "MCP server closed for shutdown");
                }
                Closing::Dialling(id, mut done) => {
                    // A sender that has simply gone means the same thing: the
                    // task that owned the transport is no longer running.
                    while done.borrow().is_none() {
                        if done.changed().await.is_err() {
                            break;
                        }
                    }
                    tracing::debug!(server_id = %id, "MCP connection attempt abandoned for shutdown");
                }
            }
        }));
        if tokio::time::timeout(budget, closing).await.is_err() {
            tracing::warn!(server_count = count, "MCP shutdown exceeded its budget; exiting anyway");
        }
    }
}

impl ActorObituary for McpRegistry {
    /// An actor stopped on its own — a dead transport, an EOF, or a deadline.
    ///
    /// Generation-checked: by the time this runs the server may already have
    /// been reconnected, and removing the entry then would take down a working
    /// connection to report a failure that has already been superseded.
    fn actor_stopped(&self, server_id: &str, generation: u64) {
        let was_current = {
            let mut servers = self.servers();
            match servers.get(server_id) {
                Some(slot) if slot.generation() == generation => {
                    servers.remove(server_id);
                    // Inside the same critical section as the check. Released
                    // in between, this notice could outlive its own generation:
                    // a reconnect publishes its tools, and then a death from
                    // the connection it replaced deletes them.
                    self.forget_tools_locked(&servers, server_id);
                    true
                }
                _ => false,
            }
        };
        if was_current {
            tracing::info!(
                server_id = %server_id,
                generation,
                "MCP connection dropped; its tools are no longer offered"
            );
        }
    }
}

/// Parse one JSON-encoded config field. Absence means an empty collection;
/// malformed persisted JSON is a configuration error.
///
/// The value is never logged: `env` and `headers` are exactly where tokens
/// live. serde's message carries a position, not the contents.
fn parse_config_field<T: Default + serde::de::DeserializeOwned>(
    raw: Option<&str>,
    field: &'static str,
) -> Result<T, String> {
    let Some(raw) = raw.filter(|s| !s.trim().is_empty()) else {
        return Ok(T::default());
    };
    serde_json::from_str(raw)
        .map_err(|error| format!("invalid MCP {field} JSON at {}:{}", error.line(), error.column()))
}

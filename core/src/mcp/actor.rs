//! One task per MCP server, owning its transport.
//!
//! The alternative — a mutex around the transport — cannot be made correct
//! here. `AsyncBufReadExt::read_line` is not cancel-safe, so a caller timing
//! out mid-read drops a future that has already pulled part of a line out of
//! the `BufReader`; that half line is gone and every later response is off by
//! one. Writes have the same problem in the other direction: a cancelled
//! `write_all` leaves a partial JSON-RPC frame in the pipe, the server answers
//! with a parse error carrying `id: null`, the id filter discards it, and the
//! call spins until it times out. The transport is dead at that point but
//! nothing knows it.
//!
//! Giving the transport its own task means a caller's timeout cancels the wait
//! for a reply, never the I/O. Frames stay whole. What that costs is the
//! ability to walk away from a server that has stopped answering — so the task
//! keeps a deadline of its own, and when it expires the whole actor stops and
//! the transport is declared dead. Recovery is a new transport, never a
//! second attempt on a suspect one.

use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::{McpTransport, TransportError};

/// How long the actor itself will wait for one request before giving up on the
/// transport. Deliberately longer than the per-transport read timeouts, which
/// are the normal way a slow call ends; reaching this one means the transport
/// stopped honouring even those.
#[cfg_attr(test, allow(dead_code))] // tests connect via spawn_with_deadline
const ACTOR_REQUEST_DEADLINE: std::time::Duration = std::time::Duration::from_secs(90);

/// Requests allowed to queue before callers are turned away.
///
/// Bounded on purpose. A server that has stopped answering holds the actor on
/// one call, and without a ceiling every later call would queue behind it and
/// wait out the deadline for nothing.
const REQUEST_QUEUE: usize = 32;

#[derive(Debug)]
pub enum McpError {
    /// The server answered and the answer was a refusal. The transport is fine.
    Protocol(String),
    /// The transport is unusable and its actor has stopped. Whoever owns the
    /// registry entry should drop it; retrying on this handle cannot work.
    Fatal(String),
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpError::Protocol(m) | McpError::Fatal(m) => write!(f, "{m}"),
        }
    }
}

impl McpError {
    pub fn is_fatal(&self) -> bool {
        matches!(self, McpError::Fatal(_))
    }
}

struct Call {
    method: String,
    params: Option<serde_json::Value>,
    reply: oneshot::Sender<Result<serde_json::Value, McpError>>,
}

/// Cheap to clone; every clone talks to the same task.
///
/// Dropping every clone is *not* how an actor is stopped. A call in flight
/// holds a clone of its own, so the channel closing cannot be relied on — and
/// even if it were closed, the task is parked on `transport.request()` and
/// would not notice until the server answered. Stopping is explicit.
#[derive(Clone)]
pub struct ActorHandle {
    tx: mpsc::Sender<Call>,
    stop: CancellationToken,
    /// Flipped once the task has finished, transport shutdown included.
    finished: watch::Receiver<bool>,
}

impl ActorHandle {
    pub async fn request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, McpError> {
        let (reply, rx) = oneshot::channel();
        let call = Call {
            method: method.to_string(),
            params,
            reply,
        };

        // `try_send`, not `send`: waiting for room in the queue is waiting on
        // the very server that has stopped answering.
        self.tx.try_send(call).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => {
                McpError::Protocol(format!("MCP server is busy; '{method}' was not sent"))
            }
            mpsc::error::TrySendError::Closed(_) => McpError::Fatal("MCP server connection has closed".into()),
        })?;

        // The sender is dropped without a value only when the actor stopped
        // mid-call, which it does exactly when it has declared the transport
        // dead.
        rx.await
            .unwrap_or_else(|_| Err(McpError::Fatal("MCP server connection was lost".into())))
    }

    /// Stop the actor and wait until it has finished, transport shutdown
    /// included.
    ///
    /// Cancelling abandons whatever request is in flight. That is safe here and
    /// nowhere else: the transport is destroyed immediately afterwards, so the
    /// half-written frame this might leave behind has nothing left to confuse.
    pub async fn stop(&self) {
        self.stop.cancel();
        let mut finished = self.finished.clone();
        // `changed` returns Err once the sender is gone, which also means done.
        while !*finished.borrow() {
            if finished.changed().await.is_err() {
                break;
            }
        }
    }
}

/// What the actor reports to when it stops.
///
/// Carries the generation it was started under so the registry can ignore a
/// death notice that arrives after the entry has already been replaced — the
/// server may well have been reconnected in the meantime, and taking that
/// entry down would be worse than the failure being reported.
pub trait ActorObituary: Send + Sync + 'static {
    fn actor_stopped(&self, server_id: &str, generation: u64);
}

/// Start the task and hand back the only way to talk to it.
#[cfg_attr(test, allow(dead_code))] // the test build calls spawn_with_deadline directly
pub fn spawn(
    server_id: String,
    generation: u64,
    transport: Box<dyn McpTransport>,
    obituary: Arc<dyn ActorObituary>,
) -> ActorHandle {
    spawn_with_deadline(server_id, generation, transport, obituary, ACTOR_REQUEST_DEADLINE)
}

/// Same, with the deadline supplied. Only the tests need this — waiting out the
/// real one would make them useless.
pub fn spawn_with_deadline(
    server_id: String,
    generation: u64,
    transport: Box<dyn McpTransport>,
    obituary: Arc<dyn ActorObituary>,
    deadline: std::time::Duration,
) -> ActorHandle {
    let (tx, rx) = mpsc::channel(REQUEST_QUEUE);
    let stop = CancellationToken::new();
    let (finished_tx, finished) = watch::channel(false);
    tokio::spawn(run(
        server_id,
        generation,
        transport,
        rx,
        obituary,
        deadline,
        stop.clone(),
        finished_tx,
    ));
    ActorHandle { tx, stop, finished }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    server_id: String,
    generation: u64,
    mut transport: Box<dyn McpTransport>,
    mut rx: mpsc::Receiver<Call>,
    obituary: Arc<dyn ActorObituary>,
    deadline: std::time::Duration,
    stop: CancellationToken,
    finished: watch::Sender<bool>,
) {
    // Set when the transport can no longer be trusted, as opposed to the actor
    // simply being asked to stop.
    let mut died: Option<String> = None;

    loop {
        let call = tokio::select! {
            biased;
            // Checked first: once asked to stop there is no point starting
            // another request, however many are queued.
            _ = stop.cancelled() => break,
            call = rx.recv() => match call {
                Some(c) => c,
                None => break,
            },
        };

        let Call { method, params, reply } = call;
        let outcome = tokio::select! {
            biased;
            // Abandons the request mid-flight. Safe only because the transport
            // is destroyed a few lines below and never used again.
            _ = stop.cancelled() => {
                let _ = reply.send(Err(McpError::Fatal(
                    "MCP server connection is closing".into(),
                )));
                break;
            }
            r = tokio::time::timeout(deadline, transport.request(&method, params)) => r,
        };

        match outcome {
            Ok(Ok(value)) => {
                // A caller that has given up leaves nobody to receive this. The
                // transport is fine either way, so carry on.
                let _ = reply.send(Ok(value));
            }
            // The server answered and said no. Framing is intact, so the
            // connection lives on.
            Ok(Err(TransportError::Rpc(e))) => {
                let _ = reply.send(Err(McpError::Protocol(e)));
            }
            // The transport failed. Carrying on would mean reading later
            // replies against the wrong requests — the desynchronisation this
            // whole design exists to prevent.
            Ok(Err(TransportError::Broken(e))) => {
                tracing::warn!(
                    server_id = %server_id,
                    method = %method,
                    error = %e,
                    "MCP transport failed; the connection will not be reused"
                );
                let _ = reply.send(Err(McpError::Fatal(e.clone())));
                died = Some(e);
                break;
            }
            Err(_) => {
                // Same conclusion by a different route: part of a frame may
                // already be on the wire and there is no way to find the
                // boundary again from here.
                let message = format!(
                    "MCP server stopped responding; '{method}' exceeded {}s",
                    deadline.as_secs()
                );
                tracing::warn!(
                    server_id = %server_id,
                    method = %method,
                    deadline_secs = deadline.as_secs(),
                    "MCP transport declared dead after exceeding the actor deadline"
                );
                let _ = reply.send(Err(McpError::Fatal(message.clone())));
                died = Some(message);
                break;
            }
        }
    }

    // Refuse anything already queued, rather than leaving those callers to wait
    // out their own timeouts on a transport that is going away.
    rx.close();
    while let Some(call) = rx.recv().await {
        let _ = call.reply.send(Err(McpError::Fatal(
            died.clone()
                .unwrap_or_else(|| "MCP server connection has closed".into()),
        )));
    }

    transport.shutdown().await;
    // Only now is the child process actually gone, which is what `stop` waits
    // for and what makes the shutdown budget mean something.
    let _ = finished.send(true);
    obituary.actor_stopped(&server_id, generation);
}

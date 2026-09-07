//! The invariants the actor rewrite exists to establish.
//!
//! Every one of these was either false before it, or true only by accident of
//! there being a single global lock.

use super::*;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

/// How a mock server behaves when a tool call arrives.
#[derive(Clone)]
enum Behaviour {
    Reply(Duration),
    /// Never answers. Used to show the actor deadline is what ends the call.
    Hang,
    /// Answers with a JSON-RPC error. The stream is still in step.
    Refuse(&'static str),
    /// The pipe broke — what a real transport reports for a closed stdout or an
    /// I/O error, and what must not be retried on.
    Break(&'static str),
}

/// Shared counters, so a test can inspect a transport the registry has taken
/// ownership of.
struct MockSpec {
    tools: Vec<&'static str>,
    behaviour: Behaviour,
    handshake_delay: Duration,
    calls: Arc<AtomicUsize>,
    shutdowns: Arc<AtomicUsize>,
    /// Separate from `shutdowns`: a connect that is abandoned mid-handshake
    /// never reaches shutdown, it is simply dropped — and dropping is what
    /// `kill_on_drop` hangs off, so it is the thing worth observing.
    drops: Arc<AtomicUsize>,
    in_flight: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
}

impl MockSpec {
    fn new(tools: Vec<&'static str>) -> Self {
        Self {
            tools,
            behaviour: Behaviour::Reply(Duration::ZERO),
            handshake_delay: Duration::ZERO,
            calls: Arc::new(AtomicUsize::new(0)),
            shutdowns: Arc::new(AtomicUsize::new(0)),
            drops: Arc::new(AtomicUsize::new(0)),
            in_flight: Arc::new(AtomicUsize::new(0)),
            max_in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn behaviour(mut self, b: Behaviour) -> Self {
        self.behaviour = b;
        self
    }

    /// Makes the connect sequence actually yield. A mock that answers the
    /// handshake without ever awaiting runs to completion on its first poll,
    /// which leaves no window for a concurrent connect to observe — the very
    /// thing some of these tests are about.
    fn slow_handshake(mut self, d: Duration) -> Self {
        self.handshake_delay = d;
        self
    }

    fn build(&self) -> Box<dyn McpTransport> {
        Box::new(MockTransport {
            tools: self.tools.clone(),
            behaviour: self.behaviour.clone(),
            handshake_delay: self.handshake_delay,
            calls: Arc::clone(&self.calls),
            shutdowns: Arc::clone(&self.shutdowns),
            drops: Arc::clone(&self.drops),
            in_flight: Arc::clone(&self.in_flight),
            max_in_flight: Arc::clone(&self.max_in_flight),
        })
    }
}

struct MockTransport {
    tools: Vec<&'static str>,
    behaviour: Behaviour,
    handshake_delay: Duration,
    calls: Arc<AtomicUsize>,
    shutdowns: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    in_flight: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
}

impl Drop for MockTransport {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl McpTransport for MockTransport {
    async fn request(
        &mut self,
        method: &str,
        _params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, TransportError> {
        match method {
            // The handshake succeeds unconditionally; only tool calls follow
            // the configured behaviour.
            "initialize" => {
                if !self.handshake_delay.is_zero() {
                    tokio::time::sleep(self.handshake_delay).await;
                }
                Ok(serde_json::json!({}))
            }
            "tools/list" => Ok(serde_json::json!({
                "tools": self.tools.iter()
                    .map(|n| serde_json::json!({ "name": n }))
                    .collect::<Vec<_>>()
            })),
            _ => {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_in_flight.fetch_max(now, Ordering::SeqCst);
                match self.behaviour {
                    Behaviour::Reply(d) => {
                        if !d.is_zero() {
                            tokio::time::sleep(d).await;
                        }
                    }
                    Behaviour::Hang => std::future::pending::<()>().await,
                    Behaviour::Refuse(m) => {
                        self.in_flight.fetch_sub(1, Ordering::SeqCst);
                        return Err(TransportError::Rpc(m.into()));
                    }
                    Behaviour::Break(m) => {
                        self.in_flight.fetch_sub(1, Ordering::SeqCst);
                        return Err(TransportError::Broken(m.into()));
                    }
                }
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok(serde_json::json!({
                    "content": [{ "type": "text", "text": "ok" }]
                }))
            }
        }
    }

    async fn notify(&mut self, _: &str, _: Option<serde_json::Value>) -> Result<(), String> {
        Ok(())
    }

    async fn shutdown(&mut self) {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
    }
}

fn server(id: &str) -> McpServerRow {
    McpServerRow {
        id: id.to_string(),
        name: id.to_string(),
        transport_type: "stdio".into(),
        command: Some("mock".into()),
        args: None,
        env: None,
        url: None,
        is_enabled: 0,
        sort_order: 0,
        created_at: 0,
        updated_at: 0,
        headers: None,
    }
}

fn stage(registry: &Arc<McpRegistry>, transport: Box<dyn McpTransport>) {
    registry.staged_transports.lock().unwrap().push(transport);
}

async fn connected(id: &str, spec: &MockSpec) -> Arc<McpRegistry> {
    let registry = McpRegistry::new();
    stage(&registry, spec.build());
    registry.connect(&server(id)).await.expect("the mock connects");
    registry
}

/// The P0 itself. A call in flight used to hold the one global lock, so every
/// other conversation waited on it merely to read its tool list.
#[tokio::test]
async fn a_call_in_flight_does_not_block_reading_the_tool_list() {
    let slow = MockSpec::new(vec!["slow"]).behaviour(Behaviour::Reply(Duration::from_millis(400)));
    let registry = connected("a", &slow).await;

    let calling = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.call_tool("mcp__a__slow", serde_json::json!({})).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;

    let started = std::time::Instant::now();
    let defs = registry.tool_definitions();
    assert_eq!(defs.len(), 1);
    assert!(
        started.elapsed() < Duration::from_millis(50),
        "reading the tool list waited on the call in flight"
    );
    assert!(calling.await.unwrap().is_ok());
}

#[tokio::test]
async fn one_server_being_slow_does_not_hold_up_another() {
    let slow = MockSpec::new(vec!["slow"]).behaviour(Behaviour::Reply(Duration::from_millis(400)));
    let quick = MockSpec::new(vec!["quick"]);
    let registry = McpRegistry::new();
    stage(&registry, slow.build());
    registry.connect(&server("a")).await.unwrap();
    stage(&registry, quick.build());
    registry.connect(&server("b")).await.unwrap();

    let slow_call = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.call_tool("mcp__a__slow", serde_json::json!({})).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;

    let started = std::time::Instant::now();
    registry
        .call_tool("mcp__b__quick", serde_json::json!({}))
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "the second server waited on the first"
    );
    assert!(slow_call.await.unwrap().is_ok());
}

/// One pipe, one conversation at a time. With no lock around the transport any
/// more, the actor is what enforces this.
#[tokio::test]
async fn calls_to_one_server_are_serialised() {
    let spec = MockSpec::new(vec!["t"]).behaviour(Behaviour::Reply(Duration::from_millis(80)));
    let registry = connected("a", &spec).await;

    let calls: Vec<_> = (0..4)
        .map(|_| {
            let registry = Arc::clone(&registry);
            tokio::spawn(async move { registry.call_tool("mcp__a__t", serde_json::json!({})).await })
        })
        .collect();
    for c in calls {
        c.await.unwrap().unwrap();
    }
    assert_eq!(spec.calls.load(Ordering::SeqCst), 4);
    assert_eq!(
        spec.max_in_flight.load(Ordering::SeqCst),
        1,
        "two requests were on the same transport at once"
    );
}

/// A server that stops answering is not retried on. The actor stops, the entry
/// goes, and its tools stop being offered to the model.
#[tokio::test]
async fn a_server_that_never_answers_is_dropped_along_with_its_tools() {
    let spec = MockSpec::new(vec!["t"]).behaviour(Behaviour::Hang);
    let registry = McpRegistry::new();
    let _ = registry.test_deadline.set(Duration::from_millis(150));
    stage(&registry, spec.build());
    registry.connect(&server("a")).await.unwrap();
    assert_eq!(registry.tool_definitions().len(), 1);

    let err = registry
        .call_tool("mcp__a__t", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(err.contains("stopped responding"), "{err}");

    // The obituary runs on the actor's own task; give it a turn to land.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        registry.tool_definitions().is_empty(),
        "its tools outlived the connection"
    );
    assert!(
        registry.all_connection_statuses().is_empty(),
        "the entry outlived the connection"
    );
    assert_eq!(spec.shutdowns.load(Ordering::SeqCst), 1);
}

/// Two connects for one server share a single attempt. Handled any other way
/// this spawns two processes and leaks whichever one loses the race.
#[tokio::test]
async fn concurrent_connects_share_one_attempt() {
    let spec = MockSpec::new(vec!["t"]).slow_handshake(Duration::from_millis(100));
    let registry = McpRegistry::new();
    // Only one transport is staged. A second dial would fall through to the
    // real stdio path and fail to spawn, which is itself the assertion.
    stage(&registry, spec.build());
    let target = server("a");
    let (a, b) = tokio::join!(registry.connect(&target), registry.connect(&target),);
    assert!(
        a.is_ok() && b.is_ok(),
        "both callers should see the same success: {a:?} {b:?}"
    );
    assert_eq!(registry.all_connection_statuses().len(), 1);
    assert_eq!(registry.tool_definitions().len(), 1);
}

#[tokio::test]
async fn reconnecting_replaces_the_previous_connection() {
    let first = MockSpec::new(vec!["old"]);
    let second = MockSpec::new(vec!["new"]);
    let registry = McpRegistry::new();
    stage(&registry, first.build());
    registry.connect(&server("a")).await.unwrap();
    stage(&registry, second.build());
    registry.connect(&server("a")).await.unwrap();

    // The replaced transport is shut down rather than dropped on the floor,
    // which is what used to leak a child process per reconnect.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        first.shutdowns.load(Ordering::SeqCst),
        1,
        "the old transport was never closed"
    );
    let names: Vec<String> = registry.tool_definitions().iter().map(|d| d.name.clone()).collect();
    assert_eq!(names, vec!["mcp__a__new".to_string()]);
}

#[tokio::test]
async fn disconnecting_removes_the_entry_and_its_tools() {
    let spec = MockSpec::new(vec!["t"]);
    let registry = connected("a", &spec).await;
    registry.disconnect("a").await;

    assert!(registry.all_connection_statuses().is_empty());
    assert!(registry.tool_definitions().is_empty());
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(spec.shutdowns.load(Ordering::SeqCst), 1);
}

/// A death notice from a connection that has since been replaced must not take
/// the replacement down with it.
#[tokio::test]
async fn a_stale_obituary_leaves_the_current_connection_alone() {
    let spec = MockSpec::new(vec!["t"]);
    let registry = connected("a", &spec).await;
    let current = registry.servers().get("a").unwrap().generation();

    registry.actor_stopped("a", current - 1);

    assert_eq!(
        registry.all_connection_statuses().len(),
        1,
        "a stale notice closed a live connection"
    );
    assert_eq!(registry.tool_definitions().len(), 1);
}

/// A refusal is the server talking, not the pipe breaking. The connection has
/// to survive it, or one bad argument would cost the whole server.
#[tokio::test]
async fn a_refused_call_leaves_the_connection_up() {
    let spec = MockSpec::new(vec!["t"]).behaviour(Behaviour::Refuse("MCP error -32602: bad params"));
    let registry = connected("a", &spec).await;

    let err = registry
        .call_tool("mcp__a__t", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(err.contains("bad params"), "{err}");

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        registry.all_connection_statuses().len(),
        1,
        "a refusal closed the connection"
    );
    assert_eq!(registry.tool_definitions().len(), 1);
    assert_eq!(spec.shutdowns.load(Ordering::SeqCst), 0);
}

/// The failure the whole design is for. A real stdio transport reports this for
/// a closed pipe or an interrupted frame, and reusing it would mean reading
/// later replies against the wrong requests.
#[tokio::test]
async fn a_broken_transport_is_never_reused() {
    let spec = MockSpec::new(vec!["t"]).behaviour(Behaviour::Break("MCP server closed stdout"));
    let registry = connected("a", &spec).await;

    let err = registry
        .call_tool("mcp__a__t", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(err.contains("closed stdout"), "{err}");

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        registry.all_connection_statuses().is_empty(),
        "a broken transport was left connected and would be used again"
    );
    assert!(
        registry.tool_definitions().is_empty(),
        "its tools outlived the transport"
    );
    assert_eq!(spec.shutdowns.load(Ordering::SeqCst), 1);
}

/// Disconnecting has to end a call that is in flight, not wait politely behind
/// it. Dropping handles cannot do that: the caller holds one of its own, and
/// the task is parked on the request regardless.
#[tokio::test]
async fn disconnecting_stops_a_server_that_is_mid_call() {
    let spec = MockSpec::new(vec!["t"]).behaviour(Behaviour::Hang);
    let registry = connected("a", &spec).await;

    let call = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.call_tool("mcp__a__t", serde_json::json!({})).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;

    let started = std::time::Instant::now();
    registry.disconnect("a").await;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "disconnect waited on the call in flight"
    );
    // The point of waiting: by the time disconnect returns the child is really
    // gone, not merely scheduled to go.
    assert_eq!(
        spec.shutdowns.load(Ordering::SeqCst),
        1,
        "the transport was never shut down"
    );
    assert!(registry.all_connection_statuses().is_empty());

    let outcome = call.await.unwrap();
    assert!(
        outcome.is_err(),
        "the in-flight call should have been told the connection went away"
    );
}

/// Same requirement at exit, where the budget only means something if it is
/// actually waiting for the transports to close.
#[tokio::test]
async fn shutdown_closes_a_server_that_is_mid_call_within_its_budget() {
    let spec = MockSpec::new(vec!["t"]).behaviour(Behaviour::Hang);
    let registry = connected("a", &spec).await;

    let _call = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.call_tool("mcp__a__t", serde_json::json!({})).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;

    let started = std::time::Instant::now();
    registry.shutdown_all(Duration::from_secs(2)).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "shutdown ran out its budget"
    );
    assert_eq!(
        spec.shutdowns.load(Ordering::SeqCst),
        1,
        "exit returned before the child process was gone"
    );
    assert!(registry.all_connection_statuses().is_empty());
}

/// The generation check has to cover the tool list, not just the slot.
///
/// This asserts the outcome, not the race. The race — a notice that passes its
/// check, releases the lock, and deletes the replacement's tools on the way out
/// — cannot be reached through the public API any more, because the check and
/// the removal happen in one critical section. That is the fix: a timing
/// problem turned into a structural one. What is left to test is that the two
/// really do agree.
#[tokio::test]
async fn a_stale_obituary_does_not_delete_a_newer_connections_tools() {
    let first = MockSpec::new(vec!["old"]);
    let second = MockSpec::new(vec!["new"]);
    let registry = McpRegistry::new();
    stage(&registry, first.build());
    registry.connect(&server("a")).await.unwrap();
    let stale = registry.servers().get("a").unwrap().generation();

    stage(&registry, second.build());
    registry.connect(&server("a")).await.unwrap();

    // Exactly the ordering the lock is there to prevent: the old connection's
    // notice lands after the new one is live.
    registry.actor_stopped("a", stale);

    let names: Vec<String> = registry.tool_definitions().iter().map(|d| d.name.clone()).collect();
    assert_eq!(
        names,
        vec!["mcp__a__new".to_string()],
        "a stale obituary removed the tools of the connection that replaced it"
    );
    assert_eq!(registry.all_connection_statuses().len(), 1);
}

/// Reconnecting takes the old tools down as it goes. If the new attempt then
/// fails there is nothing to fall back to, so the list has to be empty rather
/// than still advertising the connection that was just stopped.
#[tokio::test]
async fn a_failed_reconnect_leaves_no_tools_behind() {
    let first = MockSpec::new(vec!["old"]);
    let registry = McpRegistry::new();
    stage(&registry, first.build());
    registry.connect(&server("a")).await.unwrap();
    assert_eq!(registry.tool_definitions().len(), 1);

    // Nothing staged this time, so the dial falls through to the real stdio
    // path and fails to spawn.
    let err = registry.connect(&server("a")).await.unwrap_err();
    assert!(err.contains("spawn"), "{err}");

    assert!(
        registry.tool_definitions().is_empty(),
        "the replaced connection's tools are still being offered"
    );
    assert!(registry.all_connection_statuses().is_empty());
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(first.shutdowns.load(Ordering::SeqCst), 1);
}

/// Exit must not outrun a connect that is still dialling.
///
/// Cancelling it only asks; the task still has to be scheduled to notice, and
/// by then it may already hold a spawned child. Returning before that happens
/// hands control back to `handle.exit`, and `std::process::exit` runs no
/// destructors — so `kill_on_drop` never fires and the half-connected server
/// is left behind.
#[tokio::test]
async fn shutdown_waits_for_a_connect_that_is_still_dialling() {
    let spec = MockSpec::new(vec!["t"]).slow_handshake(Duration::from_secs(30));
    let registry = McpRegistry::new();
    stage(&registry, spec.build());

    let dialling = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.connect(&server("a")).await })
    };
    // Long enough for the handshake to be under way and the transport to exist.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        spec.drops.load(Ordering::SeqCst),
        0,
        "the transport should still be alive"
    );

    let started = std::time::Instant::now();
    registry.shutdown_all(Duration::from_secs(2)).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "shutdown ran out its budget"
    );

    // The point: by the time shutdown returns the dialling task has let go of
    // its transport, so the drop that kills the child has actually happened.
    assert_eq!(
        spec.drops.load(Ordering::SeqCst),
        1,
        "exit returned while a connection attempt still held its child process"
    );
    assert!(registry.all_connection_statuses().is_empty());
    assert!(
        dialling.await.unwrap().is_err(),
        "the cancelled connect should report failure"
    );
}

#[tokio::test]
async fn shutting_everything_down_closes_each_server() {
    let a = MockSpec::new(vec!["t"]);
    let b = MockSpec::new(vec!["t"]);
    let registry = McpRegistry::new();
    stage(&registry, a.build());
    registry.connect(&server("a")).await.unwrap();
    stage(&registry, b.build());
    registry.connect(&server("b")).await.unwrap();

    registry.shutdown_all(Duration::from_secs(1)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(registry.all_connection_statuses().is_empty());
    assert!(registry.tool_definitions().is_empty());
    assert_eq!(a.shutdowns.load(Ordering::SeqCst), 1);
    assert_eq!(b.shutdowns.load(Ordering::SeqCst), 1);
}

//! A JSON-RPC peer over the adapter's stdio.
//!
//! Four tasks, and the split between them is the whole design:
//!
//! ```text
//!   writer     owns stdin, serialises every outbound line
//!   reader     owns stdout, parses and routes; never awaits anything slow
//!   notifier   handles notifications one at a time, in arrival order
//!   supervisor owns the child, kills it on stop, notices if it dies first
//! ```
//!
//! **The reader must never await the handler.** An inbound
//! `session/request_permission` is answered by a person, minutes later; handled
//! inline it would stall the same pipe carrying the answer's own
//! `session/update` stream, and the session would look frozen for as long as
//! the card was on screen. So requests are spawned and only their *reply* comes
//! back through the writer.
//!
//! **Notifications go through a queue rather than being spawned**, because
//! spawning loses their order and they are text chunks: a paragraph reassembled
//! out of order is worse than a slow one.
//!
//! Cancelling a wait is safe here in a way it is not in [`crate::mcp`]. There
//! the reader and the caller were the same task, so abandoning a read left half
//! a line consumed and every later reply landing on the wrong request. Here one
//! task owns the stream from end to end and a caller giving up only drops its
//! own slot out of the pending table.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::process::{AdapterProcess, StderrTail};
use super::protocol::{Frame, Incoming, Notification, Request};

/// Outbound lines allowed to queue. Small: everything written here is a control
/// message or one prompt, and a backlog means the adapter has stopped reading.
const WRITE_QUEUE: usize = 64;
/// Inbound notifications allowed to queue while one is being handled. Generous:
/// a fast turn produces hundreds of text chunks, and dropping any of them
/// silently truncates an answer.
const NOTIFY_QUEUE: usize = 1024;

#[derive(Debug, Clone)]
pub enum PeerError {
    /// The agent answered, and the answer was a refusal. The pipe is fine.
    Rpc(String),
    /// The pipe is not usable. Every later call fails the same way; recovery is
    /// a new adapter, never a retry on this one.
    Dead(String),
}

impl std::fmt::Display for PeerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerError::Rpc(m) | PeerError::Dead(m) => write!(f, "{m}"),
        }
    }
}

impl PeerError {
    pub fn is_dead(&self) -> bool {
        matches!(self, PeerError::Dead(_))
    }
}

/// What the agent asks of us, and what it tells us.
///
/// Both are `async` because both write to the database. `notification` is
/// awaited in arrival order by one task, so a slow one delays its successors —
/// which is the point. `request` is spawned, so a slow one delays nothing.
#[async_trait::async_trait]
pub trait Handler: Send + Sync + 'static {
    async fn notification(&self, method: String, params: serde_json::Value);

    async fn request(&self, method: String, params: serde_json::Value) -> Result<serde_json::Value, String>;
}

/// What reaches the notifier task, in arrival order.
enum Inbound {
    Notification(String, serde_json::Value),
    /// A marker somebody pushed to find out when everything queued ahead of it
    /// has been handled. See [`Peer::drain_notifications`].
    Barrier(oneshot::Sender<()>),
}

/// What one request resolves to.
///
/// Carries a [`PeerError`] rather than a string, and that is load-bearing: the
/// two ways a call ends badly are *the agent said no* and *the pipe is gone*,
/// and only the sender knows which. Passing a bare message meant the receiver
/// had to guess, and it guessed `Rpc` — so a caller whose adapter had died was
/// told its request had been refused, which reads as a recoverable answer about
/// the request rather than the end of the session.
type Answer = Result<serde_json::Value, PeerError>;

/// Callers parked on a reply, by the id we sent.
type Waiting = HashMap<u64, oneshot::Sender<Answer>>;

/// Callers waiting on a reply, keyed by the id we sent.
#[derive(Clone, Default)]
struct Pending(Arc<Mutex<Waiting>>);

impl Pending {
    fn insert(&self, id: u64, tx: oneshot::Sender<Answer>) {
        self.lock().insert(id, tx);
    }

    fn take(&self, id: u64) -> Option<oneshot::Sender<Answer>> {
        self.lock().remove(&id)
    }

    /// Refuse everyone still waiting, because the adapter is gone. Without this
    /// a dead adapter leaves every caller parked for ever — the failure the
    /// user sees is a session that never answers rather than one that reports a
    /// problem.
    ///
    /// Always `Dead`: this is only ever called when the pipe has ended, and the
    /// caller's next move depends on knowing that.
    fn drain_with(&self, reason: &str) {
        let waiting: Vec<_> = self.lock().drain().map(|(_, tx)| tx).collect();
        for tx in waiting {
            let _ = tx.send(Err(PeerError::Dead(reason.to_string())));
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Waiting> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// First cause of death wins, and it is what every later caller is told.
///
/// Both the reader (stdout closed) and the supervisor (process exited) reach
/// here for the same underlying event, and either may be first. Keeping the
/// first is not about picking the better wording — it is about the second one
/// being a *consequence* of the first, so overwriting would replace the cause
/// with its own effect.
///
/// In practice the reader almost always wins, because a pipe closes before the
/// process it belonged to is reaped. So "the adapter closed its output" is the
/// message normally seen and "exited with code N" is the rarer, better one. Do
/// not build anything on which arrives — a test that asserted the exit code was
/// asserting the outcome of that race.
#[derive(Clone, Default)]
struct Death(Arc<Mutex<Option<String>>>);

impl Death {
    fn set(&self, reason: String) -> bool {
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return false;
        }
        *slot = Some(reason);
        true
    }

    fn get(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// The adapter has stopped accepting input.
///
/// **The writer declares this itself** rather than stopping and leaving it to
/// the reader. Leaving it was reasonable while the only failure was the pipe as
/// a whole going, where the reader is a moment behind and has the better
/// wording — and wrong in the case that matters: an adapter that closes its
/// stdin and keeps running. Its stdout never ends, so the reader reports
/// nothing, ever, and every request already parked on a reply waits for the
/// life of the process while holding that conversation's turn lease.
///
/// [`Death`] keeps the first cause, so the reader's better wording still wins
/// whenever it does arrive — including in the message the waiters are given,
/// which is read back out rather than assumed to be this one.
///
/// Cancelling `stop` is what everything downstream keys off: the supervisor
/// reaps the child, `is_alive` starts answering no, and the session is reopened
/// rather than reused.
///
/// A free function so the failure can be tested without arranging for a child
/// process to close its stdin at exactly the right moment — which is the whole
/// difficulty of the case this exists for.
fn stdin_closed(death: &Death, pending: &Pending, stop: &CancellationToken) {
    let reason = "the adapter stopped accepting input".to_string();
    if death.set(reason.clone()) {
        tracing::warn!("{reason}");
    }
    pending.drain_with(&death.get().unwrap_or(reason));
    stop.cancel();
}

pub struct Peer {
    writes: mpsc::Sender<String>,
    /// Kept so a caller can put a barrier through the same queue the updates
    /// travel in — the only way to know they have all been handled.
    notify: mpsc::Sender<Inbound>,
    pending: Pending,
    death: Death,
    next_id: AtomicU64,
    stop: CancellationToken,
    finished: watch::Receiver<bool>,
    stderr: StderrTail,
    /// Updates the reader could not queue. Never resets: a turn that lost one
    /// has an incomplete transcript, and the count is how the session finds out.
    dropped: Arc<AtomicU64>,
}

impl Peer {
    /// Take a running adapter apart and start talking to it.
    pub fn start(process: AdapterProcess, handler: Arc<dyn Handler>) -> Arc<Self> {
        let stderr = process.stderr_handle();
        let AdapterProcess {
            child,
            mut stdin,
            mut stdout,
            ..
        } = process;

        let (writes_tx, mut writes_rx) = mpsc::channel::<String>(WRITE_QUEUE);
        let (notify_tx, mut notify_rx) = mpsc::channel::<Inbound>(NOTIFY_QUEUE);
        let (finished_tx, finished) = watch::channel(false);
        let stop = CancellationToken::new();
        let pending = Pending::default();
        let death = Death::default();
        let dropped = Arc::new(AtomicU64::new(0));

        let peer = Arc::new(Self {
            writes: writes_tx.clone(),
            notify: notify_tx.clone(),
            pending: pending.clone(),
            death: death.clone(),
            next_id: AtomicU64::new(1),
            stop: stop.clone(),
            finished,
            stderr: stderr.clone(),
            dropped: dropped.clone(),
        });

        // ------------------------------------------------------------ writer
        {
            let stop = stop.clone();
            let pending = pending.clone();
            let death = death.clone();
            tokio::spawn(async move {
                loop {
                    let line = tokio::select! {
                        biased;
                        _ = stop.cancelled() => break,
                        line = writes_rx.recv() => match line {
                            Some(l) => l,
                            None => break,
                        },
                    };
                    if stdin.write_all(line.as_bytes()).await.is_err()
                        || stdin.write_all(b"\n").await.is_err()
                        || stdin.flush().await.is_err()
                    {
                        stdin_closed(&death, &pending, &stop);
                        break;
                    }
                }
            });
        }

        // ---------------------------------------------------------- notifier
        {
            let handler = handler.clone();
            tokio::spawn(async move {
                // No `stop` branch: draining what has already arrived is
                // cheap, and cutting it off would truncate the tail of the
                // last answer. The channel closing is what ends this.
                while let Some(item) = notify_rx.recv().await {
                    match item {
                        Inbound::Notification(method, params) => handler.notification(method, params).await,
                        // Everything queued before this has now been handled,
                        // because this queue has one consumer and it is here.
                        Inbound::Barrier(done) => {
                            let _ = done.send(());
                        }
                    }
                }
            });
        }

        // ------------------------------------------------------------ reader
        {
            let pending = pending.clone();
            let death = death.clone();
            let stop = stop.clone();
            let writes = writes_tx.clone();
            let handler = handler.clone();
            let stderr = stderr.clone();
            tokio::spawn(async move {
                let mut line = String::new();
                loop {
                    line.clear();
                    let read = tokio::select! {
                        biased;
                        // Abandons a partial read. Safe only because nothing
                        // reads this stream again afterwards.
                        _ = stop.cancelled() => break,
                        n = stdout.read_line(&mut line) => n,
                    };
                    match read {
                        Ok(0) => {
                            death.set(format!("the ACP adapter closed its output{}", stderr.suffix()));
                            break;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            death.set(format!("could not read from the ACP adapter: {e}{}", stderr.suffix()));
                            break;
                        }
                    }

                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let Ok(incoming) = serde_json::from_str::<Incoming>(trimmed) else {
                        // Adapters print to stdout by accident; a line we
                        // cannot parse is not a reason to end a session.
                        tracing::debug!(chars = trimmed.chars().count(), "unparseable line from the ACP adapter");
                        continue;
                    };

                    match incoming.classify() {
                        Frame::Response { id, result } => match pending.take(id) {
                            Some(tx) => {
                                // `Rpc`, always: getting here means a whole
                                // frame was read off a working pipe, whatever
                                // the agent chose to put in it.
                                let _ = tx.send(result.map_err(PeerError::Rpc));
                            }
                            // A caller that gave up, or a duplicate. Neither is
                            // fatal.
                            None => tracing::debug!(id, "reply with nobody waiting for it"),
                        },
                        Frame::Request { id, method, params } => {
                            // Spawned, never awaited here: this is how a
                            // permission question waits for a person without
                            // stopping the notifications behind it.
                            //
                            // But it must not overtake the notifications that
                            // came *before* it, and being spawned is exactly
                            // how it would. `session/request_permission`
                            // follows the `tool_call` announcing the same call,
                            // and the card the question attaches to is created
                            // by that notification — so a question that arrives
                            // first finds no card, attaches to nothing, and is
                            // silently unanswerable in the one conversation the
                            // toast queue deliberately does not cover: the open
                            // one. The turn then waits for an answer the user
                            // is never offered.
                            //
                            // A barrier queued here and awaited in the task
                            // orders it behind everything already queued,
                            // without holding up anything queued after it.
                            let (ordered, in_order) = oneshot::channel();
                            // A full queue means updates are already being
                            // dropped; ordering is not the problem then, so
                            // proceed rather than hang the agent.
                            let queued = notify_tx.try_send(Inbound::Barrier(ordered)).is_ok();
                            let handler = handler.clone();
                            let writes = writes.clone();
                            tokio::spawn(async move {
                                if queued {
                                    let _ = in_order.await;
                                }
                                let outcome = handler.request(method.clone(), params).await;
                                let reply = match outcome {
                                    Ok(result) => serde_json::json!({
                                        "jsonrpc": "2.0", "id": id, "result": result,
                                    }),
                                    Err(message) => serde_json::json!({
                                        "jsonrpc": "2.0", "id": id,
                                        "error": { "code": -32603, "message": message },
                                    }),
                                };
                                let _ = writes.send(reply.to_string()).await;
                            });
                        }
                        Frame::Notification { method, params } => {
                            // `try_send`: blocking here would stall the reader,
                            // which is the one thing this task must never do.
                            // A full queue means the handler has wedged, and
                            // the session is already broken.
                            //
                            // Counted, not just logged. What was dropped is a
                            // piece of an answer, so the turn that loses one
                            // cannot go on to report success — a transcript
                            // with a hole in it and a green tick beside it is
                            // the one failure nobody would ever notice. The
                            // session reads this in `finish`.
                            if notify_tx.try_send(Inbound::Notification(method, params)).is_err() {
                                let lost = dropped.fetch_add(1, Ordering::Relaxed) + 1;
                                tracing::warn!(lost, "the ACP notification queue is full; an update was dropped");
                            }
                        }
                        Frame::Junk => {}
                    }
                }

                let reason = death.get().unwrap_or_else(|| "the ACP session was closed".to_string());
                pending.drain_with(&reason);
                // Deliberately *not* the completion signal. This task does not
                // own the child and cannot say whether it is gone — see the
                // supervisor below, which does.
            });
        }

        // -------------------------------------------------------- supervisor
        {
            let death = death.clone();
            let stop = stop.clone();
            let pending = pending.clone();
            let stderr = stderr.clone();
            let mut child = child;
            tokio::spawn(async move {
                tokio::select! {
                    _ = stop.cancelled() => {
                        let _ = child.kill().await;
                    }
                    status = child.wait() => {
                        let code = status.ok().and_then(|s| s.code());
                        // Usually *loses* the race to the reader's EOF, which
                        // fires the moment the pipe closes while this waits for
                        // the process to be reaped. So "exited with code N" is
                        // the better message and not the one normally seen —
                        // `Death` keeps whichever arrives first, and both are
                        // true. What this arm is actually for is the case the
                        // reader cannot cover: a child that dies without its
                        // stdout closing, where EOF never comes at all.
                        death.set(match code {
                            Some(c) => format!("the ACP adapter exited with code {c}{}", stderr.suffix()),
                            None => format!("the ACP adapter was terminated{}", stderr.suffix()),
                        });
                        // The reader normally does this on EOF. Doing it here
                        // too covers a child that dies without closing stdout
                        // — a killed process on Windows, most often.
                        if let Some(reason) = death.get() {
                            pending.drain_with(&reason);
                        }
                    }
                }

                // Only here, and only from this task. Both arms above leave the
                // child actually gone: `kill` signals *and* reaps it, and
                // `wait` returning means it had already exited. That is what
                // makes this signal mean "the process is not running any more"
                // rather than "we have stopped listening to it".
                //
                // The reader used to send this, and it does not own the child.
                // It finishes the instant the stop token is cancelled, which
                // released every waiter downstream — `stop`, then `close_all`,
                // then the blocking wait on the restart path, then
                // `std::process::exit`.
                //
                // Most of the time that was survivable by accident: `start_kill`
                // is synchronous, so a supervisor that got polled even once had
                // already terminated the child. The case it did not survive is
                // the supervisor not being polled at all before the process
                // goes — and the caller most likely to produce that is the
                // restart path, which does no other awaiting between unblocking
                // and exiting. A narrow window, on the one path where losing it
                // means an adapter that outlives the app with nothing left able
                // to reach it.
                let _ = finished_tx.send(true);
            });
        }

        peer
    }

    /// Send a request and wait for its answer.
    ///
    /// No deadline of its own. `session/prompt` legitimately stays open for the
    /// length of a turn, which can be an hour, and a ceiling low enough to
    /// catch a wedged adapter would cut short a working one. Callers that need
    /// a bound wrap this in `tokio::time::timeout`; giving up that way is safe
    /// here — see the module header — and only leaves a slot in the pending
    /// table, which is cleared when the peer dies.
    pub async fn request(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, PeerError> {
        if let Some(reason) = self.death.get() {
            return Err(PeerError::Dead(reason));
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        // Registered before the write, so a reply cannot arrive before there is
        // somewhere to put it.
        self.pending.insert(id, tx);

        // **Asked again, because the check above and this insert are not one
        // step.** The adapter can die in between: the reader sets `death` and
        // `drain_with`s the table, and the entry that lands afterwards is one
        // nothing will ever complete — `rx.await` below then blocks for the
        // life of the process, which for `session/prompt` is a turn that never
        // ends and a conversation that can never be used again. Either the
        // drain saw this entry and answered it, or it did not and this does.
        if let Some(reason) = self.death.get() {
            self.pending.take(id);
            return Err(PeerError::Dead(reason));
        }

        let body = serde_json::to_string(&Request::new(id, method, Some(params)))
            .map_err(|e| PeerError::Rpc(format!("could not encode `{method}`: {e}")))?;

        if self.writes.send(body).await.is_err() {
            self.pending.take(id);
            return Err(PeerError::Dead(self.death_reason()));
        }

        match rx.await {
            Ok(answer) => answer,
            // The sender was dropped: the peer died with this call in flight.
            Err(_) => Err(PeerError::Dead(self.death_reason())),
        }
    }

    /// Send a notification. Nothing comes back, including any indication that
    /// the agent understood it.
    pub async fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), PeerError> {
        if let Some(reason) = self.death.get() {
            return Err(PeerError::Dead(reason));
        }
        let body = serde_json::to_string(&Notification::new(method, params))
            .map_err(|e| PeerError::Rpc(format!("could not encode `{method}`: {e}")))?;
        self.writes
            .send(body)
            .await
            .map_err(|_| PeerError::Dead(self.death_reason()))
    }

    /// Wait until every notification read *before this call* has been handled.
    ///
    /// The two inbound paths are not synchronised with each other, and they are
    /// not meant to be: a reply goes straight to whoever is waiting for it,
    /// while updates queue behind a single consumer so their order survives.
    /// The consequence is that `session/prompt` can resolve while the last few
    /// `session/update`s of that turn are still in the queue.
    ///
    /// Without a barrier here the turn ends first — and ending a turn takes the
    /// state those updates would have been recorded into, so they arrive to
    /// find no turn and are dropped. What is lost is the tail of a turn, which
    /// is exactly where a tool's result and the closing sentence are. A barrier
    /// through the same queue is the whole fix: one consumer, FIFO, so this
    /// resolving means everything ahead of it has been handled.
    ///
    /// `send` rather than `try_send` on purpose. A full queue is the case this
    /// most needs to survive, and the notifier only ever blocks on work that
    /// finishes.
    pub async fn drain_notifications(&self) {
        let (done, wait) = oneshot::channel();
        if self.notify.send(Inbound::Barrier(done)).await.is_err() {
            // The notifier is gone, so there is nothing left to wait for.
            return;
        }
        let _ = wait.await;
    }

    /// How many updates the reader had to throw away, over the peer's lifetime.
    ///
    /// Non-zero means some transcript is missing part of an answer. It never
    /// resets, so a turn compares against what it saw at its own start.
    pub fn dropped_notifications(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn is_alive(&self) -> bool {
        self.death.get().is_none()
    }

    /// Stop the adapter and wait until the process is actually gone.
    ///
    /// "Gone" is the literal claim and callers depend on it: the restart path
    /// blocks on this and then lets `std::process::exit` run, which collects
    /// nothing. The signal comes from the supervisor, the one task that owns
    /// the child, so waiting on it is waiting on the kill rather than on
    /// everyone losing interest.
    ///
    /// The `Err` break is the supervisor having died without reporting —
    /// nothing is left to wait for at that point, and blocking for ever would
    /// be worse than proceeding.
    pub async fn stop(&self) {
        self.death.set("the ACP session was closed".into());
        self.stop.cancel();
        let mut finished = self.finished.clone();
        while !*finished.borrow() {
            if finished.changed().await.is_err() {
                break;
            }
        }
        self.pending.drain_with(&self.death_reason());
    }

    fn death_reason(&self) -> String {
        self.death
            .get()
            .unwrap_or_else(|| format!("the ACP adapter stopped responding{}", self.stderr.suffix()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A handler that records notifications in order and can be told to block
    /// on requests until released.
    #[derive(Default)]
    struct Recorder {
        seen: Mutex<Vec<String>>,
        request_gate: Mutex<Option<oneshot::Receiver<()>>>,
        requests: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Handler for Recorder {
        async fn notification(&self, method: String, params: serde_json::Value) {
            let text = params
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            self.seen.lock().unwrap().push(format!("{method}:{text}"));
        }

        async fn request(&self, _method: String, _params: serde_json::Value) -> Result<serde_json::Value, String> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            let gate = self.request_gate.lock().unwrap().take();
            if let Some(gate) = gate {
                let _ = gate.await;
            }
            Ok(serde_json::json!({ "ok": true }))
        }
    }

    /// Everything below drives the peer's plumbing directly rather than through
    /// a real child process: `Peer::start` wants an `AdapterProcess`, and the
    /// behaviour worth pinning down is the routing, which is the same either
    /// way. What a spawned adapter adds is covered in `process.rs`.
    mod routing {
        use super::*;

        /// The three shapes go three different ways, and the reader keeps
        /// running through all of them.
        #[tokio::test]
        async fn a_reply_wakes_its_caller_and_a_notification_does_not() {
            let pending = Pending::default();
            let (tx, rx) = oneshot::channel();
            pending.insert(1, tx);

            // A reply for a caller that exists.
            let frame: Incoming = serde_json::from_str(r#"{"id":1,"result":{"sessionId":"s"}}"#).unwrap();
            if let Frame::Response { id, result } = frame.classify() {
                pending.take(id).unwrap().send(result.map_err(PeerError::Rpc)).unwrap();
            }
            assert_eq!(rx.await.unwrap().unwrap()["sessionId"], "s");

            // And one for a caller that has gone. Must not panic or wedge.
            let frame: Incoming = serde_json::from_str(r#"{"id":99,"result":{}}"#).unwrap();
            if let Frame::Response { id, .. } = frame.classify() {
                assert!(pending.take(id).is_none());
            }
        }

        /// The failure this whole module is shaped around: a dead adapter must
        /// not leave callers parked for ever.
        ///
        /// And it must tell them the *pipe* went, not that their request was
        /// refused. This drained as `Rpc` once — a distinction the whole error
        /// type exists to make, erased at the last step, so a session that had
        /// died looked to its caller like one that had answered no.
        #[tokio::test]
        async fn death_refuses_everyone_still_waiting_as_fatal() {
            let pending = Pending::default();
            let (tx1, rx1) = oneshot::channel();
            let (tx2, rx2) = oneshot::channel();
            pending.insert(1, tx1);
            pending.insert(2, tx2);

            pending.drain_with("the ACP adapter exited with code 1");

            for rx in [rx1, rx2] {
                let err = rx.await.unwrap().unwrap_err();
                assert!(err.is_dead(), "a drained caller must learn the adapter died: {err}");
                assert!(err.to_string().contains("code 1"));
            }
        }

        /// An adapter that closes its stdin and keeps running.
        ///
        /// The one shape the reader cannot report: its stdout never ends, so
        /// nothing there ever fires, and a caller parked on a reply waits for
        /// the life of the process — holding that conversation's turn lease,
        /// with a session that looks busy and is not.
        #[tokio::test]
        async fn a_half_closed_stdin_wakes_everyone_already_waiting() {
            let pending = Pending::default();
            let death = Death::default();
            let stop = CancellationToken::new();
            let (tx, rx) = oneshot::channel();
            pending.insert(1, tx);

            stdin_closed(&death, &pending, &stop);

            let err = rx.await.unwrap().unwrap_err();
            assert!(err.is_dead(), "a half-closed pipe is fatal, not a refusal: {err}");
            assert!(err.to_string().contains("stopped accepting input"));
            assert!(stop.is_cancelled(), "the supervisor has to reap the child");
            // What `Peer::request` re-reads after inserting its own slot, so the
            // next caller is refused instead of joining the parked one.
            assert!(death.get().is_some());
        }

        /// And when the reader got there first, the waiters are told *its*
        /// reason. The exit code is what says whether this is worth reporting;
        /// "stopped accepting input" is the consequence, not the cause.
        #[tokio::test]
        async fn a_death_already_explained_keeps_its_explanation() {
            let pending = Pending::default();
            let death = Death::default();
            death.set("the ACP adapter exited with code 1".into());
            let (tx, rx) = oneshot::channel();
            pending.insert(1, tx);

            stdin_closed(&death, &pending, &CancellationToken::new());

            let err = rx.await.unwrap().unwrap_err();
            assert!(err.to_string().contains("code 1"), "{err}");
        }

        /// The first explanation is the useful one; the cascade behind it is
        /// not. Both tasks report, and the exit code has to survive.
        #[test]
        fn the_first_cause_of_death_is_the_one_kept() {
            let death = Death::default();
            assert!(death.set("the ACP adapter exited with code 1".into()));
            assert!(!death.set("the ACP adapter closed its output".into()));
            assert_eq!(death.get().unwrap(), "the ACP adapter exited with code 1");
        }

        /// Notifications are queued rather than spawned, so their order is the
        /// order they arrived in — text chunks depend on it.
        #[tokio::test]
        async fn notifications_are_handled_in_arrival_order() {
            let handler = Arc::new(Recorder::default());
            let (tx, mut rx) = mpsc::channel::<(String, serde_json::Value)>(NOTIFY_QUEUE);
            let h = handler.clone();
            let pump = tokio::spawn(async move {
                while let Some((method, params)) = rx.recv().await {
                    h.notification(method, params).await;
                }
            });

            for word in ["the", "quick", "brown", "fox"] {
                tx.send(("session/update".into(), serde_json::json!({ "text": word })))
                    .await
                    .unwrap();
            }
            drop(tx);
            pump.await.unwrap();

            let seen = handler.seen.lock().unwrap().clone();
            assert_eq!(
                seen,
                vec![
                    "session/update:the",
                    "session/update:quick",
                    "session/update:brown",
                    "session/update:fox"
                ]
            );
        }

        /// The point of spawning inbound requests: one that is waiting on a
        /// person must not stop the updates behind it. Modelled here as a
        /// request that never completes while notifications keep landing.
        #[tokio::test]
        async fn a_request_awaiting_an_answer_does_not_block_notifications() {
            let (gate_tx, gate_rx) = oneshot::channel();
            let handler = Arc::new(Recorder {
                request_gate: Mutex::new(Some(gate_rx)),
                ..Default::default()
            });

            // The reader's behaviour: spawn the request, carry on.
            let h = handler.clone();
            let in_flight =
                tokio::spawn(async move { h.request("session/request_permission".into(), json_null()).await });

            let (tx, mut rx) = mpsc::channel::<(String, serde_json::Value)>(NOTIFY_QUEUE);
            let h = handler.clone();
            let pump = tokio::spawn(async move {
                while let Some((method, params)) = rx.recv().await {
                    h.notification(method, params).await;
                }
            });
            for word in ["still", "streaming"] {
                tx.send(("session/update".into(), serde_json::json!({ "text": word })))
                    .await
                    .unwrap();
            }
            drop(tx);
            pump.await.unwrap();

            // Updates arrived while the question was still unanswered.
            assert_eq!(handler.seen.lock().unwrap().len(), 2);
            assert_eq!(handler.requests.load(Ordering::Relaxed), 1);

            gate_tx.send(()).unwrap();
            assert!(in_flight.await.unwrap().is_ok());
        }

        fn json_null() -> serde_json::Value {
            serde_json::Value::Null
        }
    }

    /// End to end against a real child process.
    ///
    /// Everything above tests the routing in isolation, which is where the
    /// logic is — but not that any of it survives contact with a pipe, a
    /// spawned binary and JSON on the wire. These drive
    /// `scripts/dev/fake-acp-adapter.mjs`, a minimal ACP agent.
    ///
    /// Skipped when node is absent rather than failed: the real adapter is a
    /// node package, so a machine without node cannot run this feature at all,
    /// and a red suite there would be reporting the wrong thing.
    mod against_a_real_adapter {
        use super::*;
        use crate::acp::protocol;

        /// The script's path, or `None` when this machine cannot run it.
        fn adapter() -> Option<Vec<String>> {
            std::process::Command::new("node")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .ok()
                .filter(|s| s.success())?;
            let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../scripts/dev/fake-acp-adapter.mjs")
                .canonicalize()
                .ok()?;
            let script = script.to_string_lossy().into_owned();
            // `canonicalize` hands back the verbatim `\\?\C:\…` form on Windows,
            // and some node builds read that as a UNC share — server `?`, share
            // `C:` — then fail to stat it. Whether it works is a property of the
            // machine rather than of this tree: these tests passed on one
            // Windows box and failed on the next with nothing changed but the
            // node on it. The prefix is only ever a spelling of the same path,
            // so dropping it costs nothing and removes the variable.
            let script = script.strip_prefix(r"\\?\").unwrap_or(&script).to_string();
            Some(vec![script])
        }

        /// Records what arrived, and answers the permission question.
        #[derive(Default)]
        struct Probe {
            updates: Mutex<Vec<String>>,
            asked: Mutex<Option<oneshot::Sender<()>>>,
            answer: Mutex<Option<String>>,
            /// Time spent handling each notification. Real handlers write to
            /// the database, so the notifier lagging behind the reader is the
            /// ordinary case rather than a contrived one — and it is what makes
            /// the missing-barrier bug reproducible instead of occasional.
            per_update: std::time::Duration,
        }

        #[async_trait::async_trait]
        impl Handler for Probe {
            async fn notification(&self, method: String, params: serde_json::Value) {
                if !self.per_update.is_zero() {
                    tokio::time::sleep(self.per_update).await;
                }
                if method != "session/update" {
                    return;
                }
                let Ok(n) = serde_json::from_value::<protocol::SessionNotification>(params) else {
                    return;
                };
                let effect = crate::acp::mapping::effect_of(n.update);
                self.updates.lock().unwrap().push(format!("{effect:?}"));
            }

            async fn request(&self, method: String, params: serde_json::Value) -> Result<serde_json::Value, String> {
                assert_eq!(method, "session/request_permission");
                // Recorded in the same list as the updates, so the test can see
                // where the question fell among them.
                self.updates.lock().unwrap().push("ASKED".to_string());
                let params: protocol::RequestPermissionParams =
                    serde_json::from_value(params).map_err(|e| e.to_string())?;
                // The one this app would draw: allow, just this once.
                let option = params
                    .options
                    .iter()
                    .find(|o| o.is_allow() && o.is_once())
                    .expect("the fake adapter offers allow_once");
                let chosen = option.option_id.clone();
                *self.answer.lock().unwrap() = Some(chosen.clone());
                // Let the test see that updates keep arriving while this is
                // outstanding, then answer.
                if let Some(tx) = self.asked.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                Ok(protocol::permission_selected(&chosen))
            }
        }

        /// The whole protocol, once: spawn, greet, open a session, run a turn,
        /// answer a question halfway through, and read the result.
        #[tokio::test]
        async fn a_turn_runs_from_spawn_to_stop_reason() {
            let Some(args) = adapter() else {
                eprintln!("skipping: node is not available");
                return;
            };

            let process = AdapterProcess::spawn("node", &args).await.expect("spawn the adapter");
            let probe = Arc::new(Probe::default());
            let (asked_tx, asked_rx) = oneshot::channel();
            *probe.asked.lock().unwrap() = Some(asked_tx);
            let peer = Peer::start(process, probe.clone() as Arc<dyn Handler>);

            let init = peer
                .request(
                    "initialize",
                    serde_json::json!({
                        "protocolVersion": 1,
                        "clientCapabilities": {
                            "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false
                        },
                        "clientInfo": { "name": "meridian", "version": "test" },
                    }),
                )
                .await
                .expect("initialize");
            let init: protocol::InitializeResult = serde_json::from_value(init).unwrap();
            assert_eq!(init.protocol_version, 1);
            assert!(init.agent_capabilities.load_session);

            let session = peer
                .request("session/new", serde_json::json!({ "cwd": ".", "mcpServers": [] }))
                .await
                .expect("session/new");
            let session: protocol::NewSessionResult = serde_json::from_value(session).unwrap();

            // Stays outstanding for the whole turn, question included.
            let prompt = peer.request(
                "session/prompt",
                serde_json::json!({
                    "sessionId": session.session_id,
                    "prompt": [{ "type": "text", "text": "go" }],
                }),
            );

            // The property the whole reader design exists for: an unanswered
            // question does not stop the updates behind it.
            let watching = async {
                asked_rx.await.expect("the adapter asks for permission");
                let seen = probe.updates.lock().unwrap().len();
                assert!(seen > 0, "updates must have arrived before the question");
            };
            let (result, ()) = tokio::join!(prompt, watching);

            let result: protocol::PromptResult = serde_json::from_value(result.expect("session/prompt")).unwrap();
            assert_eq!(result.stop_reason, "end_turn");

            let joined = probe.updates.lock().unwrap().join("\n");
            // By variant and by content rather than by the whole `Debug`
            // spelling, which now carries the message id these chunks do not
            // have — a live turn's updates are not stamped with one.
            assert!(
                joined.lines().any(|l| l.starts_with("Text") && l.contains("Hello ")),
                "prose: {joined}"
            );
            assert!(
                joined
                    .lines()
                    .any(|l| l.starts_with("Reasoning") && l.contains("thinking...")),
                "thinking: {joined}"
            );
            assert!(joined.contains("ToolCall"), "the call: {joined}");
            assert!(joined.contains("Plan("), "the plan: {joined}");
            assert!(joined.contains("Usage"), "usage: {joined}");
            // The `current_mode_update` this client has never heard of.
            assert!(joined.contains("Ignored"), "an unknown update is ignored: {joined}");
            // Approved, so the call succeeded — the answer travelled back out
            // and the adapter acted on it.
            assert!(
                joined.contains("result: \"3 passed\""),
                "the approval reached the adapter: {joined}"
            );
            assert_eq!(probe.answer.lock().unwrap().as_deref(), Some("allow-once"));
            // And the streaming that continued *while* the question was open.
            assert!(joined.contains("still streaming"), "streamed during the wait: {joined}");

            peer.stop().await;
            assert!(!peer.is_alive());
        }

        /// The AIR `sessionFailure` extension on the wire, both carriers.
        ///
        /// Declaring it is what changes the shape of a failed prompt: with the
        /// capability in `initialize`, a failure comes back as a *resolved*
        /// `end_turn` with the record in `_meta`, where without it the same
        /// failure is a JSON-RPC rejection. A client that declares and then
        /// reads only `stopReason` has turned every failure into success —
        /// this test is the one that would catch the declaration going in
        /// without its reader.
        #[tokio::test]
        async fn a_typed_failure_resolves_the_prompt_and_a_warning_is_a_notice() {
            let Some(args) = adapter() else {
                eprintln!("skipping: node is not available");
                return;
            };

            let process = AdapterProcess::spawn("node", &args).await.expect("spawn the adapter");
            let probe = Arc::new(Probe::default());
            let peer = Peer::start(process, probe.clone() as Arc<dyn Handler>);

            // The capabilities this app actually sends, AIR opt-in included.
            peer.request(
                "initialize",
                serde_json::to_value(protocol::InitializeParams {
                    protocol_version: protocol::PROTOCOL_VERSION,
                    client_capabilities: protocol::ClientCapabilities::default(),
                    client_info: protocol::Implementation {
                        name: "meridian".into(),
                        title: None,
                        version: "test".into(),
                    },
                })
                .unwrap(),
            )
            .await
            .expect("initialize");
            let session = peer
                .request("session/new", serde_json::json!({ "cwd": ".", "mcpServers": [] }))
                .await
                .expect("session/new");
            let session: protocol::NewSessionResult = serde_json::from_value(session).unwrap();
            let prompt = |text: &str| {
                serde_json::json!({
                    "sessionId": session.session_id,
                    "prompt": [{ "type": "text", "text": text }],
                })
            };

            // A warning mid-turn: a `session_info_update` carrying only `_meta`.
            let reply = peer
                .request("session/prompt", prompt("air-warn"))
                .await
                .expect("air-warn");
            let reply: protocol::PromptResult = serde_json::from_value(reply).unwrap();
            assert_eq!(reply.stop_reason, "end_turn");
            assert!(reply.meta.as_ref().and_then(|m| m.session_failure()).is_none());
            let joined = probe.updates.lock().unwrap().join("\n");
            assert!(
                joined
                    .lines()
                    .any(|l| l.starts_with("SessionNotice") && l.contains("Retrying Claude")),
                "the warning is a notice: {joined}"
            );

            // A terminal failure: resolved, not rejected, and the record is on
            // the reply.
            let reply = peer
                .request("session/prompt", prompt("air-fail"))
                .await
                .expect("air-fail resolves");
            let reply: protocol::PromptResult = serde_json::from_value(reply).unwrap();
            assert_eq!(reply.stop_reason, "end_turn", "the stop reason says nothing went wrong");
            let record = reply
                .meta
                .as_ref()
                .and_then(|m| m.session_failure())
                .expect("the failure travels in _meta");
            assert_eq!(record.severity, "error");
            assert_eq!(record.revision, 2);
            assert_eq!(record.title, "Claude could not complete the request.");

            // Authentication still rejects — and reports beside the rejection.
            let err = peer
                .request("session/prompt", prompt("air-auth"))
                .await
                .expect_err("authentication is the one failure that still rejects");
            assert!(err.to_string().contains("Authentication required"), "{err}");
            let joined = probe.updates.lock().unwrap().join("\n");
            assert!(
                joined
                    .lines()
                    .any(|l| l.starts_with("SessionNotice") && l.contains("Sign in")),
                "the session-scoped record: {joined}"
            );

            // And the title frame, which is the other thing a
            // `session_info_update` carries.
            peer.request("session/prompt", prompt("title")).await.expect("title");
            let joined = probe.updates.lock().unwrap().join("\n");
            assert!(
                joined
                    .lines()
                    .any(|l| l.starts_with("SessionTitle") && l.contains("Fix the flaky title test")),
                "the title: {joined}"
            );

            // A Write's three frames: the refinement in the middle is the one
            // diff effect, carrying the pre-overwrite text and the hunk's line.
            peer.request("session/prompt", prompt("write-diff"))
                .await
                .expect("write-diff");
            let joined = probe.updates.lock().unwrap().join("\n");
            let diff_lines: Vec<&str> = joined.lines().filter(|l| l.starts_with("ToolCallDiff")).collect();
            assert_eq!(diff_lines.len(), 1, "one diff effect for the call: {joined}");
            assert!(
                diff_lines[0].contains("old line2") && diff_lines[0].contains("line: Some(1)"),
                "{joined}"
            );

            peer.stop().await;
        }

        /// A caller parked on a request when the adapter dies has to hear about
        /// it. Left waiting, the session simply never answers — the failure
        /// that reads as a hang rather than as an error.
        #[tokio::test]
        async fn a_dead_adapter_refuses_the_call_in_flight() {
            if adapter().is_none() {
                eprintln!("skipping: node is not available");
                return;
            }

            // Exits at once, so nothing will ever answer the request below.
            let process = AdapterProcess::spawn("node", &["-e".into(), "process.exit(3)".into()])
                .await
                .expect("spawn");
            let peer = Peer::start(process, Arc::new(Probe::default()) as Arc<dyn Handler>);

            let err = peer
                .request("initialize", serde_json::json!({}))
                .await
                .expect_err("a dead adapter cannot answer");
            // The half that matters: fatal, not a refusal. A caller told `Rpc`
            // here would read it as an answer about its request and carry on
            // using a session that no longer exists.
            assert!(err.is_dead(), "must be fatal, got: {err}");
            // Which of the two explanations arrives is a race between the
            // reader's EOF and the supervisor reaping the process, and both are
            // true — so this pins down only that the message names the thing
            // that died. Asserting the exit code here would be asserting the
            // outcome of that race.
            assert!(err.to_string().contains("adapter"), "should say what died, got: {err}");
        }

        /// The tail of a turn arrives *after* the turn is over, unless somebody
        /// waits for it.
        ///
        /// A reply overtakes the updates ahead of it: it goes straight to its
        /// caller while they queue behind a single consumer. So when
        /// `session/prompt` resolves, the last few `session/update`s of that
        /// turn are typically still unhandled — and the session ends the turn
        /// on that reply, taking the state they would be recorded into.
        ///
        /// It is not only the tail, either. Run without the barrier, this test
        /// reports exactly one update handled by the time the reply lands —
        /// `Reasoning("thinking...")` — with the prose, the tool call, its
        /// result and the usage all still queued. A turn ending there would
        /// have written almost none of what the agent said.
        ///
        /// `drain_notifications` is the barrier that prevents it, and the
        /// handler is slowed here to make the lag certain rather than a race
        /// that usually happens to come out right.
        #[tokio::test]
        async fn the_reply_can_overtake_the_updates_and_the_barrier_catches_them() {
            let Some(args) = adapter() else {
                eprintln!("skipping: node is not available");
                return;
            };

            let process = AdapterProcess::spawn("node", &args).await.expect("spawn");
            let probe = Arc::new(Probe {
                per_update: std::time::Duration::from_millis(20),
                ..Default::default()
            });
            let (asked_tx, _asked_rx) = oneshot::channel();
            *probe.asked.lock().unwrap() = Some(asked_tx);
            let peer = Peer::start(process, probe.clone() as Arc<dyn Handler>);

            peer.request(
                "initialize",
                serde_json::json!({ "protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {} }),
            )
            .await
            .expect("initialize");
            let session: protocol::NewSessionResult = serde_json::from_value(
                peer.request("session/new", serde_json::json!({ "cwd": ".", "mcpServers": [] }))
                    .await
                    .expect("session/new"),
            )
            .unwrap();

            peer.request(
                "session/prompt",
                serde_json::json!({
                    "sessionId": session.session_id,
                    "prompt": [{ "type": "text", "text": "go" }],
                }),
            )
            .await
            .expect("session/prompt");

            // The state this test exists to describe: the turn is over and the
            // transcript is not finished arriving.
            let at_reply = probe.updates.lock().unwrap().len();

            peer.drain_notifications().await;

            let joined = probe.updates.lock().unwrap().join("\n");
            assert!(
                joined.contains("result: \"3 passed\""),
                "the tool result must survive the end of the turn: {joined}"
            );
            assert!(joined.contains("Usage"), "and so must everything after it: {joined}");
            assert!(
                probe.updates.lock().unwrap().len() > at_reply,
                "the barrier is what waited for them; without it the turn would have ended \
                 at {at_reply} updates and the rest would have been dropped"
            );

            peer.stop().await;
        }

        /// A question never overtakes the update that gives it something to
        /// attach to.
        ///
        /// `session/request_permission` follows the `tool_call` announcing the
        /// same call, and the card it attaches to is created by that
        /// notification. Spawned without ordering, the question wins whenever
        /// the notifier is even slightly behind — and the front end has no
        /// second chance at it: `handleToolApproval` looks for the card, finds
        /// none, and attaches the `approval_id` to nothing, while
        /// `handleToolCall` arriving later does not go looking for an approval
        /// to reconcile. In the conversation being read, which the toast queue
        /// deliberately skips, that is a card stuck on "running" and a turn
        /// waiting on an answer nobody can give.
        ///
        /// The handler is slowed so the notifier is certainly behind.
        #[tokio::test]
        async fn a_permission_request_never_arrives_before_the_call_it_is_about() {
            let Some(args) = adapter() else {
                eprintln!("skipping: node is not available");
                return;
            };

            let process = AdapterProcess::spawn("node", &args).await.expect("spawn");
            let probe = Arc::new(Probe {
                per_update: std::time::Duration::from_millis(20),
                ..Default::default()
            });
            let (asked_tx, _asked_rx) = oneshot::channel();
            *probe.asked.lock().unwrap() = Some(asked_tx);
            let peer = Peer::start(process, probe.clone() as Arc<dyn Handler>);

            peer.request(
                "initialize",
                serde_json::json!({ "protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {} }),
            )
            .await
            .expect("initialize");
            let session: protocol::NewSessionResult = serde_json::from_value(
                peer.request("session/new", serde_json::json!({ "cwd": ".", "mcpServers": [] }))
                    .await
                    .expect("session/new"),
            )
            .unwrap();

            peer.request(
                "session/prompt",
                serde_json::json!({
                    "sessionId": session.session_id,
                    "prompt": [{ "type": "text", "text": "go" }],
                }),
            )
            .await
            .expect("session/prompt");
            peer.drain_notifications().await;

            let seen = probe.updates.lock().unwrap().clone();
            let asked = seen.iter().position(|s| s == "ASKED").expect("the adapter asked");
            let announced = seen
                .iter()
                .position(|s| s.starts_with("ToolCall"))
                .expect("the call was announced");
            assert!(
                announced < asked,
                "the question must come after the call it is about; got {seen:#?}"
            );

            peer.stop().await;
        }

        /// An update that could not be queued is a hole in a transcript, and
        /// the turn has to be able to find out.
        ///
        /// Logging it and carrying on was the original behaviour, and it is the
        /// failure with no symptom: the answer is simply shorter than what the
        /// agent said, and everything downstream reports success.
        #[tokio::test]
        async fn an_overflowing_queue_is_counted_not_just_logged() {
            let Some(args) = adapter() else {
                eprintln!("skipping: node is not available");
                return;
            };

            let process = AdapterProcess::spawn("node", &args).await.expect("spawn");
            // Slow enough that 4000 updates cannot possibly be handled as they
            // arrive, so the queue is certain to overflow.
            let probe = Arc::new(Probe {
                per_update: std::time::Duration::from_millis(5),
                ..Default::default()
            });
            let peer = Peer::start(process, probe.clone() as Arc<dyn Handler>);

            peer.request(
                "initialize",
                serde_json::json!({ "protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {} }),
            )
            .await
            .expect("initialize");
            let session: protocol::NewSessionResult = serde_json::from_value(
                peer.request("session/new", serde_json::json!({ "cwd": ".", "mcpServers": [] }))
                    .await
                    .expect("session/new"),
            )
            .unwrap();

            assert_eq!(peer.dropped_notifications(), 0, "nothing lost yet");

            peer.request(
                "session/prompt",
                serde_json::json!({
                    "sessionId": session.session_id,
                    "prompt": [{ "type": "text", "text": "flood" }],
                }),
            )
            .await
            .expect("session/prompt");

            assert!(
                peer.dropped_notifications() > 0,
                "4000 updates against a {NOTIFY_QUEUE}-deep queue must have dropped some"
            );

            // Not drained: the point is that the count is readable at the end of
            // the turn, which is where `session::finish` reads it.
            peer.stop().await;
        }

        /// Is a process id still a running process?
        ///
        /// Asked of a pid this test just reaped, so the textbook objection —
        /// that the number could have been handed to something else — needs the
        /// OS to recycle it within the same millisecond.
        #[cfg(windows)]
        fn is_running(pid: u32) -> bool {
            // CSV with no header, so a match is the row `"node.exe","<pid>",…`.
            // Filtering on the pid and then looking for it quoted keeps this
            // clear of the localised "no tasks are running" line, which is what
            // parsing the human format would have depended on.
            std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
                .unwrap_or(false)
        }

        #[cfg(not(windows))]
        fn is_running(pid: u32) -> bool {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }

        /// `stop` means the process is gone, not that we stopped listening.
        ///
        /// The distinction is invisible almost everywhere and decisive in one
        /// place: the restart path blocks on this and then lets
        /// `std::process::exit` run, which collects nothing. A `stop` that
        /// returns early there leaves an adapter running that no later code can
        /// reach.
        ///
        /// **What this does and does not prove.** `Child::kill` is
        /// `start_kill` — synchronous — followed by an awaited `wait`, so the
        /// moment the supervisor is polled at all the process is already
        /// terminated and only the reaping is outstanding. On Windows a
        /// terminated process leaves the task list at once, so this check
        /// cannot tell "reaped" from "terminated" and is a smoke test: it
        /// catches a `stop` that never kills anything. On Unix it discriminates
        /// properly, because an unreaped child is a zombie and `kill -0`
        /// succeeds on one — so a `stop` returning before the supervisor
        /// finished would fail here.
        ///
        /// The window the fix actually closes is narrower than either: the
        /// supervisor not being polled *even once* before the process exits,
        /// which is reachable when the reader releases every waiter and the
        /// caller goes straight into `std::process::exit`. Nothing observable
        /// from inside the runtime can reproduce that on demand, which is why
        /// the reasoning is written at the send site rather than asserted here.
        #[tokio::test]
        async fn stopping_waits_for_the_child_to_actually_die() {
            let Some(args) = adapter() else {
                eprintln!("skipping: node is not available");
                return;
            };

            let process = AdapterProcess::spawn("node", &args).await.expect("spawn");
            let pid = process.child.id().expect("a freshly spawned child has a pid");
            let peer = Peer::start(process, Arc::new(Probe::default()) as Arc<dyn Handler>);

            // Talk to it first, so this is a live adapter mid-session rather
            // than one that might still have been starting up.
            peer.request(
                "initialize",
                serde_json::json!({ "protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {} }),
            )
            .await
            .expect("initialize");
            assert!(is_running(pid), "the adapter should be up before it is stopped");

            peer.stop().await;

            assert!(
                !is_running(pid),
                "`stop` returned while pid {pid} was still running; \
                 the restart path would have exited with the adapter still alive"
            );
        }

        /// The adapter refusing a method is not the pipe breaking. Confusing
        /// the two would tear down a healthy session over one unsupported call.
        #[tokio::test]
        async fn an_unknown_method_is_a_refusal_not_a_death() {
            let Some(args) = adapter() else {
                eprintln!("skipping: node is not available");
                return;
            };

            let process = AdapterProcess::spawn("node", &args).await.expect("spawn");
            let peer = Peer::start(process, Arc::new(Probe::default()) as Arc<dyn Handler>);

            let err = peer
                .request("nonsense/method", serde_json::json!({}))
                .await
                .expect_err("the adapter says no");
            assert!(!err.is_dead(), "an RPC refusal must not kill the peer: {err}");
            assert!(peer.is_alive(), "the session lives on");

            // And it still works afterwards.
            let init = peer
                .request(
                    "initialize",
                    serde_json::json!({ "protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {} }),
                )
                .await;
            assert!(init.is_ok(), "the peer still answers after a refusal");

            peer.stop().await;
        }

        /// Resuming a session, and the two things about it that are not
        /// obvious from the schema.
        ///
        /// The reply names a *different* session than the request — the agent
        /// resumes through the SDK and answers with whatever it recovered — so
        /// a client that writes down what it asked for rather than what it was
        /// told will chase an id that does not exist on the next launch.
        ///
        /// And the history arrives as ordinary `session/update` notifications
        /// before the reply, which is why the client's replay gate has to be
        /// held open across the reply *and* a drain rather than just the
        /// request: the reply travels a different route and overtakes them.
        #[tokio::test]
        async fn a_resumed_session_recites_its_history_and_answers_with_its_own_id() {
            let Some(args) = adapter() else {
                eprintln!("skipping: node is not available");
                return;
            };

            let process = AdapterProcess::spawn("node", &args).await.expect("spawn");
            let probe = Arc::new(Probe::default());
            let peer = Peer::start(process, probe.clone() as Arc<dyn Handler>);

            let loaded = peer
                .request(
                    "session/load",
                    serde_json::to_value(protocol::LoadSessionParams {
                        session_id: "sess-7".into(),
                        cwd: ".".into(),
                        ..Default::default()
                    })
                    .unwrap(),
                )
                .await
                .expect("the adapter resumes");
            let loaded: protocol::NewSessionResult = serde_json::from_value(loaded).unwrap();
            assert_eq!(
                loaded.session_id, "sess-7-resumed",
                "the reply is authoritative about which session this now is"
            );

            peer.drain_notifications().await;
            let heard = probe.updates.lock().unwrap().join(" ");
            assert!(
                heard.contains("REPLAYED"),
                "the history really does come back as updates: {heard}"
            );

            // A session id outlives the session it names. That has to be a
            // recoverable error rather than a dead conversation.
            let gone = peer
                .request(
                    "session/load",
                    serde_json::to_value(protocol::LoadSessionParams {
                        session_id: "sess-gone".into(),
                        cwd: ".".into(),
                        ..Default::default()
                    })
                    .unwrap(),
                )
                .await
                .expect_err("that one is gone");
            assert!(!gone.is_dead(), "a refusal must not kill the peer: {gone}");
            assert!(peer.is_alive());

            // **The reply the schema actually permits.** `LoadSessionResponse`
            // has no required field, so `result: null` is a success — and read
            // as a missing member it becomes `Frame::Junk`, nothing completes
            // the pending slot, and this `await` never returns. Against a pipe
            // rather than a unit test because the failure is a hang, and only a
            // real reader can hang.
            let terse = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                peer.request(
                    "session/load",
                    serde_json::to_value(protocol::LoadSessionParams {
                        session_id: "sess-terse".into(),
                        cwd: ".".into(),
                        ..Default::default()
                    })
                    .unwrap(),
                ),
            )
            .await
            .expect("a null result must complete its caller, not hang it")
            .expect("and it is a success, not an error");
            assert_eq!(terse, serde_json::Value::Null);
            let terse = protocol::LoadSessionResult::read(terse).expect("which reads as an empty load reply");
            assert_eq!(
                terse.session_id, None,
                "and the caller falls back to the id it asked for"
            );

            peer.stop().await;
        }

        /// Listing what is on disk, and turning one of them into rows.
        ///
        /// The half of importing that no unit test can reach: that the frames
        /// really do carry `messageId`, that the recital arrives whole and in
        /// order down a real pipe, and that `import::plan` makes the right
        /// shape out of the actual bytes rather than out of hand-written
        /// `Effect`s. Everything after this is a database write.
        #[tokio::test]
        async fn sessions_are_listed_and_a_recital_becomes_rows() {
            let Some(args) = adapter() else {
                eprintln!("skipping: node is not available");
                return;
            };

            /// Keeps the effects themselves rather than their `Debug`
            /// spelling, because this test hands them to the planner.
            #[derive(Default)]
            struct Recorder {
                recital: Mutex<Vec<crate::acp::mapping::Effect>>,
            }

            #[async_trait::async_trait]
            impl Handler for Recorder {
                async fn notification(&self, method: String, params: serde_json::Value) {
                    if method != "session/update" {
                        return;
                    }
                    let Ok(n) = serde_json::from_value::<protocol::SessionNotification>(params) else {
                        return;
                    };
                    let effect = crate::acp::mapping::effect_of(n.update);
                    if effect != crate::acp::mapping::Effect::Ignored {
                        self.recital.lock().unwrap().push(effect);
                    }
                }

                async fn request(
                    &self,
                    method: String,
                    _params: serde_json::Value,
                ) -> Result<serde_json::Value, String> {
                    Err(format!("unexpected {method}"))
                }
            }

            let process = AdapterProcess::spawn("node", &args).await.expect("spawn");
            let recorder = Arc::new(Recorder::default());
            let peer = Peer::start(process, recorder.clone() as Arc<dyn Handler>);

            let init = peer
                .request(
                    "initialize",
                    serde_json::to_value(protocol::InitializeParams {
                        protocol_version: protocol::PROTOCOL_VERSION,
                        client_capabilities: protocol::ClientCapabilities::default(),
                        client_info: protocol::Implementation {
                            name: "meridian".into(),
                            title: None,
                            version: "test".into(),
                        },
                    })
                    .unwrap(),
                )
                .await
                .expect("initialize");
            let init: protocol::InitializeResult = serde_json::from_value(init).unwrap();
            assert!(
                init.agent_capabilities.lists_sessions(),
                "listing is advertised inside agentCapabilities, not beside loadSession"
            );

            let listed = peer
                .request("session/list", serde_json::json!({}))
                .await
                .expect("session/list");
            let listed: protocol::ListSessionsResult = serde_json::from_value(listed).unwrap();
            assert_eq!(listed.sessions.len(), 2);
            assert_eq!(listed.sessions[0].session_id, "sess-7");
            assert_eq!(listed.sessions[0].title.as_deref(), Some("REPLAYED session"));
            assert_eq!(
                listed.sessions[1].title, None,
                "a session with no title is still listed"
            );

            // Narrowed to one directory, which is what the attach picker asks.
            let scoped = peer
                .request("session/list", serde_json::json!({ "cwd": "/work/other" }))
                .await
                .expect("session/list");
            let scoped: protocol::ListSessionsResult = serde_json::from_value(scoped).unwrap();
            assert_eq!(scoped.sessions.len(), 1);

            peer.request(
                "session/load",
                serde_json::to_value(protocol::LoadSessionParams {
                    session_id: "sess-7".into(),
                    cwd: ".".into(),
                    ..Default::default()
                })
                .unwrap(),
            )
            .await
            .expect("session/load");
            // The reply overtakes the notifications, here as everywhere.
            peer.drain_notifications().await;

            let recital = std::mem::take(&mut *recorder.recital.lock().unwrap());
            let imported = crate::acp::import::plan_for_test(recital);
            assert_eq!(
                imported,
                vec![
                    (
                        "REPLAYED question".to_string(),
                        vec![
                            // Two chunks, one message, one row — and the call
                            // is on it with its result, which arrived after the
                            // next message had already started.
                            ("REPLAYED answer".to_string(), 1, 1),
                            ("and done".to_string(), 0, 0),
                        ]
                    ),
                    (
                        "REPLAYED follow-up".to_string(),
                        vec![("REPLAYED again".to_string(), 0, 0)]
                    ),
                ]
            );

            peer.stop().await;
        }

        /// Steering, both ways round, against a pipe.
        ///
        /// Two things are being checked and neither can be checked without one.
        /// The first is that the capability is read from the top level of the
        /// greeting: get that wrong and every adapter reads as not supporting
        /// steering, which degrades silently — interjections quietly become
        /// follow-ups and nobody finds out.
        ///
        /// The second is the `_meta` this client sends. Without it, an agent
        /// with no turn to steer starts a *detached* one and answers
        /// `startedNewTurn`; that turn would narrate itself into the session
        /// with no reply for anyone to await, no lease and no stop button. The
        /// second half of this test is a steer made with nothing running, and
        /// what it asserts is that the message came *back*.
        #[tokio::test]
        async fn a_steer_joins_the_running_turn_and_is_handed_back_when_there_is_none() {
            let Some(args) = adapter() else {
                eprintln!("skipping: node is not available");
                return;
            };

            let process = AdapterProcess::spawn("node", &args).await.expect("spawn");
            let probe = Arc::new(Probe::default());
            let peer = Peer::start(process, probe.clone() as Arc<dyn Handler>);

            let init = peer
                .request(
                    "initialize",
                    serde_json::json!({ "protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {} }),
                )
                .await
                .expect("initialize");
            let init: protocol::InitializeResult = serde_json::from_value(init).unwrap();
            assert!(init.steering_supported(), "the fake adapter advertises steering");

            let session = peer
                .request("session/new", serde_json::json!({ "cwd": ".", "mcpServers": [] }))
                .await
                .expect("session/new");
            let session: protocol::NewSessionResult = serde_json::from_value(session).unwrap();

            // A turn that will not end until it is steered, so the delivery is
            // unambiguously mid-turn rather than a race that usually wins.
            let running = peer.clone();
            let session_id = session.session_id.clone();
            let turn = tokio::spawn(async move {
                running
                    .request(
                        "session/prompt",
                        serde_json::json!({
                            "sessionId": session_id,
                            "prompt": [{ "type": "text", "text": "hold" }],
                        }),
                    )
                    .await
            });

            let steer = |text: &str| {
                serde_json::to_value(protocol::SteerParams::text(session.session_id.clone(), text)).unwrap()
            };
            // Retried rather than slept on: the turn has to have reached the
            // adapter, and how long that takes is a property of the machine.
            let mut answered = None;
            for _ in 0..100 {
                let reply = peer
                    .request(protocol::STEER_METHOD, steer("stop and say STEERED"))
                    .await
                    .expect("the adapter answers a steer");
                let outcome = serde_json::from_value::<protocol::SteerResult>(reply)
                    .unwrap()
                    .outcome();
                if outcome == protocol::SteerOutcome::Injected {
                    answered = Some(outcome);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            assert_eq!(
                answered,
                Some(protocol::SteerOutcome::Injected),
                "a steer sent while a turn is running joins it"
            );

            let stopped = turn.await.unwrap().expect("the steered turn ends normally");
            let stopped: protocol::PromptResult = serde_json::from_value(stopped).unwrap();
            assert_eq!(stopped.stop_reason, "end_turn");
            peer.drain_notifications().await;
            let heard = probe.updates.lock().unwrap().join(" ");
            assert!(heard.contains("STEERED"), "the steered text reached the turn: {heard}");

            // And now with nothing running. `promptRequired` is the message
            // coming back unconsumed; `startedNewTurn` would mean the `_meta`
            // above had been dropped and a turn had been started behind us.
            let reply = peer
                .request(protocol::STEER_METHOD, steer("too late"))
                .await
                .expect("the adapter answers");
            let outcome = serde_json::from_value::<protocol::SteerResult>(reply)
                .unwrap()
                .outcome();
            assert_eq!(
                outcome,
                protocol::SteerOutcome::PromptRequired,
                "with no turn to steer the message must be handed back, not detached into one"
            );

            peer.stop().await;
        }
    }
}

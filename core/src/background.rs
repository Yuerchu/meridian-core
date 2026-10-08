//! Commands that outlive the turn that started them.
//!
//! `run_command` with `run_in_background` hands back an id straight away and
//! leaves the command running. Everything else here follows from the command
//! outliving its turn:
//!
//! - **It is run by the same path as any other command** —
//!   [`crate::sandbox::execute_teed`], with the same sandbox, the same
//!   process-tree kill and the same cancellation. What is different is that its
//!   output is written to a log as it arrives, so it can be read before it
//!   ends. A container cannot do that and is refused, rather than run without
//!   the log it was started for.
//! - **Its ending is a debt to the model**, recorded as `notified_at IS NULL`
//!   on its row (migration 66). The next native turn in the conversation pays
//!   it — [`claim`] writes a `context` row per task and marks it told,
//!   in one transaction — and a completion or failure this process watched
//!   also *wakes* a turn to pay it, through the queue's pump. A stop wakes
//!   nothing (somebody decided it), and neither does a task lost to a restart:
//!   nobody is there, which is the queue's own rule about the empty room.
//! - **The notice is context, never a user row.** It is something that
//!   happened, not something anybody said, and the automatic reviewer weighs
//!   the two differently: a notice filed as the user would be a model-written
//!   string with the user's authority.
//!
//! Desktop only, like the sandbox it runs through. A sub-agent, a QQ session,
//! the hook reviewer and the tool bridge get no [`Launcher`], which is how
//! they are refused.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::agent::engine::{Steered, SteeredOrigin, Steering};
use crate::db::entity::background_task::{self, BackgroundKind, BackgroundRunner, BackgroundState};
use crate::db::sea::cap::Db;
use crate::db::sea::ops::background_task::{self as ops, BackgroundTaskChangeset, BackgroundTaskInsert};
use crate::sandbox::{ExecError, SandboxPolicy};
use crate::services::Services;
use crate::util::now_ms;

/// How many commands one conversation may have running at once.
pub const MAX_RUNNING: u64 = 5;
/// How long a background command may run before its process tree is killed.
pub const TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);
/// How much of a command's output is kept on disk. Past it the pipes are still
/// drained — a child blocked on a full pipe would hang — but nothing is written.
pub const OUTPUT_CAP: u64 = 16 * 1024 * 1024;
/// How much of the end of the output rides the notice.
const NOTICE_TAIL: u64 = 2 * 1024;
/// The most one read hands back.
pub const READ_MAX: usize = 256 * 1024;
/// The longest a read may wait for more output.
pub const WAIT_MAX: Duration = Duration::from_secs(60);
/// What a message row written for a notice is labelled.
pub const NOTICE_SOURCE: &str = "background_task";

/// Who stopped a task, which is what its row says about why it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoppedBy {
    /// The model, with `stop_background_task`. It knows; nothing is owed.
    Model,
    /// A person, from the task list.
    User,
    /// The conversation was deleted, or the app is exiting.
    App,
}

impl StoppedBy {
    fn reason(self) -> &'static str {
        match self {
            Self::Model => "stopped by the assistant",
            Self::User => "stopped by the user",
            Self::App => "stopped because Meridian closed the conversation",
        }
    }
}

/// A task this process is running.
struct Running {
    conversation_id: String,
    cancel: CancellationToken,
    stopped_by: Arc<Mutex<Option<StoppedBy>>>,
    progress: Arc<Progress>,
}

/// How far a task's log has got, for a read that waits for more.
#[derive(Default)]
struct Progress {
    bytes: AtomicU64,
    moved: tokio::sync::Notify,
}

/// Every background command this process is running.
///
/// Only what cannot be in the database: the handles that stop them and the
/// counters a waiting read watches. Their rows are the record.
#[derive(Default)]
pub struct BackgroundTasks {
    running: Mutex<HashMap<String, Running>>,
}

impl BackgroundTasks {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Running>> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Stop one task, if it is running here and belongs to `conversation_id`.
    /// The ending is written by the task itself, once its process tree is down.
    pub fn stop(&self, conversation_id: &str, id: &str, by: StoppedBy) -> bool {
        let running = self.lock();
        let Some(task) = running.get(id).filter(|t| t.conversation_id == conversation_id) else {
            return false;
        };
        if let Ok(mut slot) = task.stopped_by.lock() {
            slot.get_or_insert(by);
        }
        task.cancel.cancel();
        true
    }

    /// Stop everything a conversation is running — it is being deleted.
    pub fn stop_conversation(&self, conversation_id: &str) -> usize {
        let running = self.lock();
        let mut stopped = 0;
        for task in running.values().filter(|t| t.conversation_id == conversation_id) {
            if let Ok(mut slot) = task.stopped_by.lock() {
                slot.get_or_insert(StoppedBy::App);
            }
            task.cancel.cancel();
            stopped += 1;
        }
        stopped
    }

    /// Stop everything — the app is exiting. A command left behind would keep
    /// writing to a project with nobody watching it and no row that will ever
    /// say how it ended.
    pub fn stop_all(&self) -> usize {
        let running = self.lock();
        for task in running.values() {
            if let Ok(mut slot) = task.stopped_by.lock() {
                slot.get_or_insert(StoppedBy::App);
            }
            task.cancel.cancel();
        }
        running.len()
    }

    /// Wait until every task has written its ending, or `limit` passes.
    pub async fn drained(&self, limit: Duration) {
        let deadline = tokio::time::Instant::now() + limit;
        while !self.lock().is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn progress(&self, id: &str) -> Option<Arc<Progress>> {
        self.lock().get(id).map(|t| Arc::clone(&t.progress))
    }
}

/// What a turn is handed to start background commands with.
///
/// `Services` behind a name, because a task outlives the turn and needs the
/// whole app when it ends: the database, the bus, the queue that wakes the
/// next turn. Present only on the desktop's own turns.
#[derive(Clone)]
pub struct Launcher(Services);

impl std::fmt::Debug for Launcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Launcher")
    }
}

/// One command to run in the background.
pub struct Start {
    pub conversation_id: String,
    pub turn_id: Option<String>,
    pub command: String,
    pub description: Option<String>,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub policy: Option<SandboxPolicy>,
}

/// A slice of a task's log, and where it stands.
#[derive(Debug)]
pub struct Output {
    pub row: background_task::Model,
    pub offset: u64,
    pub text: String,
    /// Where the next read should start.
    pub next_offset: u64,
    /// How much of the log exists right now.
    pub total: u64,
}

impl Launcher {
    pub fn new(services: Services) -> Self {
        Self(services)
    }

    /// Record the task and start it. Returns its row, as it stands running.
    pub async fn start(&self, start: Start) -> Result<background_task::Model, String> {
        let services = self.0.clone();
        if start
            .policy
            .as_ref()
            .is_some_and(|p| p.backend == crate::sandbox::SandboxBackend::Container)
        {
            return Err(
                "background commands are not available in container mode yet. Nothing was run; \
                 run it in the foreground instead."
                    .into(),
            );
        }

        let id = new_id();
        let dir = log_dir(&services.paths.data_dir, &start.conversation_id);
        std::fs::create_dir_all(&dir).map_err(|e| format!("could not create the output directory: {e}"))?;
        let path = dir.join(format!("{id}.log"));
        let file = std::fs::File::create(&path).map_err(|e| format!("could not create the output file: {e}"))?;

        let backend = start
            .policy
            .as_ref()
            .map(|p| p.backend)
            .unwrap_or(crate::sandbox::SandboxBackend::Host);
        let sandbox = serde_json::to_value(backend)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string));
        let cwd = start.cwd.display().to_string();
        let output_path = path.display().to_string();
        // The count and the insert it guards are one write: read outside it,
        // two starts could both see four running and both insert a fifth.
        let row = services
            .sea
            .write(async |tx| {
                if ops::count_running(tx, &start.conversation_id).await? >= MAX_RUNNING {
                    return Ok::<_, sea_orm::DbErr>(None);
                }
                ops::insert(
                    tx,
                    &BackgroundTaskInsert {
                        id: &id,
                        conversation_id: &start.conversation_id,
                        runner: BackgroundRunner::Native,
                        kind: BackgroundKind::Command,
                        spawned_turn_id: start.turn_id.as_deref(),
                        command: Some(&start.command),
                        description: start.description.as_deref(),
                        cwd: Some(&cwd),
                        sandbox: sandbox.as_deref(),
                        output_path: Some(&output_path),
                        started_at: now_ms(),
                    },
                )
                .await?;
                ops::get(tx, &id).await
            })
            .await
            .map_err(|e| e.to_string())?;
        let Some(row) = row else {
            let _ = std::fs::remove_file(&path);
            return Err(format!(
                "this conversation already has {MAX_RUNNING} background commands running. \
                 Stop one with stop_background_task, or wait for one to finish."
            ));
        };

        let cancel = CancellationToken::new();
        let stopped_by = Arc::new(Mutex::new(None));
        let progress = Arc::new(Progress::default());
        services.background_tasks.lock().insert(
            id.clone(),
            Running {
                conversation_id: start.conversation_id.clone(),
                cancel: cancel.clone(),
                stopped_by: Arc::clone(&stopped_by),
                progress: Arc::clone(&progress),
            },
        );
        announce(&services, &start.conversation_id);
        tracing::info!(
            conversation_id = %start.conversation_id,
            task_id = %id,
            backend = ?backend,
            "a background command started"
        );

        tokio::spawn(run(services, id, start, file, cancel, stopped_by, progress));
        Ok(row)
    }

    /// Stop one of this conversation's tasks.
    pub async fn stop(&self, conversation_id: &str, id: &str, by: StoppedBy) -> Result<background_task::Model, String> {
        let row = get_in(&self.0.sea, conversation_id, id).await?;
        if row.state != BackgroundState::Running {
            return Ok(row);
        }
        if !self.0.background_tasks.stop(conversation_id, id, by) {
            return Err(format!(
                "background task {id} is not running in this process, so it cannot be stopped"
            ));
        }
        // The task writes its own ending once its tree is down; wait briefly
        // so the answer can say how it ended rather than "stopping".
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let row = get_in(&self.0.sea, conversation_id, id).await?;
            if row.state != BackgroundState::Running || tokio::time::Instant::now() >= deadline {
                return Ok(row);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn list(&self, conversation_id: &str) -> Result<Vec<background_task::Model>, String> {
        list(&self.0.sea, conversation_id).await
    }

    /// Read a task's log from `offset`, waiting up to `wait` for something new
    /// if the task is still running and there is nothing past `offset` yet.
    pub async fn read(
        &self,
        conversation_id: &str,
        id: &str,
        offset: u64,
        max_bytes: usize,
        wait: Duration,
    ) -> Result<Output, String> {
        read(&self.0, conversation_id, id, offset, max_bytes, wait).await
    }
}

/// A conversation's tasks, oldest first.
pub async fn list(db: &Db, conversation_id: &str) -> Result<Vec<background_task::Model>, String> {
    ops::list_for_conversation(db, conversation_id)
        .await
        .map_err(|e| e.to_string())
}

/// How many commands each conversation has running, for the conversations
/// that have any — what the sidebar marks.
pub async fn running_counts(db: &Db) -> Result<Vec<(String, i64)>, String> {
    ops::running_by_conversation(db).await.map_err(|e| e.to_string())
}

/// Read a task's log. See [`Launcher::read`].
pub async fn read(
    services: &Services,
    conversation_id: &str,
    id: &str,
    offset: u64,
    max_bytes: usize,
    wait: Duration,
) -> Result<Output, String> {
    let max_bytes = max_bytes.clamp(1, READ_MAX);
    let wait = wait.min(WAIT_MAX);
    let row = get_in(&services.sea, conversation_id, id).await?;
    let path = row
        .output_path
        .clone()
        .ok_or_else(|| format!("background task {id} has no output file"))?;

    if row.state == BackgroundState::Running
        && !wait.is_zero()
        && let Some(progress) = services.background_tasks.progress(id)
        && progress.bytes.load(Ordering::Acquire) <= offset
    {
        // Registered before the second look, so a write landing between the
        // two still wakes this.
        let moved = progress.moved.notified();
        if progress.bytes.load(Ordering::Acquire) <= offset {
            let _ = tokio::time::timeout(wait, moved).await;
        }
    }

    // Read the row again: the task may have ended while this waited, and
    // what it says has to match the bytes handed back.
    let row = get_in(&services.sea, conversation_id, id).await?;
    let (text, total) = tokio::task::spawn_blocking(move || read_slice(Path::new(&path), offset, max_bytes))
        .await
        .map_err(|e| e.to_string())??;
    let next_offset = offset.min(total) + text.len() as u64;
    Ok(Output {
        row,
        offset: offset.min(total),
        text: String::from_utf8_lossy(&text).into_owned(),
        next_offset,
        total,
    })
}

fn read_slice(path: &Path, offset: u64, max_bytes: usize) -> Result<(Vec<u8>, u64), String> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(e) => return Err(format!("could not open the output file: {e}")),
    };
    let total = file.metadata().map_err(|e| e.to_string())?.len();
    let start = offset.min(total);
    file.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    file.take(max_bytes as u64)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    Ok((buf, total))
}

async fn get_in(db: &Db, conversation_id: &str, id: &str) -> Result<background_task::Model, String> {
    let row = ops::get(db, id).await.map_err(|e| e.to_string())?;
    // Another conversation's task reads as no task at all: its id is not a
    // secret, but its output may be.
    row.filter(|r| r.conversation_id == conversation_id)
        .ok_or_else(|| format!("there is no background task {id} in this conversation"))
}

/// The task itself, from spawn to the row that says how it ended.
async fn run(
    services: Services,
    id: String,
    start: Start,
    file: std::fs::File,
    cancel: CancellationToken,
    stopped_by: Arc<Mutex<Option<StoppedBy>>>,
    progress: Arc<Progress>,
) {
    let log = Arc::new(Mutex::new(Log {
        file,
        cap: OUTPUT_CAP,
        written: 0,
        truncated: false,
    }));
    let tee: crate::sandbox::OutputTee = {
        let log = Arc::clone(&log);
        let progress = Arc::clone(&progress);
        Arc::new(move |chunk: &[u8]| {
            let written = log.lock().map(|mut log| log.append(chunk)).unwrap_or(0);
            progress.bytes.store(written, Ordering::Release);
            progress.moved.notify_waiters();
        })
    };

    let result = crate::sandbox::execute_teed(
        &start.argv,
        &start.cwd,
        start.policy.as_ref(),
        TIMEOUT,
        &cancel,
        Some(tee),
    )
    .await;

    let (written, truncated) = log
        .lock()
        .map(|mut log| {
            let _ = log.file.flush();
            (log.written, log.truncated)
        })
        .unwrap_or((0, false));
    let stopped = stopped_by.lock().ok().and_then(|slot| *slot);
    let (state, exit_code, reason) = match &result {
        Err(ExecError::Cancelled) => (
            BackgroundState::Stopped,
            None,
            Some(stopped.unwrap_or(StoppedBy::App).reason().to_string()),
        ),
        Err(error) => (BackgroundState::Failed, None, Some(error.to_string())),
        Ok(done) if done.timed_out => (
            BackgroundState::Failed,
            None,
            Some(format!(
                "timed out after {} hours; process tree killed",
                TIMEOUT.as_secs() / 3600
            )),
        ),
        Ok(done) if done.exit_code == 0 => (BackgroundState::Completed, Some(0), None),
        Ok(done) => (BackgroundState::Failed, Some(done.exit_code), None),
    };

    let told = stopped == Some(StoppedBy::Model);
    let written_db = services
        .sea
        .write(async |tx| {
            let changed = ops::finish(
                tx,
                &id,
                &BackgroundTaskChangeset {
                    state,
                    exit_code,
                    ended_reason: reason.as_deref(),
                    output_bytes: written as i64,
                    output_truncated: truncated,
                    ended_at: now_ms(),
                },
            )
            .await?;
            // The model stopped it, so it already knows. Owing it a notice
            // would have the next turn announce the model's own action back
            // to it.
            if told && changed == 1 {
                ops::mark_notified(tx, &id, None, now_ms()).await?;
            }
            Ok::<_, sea_orm::DbErr>(changed)
        })
        .await;
    if let Err(error) = written_db {
        tracing::error!(%error, task_id = %id, "could not record how a background command ended");
    }

    services.background_tasks.lock().remove(&id);
    progress.moved.notify_waiters();
    tracing::info!(
        conversation_id = %start.conversation_id,
        task_id = %id,
        state = state.as_str(),
        exit_code,
        bytes = written,
        "a background command ended"
    );
    announce(&services, &start.conversation_id);

    // Only an ending this process watched, and only one somebody did not
    // choose, is worth a turn. The pump decides whether one can start now; a
    // turn already running takes the notice at its next round instead.
    if matches!(state, BackgroundState::Completed | BackgroundState::Failed) {
        crate::agent::queue::pump_later(&services, &start.conversation_id);
    }
}

/// The on-disk log of one task.
struct Log {
    file: std::fs::File,
    cap: u64,
    written: u64,
    truncated: bool,
}

impl Log {
    /// Append what fits under the cap; answer how much is on disk.
    fn append(&mut self, chunk: &[u8]) -> u64 {
        let room = self.cap.saturating_sub(self.written) as usize;
        let take = chunk.len().min(room);
        if take < chunk.len() {
            self.truncated = true;
        }
        if take > 0 && self.file.write_all(&chunk[..take]).is_ok() {
            self.written += take as u64;
        }
        self.written
    }
}

/// Tell every window a conversation's task list moved.
fn announce(services: &Services, conversation_id: &str) {
    let _ = services
        .events
        .emit_background_tasks_updated(&crate::events::BackgroundTasksUpdatedEvent::new(conversation_id));
}

fn new_id() -> String {
    let raw = uuid::Uuid::new_v4().simple().to_string();
    format!("b{}", &raw[..8])
}

/// Where a conversation's logs live. Removed with the conversation.
pub fn log_dir(data_dir: &Path, conversation_id: &str) -> PathBuf {
    data_dir.join("background").join(conversation_id)
}

/// Whether a conversation has an ending worth starting a turn for.
pub async fn has_wake(db: &Db, conversation_id: &str) -> bool {
    match ops::unnotified(db, conversation_id, true).await {
        Ok(rows) => !rows.is_empty(),
        Err(error) => {
            tracing::warn!(%error, conversation_id, "could not read whether a background task is owed a turn");
            false
        }
    }
}

/// One notice paid: the row it became, the task it is about, and what it says.
#[derive(Debug, Clone)]
pub struct Notice {
    pub message_id: String,
    pub task_id: String,
    pub text: String,
    /// The row's `created_at`, which is also when the model is told it arrived.
    pub created_at: i64,
}

/// Pay every notice a conversation owes, as `context` rows hung off its head,
/// in one transaction with the claims. Oldest ending first.
///
/// One `BEGIN IMMEDIATE`, because the read and the claims are one step: two
/// turns can both get here — a wake and a round boundary — and the write lock
/// taken up front is what makes the second one find nothing.
pub async fn claim(db: &Db, conversation_id: &str, turn_id: &str) -> Result<Vec<Notice>, String> {
    let now = now_ms();
    db.write(async |tx| {
        let owed = ops::unnotified(tx, conversation_id, false).await?;
        let mut written = Vec::new();
        for task in owed {
            if ops::mark_notified(tx, &task.id, Some(turn_id), now).await? == 0 {
                continue;
            }
            let text = notice(&task);
            let head = crate::db::sea::ops::conversation::get_conversation(tx, conversation_id)
                .await?
                .and_then(|c| c.head_message_id);
            let message_id = uuid::Uuid::new_v4().to_string();
            crate::db::sea::ops::message::append_context(
                tx,
                &crate::db::sea::ops::message::ContextRowInsert {
                    id: &message_id,
                    conversation_id,
                    content: &text,
                    source: NOTICE_SOURCE,
                    turn_id: Some(turn_id),
                    created_at: now,
                },
                head.as_deref(),
            )
            .await?;
            written.push(Notice {
                message_id,
                task_id: task.id.clone(),
                text,
                created_at: now,
            });
        }
        Ok::<_, sea_orm::DbErr>(written)
    })
    .await
    .map_err(|e| e.to_string())
}

/// The id of the task a wake would be answering, read before the turn opens so
/// its record can name it. Only a hint: the claim inside the turn is what
/// decides which notices it pays, and it may find another turn got there first.
pub async fn first_wake(db: &Db, conversation_id: &str) -> Option<String> {
    ops::unnotified(db, conversation_id, true)
        .await
        .ok()
        .and_then(|rows| rows.into_iter().next())
        .map(|row| row.id)
}

/// What the model is told about one task that ended.
///
/// Fixed prose around the facts, and the end of the log verbatim — the log is
/// the command's own output, which the model would have seen in the foreground
/// too.
pub fn notice(task: &background_task::Model) -> String {
    let status = match task.state {
        BackgroundState::Completed => "completed (exit code 0)".to_string(),
        BackgroundState::Failed => match (task.exit_code, task.ended_reason.as_deref()) {
            (Some(code), _) => format!("failed (exit code {code})"),
            (None, Some(reason)) => format!("failed: {reason}"),
            (None, None) => "failed".to_string(),
        },
        BackgroundState::Stopped => format!("stopped ({})", task.ended_reason.as_deref().unwrap_or("stopped")),
        BackgroundState::Lost => "lost: Meridian stopped while it was running, so how it ended is unknown. \
             Check whatever it was meant to produce before relying on it"
            .to_string(),
        BackgroundState::Running => "running".to_string(),
    };
    let mut out = String::from("<background_task_notification>\n");
    out.push_str(&format!("task_id: {}\n", task.id));
    out.push_str(&format!("status: {status}\n"));
    if let Some(command) = &task.command {
        out.push_str(&format!("command: {command}\n"));
    }
    if let Some(description) = &task.description {
        out.push_str(&format!("description: {description}\n"));
    }
    if let Some(path) = &task.output_path {
        let truncated = if task.output_truncated.get() {
            ", truncated at the cap"
        } else {
            ""
        };
        out.push_str(&format!(
            "output: {} bytes{truncated}; read it with read_background_output\n",
            task.output_bytes
        ));
        if let Some(tail) = tail(Path::new(path), NOTICE_TAIL)
            && !tail.trim().is_empty()
        {
            out.push_str("output_tail:\n");
            out.push_str(tail.trim_end());
            out.push('\n');
        }
    }
    out.push_str("</background_task_notification>");
    out
}

fn tail(path: &Path, bytes: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(bytes))).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Background endings, offered to a running turn at each round boundary.
///
/// The same claim [`claim`] makes at the start of a turn, for a task
/// that ends while the turn is running: waking another turn would have to wait
/// for this one, and the model working now is the one that wants to know.
pub struct TaskNotices {
    db: Db,
    conversation_id: String,
    turn_id: String,
}

impl TaskNotices {
    pub fn new(db: Db, conversation_id: String, turn_id: String) -> Self {
        Self {
            db,
            conversation_id,
            turn_id,
        }
    }
}

#[async_trait::async_trait]
impl Steering for TaskNotices {
    async fn drain(&self) -> Vec<Steered> {
        match claim(&self.db, &self.conversation_id, &self.turn_id).await {
            Ok(notices) => notices
                .into_iter()
                .map(|notice| Steered {
                    text: notice.text,
                    origin: SteeredOrigin::System,
                    row: Some(notice.message_id),
                    received_at: notice.created_at,
                })
                .collect(),
            Err(error) => {
                tracing::warn!(%error, conversation_id = %self.conversation_id, "could not take background notices");
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The platform shell running `script`, the way `run_command` builds it.
    fn argv(script: &str) -> Vec<String> {
        if cfg!(windows) {
            vec!["cmd".into(), "/C".into(), script.into()]
        } else {
            vec!["/bin/sh".into(), "-c".into(), script.into()]
        }
    }

    /// Print, pause, print: the shape of anything worth running in the
    /// background, in each shell's words.
    fn slow_echo() -> Vec<String> {
        if cfg!(windows) {
            argv("echo first && ping -n 3 127.0.0.1 >NUL && echo second")
        } else {
            argv("echo first; sleep 2; echo second")
        }
    }

    /// A conversation row, written the way the SeaORM tests write theirs.
    async fn conversation(services: &Services, id: &str) {
        crate::db::sea::execute_for_tests(
            &services.sea,
            &format!(
                "INSERT INTO conversations (id, title, is_pinned, is_archived, message_count, created_at, updated_at, fast_mode)
                 VALUES ('{id}', 't', 0, 0, 0, 0, 0, 0)"
            ),
        )
        .await
        .unwrap();
    }

    async fn setup(dir: &Path) -> (Services, Launcher) {
        let services = crate::services::bare_services(dir).await;
        conversation(&services, "c1").await;
        let launcher = Launcher::new(services.clone());
        (services, launcher)
    }

    fn start(argv: Vec<String>, dir: &Path) -> Start {
        Start {
            conversation_id: "c1".into(),
            turn_id: Some("t1".into()),
            command: argv.last().unwrap().clone(),
            description: Some("a test".into()),
            argv,
            cwd: dir.to_path_buf(),
            policy: None,
        }
    }

    async fn ended(launcher: &Launcher, id: &str) -> background_task::Model {
        for _ in 0..400 {
            let row = get_in(&launcher.0.sea, "c1", id).await.unwrap();
            if row.state != BackgroundState::Running {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("task {id} never ended");
    }

    /// The whole life of one: it answers at once, keeps running, writes its
    /// log as it goes, and ends owing the model a notice worth a turn.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_command_runs_on_after_it_is_started_and_leaves_a_debt() {
        let dir = tempfile::tempdir().unwrap();
        let (services, launcher) = setup(dir.path()).await;

        let row = launcher.start(start(slow_echo(), dir.path())).await.unwrap();
        assert_eq!(
            row.state,
            BackgroundState::Running,
            "answered before the command finished"
        );
        assert_eq!(row.spawned_turn_id.as_deref(), Some("t1"));

        let row = ended(&launcher, &row.id).await;
        assert_eq!(row.state, BackgroundState::Completed);
        assert_eq!(row.exit_code, Some(0));
        let log = std::fs::read_to_string(row.output_path.as_ref().unwrap()).unwrap();
        assert!(log.contains("first") && log.contains("second"), "{log:?}");
        assert_eq!(row.output_bytes as usize, log.len());
        assert!(services.background_tasks.lock().is_empty(), "no longer registered");
        assert!(
            has_wake(&services.sea, "c1").await,
            "a completion this process saw is worth a turn"
        );
    }

    /// A notice is paid once, as context, with the end of the log in it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_notice_is_context_and_is_paid_once() {
        let dir = tempfile::tempdir().unwrap();
        let (services, launcher) = setup(dir.path()).await;
        let row = launcher
            .start(start(argv("echo hello-notice"), dir.path()))
            .await
            .unwrap();
        ended(&launcher, &row.id).await;

        let notices = claim(&services.sea, "c1", "t2").await.unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].task_id, row.id);
        assert!(notices[0].text.contains("status: completed"), "{}", notices[0].text);
        assert!(
            notices[0].text.contains("hello-notice"),
            "the tail rides along: {}",
            notices[0].text
        );

        let stored = crate::db::sea::ops::message::get_message(&services.sea, &notices[0].message_id)
            .await
            .unwrap()
            .expect("the notice row exists");
        assert_eq!(stored.role, "context", "never a user row");
        assert_eq!(stored.source.as_deref(), Some(NOTICE_SOURCE));
        assert_eq!(stored.turn_id.as_deref(), Some("t2"));

        assert!(claim(&services.sea, "c1", "t3").await.unwrap().is_empty(), "paid once");
        assert!(!has_wake(&services.sea, "c1").await);
    }

    /// The model stopping its own task needs no notice about it; a person
    /// stopping it is owed one, but not a turn.
    #[tokio::test(flavor = "multi_thread")]
    async fn who_stopped_it_decides_what_is_owed() {
        let dir = tempfile::tempdir().unwrap();
        let (services, launcher) = setup(dir.path()).await;
        let long = if cfg!(windows) {
            argv("ping -n 30 127.0.0.1 >NUL")
        } else {
            argv("sleep 30")
        };

        let by_model = launcher.start(start(long.clone(), dir.path())).await.unwrap();
        let row = launcher.stop("c1", &by_model.id, StoppedBy::Model).await.unwrap();
        assert_eq!(row.state, BackgroundState::Stopped);
        assert!(row.notified_at.is_some(), "the model knows what it did");

        let by_user = launcher.start(start(long, dir.path())).await.unwrap();
        let row = launcher.stop("c1", &by_user.id, StoppedBy::User).await.unwrap();
        assert_eq!(row.state, BackgroundState::Stopped);
        assert_eq!(row.ended_reason.as_deref(), Some("stopped by the user"));
        assert!(row.notified_at.is_none(), "owed to the next turn");
        assert!(!has_wake(&services.sea, "c1").await, "but nobody asked for a turn");
    }

    /// Another conversation's task does not exist from here — not to read, not
    /// to stop.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_task_belongs_to_its_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let (services, launcher) = setup(dir.path()).await;
        conversation(&services, "c2").await;
        let row = launcher.start(start(argv("echo mine"), dir.path())).await.unwrap();
        assert!(launcher.read("c2", &row.id, 0, READ_MAX, Duration::ZERO).await.is_err());
        assert!(launcher.stop("c2", &row.id, StoppedBy::Model).await.is_err());
        ended(&launcher, &row.id).await;
    }

    /// A read that waits is woken by the next write, not by its timeout.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_waiting_read_is_answered_by_new_output() {
        let dir = tempfile::tempdir().unwrap();
        let (_services, launcher) = setup(dir.path()).await;
        let row = launcher.start(start(slow_echo(), dir.path())).await.unwrap();

        let mut first = launcher
            .read("c1", &row.id, 0, READ_MAX, Duration::from_secs(10))
            .await
            .unwrap();
        for _ in 0..40 {
            if first.text.contains("first") {
                break;
            }
            first = launcher
                .read("c1", &row.id, 0, READ_MAX, Duration::from_millis(250))
                .await
                .unwrap();
        }
        assert!(first.text.contains("first"), "{:?}", first.text);

        let started = std::time::Instant::now();
        let second = launcher
            .read("c1", &row.id, first.next_offset, READ_MAX, Duration::from_secs(20))
            .await
            .unwrap();
        assert!(second.text.contains("second") || second.row.state != BackgroundState::Running);
        assert!(started.elapsed() < Duration::from_secs(15), "woken, not timed out");
        assert!(!second.text.contains("first"), "read from the offset, not the start");
    }

    /// Five is the ceiling, and the sixth is refused without leaving a row or
    /// a log behind.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_conversation_runs_at_most_five() {
        let dir = tempfile::tempdir().unwrap();
        let (services, launcher) = setup(dir.path()).await;
        let long = if cfg!(windows) {
            argv("ping -n 30 127.0.0.1 >NUL")
        } else {
            argv("sleep 30")
        };
        let mut ids = Vec::new();
        for _ in 0..MAX_RUNNING {
            ids.push(launcher.start(start(long.clone(), dir.path())).await.unwrap().id);
        }
        let refused = launcher.start(start(long, dir.path())).await.unwrap_err();
        assert!(refused.contains("already has 5"), "{refused}");
        assert_eq!(list(&services.sea, "c1").await.unwrap().len(), MAX_RUNNING as usize);
        assert_eq!(services.background_tasks.stop_conversation("c1"), MAX_RUNNING as usize);
        for id in &ids {
            assert_eq!(ended(&launcher, id).await.state, BackgroundState::Stopped);
        }
    }

    /// A container returns its output only once it has finished, so a
    /// background command there would have no log. Refused, and nothing runs.
    #[tokio::test]
    async fn a_container_is_refused_rather_than_run_without_a_log() {
        let dir = tempfile::tempdir().unwrap();
        let (services, launcher) = setup(dir.path()).await;
        let mut request = start(argv("echo no"), dir.path());
        request.policy = Some(SandboxPolicy {
            backend: crate::sandbox::SandboxBackend::Container,
            ..Default::default()
        });
        assert!(launcher.start(request).await.is_err());
        assert!(list(&services.sea, "c1").await.unwrap().is_empty());
    }

    /// Past the cap nothing more is written, and the log says it was cut.
    #[test]
    fn the_log_stops_at_its_cap_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.log");
        let mut log = Log {
            file: std::fs::File::create(&path).unwrap(),
            cap: 10,
            written: 0,
            truncated: false,
        };
        assert_eq!(log.append(b"123456"), 6);
        assert!(!log.truncated);
        assert_eq!(log.append(b"789abc"), 10);
        assert!(log.truncated);
        assert_eq!(log.append(b"more"), 10);
        drop(log);
        assert_eq!(std::fs::read(&path).unwrap(), b"123456789a");
    }

    struct Fixed(Vec<&'static str>, Option<std::collections::HashSet<String>>);

    #[async_trait::async_trait]
    impl Steering for Fixed {
        async fn drain(&self) -> Vec<Steered> {
            self.0
                .iter()
                .map(|t| Steered::typed((*t).into(), SteeredOrigin::System, 0))
                .collect()
        }
        fn narrowed(&self) -> Option<std::collections::HashSet<String>> {
            self.1.clone()
        }
    }

    /// A chain drains in order and can only narrow.
    #[tokio::test]
    async fn a_chain_keeps_order_and_only_narrows() {
        let set = |xs: &[&str]| {
            Some(
                xs.iter()
                    .map(|x| x.to_string())
                    .collect::<std::collections::HashSet<_>>(),
            )
        };
        let typed = Fixed(vec!["typed"], set(&["a", "b"]));
        let notice = Fixed(vec!["notice"], set(&["b", "c"]));
        let open = Fixed(vec![], None);
        let chain = crate::agent::engine::Chain(vec![&typed, &notice, &open]);
        let texts: Vec<_> = chain.drain().await.into_iter().map(|s| s.text).collect();
        assert_eq!(texts, ["typed", "notice"]);
        assert_eq!(chain.narrowed(), set(&["b"]));
        assert_eq!(crate::agent::engine::Chain(vec![&open]).narrowed(), None);
    }
}

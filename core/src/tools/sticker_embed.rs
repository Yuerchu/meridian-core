//! Optional on-device sticker embeddings with durable SQLite storage and
//! background indexing.
//!
//! Indexing (write path) and query matching (read path) are strictly separated:
//! - Background reconciliation runs on a dedicated single-owner worker thread
//!   (never on Tokio worker threads), hashes raw sticker files to skip
//!   unchanged rows without decoding GIF frames, limits CPU threads, and
//!   persists vectors into `emoji_embeddings`.
//! - `send_sticker` (read path) never embeds stickers on the fly: it
//!   short-circuits on `sticker_id` and exact name matches, reads already
//!   indexed vectors from SQLite, and submits at most one high-priority query
//!   text embedding with a hard timeout, failing open to lexical ranking.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, UNIX_EPOCH};

use litert_lm::embedding::{EmbeddingConfig, EmbeddingEngine, EmbeddingPart};
use litert_lm::engine::Backend;
use litert_lm::model_info::ModelKind;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::sticker::{pick_from_roster, pick_from_roster_with_vectors, try_fast_pick};
use crate::db::entity::emoji;
use crate::db::sea::cap::Db;
use crate::db::sea::ops::{emoji as emoji_ops, emoji_embedding as embed_ops, preference as pref_ops};
use crate::util::now_ms;

pub const EMBEDDER_PREFERENCE: &str = "emoji.embedder";

const QUERY_PREFIX: &str = "task: search query | query: ";
const DOCUMENT_PREFIX: &str = "task: search result | query: ";

/// Hard ceiling for an online `send_sticker` query embedding. Exceeding it
/// fails open to lexical matching so a busy background index or slow model
/// can never stall a chat turn or OneBot tool call.
const DEFAULT_QUERY_TIMEOUT: Duration = Duration::from_secs(3);

/// Cooperative yield between stickers during background indexing so co-located
/// services (e.g. QQNT / SnowLuma on a NAS) are not CPU-starved.
const DEFAULT_INDEX_PACE: Duration = Duration::from_millis(50);

/// Maximum raw file size hashed for source fingerprinting (matches `emoji.rs`).
const MAX_HASH_FILE_SIZE: u64 = 5 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbedderState {
    Disabled,
    Loading,
    Indexing,
    Ready,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedderStatusSnapshot {
    pub state: EmbedderState,
    pub vision: bool,
    pub indexed: u64,
    pub total: u64,
    pub error: Option<String>,
    /// Increments only when a run actually performs indexing work or hits an
    /// error, so warm restarts hitting 100% SQLite cache do not raise a noisy
    /// system notice.
    pub work_run_id: u64,
}

impl Default for EmbedderStatusSnapshot {
    fn default() -> Self {
        Self {
            state: EmbedderState::Disabled,
            vision: false,
            indexed: 0,
            total: 0,
            error: None,
            work_run_id: 0,
        }
    }
}

pub fn document_text(name: &str, tags: Option<&str>) -> String {
    match tags.map(str::trim).filter(|s| !s.is_empty()) {
        Some(tags) => format!("{DOCUMENT_PREFIX}{name} {tags}"),
        None => format!("{DOCUMENT_PREFIX}{name}"),
    }
}

pub fn query_text(query: &str) -> String {
    format!("{QUERY_PREFIX}{query}")
}

/// Content fingerprint computed from metadata plus the raw on-disk sticker
/// bytes (without decoding GIF/APNG frames), so startup reconciliation skips
/// unchanged stickers in milliseconds while still catching any file edit.
pub fn source_hash(name: &str, tags: Option<&str>, file_name: &str, raw_image: Option<&[u8]>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.update([0x1f]);
    hasher.update(tags.unwrap_or("").as_bytes());
    hasher.update([0x1f]);
    hasher.update(file_name.as_bytes());
    hasher.update([0x1f]);
    if let Some(image) = raw_image {
        hasher.update(image);
    }
    format!("{:x}", hasher.finalize())
}

fn default_embedder_threads() -> i32 {
    let cores = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(2);
    (cores / 2).clamp(1, 4) as i32
}

fn compute_model_fingerprint(path: &Path, vision: bool) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("cannot stat embedder model: {e}"))?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    Ok(format!(
        "{}|{}|{}|vision={}",
        canonical.display(),
        meta.len(),
        mtime_ms,
        u8::from(vision)
    ))
}

fn read_raw_file_bytes(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_HASH_FILE_SIZE {
        return None;
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_HASH_FILE_SIZE + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_HASH_FILE_SIZE {
        return None;
    }
    Some(bytes)
}

fn compute_sticker_source_hash(data_dir: &Path, sticker: &emoji::Model, vision: bool) -> String {
    let raw = if vision && !sticker.file_name.is_empty() {
        let path = crate::emoji::emoji_path(data_dir, &sticker.pack_id, &sticker.file_name);
        read_raw_file_bytes(&path)
    } else {
        None
    };
    source_hash(
        &sticker.name,
        sticker.tags.as_deref(),
        &sticker.file_name,
        raw.as_deref(),
    )
}

fn prepare_sticker_embedding_image(data_dir: &Path, sticker: &emoji::Model, vision: bool) -> Option<Vec<u8>> {
    if !vision || sticker.file_name.is_empty() {
        return None;
    }
    let path = crate::emoji::emoji_path(data_dir, &sticker.pack_id, &sticker.file_name);
    match crate::emoji::embedding_image_bytes(&path) {
        Ok(bytes) => Some(bytes),
        Err(error) => {
            tracing::warn!(error = %error, sticker_id = %sticker.id, "sticker image skipped for embedding");
            None
        }
    }
}

/// Abstract embedding engine so unit tests can drive concurrency, preemption,
/// timeouts, and persistence without loading a 740 MB `.litertlm` binary.
pub(crate) trait EmbedEngine: Send + 'static {
    fn vision(&self) -> bool;
    fn model_fingerprint(&self) -> &str;
    fn embed_text(&self, text: &str) -> Result<Vec<f32>, String>;
    fn embed_image_and_text(&self, image: &[u8], text: &str) -> Result<Vec<f32>, String>;
}

pub(crate) type EngineLoader = Arc<dyn Fn(&Path, &Path) -> Result<Box<dyn EmbedEngine>, String> + Send + Sync>;

struct LiteRtEmbedEngine {
    engine: EmbeddingEngine,
    vision: bool,
    fingerprint: String,
}

impl EmbedEngine for LiteRtEmbedEngine {
    fn vision(&self) -> bool {
        self.vision
    }

    fn model_fingerprint(&self) -> &str {
        &self.fingerprint
    }

    fn embed_text(&self, text: &str) -> Result<Vec<f32>, String> {
        self.engine.embed_text(text).map_err(|e| e.to_string())
    }

    fn embed_image_and_text(&self, image: &[u8], text: &str) -> Result<Vec<f32>, String> {
        match self
            .engine
            .embed_parts(&[EmbeddingPart::Image(image), EmbeddingPart::Text(text)])
        {
            Ok(vec) => Ok(vec),
            Err(error) => {
                tracing::warn!(error = %error, "sticker image embed failed; using name and tags");
                self.engine.embed_text(text).map_err(|e| e.to_string())
            }
        }
    }
}

fn vision_backend(info: &litert_lm::model_info::ModelInfo) -> Option<Backend> {
    let backends = info.backends(litert_lm::model_info::Modality::Vision);
    if backends.contains(&Backend::Cpu) {
        Some(Backend::Cpu)
    } else {
        backends.first().copied()
    }
}

fn open_litert_engine(path: &Path, data_dir: &Path) -> Result<Box<dyn EmbedEngine>, String> {
    let rt = crate::provider::litert_lm_provider::runtime_for_embedder()?;
    let path_str = path.to_str().ok_or("embedder path is not UTF-8")?;
    let info = rt.inspect_model(path_str).map_err(|e| e.to_string())?;
    if info.kind != ModelKind::Embedding {
        return Err("emoji.embedder must be an embedding .litertlm, not a chat model".into());
    }
    let mut config = EmbeddingConfig::new(path_str, Backend::Cpu);
    config.num_threads = Some(default_embedder_threads());
    let cache_dir = data_dir.join("cache").join("emoji_embedder");
    if std::fs::create_dir_all(&cache_dir).is_ok()
        && let Some(dir_str) = cache_dir.to_str()
    {
        config.cache_dir = Some(dir_str.to_owned());
    }

    let vision = vision_backend(&info).or(Some(Backend::Cpu));
    if let Some(backend) = vision {
        config.vision_backend = Some(backend);
        match EmbeddingEngine::new(&rt, &config) {
            Ok(engine) => {
                let fingerprint = compute_model_fingerprint(path, true)?;
                return Ok(Box::new(LiteRtEmbedEngine {
                    engine,
                    vision: true,
                    fingerprint,
                }));
            }
            Err(error) => {
                tracing::warn!(error = %error, "embedder vision backend refused; using text only");
                config.vision_backend = None;
            }
        }
    }
    let engine = EmbeddingEngine::new(&rt, &config).map_err(|e| e.to_string())?;
    let fingerprint = compute_model_fingerprint(path, false)?;
    Ok(Box::new(LiteRtEmbedEngine {
        engine,
        vision: false,
        fingerprint,
    }))
}

#[derive(Debug, Clone)]
struct LoadedMeta {
    generation: u64,
    model_fingerprint: String,
    vision: bool,
}

enum HighCmd {
    Configure {
        generation: u64,
        path: Option<PathBuf>,
        reply: oneshot::Sender<Result<Option<LoadedMeta>, String>>,
    },
    EmbedQuery {
        generation: u64,
        text: String,
        reply: oneshot::Sender<Result<Vec<f32>, String>>,
    },
}

struct LowCmd {
    generation: u64,
    text: String,
    image: Option<Vec<u8>>,
    reply: oneshot::Sender<Result<Vec<f32>, String>>,
}

struct WorkerQueue {
    closed: bool,
    high: VecDeque<HighCmd>,
    low: VecDeque<LowCmd>,
}

/// Single-owner blocking thread for `EmbedEngine`.
///
/// Online queries arrive on `high` and always preempt queued background
/// document jobs on `low`. Spawns lazily on first non-empty configure so
/// machines without `emoji.embedder` pay zero threads and zero RAM.
struct EngineWorker {
    queue: Arc<(Mutex<WorkerQueue>, Condvar)>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    loader: EngineLoader,
    data_dir: PathBuf,
}

impl EngineWorker {
    fn new(data_dir: PathBuf, loader: EngineLoader) -> Self {
        Self {
            queue: Arc::new((
                Mutex::new(WorkerQueue {
                    closed: false,
                    high: VecDeque::new(),
                    low: VecDeque::new(),
                }),
                Condvar::new(),
            )),
            thread: Mutex::new(None),
            loader,
            data_dir,
        }
    }

    fn ensure_started(&self) {
        let mut slot = self.thread.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return;
        }
        let queue = Arc::clone(&self.queue);
        let loader = Arc::clone(&self.loader);
        let data_dir = self.data_dir.clone();
        let handle = std::thread::Builder::new()
            .name("emoji-embedder".into())
            .spawn(move || run_worker_loop(queue, loader, data_dir))
            .expect("failed to spawn emoji-embedder thread");
        *slot = Some(handle);
    }

    fn is_started(&self) -> bool {
        self.thread.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    async fn configure(&self, generation: u64, path: Option<PathBuf>) -> Result<Option<LoadedMeta>, String> {
        if path.is_none() && !self.is_started() {
            return Ok(None);
        }
        self.ensure_started();
        let (tx, rx) = oneshot::channel();
        {
            let (lock, cv) = &*self.queue;
            let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
            if q.closed {
                return Err("embedder worker is closed".into());
            }
            // Drop any stale background document jobs from superseded generations.
            q.low.retain(|job| job.generation >= generation);
            q.high.push_back(HighCmd::Configure {
                generation,
                path,
                reply: tx,
            });
            cv.notify_one();
        }
        rx.await.map_err(|_| "embedder worker stopped".to_string())?
    }

    async fn embed_query(&self, generation: u64, text: String, timeout: Duration) -> Result<Vec<f32>, String> {
        if !self.is_started() {
            return Err("embedder is not loaded".into());
        }
        let (tx, rx) = oneshot::channel();
        {
            let (lock, cv) = &*self.queue;
            let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
            if q.closed {
                return Err("embedder worker is closed".into());
            }
            q.high.push_back(HighCmd::EmbedQuery {
                generation,
                text,
                reply: tx,
            });
            cv.notify_one();
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err("embedder query cancelled".into()),
            Err(_) => Err(format!("embedder query timed out after {}ms", timeout.as_millis())),
        }
    }

    async fn embed_document(&self, generation: u64, text: String, image: Option<Vec<u8>>) -> Result<Vec<f32>, String> {
        if !self.is_started() {
            return Err("embedder is not loaded".into());
        }
        let (tx, rx) = oneshot::channel();
        {
            let (lock, cv) = &*self.queue;
            let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
            if q.closed {
                return Err("embedder worker is closed".into());
            }
            q.low.push_back(LowCmd {
                generation,
                text,
                image,
                reply: tx,
            });
            cv.notify_one();
        }
        rx.await.map_err(|_| "embedder worker stopped".to_string())?
    }
}

impl Drop for EngineWorker {
    fn drop(&mut self) {
        {
            let (lock, cv) = &*self.queue;
            let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
            q.closed = true;
            q.high.clear();
            q.low.clear();
            cv.notify_all();
        }
        if let Some(handle) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = handle.join();
        }
    }
}

enum NextWork {
    High(HighCmd),
    Low(LowCmd),
    Stop,
}

fn run_worker_loop(queue: Arc<(Mutex<WorkerQueue>, Condvar)>, loader: EngineLoader, data_dir: PathBuf) {
    let mut active_gen: u64 = 0;
    let mut loaded_path: Option<PathBuf> = None;
    let mut engine: Option<Box<dyn EmbedEngine>> = None;

    loop {
        let next = {
            let (lock, cv) = &*queue;
            let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if q.closed {
                    break NextWork::Stop;
                }
                if let Some(cmd) = q.high.pop_front() {
                    break NextWork::High(cmd);
                }
                if let Some(cmd) = q.low.pop_front() {
                    break NextWork::Low(cmd);
                }
                q = cv.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };

        match next {
            NextWork::Stop => break,
            NextWork::High(HighCmd::Configure {
                generation,
                path,
                reply,
            }) => {
                if generation < active_gen {
                    let _ = reply.send(Err("stale embedder configure".into()));
                    continue;
                }
                active_gen = generation;
                match path {
                    None => {
                        engine = None;
                        loaded_path = None;
                        let _ = reply.send(Ok(None));
                    }
                    Some(path) => {
                        if loaded_path.as_deref() == Some(path.as_path())
                            && let Some(ref existing) = engine
                        {
                            let _ = reply.send(Ok(Some(LoadedMeta {
                                generation,
                                model_fingerprint: existing.model_fingerprint().to_owned(),
                                vision: existing.vision(),
                            })));
                            continue;
                        }
                        // Unload old model before loading the replacement to halve peak RAM.
                        engine = None;
                        loaded_path = None;
                        match loader(&path, &data_dir) {
                            Ok(new_engine) => {
                                let meta = LoadedMeta {
                                    generation,
                                    model_fingerprint: new_engine.model_fingerprint().to_owned(),
                                    vision: new_engine.vision(),
                                };
                                loaded_path = Some(path);
                                engine = Some(new_engine);
                                let _ = reply.send(Ok(Some(meta)));
                            }
                            Err(error) => {
                                let _ = reply.send(Err(error));
                            }
                        }
                    }
                }
            }
            NextWork::High(HighCmd::EmbedQuery {
                generation,
                text,
                reply,
            }) => {
                if reply.is_closed() {
                    continue;
                }
                let result = match engine.as_ref() {
                    Some(eng) if generation == active_gen => eng.embed_text(&text),
                    _ => Err("embedder is not ready for this generation".into()),
                };
                let _ = reply.send(result);
            }
            NextWork::Low(LowCmd {
                generation,
                text,
                image,
                reply,
            }) => {
                if reply.is_closed() || generation != active_gen {
                    continue;
                }
                let result = match engine.as_ref() {
                    Some(eng) => match image.as_deref() {
                        Some(bytes) => eng.embed_image_and_text(bytes, &text),
                        None => eng.embed_text(&text),
                    },
                    None => Err("embedder is not loaded".into()),
                };
                let _ = reply.send(result);
            }
        }
    }
}

struct ActiveControl {
    cancel: CancellationToken,
    pending_ids: Arc<Mutex<VecDeque<String>>>,
    wake: Arc<Notify>,
    refresh_counts: Arc<Notify>,
}

/// Long-lived coordinator for on-device sticker embeddings, owned by `Services`.
pub struct StickerEmbedIndex {
    db: Db,
    data_dir: PathBuf,
    worker: EngineWorker,
    generation: AtomicU64,
    work_run_seq: AtomicU64,
    active_meta: RwLock<Option<LoadedMeta>>,
    control: Mutex<Option<ActiveControl>>,
    status_tx: watch::Sender<EmbedderStatusSnapshot>,
    query_timeout: Duration,
    index_pace: Duration,
}

impl StickerEmbedIndex {
    pub fn new(db: Db, data_dir: PathBuf) -> Arc<Self> {
        Self::with_loader(
            db,
            data_dir,
            Arc::new(open_litert_engine),
            DEFAULT_QUERY_TIMEOUT,
            DEFAULT_INDEX_PACE,
        )
    }

    pub(crate) fn with_loader(
        db: Db,
        data_dir: PathBuf,
        loader: EngineLoader,
        query_timeout: Duration,
        index_pace: Duration,
    ) -> Arc<Self> {
        let (status_tx, _) = watch::channel(EmbedderStatusSnapshot::default());
        Arc::new(Self {
            db,
            data_dir: data_dir.clone(),
            worker: EngineWorker::new(data_dir, loader),
            generation: AtomicU64::new(0),
            work_run_seq: AtomicU64::new(0),
            active_meta: RwLock::new(None),
            control: Mutex::new(None),
            status_tx,
            query_timeout,
            index_pace,
        })
    }

    pub fn status(&self) -> EmbedderStatusSnapshot {
        self.status_tx.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<EmbedderStatusSnapshot> {
        self.status_tx.subscribe()
    }

    fn publish_status(&self, snapshot: EmbedderStatusSnapshot) {
        self.status_tx.send_replace(snapshot);
    }

    fn next_work_run_id(&self) -> u64 {
        self.work_run_seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Read `emoji.embedder` from preferences and start background
    /// reconciliation if configured. Safe to call at app startup; does nothing
    /// when unset.
    pub async fn start_from_preference(self: &Arc<Self>) {
        let stored = match pref_ops::get(&self.db, EMBEDDER_PREFERENCE).await {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "could not read emoji.embedder preference at startup");
                return;
            }
        };
        let path = stored.map(|p| p.trim().to_string()).filter(|p| !p.is_empty());
        if path.is_some() {
            self.reload(path);
        }
    }

    /// Apply a new `emoji.embedder` value (`None` disables and unloads the
    /// engine; `Some(path)` cancels any active scan, reloads the model, and
    /// reconciles all confirmed stickers in the background).
    pub fn reload(self: &Arc<Self>, raw_path: Option<String>) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let prev = self.control.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(prev) = prev {
            prev.cancel.cancel();
        }
        *self.active_meta.write().unwrap_or_else(|e| e.into_inner()) = None;

        let trimmed = raw_path.map(|p| p.trim().to_string()).filter(|p| !p.is_empty());
        let Some(path_str) = trimmed else {
            let this = Arc::clone(self);
            self.publish_status(EmbedderStatusSnapshot {
                state: EmbedderState::Disabled,
                vision: false,
                indexed: 0,
                total: 0,
                error: None,
                work_run_id: this.work_run_seq.load(Ordering::SeqCst),
            });
            tokio::spawn(async move {
                let _ = this.worker.configure(generation, None).await;
            });
            return;
        };

        let path = PathBuf::from(&path_str);
        if !path.is_absolute() {
            tracing::warn!("emoji.embedder is not an absolute path; using lexical sticker pick");
            let run_id = self.next_work_run_id();
            self.publish_status(EmbedderStatusSnapshot {
                state: EmbedderState::Error,
                vision: false,
                indexed: 0,
                total: 0,
                error: Some("emoji.embedder must be an absolute path".into()),
                work_run_id: run_id,
            });
            let this = Arc::clone(self);
            tokio::spawn(async move {
                let _ = this.worker.configure(generation, None).await;
            });
            return;
        }

        let cancel = CancellationToken::new();
        let pending_ids = Arc::new(Mutex::new(VecDeque::new()));
        let wake = Arc::new(Notify::new());
        let refresh_counts = Arc::new(Notify::new());
        *self.control.lock().unwrap_or_else(|e| e.into_inner()) = Some(ActiveControl {
            cancel: cancel.clone(),
            pending_ids: Arc::clone(&pending_ids),
            wake: Arc::clone(&wake),
            refresh_counts: Arc::clone(&refresh_counts),
        });

        let current_run_id = self.work_run_seq.load(Ordering::SeqCst);
        self.publish_status(EmbedderStatusSnapshot {
            state: EmbedderState::Loading,
            vision: false,
            indexed: 0,
            total: 0,
            error: None,
            work_run_id: current_run_id,
        });

        let this = Arc::clone(self);
        tokio::spawn(async move {
            this.run_generation(generation, path, cancel, pending_ids, wake, refresh_counts)
                .await;
        });
    }

    /// Enqueue specific sticker IDs for incremental background embedding after
    /// import, confirmation, or rename. No-op when the embedder is disabled.
    pub fn enqueue_emojis(&self, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        let guard = self.control.lock().unwrap_or_else(|e| e.into_inner());
        let Some(control) = guard.as_ref() else {
            return;
        };
        {
            let mut pending = control.pending_ids.lock().unwrap_or_else(|e| e.into_inner());
            for id in ids {
                if !pending.contains(&id) {
                    pending.push_back(id);
                }
            }
        }
        control.wake.notify_one();
    }

    /// Refresh persisted counts after stickers or packs are deleted (cascade
    /// deletion removes the rows automatically in SQLite).
    pub fn notify_deleted(&self) {
        let guard = self.control.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(control) = guard.as_ref() {
            control.refresh_counts.notify_one();
        }
    }

    /// Online hot path for `send_sticker`.
    ///
    /// Never embeds sticker documents or images: short-circuits on `sticker_id`
    /// and exact name matches, reads already-persisted vectors from SQLite, and
    /// submits at most one high-priority query embedding with a hard timeout.
    pub async fn pick_emojis<'a>(
        &self,
        roster: &'a [emoji::Model],
        sticker_id: Option<&str>,
        query: Option<&str>,
    ) -> Result<&'a emoji::Model, String> {
        if let Some(fast) = try_fast_pick(roster, sticker_id, query) {
            return fast;
        }

        let meta = self.active_meta.read().unwrap_or_else(|e| e.into_inner()).clone();
        let Some(meta) = meta else {
            return pick_from_roster(roster, sticker_id, query);
        };

        let ids: Vec<String> = roster.iter().map(|s| s.id.clone()).collect();
        let vectors = match embed_ops::list_vectors_for_emojis(&self.db, &ids, &meta.model_fingerprint).await {
            Ok(vectors) => vectors,
            Err(error) => {
                tracing::warn!(%error, "could not load persisted sticker embeddings; using lexical pick");
                return pick_from_roster(roster, sticker_id, query);
            }
        };
        if vectors.is_empty() {
            return pick_from_roster(roster, sticker_id, query);
        }

        let query_trimmed = query.map(str::trim).filter(|s| !s.is_empty());
        let query_vec = match query_trimmed {
            Some(q) => match self
                .worker
                .embed_query(meta.generation, query_text(q), self.query_timeout)
                .await
            {
                Ok(vec) => Some(vec),
                Err(error) => {
                    tracing::warn!(%error, "sticker query embedding unavailable; using lexical pick");
                    None
                }
            },
            None => None,
        };

        pick_from_roster_with_vectors(roster, sticker_id, query, query_vec.as_deref(), |item| {
            vectors.get(&item.id).map(Vec::as_slice)
        })
    }

    async fn run_generation(
        self: Arc<Self>,
        generation: u64,
        path: PathBuf,
        cancel: CancellationToken,
        pending_ids: Arc<Mutex<VecDeque<String>>>,
        wake: Arc<Notify>,
        refresh_counts: Arc<Notify>,
    ) {
        let meta = match self.worker.configure(generation, Some(path.clone())).await {
            Ok(Some(meta)) => meta,
            Ok(None) => return,
            Err(error) => {
                if cancel.is_cancelled() || self.generation.load(Ordering::SeqCst) != generation {
                    return;
                }
                tracing::warn!(%error, path = %path.display(), "sticker embedder failed to load");
                let run_id = self.next_work_run_id();
                self.publish_status(EmbedderStatusSnapshot {
                    state: EmbedderState::Error,
                    vision: false,
                    indexed: 0,
                    total: 0,
                    error: Some(error),
                    work_run_id: run_id,
                });
                return;
            }
        };

        if cancel.is_cancelled() || self.generation.load(Ordering::SeqCst) != generation {
            return;
        }

        *self.active_meta.write().unwrap_or_else(|e| e.into_inner()) = Some(meta.clone());
        tracing::info!(
            vision = meta.vision,
            model = %path.display(),
            "sticker embedder loaded"
        );

        // Initial full reconciliation of all confirmed stickers.
        self.reconcile_all(&meta, &cancel).await;
        if cancel.is_cancelled() || self.generation.load(Ordering::SeqCst) != generation {
            return;
        }

        // Prune rows left behind by any older model once this model's scan settled.
        if let Err(error) = self
            .db
            .write(async |tx| embed_ops::delete_other_models(tx, &meta.model_fingerprint).await)
            .await
        {
            tracing::warn!(%error, "could not prune stale sticker embeddings from previous model");
        }

        // Stay alive for incremental updates until cancelled (reload / disable).
        loop {
            let drained = drain_ids(&pending_ids);
            if !drained.is_empty() {
                self.reconcile_subset(&meta, &cancel, &drained).await;
                if cancel.is_cancelled() || self.generation.load(Ordering::SeqCst) != generation {
                    return;
                }
                continue;
            }

            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = wake.notified() => {}
                _ = refresh_counts.notified() => {
                    self.refresh_ready_counts(&meta, &cancel).await;
                }
            }
        }
    }

    async fn reconcile_all(&self, meta: &LoadedMeta, cancel: &CancellationToken) {
        let stickers = match emoji_ops::list_all_confirmed(&self.db).await {
            Ok(list) => list,
            Err(error) => {
                tracing::warn!(%error, "could not list confirmed stickers for embedding");
                return;
            }
        };
        let existing = match embed_ops::list_source_hashes_for_model(&self.db, &meta.model_fingerprint).await {
            Ok(map) => map,
            Err(error) => {
                tracing::warn!(%error, "could not list existing sticker embeddings");
                HashMap::new()
            }
        };

        let total = stickers.len() as u64;
        let mut dirty: Vec<(emoji::Model, String)> = Vec::new();
        let mut already_indexed: u64 = 0;

        let data_dir = self.data_dir.clone();
        let vision = meta.vision;
        let hashed = tokio::task::spawn_blocking(move || {
            stickers
                .into_iter()
                .map(|sticker| {
                    let hash = compute_sticker_source_hash(&data_dir, &sticker, vision);
                    (sticker, hash)
                })
                .collect::<Vec<_>>()
        })
        .await;
        let Ok(hashed) = hashed else {
            return;
        };
        if cancel.is_cancelled() {
            return;
        }

        for (sticker, hash) in hashed {
            if existing.get(&sticker.id).is_some_and(|stored| stored == &hash) {
                already_indexed += 1;
            } else {
                dirty.push((sticker, hash));
            }
        }

        if dirty.is_empty() {
            let current_run_id = self.work_run_seq.load(Ordering::SeqCst);
            self.publish_status(EmbedderStatusSnapshot {
                state: EmbedderState::Ready,
                vision: meta.vision,
                indexed: already_indexed,
                total,
                error: None,
                work_run_id: current_run_id,
            });
            return;
        }

        let run_id = self.next_work_run_id();
        let mut indexed = already_indexed;
        self.publish_status(EmbedderStatusSnapshot {
            state: EmbedderState::Indexing,
            vision: meta.vision,
            indexed,
            total,
            error: None,
            work_run_id: run_id,
        });

        for (sticker, hash) in dirty {
            if cancel.is_cancelled() {
                return;
            }
            if self.embed_and_store_one(meta, &sticker, &hash).await {
                indexed += 1;
                self.publish_status(EmbedderStatusSnapshot {
                    state: EmbedderState::Indexing,
                    vision: meta.vision,
                    indexed,
                    total,
                    error: None,
                    work_run_id: run_id,
                });
            }
            if !self.index_pace.is_zero() {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(self.index_pace) => {}
                }
            }
        }

        if cancel.is_cancelled() {
            return;
        }
        self.publish_status(EmbedderStatusSnapshot {
            state: EmbedderState::Ready,
            vision: meta.vision,
            indexed,
            total,
            error: None,
            work_run_id: run_id,
        });
    }

    async fn reconcile_subset(&self, meta: &LoadedMeta, cancel: &CancellationToken, ids: &[String]) {
        let stickers = match emoji_ops::list_confirmed_by_ids(&self.db, ids).await {
            Ok(list) => list,
            Err(error) => {
                tracing::warn!(%error, "could not load stickers for incremental embedding");
                return;
            }
        };
        if stickers.is_empty() {
            self.refresh_ready_counts(meta, cancel).await;
            return;
        }
        let existing = embed_ops::list_source_hashes_for_model(&self.db, &meta.model_fingerprint)
            .await
            .unwrap_or_default();
        let data_dir = self.data_dir.clone();
        let vision = meta.vision;
        let Ok(hashed) = tokio::task::spawn_blocking(move || {
            stickers
                .into_iter()
                .map(|sticker| {
                    let hash = compute_sticker_source_hash(&data_dir, &sticker, vision);
                    (sticker, hash)
                })
                .collect::<Vec<_>>()
        })
        .await
        else {
            return;
        };
        let dirty: Vec<(emoji::Model, String)> = hashed
            .into_iter()
            .filter(|(s, h)| existing.get(&s.id).is_none_or(|stored| stored != h))
            .collect();
        if dirty.is_empty() {
            self.refresh_ready_counts(meta, cancel).await;
            return;
        }

        let total = emoji_ops::list_all_confirmed(&self.db)
            .await
            .map(|v| v.len() as u64)
            .unwrap_or(dirty.len() as u64);
        let mut indexed = embed_ops::count_for_model(&self.db, &meta.model_fingerprint)
            .await
            .unwrap_or(0)
            .min(total);
        // Only bump work_run_id for multi-item batches so confirming a single
        // sticker stays quiet in the notification center while updating settings.
        let run_id = if dirty.len() > 1 {
            self.next_work_run_id()
        } else {
            self.work_run_seq.load(Ordering::SeqCst)
        };
        self.publish_status(EmbedderStatusSnapshot {
            state: EmbedderState::Indexing,
            vision: meta.vision,
            indexed,
            total,
            error: None,
            work_run_id: run_id,
        });

        for (sticker, hash) in dirty {
            if cancel.is_cancelled() {
                return;
            }
            let was_existing = existing.contains_key(&sticker.id);
            if self.embed_and_store_one(meta, &sticker, &hash).await && !was_existing {
                indexed = (indexed + 1).min(total);
            }
            self.publish_status(EmbedderStatusSnapshot {
                state: EmbedderState::Indexing,
                vision: meta.vision,
                indexed,
                total,
                error: None,
                work_run_id: run_id,
            });
            if !self.index_pace.is_zero() {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(self.index_pace) => {}
                }
            }
        }

        if !cancel.is_cancelled() {
            self.refresh_ready_counts(meta, cancel).await;
        }
    }

    async fn embed_and_store_one(&self, meta: &LoadedMeta, sticker: &emoji::Model, hash: &str) -> bool {
        let data_dir = self.data_dir.clone();
        let sticker_clone = sticker.clone();
        let vision = meta.vision;
        let image =
            tokio::task::spawn_blocking(move || prepare_sticker_embedding_image(&data_dir, &sticker_clone, vision))
                .await
                .ok()
                .flatten();
        let doc = document_text(&sticker.name, sticker.tags.as_deref());
        let vec = match self.worker.embed_document(meta.generation, doc, image).await {
            Ok(vec) => vec,
            Err(error) => {
                tracing::warn!(%error, sticker_id = %sticker.id, "sticker embedding failed; skipping");
                return false;
            }
        };
        let emoji_id = sticker.id.clone();
        let fp = meta.model_fingerprint.clone();
        let hash = hash.to_owned();
        let ts = now_ms();
        match self
            .db
            .write(async |tx| embed_ops::upsert(tx, &emoji_id, &fp, &hash, &vec, ts).await)
            .await
        {
            Ok(()) => true,
            Err(error) => {
                // Deleting a sticker while it was being embedded hits the FK;
                // benign, skip without failing the scan.
                tracing::debug!(%error, sticker_id = %sticker.id, "could not persist sticker embedding");
                false
            }
        }
    }

    async fn refresh_ready_counts(&self, meta: &LoadedMeta, cancel: &CancellationToken) {
        if cancel.is_cancelled() {
            return;
        }
        let total = emoji_ops::list_all_confirmed(&self.db)
            .await
            .map(|v| v.len() as u64)
            .unwrap_or(0);
        let indexed = embed_ops::count_for_model(&self.db, &meta.model_fingerprint)
            .await
            .unwrap_or(0)
            .min(total);
        let current_run_id = self.work_run_seq.load(Ordering::SeqCst);
        self.publish_status(EmbedderStatusSnapshot {
            state: EmbedderState::Ready,
            vision: meta.vision,
            indexed,
            total,
            error: None,
            work_run_id: current_run_id,
        });
    }
}

fn drain_ids(pending: &Mutex<VecDeque<String>>) -> Vec<String> {
    let mut guard = pending.lock().unwrap_or_else(|e| e.into_inner());
    guard.drain(..).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::db::entity::emoji::EmojiSemanticStatus;
    use crate::db::entity::emoji_pack::EmojiPackKind;
    use crate::db::sea::ops::emoji::tests::{insert as insert_emoji, sticker};
    use crate::db::sea::ops::emoji_pack::tests::{insert as insert_pack, pack};
    use crate::db::sea::sea_test_db;

    struct FakeEngine {
        fingerprint: String,
        vision: bool,
        doc_delay: Duration,
        query_delay: Duration,
        doc_calls: Arc<AtomicUsize>,
        query_calls: Arc<AtomicUsize>,
    }

    impl EmbedEngine for FakeEngine {
        fn vision(&self) -> bool {
            self.vision
        }

        fn model_fingerprint(&self) -> &str {
            &self.fingerprint
        }

        fn embed_text(&self, text: &str) -> Result<Vec<f32>, String> {
            if text.starts_with(QUERY_PREFIX) {
                self.query_calls.fetch_add(1, Ordering::SeqCst);
                if !self.query_delay.is_zero() {
                    std::thread::sleep(self.query_delay);
                }
                if text.contains("无语") {
                    return Ok(vec![0.0, 1.0]);
                }
                return Ok(vec![1.0, 0.0]);
            }
            self.doc_calls.fetch_add(1, Ordering::SeqCst);
            if !self.doc_delay.is_zero() {
                std::thread::sleep(self.doc_delay);
            }
            if text.contains("黑人问号") {
                Ok(vec![0.1, 0.9])
            } else {
                Ok(vec![1.0, 0.0])
            }
        }

        fn embed_image_and_text(&self, _image: &[u8], text: &str) -> Result<Vec<f32>, String> {
            self.embed_text(text)
        }
    }

    async fn wait_for_state(index: &StickerEmbedIndex, want: EmbedderState) -> EmbedderStatusSnapshot {
        let mut rx = index.subscribe();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snap = rx.borrow().clone();
                if snap.state == want {
                    return snap;
                }
                rx.changed().await.unwrap();
            }
        })
        .await
        .expect("timed out waiting for embedder state")
    }

    #[test]
    fn prefixes_are_the_embeddinggemma_search_pair() {
        assert_eq!(query_text("无语"), "task: search query | query: 无语");
        assert_eq!(
            document_text("黑人问号", Some("尴尬,无语")),
            "task: search result | query: 黑人问号 尴尬,无语"
        );
        assert_eq!(document_text("点赞", None), "task: search result | query: 点赞");
    }

    #[test]
    fn source_hash_changes_when_the_image_bytes_change() {
        let name = "点赞";
        let without = source_hash(name, None, "a.png", None);
        let with = source_hash(name, None, "a.png", Some(b"png-a"));
        let other = source_hash(name, None, "a.png", Some(b"png-b"));
        assert_ne!(without, with);
        assert_ne!(with, other);
        assert_eq!(with, source_hash(name, None, "a.png", Some(b"png-a")));
    }

    #[tokio::test]
    async fn background_index_persists_vectors_and_warm_restart_does_zero_recompute() {
        let db = sea_test_db().await;
        let dir = tempfile::tempdir().unwrap();
        let fake_model = dir.path().join("embed.litertlm");
        std::fs::write(&fake_model, b"model").unwrap();

        insert_pack(&db, pack("p1", EmojiPackKind::Manual, None, 0)).await;
        let mut s1 = sticker("e1", "p1", EmojiSemanticStatus::Confirmed);
        s1.name = "点赞".into();
        s1.tags = None;
        s1.file_name = String::new();
        let mut s2 = sticker("e2", "p1", EmojiSemanticStatus::Confirmed);
        s2.name = "黑人问号".into();
        s2.tags = Some("尴尬".into());
        s2.file_name = String::new();
        let s1 = insert_emoji(&db, s1).await;
        let s2 = insert_emoji(&db, s2).await;

        let doc_calls = Arc::new(AtomicUsize::new(0));
        let query_calls = Arc::new(AtomicUsize::new(0));
        let make_loader = |dc: Arc<AtomicUsize>, qc: Arc<AtomicUsize>| -> EngineLoader {
            Arc::new(move |_path, _dir| {
                Ok(Box::new(FakeEngine {
                    fingerprint: "fp-v1".into(),
                    vision: false,
                    doc_delay: Duration::ZERO,
                    query_delay: Duration::ZERO,
                    doc_calls: Arc::clone(&dc),
                    query_calls: Arc::clone(&qc),
                }))
            })
        };

        let index = StickerEmbedIndex::with_loader(
            db.clone(),
            dir.path().to_path_buf(),
            make_loader(Arc::clone(&doc_calls), Arc::clone(&query_calls)),
            Duration::from_secs(2),
            Duration::ZERO,
        );
        index.reload(Some(fake_model.to_string_lossy().into_owned()));
        let ready = wait_for_state(&index, EmbedderState::Ready).await;
        assert_eq!((ready.indexed, ready.total), (2, 2));
        assert_eq!(doc_calls.load(Ordering::SeqCst), 2);
        assert_eq!(ready.work_run_id, 1);

        let roster = vec![s1.clone(), s2.clone()];

        // Exact name match short-circuits without touching the embedding engine.
        let exact = index.pick_emojis(&roster, None, Some("点赞")).await.unwrap();
        assert_eq!(exact.id, "e1");
        assert_eq!(query_calls.load(Ordering::SeqCst), 0);

        // Mood query runs only 1 query embedding and picks via persisted vectors.
        let semantic = index.pick_emojis(&roster, None, Some("有点无语")).await.unwrap();
        assert_eq!(semantic.id, "e2");
        assert_eq!(query_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            doc_calls.load(Ordering::SeqCst),
            2,
            "hot path must never embed documents"
        );

        drop(index);

        // Simulated restart with a fresh index over the same DB: zero document recomputation.
        let doc_calls_2 = Arc::new(AtomicUsize::new(0));
        let query_calls_2 = Arc::new(AtomicUsize::new(0));
        let restarted = StickerEmbedIndex::with_loader(
            db.clone(),
            dir.path().to_path_buf(),
            make_loader(Arc::clone(&doc_calls_2), Arc::clone(&query_calls_2)),
            Duration::from_secs(2),
            Duration::ZERO,
        );
        restarted.reload(Some(fake_model.to_string_lossy().into_owned()));
        let warm = wait_for_state(&restarted, EmbedderState::Ready).await;
        assert_eq!((warm.indexed, warm.total), (2, 2));
        assert_eq!(
            doc_calls_2.load(Ordering::SeqCst),
            0,
            "warm restart must reuse SQLite vectors"
        );
        assert_eq!(warm.work_run_id, 0, "warm cache hit must not raise a work run");
    }

    #[tokio::test]
    async fn query_timeout_falls_back_to_lexical_pick() {
        let db = sea_test_db().await;
        let dir = tempfile::tempdir().unwrap();
        let fake_model = dir.path().join("slow.litertlm");
        std::fs::write(&fake_model, b"model").unwrap();

        insert_pack(&db, pack("p1", EmojiPackKind::Manual, None, 0)).await;
        let mut s1 = sticker("e1", "p1", EmojiSemanticStatus::Confirmed);
        s1.name = "心虚表情".into();
        s1.tags = Some("心虚".into());
        s1.file_name = String::new();
        let s1 = insert_emoji(&db, s1).await;

        let loader: EngineLoader = Arc::new(move |_path, _dir| {
            Ok(Box::new(FakeEngine {
                fingerprint: "fp-slow".into(),
                vision: false,
                doc_delay: Duration::ZERO,
                query_delay: Duration::from_millis(400),
                doc_calls: Arc::new(AtomicUsize::new(0)),
                query_calls: Arc::new(AtomicUsize::new(0)),
            }))
        });

        let index = StickerEmbedIndex::with_loader(
            db,
            dir.path().to_path_buf(),
            loader,
            Duration::from_millis(50),
            Duration::ZERO,
        );
        index.reload(Some(fake_model.to_string_lossy().into_owned()));
        wait_for_state(&index, EmbedderState::Ready).await;

        let roster = vec![s1];
        let picked = index.pick_emojis(&roster, None, Some("心虚")).await.unwrap();
        assert_eq!(picked.id, "e1");
    }

    #[tokio::test]
    async fn clearing_embedder_disables_index_and_cancels_active_scan() {
        let db = sea_test_db().await;
        let dir = tempfile::tempdir().unwrap();
        let fake_model = dir.path().join("embed.litertlm");
        std::fs::write(&fake_model, b"model").unwrap();

        insert_pack(&db, pack("p1", EmojiPackKind::Manual, None, 0)).await;
        for i in 0..8 {
            let mut s = sticker(&format!("e{i}"), "p1", EmojiSemanticStatus::Confirmed);
            s.file_name = String::new();
            insert_emoji(&db, s).await;
        }

        let loader: EngineLoader = Arc::new(move |_path, _dir| {
            Ok(Box::new(FakeEngine {
                fingerprint: "fp-cancel".into(),
                vision: false,
                doc_delay: Duration::from_millis(80),
                query_delay: Duration::ZERO,
                doc_calls: Arc::new(AtomicUsize::new(0)),
                query_calls: Arc::new(AtomicUsize::new(0)),
            }))
        });

        let index = StickerEmbedIndex::with_loader(
            db,
            dir.path().to_path_buf(),
            loader,
            Duration::from_secs(2),
            Duration::ZERO,
        );
        index.reload(Some(fake_model.to_string_lossy().into_owned()));
        wait_for_state(&index, EmbedderState::Indexing).await;

        index.reload(None);
        let disabled = wait_for_state(&index, EmbedderState::Disabled).await;
        assert_eq!(disabled.state, EmbedderState::Disabled);
        assert_eq!(disabled.indexed, 0);
    }
}

//! Application logging: `tracing` events at info and above are written to a
//! size-bounded JSONL file that the user can read in Settings → About and the
//! assistant can read through the `read_app_logs` tool.
//!
//! Conventions for anything that logs:
//!
//! - **Never log message bodies, prompts or tool output.** Log a length instead
//!   (`chars = body.chars().count()`). The file leaves this machine whenever a
//!   user exports it.
//! - URLs go in as host and path only; tokens travel in query strings.
//! - Prefer `error = %e` over `format!("{e}")` — the visitor walks `source()`
//!   and records the whole chain, which is usually where the real cause is.
//! - New call sites default to `debug!`. Only a user-visible state change or a
//!   failure earns `info!` and above, because only those reach the file.
//! - Open a span where a request begins (`info_span!("chat", conversation_id =
//!   %id)`); every event underneath inherits it and becomes attributable.

mod config;
mod layer;
pub mod reader;
mod record;
mod redact;
mod writer;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::reload;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry};

use layer::{JsonlLayer, LineSink};
use writer::RollingWriter;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
}

impl LogLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
        }
    }

    pub const fn rank(self) -> u8 {
        match self {
            Self::Error => 4,
            Self::Warn => 3,
            Self::Info => 2,
            Self::Debug => 1,
        }
    }
}

/// Lines held while the destination is still unknown.
///
/// Logging has to start before `app_data_dir()` is available, and the gap covers
/// exactly the failures worth keeping: resolving that directory, creating it,
/// opening the secrets store, migrating the database.
const EARLY_BUFFER_LINES: usize = 512;

struct EarlyBuffer {
    lines: Vec<Vec<u8>>,
    dropped: u64,
}

/// The write side. Not behind a lock on the hot path: once the writer is set,
/// `emit` is an atomic load and a call.
struct FileSink {
    writer: OnceLock<RollingWriter>,
    early: Mutex<Option<EarlyBuffer>>,
}

impl LineSink for FileSink {
    fn emit(&self, line: &[u8]) {
        if let Some(writer) = self.writer.get() {
            writer.write_line(line);
            return;
        }
        let mut early = self.early.lock().unwrap_or_else(|p| p.into_inner());
        // The writer may have been installed while this thread waited.
        if let Some(writer) = self.writer.get() {
            drop(early);
            writer.write_line(line);
            return;
        }
        let buffer = early.get_or_insert_with(|| EarlyBuffer {
            lines: Vec::new(),
            dropped: 0,
        });
        if buffer.lines.len() >= EARLY_BUFFER_LINES {
            // Drop the newest, keep the oldest: when startup goes wrong the first
            // error explains the rest.
            buffer.dropped += 1;
        } else {
            buffer.lines.push(line.to_vec());
        }
    }
}

static SINK: FileSink = FileSink {
    writer: OnceLock::new(),
    early: Mutex::new(None),
};

static LEVEL_RELOAD: OnceLock<reload::Handle<EnvFilter, Registry>> = OnceLock::new();
static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Install the subscriber. Called before Tauri starts, so the file half is not
/// wired up yet — see [`attach_file_sink`].
pub fn init_early() {
    let (file_filter, reload_handle) = reload::Layer::new(config::build_filter(config::DEFAULT_LEVEL));
    let _ = LEVEL_RELOAD.set(reload_handle);

    // `RUST_LOG` steers stdout only. If it reached the file filter, a developer
    // running with `RUST_LOG=trace` would burn through the whole size budget and
    // evict the records they were trying to keep.
    let stdout_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| config::build_filter(config::DEFAULT_LEVEL));

    // The file layer goes on first so its reloadable filter is parameterised by
    // the bare `Registry`, which is what `LEVEL_RELOAD` stores a handle to.
    let result = Registry::default()
        .with(JsonlLayer::new(&SINK).with_filter(file_filter))
        .with(tracing_subscriber::fmt::layer().with_filter(stdout_filter))
        .try_init();

    // `try_init` rather than `init`: on Android the mobile entry point can run
    // again in the same process when the activity is recreated, and installing
    // a global subscriber twice panics.
    if let Err(e) = result {
        eprintln!("[logging] subscriber already installed: {e}");
    }
}

/// Route panics through the log, then on to whatever hook was already there.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        // force_capture rather than capture: a release build has no
        // RUST_BACKTRACE set, and a panic with no stack is barely a report.
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        // Which thread died decides how bad this is: the UI thread means the
        // window is gone, a tokio worker means one turn failed. The event's own
        // `thread` field is absent for unnamed threads, which is exactly the
        // case for the windowing and webview threads.
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("unnamed").to_string();

        tracing::error!(
            location = %location,
            thread = %thread_name,
            backtrace = %backtrace,
            "panic: {payload}"
        );

        // Last, because the previous hook may abort the process.
        previous(info);
    }));
}

/// Point the log at a directory and flush everything buffered until now.
pub(crate) fn attach_file_sink(data_dir: &Path) {
    let dir = data_dir.join("logs");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("[logging] could not create {}: {e}", dir.display());
        return;
    }
    writer::prune_on_startup(&dir);

    let writer = match RollingWriter::open(&dir) {
        Ok(writer) => writer,
        Err(e) => {
            eprintln!("[logging] could not open the log file in {}: {e}", dir.display());
            return;
        }
    };

    // Held across the whole handover so a concurrent `emit` either buffers
    // before the drain or writes after the writer is visible, never both.
    let mut early = SINK.early.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(buffer) = early.take() {
        for line in &buffer.lines {
            writer.write_line(line);
        }
        if buffer.dropped > 0 {
            // Reported rather than swallowed: a silent gap in the file is worse
            // than knowing how big the gap is.
            let ts_ms = crate::util::now_ms();
            let note = serde_json::json!({
                "v": record::SCHEMA_VERSION,
                "ts": record::format_ts(ts_ms),
                "ts_ms": ts_ms,
                "level": "WARN",
                "target": "meridian::logging",
                "msg": format!("{} early log lines were dropped before the file was ready", buffer.dropped),
                "pid": std::process::id(),
            });
            if let Ok(mut line) = serde_json::to_vec(&note) {
                line.push(b'\n');
                writer.write_line(&line);
            }
        }
    }
    let _ = SINK.writer.set(writer);
    let _ = LOG_DIR.set(dir);
}

pub const LEVEL_PREFERENCE_KEY: &str = "logging.level";

pub fn validate_level(level: &str) -> Result<LogLevel, String> {
    config::normalize_level(level).ok_or_else(|| format!("unsupported log level {level:?}"))
}

/// Read the exact saved spelling. Absence means the documented default; a row
/// that exists but is invalid is a broken contract and is returned as an error.
pub fn load_saved_level(pool: &crate::db::DbPool) -> Result<LogLevel, String> {
    let mut conn = pool.get().map_err(|error| error.to_string())?;
    let saved = crate::db::ops::preference::get_preference(&mut conn, LEVEL_PREFERENCE_KEY)
        .map_err(|error| format!("failed to read preference {LEVEL_PREFERENCE_KEY}: {error}"))?;
    match saved {
        None => Ok(config::DEFAULT_LEVEL),
        Some(level) => validate_level(&level),
    }
}

/// Apply the saved level. Runs after the database is up, so the handful of lines
/// written before this point use the default level.
pub(crate) fn apply_saved_level(pool: &crate::db::DbPool) -> Result<(), String> {
    set_level(load_saved_level(pool)?)
}

/// Change the file log level for the running process.
pub fn set_level(level: LogLevel) -> Result<(), String> {
    LEVEL_RELOAD
        .get()
        .ok_or("logging is not initialised")?
        .reload(config::build_filter(level))
        .map_err(|e| e.to_string())
}

/// The directory holding the log files, once one has been attached.
///
/// Commands read it from here instead of recomputing it from `app_data_dir()`:
/// one source of truth, and a caller can tell "logging failed to start" from
/// "the directory happens to be empty".
pub fn log_dir() -> Option<&'static Path> {
    LOG_DIR.get().map(PathBuf::as_path)
}

/// `(bytes per file, files kept including the current one)`, for display.
pub fn file_limits() -> (u64, usize) {
    (writer::MAX_FILE_BYTES, writer::MAX_ARCHIVES + 1)
}

pub fn selectable_levels() -> &'static [LogLevel] {
    config::SELECTABLE_LEVELS
}

pub fn default_level() -> LogLevel {
    config::DEFAULT_LEVEL
}

/// Log files newest first, as `(name, size in bytes)`.
pub fn list_files(dir: &Path) -> Vec<(String, u64)> {
    reader::existing_files(dir, true)
        .into_iter()
        .map(|(_, path)| {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            (name, size)
        })
        .collect()
}

/// Concatenate every log file into `output`, oldest first, and return the bytes
/// written.
///
/// Streams rather than reading into memory, and copies the files verbatim: a log
/// handed to whoever is diagnosing the problem has to be complete, and a
/// silently filtered export is worse than none.
pub fn export_to(dir: &Path, output: &Path) -> std::io::Result<u64> {
    use std::io::Write;

    let mut files = reader::existing_files(dir, true);
    files.sort_by_key(|(index, _)| std::cmp::Reverse(*index));

    let mut out = std::fs::File::create(output)?;
    let mut total = 0u64;
    for (_, path) in files {
        let mut input = std::fs::File::open(&path)?;
        total += std::io::copy(&mut input, &mut out)?;
    }
    out.flush()?;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn stored_log_level_is_strict_and_errors_are_not_defaulted() {
        let pool = crate::db::test_db();
        assert_eq!(load_saved_level(&pool).unwrap(), default_level());

        for raw in ["WARN", " info ", "trace", "future"] {
            {
                let mut conn = pool.get().unwrap();
                crate::db::ops::preference::set_preference(&mut conn, LEVEL_PREFERENCE_KEY, raw, 1).unwrap();
            }
            let error = load_saved_level(&pool).expect_err("invalid stored log level must fail");
            assert!(error.contains("unsupported log level"), "{raw:?}: {error}");
        }
    }

    /// A sink writing to a real rolling file, so the whole chain is exercised.
    struct WriterSink(RollingWriter);

    impl LineSink for WriterSink {
        fn emit(&self, line: &[u8]) {
            self.0.write_line(line);
        }
    }

    /// The writer and the reader agree on the format, or neither is any use.
    /// The layer's own tests stop at the JSON; this one goes to disk and back.
    #[test]
    fn what_the_layer_writes_is_what_the_reader_reads() {
        let dir = tempfile::tempdir().unwrap();
        let sink: &'static WriterSink = Box::leak(Box::new(WriterSink(RollingWriter::open(dir.path()).unwrap())));

        let subscriber = Registry::default().with(JsonlLayer::new(sink));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("plain event");
            let span = tracing::info_span!("chat", conversation_id = "c-42");
            span.in_scope(|| tracing::warn!(status = 401, api_key = "sk-live-should-not-appear", "auth failed"));
        });

        let page = reader::query(
            dir.path(),
            &reader::LogQuery {
                limit: 10,
                include_rotated: true,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(page.entries.len(), 2);
        // Newest first.
        let newest = &page.entries[0];
        assert_eq!(newest.level, reader::LogRecordLevel::Warn);
        assert_eq!(newest.msg, "auth failed");
        assert_eq!(newest.fields.get("status").and_then(|v| v.as_i64()), Some(401));
        assert_eq!(
            newest.span_fields.get("conversation_id").and_then(|v| v.as_str()),
            Some("c-42")
        );
        assert_eq!(page.entries[1].msg, "plain event");

        // The credential never reached the disk.
        let on_disk = std::fs::read_to_string(dir.path().join(writer::BASE_NAME)).unwrap();
        assert!(!on_disk.contains("sk-live-should-not-appear"), "{on_disk}");
    }

    /// The conversation filter is what makes "why did this chat fail" a single
    /// query, so it has to work against records the layer actually produced.
    #[test]
    fn records_can_be_narrowed_to_one_conversation_after_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let sink: &'static WriterSink = Box::leak(Box::new(WriterSink(RollingWriter::open(dir.path()).unwrap())));

        let subscriber = Registry::default().with(JsonlLayer::new(sink));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info_span!("chat", conversation_id = "wanted").in_scope(|| tracing::error!("this one"));
            tracing::info_span!("chat", conversation_id = "other").in_scope(|| tracing::error!("not this one"));
        });

        let page = reader::query(
            dir.path(),
            &reader::LogQuery {
                limit: 10,
                include_rotated: true,
                conversation_id: Some("wanted".into()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].msg, "this one");
    }
}

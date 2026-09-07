//! Size-based rolling file writer.
//!
//! `meridian.log` is the file being written; once it would exceed
//! [`MAX_FILE_BYTES`] it becomes `meridian.1.log` and the older archives shift
//! down, so total disk use is bounded whatever happens. A OneBot deployment can
//! run for weeks, which rules out an unbounded single file, and bounding by day
//! instead of by size would leave one busy day unbounded.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::util::now_ms;

pub(crate) const BASE_NAME: &str = "meridian.log";

/// Desktop keeps 5 MB × 5 files ≈ 25 MB. Android is tighter on storage and its
/// log is the only diagnostic channel that survives a crash, so it trades size
/// for keeping the same number of rotations.
#[cfg(not(target_os = "android"))]
pub(crate) const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;
#[cfg(target_os = "android")]
pub(crate) const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Archives kept alongside the current file: `meridian.1.log` ..= `meridian.N.log`.
#[cfg(not(target_os = "android"))]
pub(crate) const MAX_ARCHIVES: usize = 4;
#[cfg(target_os = "android")]
pub(crate) const MAX_ARCHIVES: usize = 2;

/// Minimum gap between failure reports. When the disk fills, *every* line fails;
/// without throttling the terminal becomes unusable and stderr itself becomes
/// the bottleneck.
const ERR_THROTTLE_MS: i64 = 30_000;

pub(crate) fn archive_name(n: usize) -> String {
    format!("meridian.{n}.log")
}

/// True for `meridian.log` and `meridian.<n>.log`, false for anything else in
/// the directory. Pruning and the read commands both key off this, so a file the
/// user parked there by hand is never touched or served.
pub(crate) fn parse_log_name(name: &str) -> Option<usize> {
    if name == BASE_NAME {
        return Some(0);
    }
    let rest = name.strip_prefix("meridian.")?.strip_suffix(".log")?;
    // `meridian..log` and `meridian.01.log` are not names we write.
    if rest.is_empty() || rest.len() > 3 || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: usize = rest.parse().ok()?;
    if n == 0 { None } else { Some(n) }
}

pub(crate) struct RollingWriter {
    dir: PathBuf,
    limit: u64,
    state: Mutex<State>,
}

struct State {
    /// `None` once the file became unusable. Logging then degrades to dropping
    /// lines rather than failing the operation that emitted them.
    file: Option<File>,
    written: u64,
    last_err_ms: i64,
    suppressed_errs: u64,
}

impl RollingWriter {
    pub(crate) fn open(dir: &Path) -> io::Result<Self> {
        Self::open_with_limit(dir, MAX_FILE_BYTES)
    }

    /// Split out so the rotation tests don't have to write five megabytes.
    pub(crate) fn open_with_limit(dir: &Path, limit: u64) -> io::Result<Self> {
        let path = dir.join(BASE_NAME);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        // Continue the existing file rather than starting the count at zero:
        // otherwise every restart grants another full budget and the file grows
        // without bound.
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            dir: dir.to_path_buf(),
            limit,
            state: Mutex::new(State {
                file: Some(file),
                written,
                last_err_ms: 0,
                suppressed_errs: 0,
            }),
        })
    }

    /// Append one complete line, terminator included, in a single `write_all`.
    ///
    /// Splitting a record across several writes lets concurrent threads — and
    /// concurrent *processes*, since nothing stops a second instance opening the
    /// same file — interleave mid-record and tear the JSONL. One write per line
    /// is what keeps every line independently parseable.
    pub(crate) fn write_line(&self, line: &[u8]) {
        // A poisoned lock means some other thread panicked while logging. The
        // data it guards is a file handle and a byte count, neither of which a
        // panic can leave inconsistent, so recovering beats losing all logging
        // from that point on.
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());

        // Rotate before writing, never after, so the file cannot exceed the
        // limit. `written > 0` keeps a single oversized line from rotating an
        // empty file forever.
        if st.written > 0 && st.written.saturating_add(line.len() as u64) > self.limit {
            self.rotate(&mut st);
        }

        let result = match st.file.as_mut() {
            Some(f) => f.write_all(line),
            None => return,
        };
        match result {
            Ok(()) => st.written += line.len() as u64,
            Err(e) => {
                st.file = None;
                Self::report(&mut st, format_args!("write failed: {e}"));
            }
        }
    }

    fn rotate(&self, st: &mut State) {
        // Close first: on Windows renaming a file with an open handle fails with
        // ERROR_SHARING_VIOLATION. This is also the only place we sync — doing it
        // per line would turn every log call into an fsync.
        if let Some(f) = st.file.take() {
            let _ = f.sync_all();
        }

        // Rotating the oldest archive "off the end" is just deleting it.
        let _ = fs::remove_file(self.dir.join(archive_name(MAX_ARCHIVES)));
        // Descending, so each rename lands on a slot that was just vacated.
        for i in (1..MAX_ARCHIVES).rev() {
            let _ = rename_replacing(&self.dir.join(archive_name(i)), &self.dir.join(archive_name(i + 1)));
        }
        if let Err(e) = rename_replacing(&self.dir.join(BASE_NAME), &self.dir.join(archive_name(1))) {
            // Antivirus, a search indexer or a second instance can hold the file
            // briefly. Truncating would destroy logs; giving up would stop
            // logging. Keep appending and try again on the next line — the file
            // temporarily exceeding its limit is the cheapest of the three.
            Self::report(st, format_args!("rotate failed: {e}"));
        }

        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(BASE_NAME))
        {
            Ok(f) => {
                st.written = f.metadata().map(|m| m.len()).unwrap_or(0);
                st.file = Some(f);
            }
            Err(e) => {
                st.file = None;
                Self::report(st, format_args!("reopen failed: {e}"));
            }
        }
    }

    /// Never routes through `tracing`: that would re-enter the layer, come back
    /// here, and deadlock on the mutex this is already holding.
    fn report(st: &mut State, args: std::fmt::Arguments<'_>) {
        let now = now_ms();
        if now.saturating_sub(st.last_err_ms) < ERR_THROTTLE_MS {
            st.suppressed_errs += 1;
            return;
        }
        let suppressed = std::mem::take(&mut st.suppressed_errs);
        st.last_err_ms = now;
        if suppressed > 0 {
            eprintln!("[logging] {args} ({suppressed} similar suppressed)");
        } else {
            eprintln!("[logging] {args}");
        }
    }
}

/// `fs::rename` with the Windows replace fallback, mirroring the handling in
/// `secrets::local::write_file_atomically`.
fn rename_replacing(from: &Path, to: &Path) -> io::Result<()> {
    if !from.exists() {
        return Ok(());
    }
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(initial) => {
            #[cfg(target_os = "windows")]
            {
                if to.exists() {
                    fs::remove_file(to)?;
                    return fs::rename(from, to);
                }
            }
            Err(initial)
        }
    }
}

/// Clear out what rotation alone cannot: archives left behind after
/// `MAX_ARCHIVES` was lowered, or duplicates from a crash mid-rotation. Runs
/// once at startup and never on the write path, the same placement the database
/// uses for its own housekeeping (`db::init_db`).
pub(crate) fn prune_on_startup(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut archives: Vec<(usize, PathBuf, u64)> = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // Anything that is not one of our own files stays untouched — the user
        // may have parked a copy here on purpose.
        let Some(index) = parse_log_name(name) else { continue };
        if index == 0 {
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        archives.push((index, entry.path(), size));
    }

    archives.sort_by_key(|(index, _, _)| *index);

    let budget = MAX_FILE_BYTES.saturating_mul(MAX_ARCHIVES as u64);
    let mut total: u64 = archives.iter().map(|(_, _, size)| *size).sum();

    for (index, path, size) in archives.iter().rev() {
        // Oldest first: over the retained count, or still over budget.
        if (*index > MAX_ARCHIVES || total > budget) && fs::remove_file(path).is_ok() {
            total = total.saturating_sub(*size);
        }
    }
}

/// Compatibility shim only.
///
/// Every caller inside this crate uses [`RollingWriter::write_line`]. Handing
/// this to something like `serde_json::to_writer` would emit a record as dozens
/// of small writes, each taking the lock separately, which tears the JSONL as
/// soon as two threads log at once.
impl Write for &RollingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_line(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Opened in append mode with no userspace buffering; the OS page cache
        // is the only thing between us and the disk.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(dir: &Path, name: &str) -> String {
        fs::read_to_string(dir.join(name)).unwrap_or_default()
    }

    #[test]
    fn log_names_are_recognised_and_others_left_alone() {
        assert_eq!(parse_log_name("meridian.log"), Some(0));
        assert_eq!(parse_log_name("meridian.1.log"), Some(1));
        assert_eq!(parse_log_name("meridian.42.log"), Some(42));

        assert_eq!(parse_log_name("meridian.log.bak"), None);
        assert_eq!(parse_log_name("meridian..log"), None);
        assert_eq!(parse_log_name("meridian.0.log"), None);
        assert_eq!(parse_log_name("meridian.x.log"), None);
        assert_eq!(parse_log_name("readme.txt"), None);
    }

    #[test]
    fn writing_past_the_limit_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let w = RollingWriter::open_with_limit(dir.path(), 64).unwrap();

        w.write_line(b"first line, forty-two bytes of padding ok\n");
        w.write_line(b"second line, which does not fit alongside\n");

        assert!(read(dir.path(), "meridian.1.log").starts_with("first line"));
        assert!(read(dir.path(), BASE_NAME).starts_with("second line"));
    }

    #[test]
    fn the_current_file_never_exceeds_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let w = RollingWriter::open_with_limit(dir.path(), 128).unwrap();
        for i in 0..50 {
            w.write_line(format!("line {i:03} with a bit of padding to take up room\n").as_bytes());
        }
        let size = fs::metadata(dir.path().join(BASE_NAME)).unwrap().len();
        assert!(size <= 128, "current file grew to {size} bytes");
    }

    #[test]
    fn rotation_shifts_every_archive_down_and_drops_the_oldest() {
        let dir = tempfile::tempdir().unwrap();
        for i in 1..=MAX_ARCHIVES {
            fs::write(dir.path().join(archive_name(i)), format!("archive {i}")).unwrap();
        }
        fs::write(dir.path().join(BASE_NAME), "current").unwrap();

        let w = RollingWriter::open_with_limit(dir.path(), 4).unwrap();
        w.write_line(b"trigger\n");

        assert_eq!(read(dir.path(), &archive_name(1)), "current");
        for i in 2..=MAX_ARCHIVES {
            assert_eq!(read(dir.path(), &archive_name(i)), format!("archive {}", i - 1));
        }
        // The one that was oldest is gone, and nothing spilled past the limit.
        assert!(!dir.path().join(archive_name(MAX_ARCHIVES + 1)).exists());
        assert_eq!(read(dir.path(), BASE_NAME), "trigger\n");
    }

    #[test]
    fn reopening_continues_the_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        {
            let w = RollingWriter::open_with_limit(dir.path(), 1024).unwrap();
            w.write_line(b"before restart\n");
        }
        let w = RollingWriter::open_with_limit(dir.path(), 1024).unwrap();
        w.write_line(b"after restart\n");

        let body = read(dir.path(), BASE_NAME);
        assert!(body.contains("before restart"), "{body}");
        assert!(body.contains("after restart"), "{body}");
    }

    #[test]
    fn a_restart_does_not_grant_a_fresh_size_budget() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(BASE_NAME), "x".repeat(100)).unwrap();

        let w = RollingWriter::open_with_limit(dir.path(), 64).unwrap();
        w.write_line(b"next\n");

        // The pre-existing content counted, so the first line rotated it away.
        assert_eq!(read(dir.path(), &archive_name(1)).len(), 100);
        assert_eq!(read(dir.path(), BASE_NAME), "next\n");
    }

    #[test]
    fn pruning_removes_extra_archives_and_keeps_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        for i in 1..=(MAX_ARCHIVES + 4) {
            fs::write(dir.path().join(archive_name(i)), "x").unwrap();
        }
        fs::write(dir.path().join(BASE_NAME), "current").unwrap();
        fs::write(dir.path().join("readme.txt"), "keep me").unwrap();

        prune_on_startup(dir.path());

        for i in 1..=MAX_ARCHIVES {
            assert!(dir.path().join(archive_name(i)).exists(), "archive {i} should survive");
        }
        for i in (MAX_ARCHIVES + 1)..=(MAX_ARCHIVES + 4) {
            assert!(!dir.path().join(archive_name(i)).exists(), "archive {i} should be gone");
        }
        assert_eq!(read(dir.path(), "readme.txt"), "keep me");
        assert_eq!(read(dir.path(), BASE_NAME), "current");
    }

    #[test]
    fn failure_reports_are_throttled() {
        let mut st = State {
            file: None,
            written: 0,
            last_err_ms: 0,
            suppressed_errs: 0,
        };
        RollingWriter::report(&mut st, format_args!("first"));
        RollingWriter::report(&mut st, format_args!("second"));
        RollingWriter::report(&mut st, format_args!("third"));

        // The first one printed and set the clock; the rest were counted.
        assert_eq!(st.suppressed_errs, 2);
        assert!(st.last_err_ms > 0);
    }
}

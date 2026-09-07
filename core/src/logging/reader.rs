//! Reading the log back.
//!
//! One implementation serves both consumers: the Settings panel serialises the
//! page to JSON, the `read_app_logs` tool renders it as text. They need exactly
//! the same "walk backwards from newest, filter, stop early" behaviour, and a
//! second implementation would drift.
//!
//! Everything here takes a `&Path`, so it can be tested against a temporary
//! directory without standing up Tauri.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::LogLevel;
use super::writer::{BASE_NAME, MAX_ARCHIVES, archive_name};

/// Read granularity when walking a file backwards.
const CHUNK: usize = 64 * 1024;
/// Ceiling on how much a single request may read. The time window normally
/// stops the scan long before this; it exists for the case where a filter
/// matches nothing and the window is "all time".
const MAX_SCAN_BYTES: u64 = 8 * 1024 * 1024;
/// A record longer than this was not written by us — refuse to buffer it whole.
const MAX_LINE_BYTES: usize = 256 * 1024;
/// Where a page stopped, so the next one can pick up.
///
/// Deliberately a position rather than a timestamp: records routinely share a
/// millisecond, and a timestamp cursor would either repeat or skip whatever sits
/// on the boundary. The reader already knows its byte offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// 0 is `meridian.log`, 1 is `meridian.1.log`, and so on — newest first.
    pub file_index: usize,
    /// Start of the oldest record already returned; the next page reads
    /// backwards from here.
    pub byte_offset: u64,
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub ts: String,
    pub ts_ms: i64,
    pub level: LogRecordLevel,
    pub target: String,
    pub msg: String,
    pub fields: Map<String, Value>,
    pub spans: Vec<String>,
    pub span_fields: Map<String, Value>,
    pub file: Option<String>,
    pub line: Option<u32>,
    /// Identifies this record for React keys and for resuming a scan.
    pub cursor: Cursor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LogRecordLevel {
    Error,
    Warn,
    Info,
    Debug,
}

impl LogRecordLevel {
    const fn rank(self) -> u8 {
        match self {
            Self::Error => 4,
            Self::Warn => 3,
            Self::Info => 2,
            Self::Debug => 1,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LogQuery {
    /// Minimum severity, matched case-insensitively against the record's level.
    pub min_level: Option<LogLevel>,
    pub limit: usize,
    /// Case-insensitive substring over the message, target and field values.
    /// Substring rather than regex: no catastrophic backtracking, and no
    /// model-authored pattern that quietly matches everything.
    pub contains: Option<String>,
    pub target_prefix: Option<String>,
    pub conversation_id: Option<String>,
    pub since_ts_ms: Option<i64>,
    pub until_ts_ms: Option<i64>,
    pub include_rotated: bool,
    pub cursor: Option<Cursor>,
}

#[derive(Debug, Default)]
pub struct LogPage {
    /// Newest first.
    pub entries: Vec<LogEntry>,
    /// `None` once the scan reached the oldest available record.
    pub next_cursor: Option<Cursor>,
    /// The scan hit its byte budget rather than running out of records, so
    /// older matches may exist.
    pub scan_truncated: bool,
    pub files_scanned: Vec<String>,
}

/// Log files newest first, skipping ones that are absent.
pub(crate) fn existing_files(dir: &Path, include_rotated: bool) -> Vec<(usize, PathBuf)> {
    let mut out = Vec::new();
    let current = dir.join(BASE_NAME);
    if current.is_file() {
        out.push((0, current));
    }
    if include_rotated {
        // Rotation renames, so index order is recency order. File mtimes would
        // be the obvious alternative and are wrong: a rename does not
        // necessarily update them, and a restore from backup rewrites them all.
        for i in 1..=MAX_ARCHIVES {
            let path = dir.join(archive_name(i));
            if path.is_file() {
                out.push((i, path));
            }
        }
    }
    out
}

/// Walk one file backwards, newest line first.
///
/// `from_offset` bounds the region considered; lines starting at or after it are
/// skipped, which is how paging resumes. `visit` receives the line, its byte
/// offset and how much of this file has been read so far, and returns false to
/// stop. Returns the number of bytes read.
fn for_each_line_backward(
    path: &Path,
    from_offset: Option<u64>,
    mut visit: impl FnMut(&[u8], u64, u64) -> bool,
) -> io::Result<u64> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut end = from_offset.unwrap_or(file_len).min(file_len);
    let mut scanned: u64 = 0;

    // Drop any tail without a terminator first: that is a record still being
    // written, and handing back half of one is worse than missing it. Doing it
    // up front rather than inside the loop leaves the loop one invariant to
    // rely on — `end` is always just past a newline.
    while end > 0 {
        let chunk_len = CHUNK.min(end as usize);
        let start = end - chunk_len as u64;
        file.seek(SeekFrom::Start(start))?;
        let mut buf = vec![0u8; chunk_len];
        file.read_exact(&mut buf)?;
        scanned += chunk_len as u64;
        match buf.iter().rposition(|b| *b == b'\n') {
            Some(nl) => {
                end = start + nl as u64 + 1;
                break;
            }
            // Not one newline in the whole chunk: keep looking backwards.
            None => end = start,
        }
    }

    // Holds a line's tail whose start lies in a chunk not read yet, so it can be
    // prepended to on the next pass.
    let mut carry: Vec<u8> = Vec::new();

    while end > 0 {
        let chunk_len = CHUNK.min(end as usize);
        let start = end - chunk_len as u64;
        file.seek(SeekFrom::Start(start))?;
        let mut buf = vec![0u8; chunk_len];
        file.read_exact(&mut buf)?;
        scanned += chunk_len as u64;

        buf.extend_from_slice(&carry);
        carry.clear();

        // Invariant: `buf[..cut]` is unprocessed and ends with a newline.
        let mut cut = buf.len();
        while cut > 0 {
            let content_end = cut - 1;
            match buf[..content_end].iter().rposition(|b| *b == b'\n') {
                Some(nl) => {
                    let line = &buf[nl + 1..content_end];
                    let offset = start + nl as u64 + 1;
                    if !line.is_empty() && !visit(line, offset, scanned) {
                        return Ok(scanned);
                    }
                    cut = nl + 1;
                }
                // No newline left in front: this line begins in an earlier chunk.
                None => break,
            }
        }

        carry = buf[..cut].to_vec();
        // A record this long did not come from the writer. Stop rather than
        // grow the buffer without bound on a corrupt file.
        if carry.len() > MAX_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "log record exceeds the maximum line size",
            ));
        }
        end = start;
    }

    // Whatever is left starts at byte zero; strip its terminator.
    if carry.len() > 1 {
        visit(&carry[..carry.len() - 1], 0, scanned);
    }
    Ok(scanned)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredLogRecord {
    v: u32,
    ts: String,
    ts_ms: i64,
    level: LogRecordLevel,
    target: String,
    msg: String,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    line: Option<u32>,
    #[serde(rename = "pid")]
    _pid: u32,
    #[serde(default, rename = "thread")]
    _thread: Option<String>,
    #[serde(default)]
    fields: Map<String, Value>,
    #[serde(default)]
    spans: Vec<String>,
    #[serde(default)]
    span_fields: Map<String, Value>,
}

fn parse_line(line: &[u8], cursor: Cursor) -> Result<LogEntry, String> {
    let text = std::str::from_utf8(line).map_err(|error| format!("log record is not UTF-8: {error}"))?;
    let record: StoredLogRecord = serde_json::from_str(text).map_err(|error| format!("invalid log record: {error}"))?;
    if record.v != super::record::SCHEMA_VERSION {
        return Err(format!(
            "unsupported log schema version {}; expected {}",
            record.v,
            super::record::SCHEMA_VERSION
        ));
    }
    chrono::DateTime::parse_from_rfc3339(&record.ts)
        .map_err(|error| format!("invalid log timestamp {:?}: {error}", record.ts))?;
    let StoredLogRecord {
        ts,
        ts_ms,
        level,
        target,
        msg,
        file,
        line,
        fields,
        spans,
        span_fields,
        ..
    } = record;
    Ok(LogEntry {
        ts,
        ts_ms,
        level,
        target,
        msg,
        fields,
        spans,
        span_fields,
        file,
        line,
        cursor,
    })
}

fn matches(entry: &LogEntry, q: &LogQuery) -> bool {
    if let Some(min) = &q.min_level
        && entry.level.rank() < min.rank()
    {
        return false;
    }
    if let Some(prefix) = &q.target_prefix
        && !entry.target.starts_with(prefix.as_str())
    {
        return false;
    }
    if let Some(want) = &q.conversation_id {
        let found = entry
            .span_fields
            .get("conversation_id")
            .or_else(|| entry.fields.get("conversation_id"))
            .and_then(Value::as_str);
        if found != Some(want.as_str()) {
            return false;
        }
    }
    if let Some(needle) = &q.contains {
        let needle = needle.to_lowercase();
        let haystack_hit = entry.msg.to_lowercase().contains(&needle)
            || entry.target.to_lowercase().contains(&needle)
            || Value::Object(entry.fields.clone())
                .to_string()
                .to_lowercase()
                .contains(&needle)
            || Value::Object(entry.span_fields.clone())
                .to_string()
                .to_lowercase()
                .contains(&needle);
        if !haystack_hit {
            return false;
        }
    }
    if let Some(until) = q.until_ts_ms
        && entry.ts_ms > until
    {
        return false;
    }
    true
}

/// Read a page of records, newest first.
pub fn query(dir: &Path, q: &LogQuery) -> Result<LogPage, String> {
    let limit = q.limit.max(1);
    let files = existing_files(dir, q.include_rotated);
    let mut page = LogPage::default();
    let mut scanned: u64 = 0;
    let mut reached_end = true;

    for (index, path) in &files {
        // Resume: skip the files already exhausted, and start mid-file for the
        // one the last page stopped in.
        let from_offset = match q.cursor {
            Some(cursor) if *index < cursor.file_index => continue,
            Some(cursor) if *index == cursor.file_index => Some(cursor.byte_offset),
            _ => None,
        };

        page.files_scanned.push(
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string(),
        );

        let mut stopped_early = false;
        let mut parse_error = None;
        let scanned_before = scanned;
        let result = for_each_line_backward(path, from_offset, |line, offset, in_file| {
            let cursor = Cursor {
                file_index: *index,
                byte_offset: offset,
            };
            let entry = match parse_line(line, cursor) {
                Ok(entry) => entry,
                Err(error) => {
                    parse_error = Some(format!("failed to read {} at byte {offset}: {error}", path.display()));
                    return false;
                }
            };

            // Records are appended in order and files are ordered, so the first
            // record older than the window means nothing older can match. This
            // is what keeps a five-megabyte log to a few kilobytes of reading.
            if let Some(since) = q.since_ts_ms
                && entry.ts_ms != 0
                && entry.ts_ms < since
            {
                stopped_early = true;
                return false;
            }

            if matches(&entry, q) {
                page.entries.push(entry);
                if page.entries.len() >= limit {
                    return false;
                }
            }

            if scanned_before + in_file >= MAX_SCAN_BYTES {
                page.scan_truncated = true;
                return false;
            }
            true
        });

        if let Some(error) = parse_error {
            return Err(error);
        }

        match result {
            Ok(read) => scanned += read,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // A rotation between listing and opening is normal, not an
                // error worth failing the page over.
                tracing::debug!(error = %e, path = %path.display(), "log file went away mid-scan");
                continue;
            }
            Err(e) => return Err(format!("failed to read {}: {e}", path.display())),
        }

        if stopped_early {
            reached_end = true;
            break;
        }
        if page.entries.len() >= limit || page.scan_truncated {
            reached_end = false;
            break;
        }
    }

    // The oldest record returned is where the next page resumes.
    page.next_cursor = if reached_end {
        None
    } else {
        page.entries.last().map(|e| e.cursor)
    };
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_log(dir: &Path, name: &str, lines: &[String]) {
        let mut file = File::create(dir.join(name)).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    fn record(ts_ms: i64, level: &str, target: &str, msg: &str) -> String {
        serde_json::json!({
            "v": 1, "ts": super::super::record::format_ts(ts_ms), "ts_ms": ts_ms,
            "level": level, "target": target, "msg": msg, "pid": 1,
        })
        .to_string()
    }

    fn q(limit: usize) -> LogQuery {
        LogQuery {
            limit,
            include_rotated: true,
            ..Default::default()
        }
    }

    #[test]
    fn records_come_back_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        write_log(
            dir.path(),
            BASE_NAME,
            &[
                record(1000, "INFO", "a", "oldest"),
                record(2000, "INFO", "a", "middle"),
                record(3000, "INFO", "a", "newest"),
            ],
        );

        let page = query(dir.path(), &q(10)).unwrap();
        let messages: Vec<&str> = page.entries.iter().map(|e| e.msg.as_str()).collect();
        assert_eq!(messages, ["newest", "middle", "oldest"]);
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn a_partial_trailing_line_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = File::create(dir.path().join(BASE_NAME)).unwrap();
        writeln!(file, "{}", record(1000, "INFO", "a", "complete")).unwrap();
        // No newline: this is what a record still being written looks like.
        write!(file, "{{\"v\":1,\"msg\":\"half writt").unwrap();
        drop(file);

        let page = query(dir.path(), &q(10)).unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].msg, "complete");
    }

    #[test]
    fn a_corrupt_line_fails_the_page() {
        let dir = tempfile::tempdir().unwrap();
        write_log(
            dir.path(),
            BASE_NAME,
            &[
                record(1000, "INFO", "a", "before"),
                "{not json at all".to_string(),
                record(3000, "INFO", "a", "after"),
            ],
        );

        let error = query(dir.path(), &q(10)).unwrap_err();
        assert!(error.contains("invalid log record"), "{error}");
    }

    #[test]
    fn a_line_longer_than_one_chunk_is_reassembled() {
        let dir = tempfile::tempdir().unwrap();
        let long_msg = "x".repeat(CHUNK * 2 + 137);
        write_log(
            dir.path(),
            BASE_NAME,
            &[
                record(1000, "INFO", "a", "before"),
                record(2000, "INFO", "a", &long_msg),
            ],
        );

        let page = query(dir.path(), &q(10)).unwrap();
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.entries[0].msg.len(), long_msg.len());
        assert_eq!(page.entries[1].msg, "before");
    }

    #[test]
    fn an_empty_or_missing_file_yields_an_empty_page() {
        let dir = tempfile::tempdir().unwrap();
        let page = query(dir.path(), &q(10)).unwrap();
        assert!(page.entries.is_empty());
        assert!(page.next_cursor.is_none());

        File::create(dir.path().join(BASE_NAME)).unwrap();
        let page = query(dir.path(), &q(10)).unwrap();
        assert!(page.entries.is_empty());
    }

    #[test]
    fn paging_covers_every_record_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let lines: Vec<String> = (0..25)
            .map(|i| record(1000 + i, "INFO", "a", &format!("line {i}")))
            .collect();
        write_log(dir.path(), BASE_NAME, &lines);

        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let page = query(dir.path(), &LogQuery { cursor, ..q(7) }).unwrap();
            seen.extend(page.entries.iter().map(|e| e.msg.clone()));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        assert_eq!(seen.len(), 25, "{seen:?}");
        let mut sorted = seen.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 25, "records repeated across pages");
    }

    #[test]
    fn paging_continues_into_rotated_files() {
        let dir = tempfile::tempdir().unwrap();
        write_log(dir.path(), &archive_name(1), &[record(1000, "INFO", "a", "in archive")]);
        write_log(dir.path(), BASE_NAME, &[record(2000, "INFO", "a", "in current")]);

        let first = query(dir.path(), &q(1)).unwrap();
        assert_eq!(first.entries[0].msg, "in current");

        let second = query(
            dir.path(),
            &LogQuery {
                cursor: first.next_cursor,
                ..q(1)
            },
        )
        .unwrap();
        assert_eq!(second.entries[0].msg, "in archive");
    }

    #[test]
    fn rotated_files_are_skipped_when_not_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        write_log(dir.path(), &archive_name(1), &[record(1000, "INFO", "a", "in archive")]);
        write_log(dir.path(), BASE_NAME, &[record(2000, "INFO", "a", "in current")]);

        let page = query(
            dir.path(),
            &LogQuery {
                include_rotated: false,
                ..q(10)
            },
        )
        .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.files_scanned, [BASE_NAME]);
    }

    #[test]
    fn the_level_filter_is_a_floor() {
        let dir = tempfile::tempdir().unwrap();
        write_log(
            dir.path(),
            BASE_NAME,
            &[
                record(1000, "INFO", "a", "info"),
                record(2000, "WARN", "a", "warn"),
                record(3000, "ERROR", "a", "error"),
            ],
        );

        let page = query(
            dir.path(),
            &LogQuery {
                min_level: Some(LogLevel::Warn),
                ..q(10)
            },
        )
        .unwrap();
        let messages: Vec<&str> = page.entries.iter().map(|e| e.msg.as_str()).collect();
        assert_eq!(messages, ["error", "warn"]);
    }

    #[test]
    fn searching_is_case_insensitive_and_covers_fields() {
        let dir = tempfile::tempdir().unwrap();
        let with_field = serde_json::json!({
            "v": 1, "ts": super::super::record::format_ts(2000), "ts_ms": 2000,
            "level": "WARN", "target": "b", "msg": "nothing here", "pid": 1,
            "fields": {"model": "gpt-5.6-sol"},
        })
        .to_string();
        write_log(
            dir.path(),
            BASE_NAME,
            &[record(1000, "INFO", "a", "Provider Rejected"), with_field],
        );

        let by_msg = query(
            dir.path(),
            &LogQuery {
                contains: Some("provider".into()),
                ..q(10)
            },
        )
        .unwrap();
        assert_eq!(by_msg.entries.len(), 1);
        assert_eq!(by_msg.entries[0].msg, "Provider Rejected");

        let by_field = query(
            dir.path(),
            &LogQuery {
                contains: Some("GPT-5.6".into()),
                ..q(10)
            },
        )
        .unwrap();
        assert_eq!(by_field.entries.len(), 1);
        assert_eq!(by_field.entries[0].target, "b");
    }

    #[test]
    fn the_conversation_filter_reads_span_context() {
        let dir = tempfile::tempdir().unwrap();
        let scoped = serde_json::json!({
            "v": 1, "ts": super::super::record::format_ts(2000), "ts_ms": 2000,
            "level": "WARN", "target": "chat", "msg": "compact failed", "pid": 1,
            "spans": ["chat"], "span_fields": {"conversation_id": "c-1"},
        })
        .to_string();
        write_log(
            dir.path(),
            BASE_NAME,
            &[record(1000, "WARN", "chat", "unrelated"), scoped],
        );

        let page = query(
            dir.path(),
            &LogQuery {
                conversation_id: Some("c-1".into()),
                ..q(10)
            },
        )
        .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].msg, "compact failed");
    }

    #[test]
    fn the_scan_stops_at_the_start_of_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let lines: Vec<String> = (0..100)
            .map(|i| record(1000 + i * 10, "INFO", "a", &format!("line {i}")))
            .collect();
        write_log(dir.path(), BASE_NAME, &lines);

        // Nothing matches the text, so only the window can stop the scan.
        let page = query(
            dir.path(),
            &LogQuery {
                since_ts_ms: Some(1900),
                contains: Some("no-such-text".into()),
                ..q(50)
            },
        )
        .unwrap();

        assert!(page.entries.is_empty());
        // Having stopped on the window rather than the budget, there is no more
        // to offer.
        assert!(page.next_cursor.is_none());
        assert!(!page.scan_truncated);
    }

    #[test]
    fn the_end_time_bound_excludes_newer_records() {
        let dir = tempfile::tempdir().unwrap();
        write_log(
            dir.path(),
            BASE_NAME,
            &[record(1000, "INFO", "a", "old"), record(5000, "INFO", "a", "new")],
        );

        let page = query(
            dir.path(),
            &LogQuery {
                until_ts_ms: Some(2000),
                ..q(10)
            },
        )
        .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].msg, "old");
    }
}

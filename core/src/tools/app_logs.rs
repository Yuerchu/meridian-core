use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Permission, Tool, ToolContext};
use crate::logging::LogLevel;
use crate::logging::reader::{self, LogEntry, LogQuery, LogRecordLevel};
use crate::util::{now_ms, take_bytes_at_char_boundary};

/// Reads the application's own log so the assistant can answer "why did that
/// fail" from evidence instead of guesswork.
///
/// Like `LoadSkillTool`, the directory is injected at construction and the tool
/// never goes through `ToolContext::resolve_and_validate`. That is not a
/// shortcut: the log lives in app-private storage, which sits outside every
/// `FileAccess` root on Android and outside the project sandbox on the desktop,
/// so a path-based tool could not reach it in most configurations. Taking no
/// path parameter at all also leaves nothing to traverse.
pub struct ReadAppLogsTool {
    dir: PathBuf,
}

/// Ceiling on the rendered output. Deliberately well under the generic tool
/// truncator's budget so a record is never cut in half — and truncation there
/// would drop the *newest* lines, which are the ones being asked about.
const MAX_OUTPUT_BYTES: usize = 12 * 1024;
const MAX_MSG_CHARS: usize = 300;
const MAX_FIELD_CHARS: usize = 120;
const MAX_FIELDS_SHOWN: usize = 8;
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;
const DEFAULT_SINCE_MINUTES: i64 = 60;
/// A week. Beyond this the rotated files have almost certainly been recycled.
const MAX_SINCE_MINUTES: i64 = 7 * 24 * 60;

impl ReadAppLogsTool {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

#[async_trait]
impl Tool for ReadAppLogsTool {
    fn name(&self) -> &str {
        "read_app_logs"
    }

    fn description(&self) -> &str {
        "Read Meridian's own application log to find out why something failed. This is the app's \
         internal runtime log, not the user's files and not the conversation history. Use it for \
         provider and API errors, request timeouts, MCP servers failing to connect, context \
         compaction not running, and tools being denied. Records come back newest first, already \
         filtered and formatted — there is nothing to grep."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "level": {
                    "type": "string",
                    "enum": ["error", "warn", "info"],
                    "description": "Minimum severity to return. The log records info and above."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_LIMIT,
                    "description": "Maximum records to return, newest first. Defaults to 50."
                },
                "contains": {
                    "type": "string",
                    "description": "Case-insensitive substring matched against the message, the \
                                    target and the field values. Not a regular expression."
                },
                "target_prefix": {
                    "type": "string",
                    "description": "Restrict to one subsystem by log target prefix, e.g. \
                                    'meridian_lib::provider' or 'meridian_lib::mcp'."
                },
                "this_conversation": {
                    "type": "boolean",
                    "description": "Only records emitted while handling the current conversation. \
                                    Start here when the user asks why something just failed."
                },
                "since_minutes": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_SINCE_MINUTES,
                    "description": "How far back to look. Defaults to 60."
                }
            },
            "required": []
        })
    }

    /// Read-only, and confined to the app's own log. Prompting for approval
    /// would turn a one-turn answer into a three-tap ceremony.
    fn default_permission(&self) -> Permission {
        Permission::Always
    }

    async fn execute(&self, args: Value, context: &ToolContext) -> Result<String, String> {
        let level = crate::logging::validate_level(args.get("level").and_then(Value::as_str).unwrap_or("warn"))?;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| (n as usize).clamp(1, MAX_LIMIT))
            .unwrap_or(DEFAULT_LIMIT);
        let since_minutes = args
            .get("since_minutes")
            .and_then(Value::as_i64)
            .unwrap_or(DEFAULT_SINCE_MINUTES)
            .clamp(1, MAX_SINCE_MINUTES);
        let this_conversation = args.get("this_conversation").and_then(Value::as_bool).unwrap_or(false);

        let since_ts_ms = now_ms() - since_minutes * 60_000;
        let query = LogQuery {
            min_level: Some(level),
            limit,
            contains: args
                .get("contains")
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|s| !s.trim().is_empty()),
            target_prefix: args
                .get("target_prefix")
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|s| !s.trim().is_empty()),
            // The model cannot know its own conversation id — it never appears
            // in the context — so it asks for "this one" and we resolve it.
            conversation_id: if this_conversation {
                context.conversation_id.clone()
            } else {
                None
            },
            since_ts_ms: Some(since_ts_ms),
            until_ts_ms: None,
            include_rotated: true,
            cursor: None,
        };

        let dir = self.dir.clone();
        let page = tokio::task::spawn_blocking(move || reader::query(&dir, &query))
            .await
            .map_err(|e| e.to_string())??;

        Ok(render(&page.entries, level, since_minutes, page.scan_truncated))
    }
}

fn render(entries: &[LogEntry], level: LogLevel, since_minutes: i64, scan_truncated: bool) -> String {
    if entries.is_empty() {
        // A sentence, not an error: an Err reads to the model as "the tool is
        // broken" and it retries instead of reporting what it found.
        return format!(
            "No {} records in the last {since_minutes} minutes. \
             Either nothing went wrong in that window, or it happened earlier — \
             try a wider since_minutes or a lower level.",
            level.as_str().to_uppercase()
        );
    }

    // Timestamps are stated as UTC here rather than left ambiguous: the user
    // will quote their own local clock, and the two are rarely the same.
    let mut out = format!(
        "Application log, newest first. Window: last {since_minutes} minutes, minimum level {}. \
         Times are UTC.\n\n",
        level.as_str().to_uppercase()
    );

    let mut shown = 0usize;
    for entry in entries {
        let block = render_entry(entry);
        // Stop on the byte budget rather than letting the generic truncator cut
        // a record in half further downstream.
        if out.len() + block.len() > MAX_OUTPUT_BYTES {
            break;
        }
        out.push_str(&block);
        shown += 1;
    }

    let omitted = entries.len() - shown;
    if omitted > 0 {
        // Said explicitly: a silent cut makes the model conclude there were only
        // as many problems as it could see.
        out.push_str(&format!(
            "\n{omitted} more matching record(s) were omitted to keep this readable. \
             Narrow with contains or target_prefix to see them.\n"
        ));
    }
    if scan_truncated {
        out.push_str(
            "\nThe scan stopped at its size budget, so older records in this window were not \
             examined.\n",
        );
    }
    out
}

fn render_entry(entry: &LogEntry) -> String {
    // Time only: the window is stated once in the header, and the date would
    // cost a dozen characters on every line.
    let time = entry.ts.split('T').nth(1).unwrap_or(&entry.ts).trim_end_matches('Z');
    let mut block = format!(
        "[{time}] {} {} — {}\n",
        match entry.level {
            LogRecordLevel::Error => "ERROR",
            LogRecordLevel::Warn => "WARN",
            LogRecordLevel::Info => "INFO",
            LogRecordLevel::Debug => "DEBUG",
        },
        entry.target,
        clip(&entry.msg, MAX_MSG_CHARS)
    );

    let mut pairs: Vec<String> = Vec::new();
    for (key, value) in entry.span_fields.iter().chain(entry.fields.iter()) {
        if pairs.len() >= MAX_FIELDS_SHOWN {
            break;
        }
        let rendered = match value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        pairs.push(format!("{key}={}", clip(&rendered, MAX_FIELD_CHARS)));
    }
    let total_fields = entry.span_fields.len() + entry.fields.len();
    if total_fields > pairs.len() {
        pairs.push(format!("(+{} more fields)", total_fields - pairs.len()));
    }
    if !pairs.is_empty() {
        block.push_str(&format!("    {}\n", pairs.join("  ")));
    }
    block
}

fn clip(text: &str, max_chars: usize) -> String {
    let flat = text.replace('\n', " ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    // Byte-safe truncation; max_chars is an upper bound either way.
    format!("{}…", take_bytes_at_char_boundary(&flat, max_chars))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    fn entry(level: LogRecordLevel, target: &str, msg: &str) -> LogEntry {
        LogEntry {
            ts: "2026-07-29T09:58:31.204Z".into(),
            ts_ms: 1_785_066_000_000,
            level,
            target: target.into(),
            msg: msg.into(),
            fields: Map::new(),
            spans: Vec::new(),
            span_fields: Map::new(),
            file: None,
            line: None,
            cursor: reader::Cursor {
                file_index: 0,
                byte_offset: 0,
            },
        }
    }

    #[test]
    fn an_empty_result_explains_itself_rather_than_erroring() {
        let out = render(&[], LogLevel::Warn, 60, false);
        assert!(out.contains("No WARN records"), "{out}");
        assert!(out.contains("since_minutes"), "{out}");
    }

    #[test]
    fn a_record_renders_as_one_compact_line_plus_its_fields() {
        let mut e = entry(
            LogRecordLevel::Error,
            "meridian_lib::provider",
            "HTTP 401 from provider",
        );
        e.fields.insert("status".into(), Value::from(401));
        e.span_fields.insert("conversation_id".into(), Value::from("c-42"));

        let out = render(&[e], LogLevel::Warn, 60, false);
        assert!(
            out.contains("[09:58:31.204] ERROR meridian_lib::provider — HTTP 401"),
            "{out}"
        );
        assert!(out.contains("conversation_id=c-42"), "{out}");
        assert!(out.contains("status=401"), "{out}");
    }

    #[test]
    fn output_is_capped_and_says_how_much_it_dropped() {
        let entries: Vec<LogEntry> = (0..400)
            .map(|i| {
                entry(
                    LogRecordLevel::Error,
                    "meridian_lib::provider",
                    &format!("failure number {i}"),
                )
            })
            .collect();

        let out = render(&entries, LogLevel::Error, 60, false);
        assert!(out.len() <= MAX_OUTPUT_BYTES + 512, "output was {} bytes", out.len());
        // Being explicit about the cut is the point: otherwise the model reports
        // only the failures it happened to see.
        assert!(out.contains("more matching record(s) were omitted"), "{out}");
    }

    #[test]
    fn long_messages_are_clipped_on_a_char_boundary() {
        let long = "多字节".repeat(500);
        let out = render(&[entry(LogRecordLevel::Warn, "t", &long)], LogLevel::Warn, 60, false);
        assert!(out.contains('…'), "{out}");
        assert!(out.len() < long.len());
    }

    #[test]
    fn a_budget_stop_is_reported() {
        let out = render(&[entry(LogRecordLevel::Error, "t", "boom")], LogLevel::Error, 60, true);
        assert!(out.contains("size budget"), "{out}");
    }
}

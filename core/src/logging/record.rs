//! The JSONL record: one line per event, written by the layer and read back by
//! both the log panel and the `read_app_logs` tool.

use serde::Serialize;
use serde_json::{Map, Value};
use tracing::field::{Field, Visit};

use super::redact;

/// Bumped when the shape changes incompatibly. Readers are defensive about
/// missing fields, but a version lets them tell "old file" from "corrupt file"
/// instead of guessing.
pub(crate) const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize)]
pub(crate) struct LogRecord<'a> {
    pub v: u32,
    /// RFC3339 with milliseconds, UTC. Lexical order equals chronological order,
    /// which is what lets the reader stop scanning early.
    pub ts: String,
    /// The same instant as `ts`, in the unit `util::now_ms` uses, so the
    /// frontend can sort and range-filter without parsing a date.
    pub ts_ms: i64,
    pub level: &'a str,
    pub target: &'a str,
    pub msg: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// Two instances writing the same file interleave legibly with this.
    pub pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    /// Fields the event itself carried.
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub fields: Map<String, Value>,
    /// Enclosing span names, outermost first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub spans: Vec<String>,
    /// Fields inherited from those spans. Kept apart from `fields` so a reader
    /// can tell what the event said from what its context said.
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub span_fields: Map<String, Value>,
}

/// Collects `tracing` fields into JSON, redacting on the way in.
///
/// This is the single choke point for redaction: events and spans both record
/// through it, so there is no second path a credential could take.
#[derive(Default)]
pub(crate) struct JsonVisitor {
    pub message: Option<String>,
    pub fields: Map<String, Value>,
}

impl JsonVisitor {
    fn put(&mut self, field: &Field, value: Value) {
        self.fields
            .insert(field.name().to_string(), redact::scrub_field(field.name(), value));
    }
}

impl Visit for JsonVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // The `message` field is how the macros pass the format string's output.
        // It arrives as `format_args!`, whose Debug forwards to Display, so this
        // is the rendered text rather than a quoted literal.
        if field.name() == "message" {
            self.message = Some(redact::scrub_text(&format!("{value:?}")));
            return;
        }
        self.put(field, Value::String(format!("{value:?}")));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(redact::scrub_text(value));
            return;
        }
        self.put(field, Value::String(value.to_string()));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, Value::from(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, Value::Bool(value));
    }

    /// Walks the `source()` chain. The outermost error is usually a wrapper
    /// ("request failed"); the cause worth reading is three links down.
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        let mut chain = vec![Value::String(value.to_string())];
        let mut source = value.source();
        while let Some(err) = source {
            chain.push(Value::String(err.to_string()));
            source = err.source();
        }
        self.put(field, Value::Array(chain));
    }
}

/// Formats an epoch-millisecond instant the way `ts` needs it.
pub(crate) fn format_ts(ts_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ts_ms)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives a visitor the way `tracing` does, without standing up a subscriber.
    fn visit(f: impl FnOnce(&mut JsonVisitor, &tracing::Metadata<'static>)) -> JsonVisitor {
        let mut visitor = JsonVisitor::default();
        let meta = test_metadata();
        f(&mut visitor, meta);
        visitor
    }

    fn test_metadata() -> &'static tracing::Metadata<'static> {
        // A callsite with the fields the tests need to look up by name.
        struct Cs;
        impl tracing::callsite::Callsite for Cs {
            fn set_interest(&self, _: tracing::subscriber::Interest) {}
            fn metadata(&self) -> &tracing::Metadata<'_> {
                META.get_or_init(|| {
                    tracing::Metadata::new(
                        "test",
                        "logging::record::tests",
                        tracing::Level::INFO,
                        None,
                        None,
                        None,
                        tracing::field::FieldSet::new(
                            &["message", "api_key", "status", "error"],
                            tracing::callsite::Identifier(&Cs),
                        ),
                        tracing::metadata::Kind::EVENT,
                    )
                })
            }
        }
        static META: std::sync::OnceLock<tracing::Metadata<'static>> = std::sync::OnceLock::new();
        static CS: Cs = Cs;
        tracing::callsite::Callsite::metadata(&CS)
    }

    fn field(name: &str) -> Field {
        test_metadata().fields().field(name).expect("field declared above")
    }

    #[test]
    fn the_message_field_becomes_the_message_not_a_field() {
        let v = visit(|v, _| v.record_str(&field("message"), "something failed"));
        assert_eq!(v.message.as_deref(), Some("something failed"));
        assert!(v.fields.is_empty());
    }

    #[test]
    fn a_credential_field_is_redacted_on_the_way_in() {
        let v = visit(|v, _| v.record_str(&field("api_key"), "sk-live-abcdefgh"));
        let stored = v.fields.get("api_key").unwrap().as_str().unwrap();
        assert!(stored.starts_with("[REDACTED"), "{stored}");
    }

    #[test]
    fn numeric_fields_stay_numeric() {
        let v = visit(|v, _| v.record_i64(&field("status"), 401));
        assert_eq!(v.fields.get("status").unwrap(), &Value::from(401));
    }

    #[test]
    fn an_error_is_recorded_as_its_whole_source_chain() {
        #[derive(Debug)]
        struct Inner;
        impl std::fmt::Display for Inner {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "connection refused")
            }
        }
        impl std::error::Error for Inner {}

        #[derive(Debug)]
        struct Outer(Inner);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "request failed")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let v = visit(|v, _| v.record_error(&field("error"), &Outer(Inner)));
        let chain = v.fields.get("error").unwrap().as_array().unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0], Value::from("request failed"));
        assert_eq!(chain[1], Value::from("connection refused"));
    }

    #[test]
    fn timestamps_sort_lexically_in_time_order() {
        let earlier = format_ts(1_785_066_896_789);
        let later = format_ts(1_785_066_896_790);
        assert!(earlier.ends_with("Z"), "{earlier}");
        assert!(earlier < later, "{earlier} !< {later}");
    }
}

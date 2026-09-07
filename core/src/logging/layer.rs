//! The `tracing` layer that turns events into JSONL lines.

use serde_json::{Map, Value};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use super::record::{JsonVisitor, LogRecord, SCHEMA_VERSION, format_ts};
use crate::util::now_ms;

/// Where finished lines go. Behind a trait so tests can collect into a `Vec`
/// instead of standing up the real file writer and its global state.
pub(crate) trait LineSink: Send + Sync {
    /// `line` is one complete record including its trailing newline.
    fn emit(&self, line: &[u8]);
}

/// Span fields, stashed on the span when it opens so events inside it can pick
/// them up. `Attributes` is only offered at creation time.
struct SpanFields(Map<String, Value>);

pub(crate) struct JsonlLayer {
    sink: &'static dyn LineSink,
}

impl JsonlLayer {
    pub(crate) fn new(sink: &'static dyn LineSink) -> Self {
        Self { sink }
    }
}

thread_local! {
    /// Reused across events so a busy thread is not allocating a buffer per log
    /// line.
    static BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

impl<S> Layer<S> for JsonlLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut visitor = JsonVisitor::default();
        attrs.record(&mut visitor);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(SpanFields(visitor.fields));
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut visitor = JsonVisitor::default();
        values.record(&mut visitor);
        let mut extensions = span.extensions_mut();
        match extensions.get_mut::<SpanFields>() {
            Some(existing) => existing.0.extend(visitor.fields),
            None => extensions.insert(SpanFields(visitor.fields)),
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut spans = Vec::new();
        let mut span_fields = Map::new();
        if let Some(scope) = ctx.event_scope(event) {
            // Outermost first, so an inner span that redeclares a field wins —
            // the nearer context is the more specific one.
            for span in scope.from_root() {
                spans.push(span.name().to_string());
                if let Some(fields) = span.extensions().get::<SpanFields>() {
                    for (k, v) in &fields.0 {
                        span_fields.insert(k.clone(), v.clone());
                    }
                }
            }
        }

        let mut visitor = JsonVisitor::default();
        event.record(&mut visitor);

        let meta = event.metadata();
        let ts_ms = now_ms();
        let record = LogRecord {
            v: SCHEMA_VERSION,
            // Stamped here rather than where the line is finally written: an
            // event buffered before the file exists still carries the instant it
            // happened, so timestamps in the file stay monotonic.
            ts: format_ts(ts_ms),
            ts_ms,
            level: meta.level().as_str(),
            target: meta.target(),
            msg: visitor.message.unwrap_or_default(),
            file: meta.file(),
            line: meta.line(),
            pid: std::process::id(),
            thread: std::thread::current().name().map(str::to_string),
            fields: visitor.fields,
            spans,
            span_fields,
        };

        BUF.with(|buf| {
            let Ok(mut buf) = buf.try_borrow_mut() else { return };
            buf.clear();
            if serde_json::to_writer(&mut *buf, &record).is_err() {
                return;
            }
            buf.push(b'\n');
            // One call, one whole line: see RollingWriter::write_line.
            self.sink.emit(&buf);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Default)]
    struct CollectingSink(Mutex<Vec<String>>);

    impl LineSink for CollectingSink {
        fn emit(&self, line: &[u8]) {
            self.0.lock().unwrap().push(String::from_utf8_lossy(line).into_owned());
        }
    }

    /// Runs `body` against a subscriber scoped to this thread.
    ///
    /// Deliberately not a global subscriber: only one can ever be installed per
    /// process, so a global here would make the tests order-dependent and stop
    /// them running in parallel.
    fn capture(body: impl FnOnce()) -> Vec<Value> {
        let sink: &'static CollectingSink = Box::leak(Box::new(CollectingSink::default()));
        let subscriber = Registry::default().with(JsonlLayer::new(sink));
        tracing::subscriber::with_default(subscriber, body);
        sink.0
            .lock()
            .unwrap()
            .iter()
            .map(|line| serde_json::from_str(line).expect("each line is valid JSON"))
            .collect()
    }

    #[test]
    fn an_event_becomes_one_parseable_line() {
        let records = capture(|| tracing::info!(status = 401, "provider rejected the request"));

        assert_eq!(records.len(), 1);
        let r = &records[0];
        assert_eq!(r["v"], SCHEMA_VERSION);
        assert_eq!(r["level"], "INFO");
        assert_eq!(r["msg"], "provider rejected the request");
        assert_eq!(r["fields"]["status"], 401);
        assert!(r["ts"].as_str().unwrap().ends_with('Z'));
        assert!(r["ts_ms"].as_i64().unwrap() > 0);
        assert!(r["target"].as_str().unwrap().contains("logging::layer"));
    }

    #[test]
    fn every_line_ends_with_exactly_one_newline() {
        let sink: &'static CollectingSink = Box::leak(Box::new(CollectingSink::default()));
        let subscriber = Registry::default().with(JsonlLayer::new(sink));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("one");
            tracing::info!("two");
        });

        let lines = sink.0.lock().unwrap();
        assert_eq!(lines.len(), 2);
        for line in lines.iter() {
            assert!(line.ends_with('\n'), "{line:?}");
            assert_eq!(line.matches('\n').count(), 1, "{line:?}");
        }
    }

    #[test]
    fn span_context_travels_with_the_event() {
        let records = capture(|| {
            let span = tracing::info_span!("chat", conversation_id = "c-123");
            span.in_scope(|| tracing::warn!("auto-compact failed"));
        });

        let r = &records[0];
        assert_eq!(r["spans"], serde_json::json!(["chat"]));
        assert_eq!(r["span_fields"]["conversation_id"], "c-123");
        // Context stays separate from what the event itself said.
        assert!(r["fields"].get("conversation_id").is_none());
    }

    #[test]
    fn the_nearest_span_wins_a_redeclared_field() {
        let records = capture(|| {
            let outer = tracing::info_span!("outer", scope = "outer");
            outer.in_scope(|| {
                let inner = tracing::info_span!("inner", scope = "inner");
                inner.in_scope(|| tracing::info!("hi"));
            });
        });

        assert_eq!(records[0]["span_fields"]["scope"], "inner");
        assert_eq!(records[0]["spans"], serde_json::json!(["outer", "inner"]));
    }

    #[test]
    fn fields_recorded_after_the_span_opened_are_picked_up() {
        let records = capture(|| {
            let span = tracing::info_span!("chat", conversation_id = tracing::field::Empty);
            span.record("conversation_id", "c-late");
            span.in_scope(|| tracing::info!("hi"));
        });

        assert_eq!(records[0]["span_fields"]["conversation_id"], "c-late");
    }

    #[test]
    fn credentials_are_redacted_before_they_reach_the_line() {
        let records = capture(|| tracing::error!(api_key = "sk-live-secret-value", "auth failed"));

        let line = records[0].to_string();
        assert!(!line.contains("sk-live-secret-value"), "{line}");
        assert!(line.contains("REDACTED"), "{line}");
    }

    #[test]
    fn an_empty_span_stack_omits_the_context_fields_entirely() {
        let records = capture(|| tracing::info!("standalone"));
        assert!(records[0].get("spans").is_none());
        assert!(records[0].get("span_fields").is_none());
    }
}

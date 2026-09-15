//! Everywhere an alert can come out.
//!
//! Webhooks are not in this registry — they are read from the database on every
//! dispatch, because that is where they live and a cached copy would be one
//! more thing that can disagree with the settings page. What registers here is
//! an outlet that exists only while something else is running: today that is
//! the OneBot server, whose sink is a QQ private message to the admins and
//! which must stop receiving the moment the server does.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::alert::Alert;

#[async_trait::async_trait]
pub trait AlertSink: Send + Sync {
    /// For the log line when one fails. A sink is behind an `Arc<dyn _>` by the
    /// time anything goes wrong with it, so it cannot otherwise be named.
    fn name(&self) -> &'static str;

    async fn deliver(&self, alert: &Alert) -> Result<(), String>;
}

/// Names a registration so it can be taken back out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AlertSinkId(u64);

#[derive(Clone, Default)]
pub struct AlertSinks(Arc<Inner>);

#[derive(Default)]
struct Inner {
    /// A `std::sync::RwLock` holding only clones handed out under it: delivery
    /// awaits, and a lock may not be held across that.
    sinks: std::sync::RwLock<Vec<(AlertSinkId, Arc<dyn AlertSink>)>>,
    next_id: AtomicU64,
}

impl AlertSinks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, sink: Arc<dyn AlertSink>) -> AlertSinkId {
        let id = AlertSinkId(self.0.next_id.fetch_add(1, Ordering::Relaxed));
        match self.0.sinks.write() {
            Ok(mut sinks) => sinks.push((id, sink)),
            Err(poisoned) => poisoned.into_inner().push((id, sink)),
        }
        id
    }

    /// Stop delivering to a sink that has outlived its purpose.
    ///
    /// The OneBot server calls this on stop. Left registered, the outgoing
    /// generation's shared state — its admin list included — would keep
    /// receiving alerts and trying to send them over a connection it no longer
    /// owns.
    pub fn unregister(&self, id: AlertSinkId) {
        match self.0.sinks.write() {
            Ok(mut sinks) => sinks.retain(|(existing, _)| *existing != id),
            Err(poisoned) => poisoned.into_inner().retain(|(existing, _)| *existing != id),
        }
    }

    /// The registered sinks, cloned out so nothing is held across an await.
    pub fn snapshot(&self) -> Vec<Arc<dyn AlertSink>> {
        match self.0.sinks.read() {
            Ok(sinks) => sinks.iter().map(|(_, sink)| sink.clone()).collect(),
            Err(poisoned) => poisoned.into_inner().iter().map(|(_, sink)| sink.clone()).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct Counter(&'static str, Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl AlertSink for Counter {
        fn name(&self) -> &'static str {
            self.0
        }
        async fn deliver(&self, _alert: &Alert) -> Result<(), String> {
            self.1.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn a_sink_stops_receiving_once_it_is_unregistered() {
        let sinks = AlertSinks::new();
        let hits = Arc::new(AtomicUsize::new(0));
        let id = sinks.register(Arc::new(Counter("one", hits.clone())));
        sinks.register(Arc::new(Counter("two", hits.clone())));
        assert_eq!(sinks.snapshot().len(), 2);

        sinks.unregister(id);
        let remaining = sinks.snapshot();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].name(), "two");

        // Unregistering twice is not an error: a server that stops after
        // already having been replaced must not panic on the way out.
        sinks.unregister(id);
        assert_eq!(sinks.snapshot().len(), 1);
    }
}

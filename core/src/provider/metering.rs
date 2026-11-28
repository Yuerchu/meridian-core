//! Provider-boundary request metering port (`L1`).
//!
//! Metering wraps [`ChatProvider`] rather than hooking only `engine::run_turn`
//! so every billable upstream request — chat turns, title generation,
//! compaction summaries, auto-review, delegated sub-agents, cache warming, and
//! Auto-Dream consolidation — passes through one uniform reserve/settle
//! contract.
//!
//! ## Placement relative to retries
//!
//! [`MeteredProvider`] **must** wrap outside the transport retry loop
//! ([`meridian_provider_sdk::client::run_with_retry`]). Every built-in
//! [`ChatProvider`] executes transport retries internally inside its trait
//! methods, so decorating a [`ChatProvider`] places metering automatically on
//! the outside: transient 5xx / network retries happen under a single hold and
//! are never misclassified as dropped/cancelled streams.
//!
//! ## Settlement semantics
//!
//! - [`ReserveOutcome::Refused`] -> fails immediately with
//!   [`ProviderError::MeteringRefused`] without calling the inner provider
//!   (never retried by the turn loop).
//! - Upstream reports usage -> [`Settlement::Usage`].
//! - Upstream fails before producing usage (or closes without reporting usage)
//!   -> [`Settlement::NoUsage`] (full refund).
//! - Caller drops an in-flight request future or stream before completion
//!   (e.g. user pressed Stop or disconnected) -> [`Settlement::Cancelled`]
//!   (`keep_all`).
//!
//! Desktop and BYOK deployments use [`NoopMetering`] via [`wrap_provider`] /
//! [`wrap_provider_arc`], which returns the inner provider unwrapped.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures::Stream;

use crate::provider::{
    AgentResponse, ChatMessage, ChatParams, ChatProvider, ChatStream, LocalInferenceBackend, ProviderCapabilities,
    ProviderError, RemoteCompactResult, StreamEvent, SwitchImpact, TokenUsage, ToolDefinition,
};

/// Opaque identifier for a pending balance reservation returned by
/// [`Metering::reserve`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HoldId(pub String);

impl HoldId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Which [`ChatProvider`] entrypoint is being reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    StreamChat,
    Chat,
    ChatWithTools,
    CompactRemote,
}

/// Snapshot of an outgoing provider call handed to [`Metering::reserve`] so the
/// ledger can estimate prompt + max-output pre-authorization.
pub struct ReserveRequest<'a> {
    pub adapter_name: &'static str,
    pub kind: RequestKind,
    pub model: &'a str,
    pub messages: &'a [ChatMessage],
    pub tools: &'a [ToolDefinition],
    pub params: &'a ChatParams,
}

/// Successful pre-authorization holding funds until [`Metering::settle`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    pub hold_id: HoldId,
}

/// Explicit denial before the upstream request is issued (e.g. insufficient
/// credits or suspended account).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub reason: String,
}

/// Result of [`Metering::reserve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReserveOutcome {
    Reserved(Reservation),
    Refused(Refusal),
}

/// Final disposition reported to [`Metering::settle`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settlement {
    /// Upstream produced billable usage; adjust hold to actual cost/sale.
    Usage(TokenUsage),
    /// Upstream failed before producing usage (or closed cleanly with none);
    /// refund the entire reservation.
    NoUsage,
    /// Caller abandoned an in-flight request or stream before completion
    /// (Stop / disconnect); retain the full reservation (`keep_all`).
    Cancelled,
}

/// Port implemented by billing/ledger backends (such as `LedgerMetering` in
/// `meridian-server`) and by [`NoopMetering`] on desktop/BYOK builds.
#[async_trait]
pub trait Metering: Send + Sync {
    /// Reserve funds before calling the upstream provider.
    ///
    /// Returning `Err(msg)` fails closed with [`ProviderError::MeteringRefused`]
    /// so an unavailable ledger never lets unmetered upstream calls through.
    async fn reserve(&self, request: ReserveRequest<'_>) -> Result<ReserveOutcome, String>;

    /// Settle a previously created hold. Implementations should be idempotent
    /// per [`HoldId`].
    async fn settle(&self, hold_id: HoldId, outcome: Settlement);

    /// Whether this implementation is a no-op so [`wrap_provider`] can skip
    /// decorating the inner provider altogether.
    fn is_noop(&self) -> bool {
        false
    }
}

/// Zero-overhead desktop / BYOK metering port.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopMetering;

#[async_trait]
impl Metering for NoopMetering {
    async fn reserve(&self, _request: ReserveRequest<'_>) -> Result<ReserveOutcome, String> {
        Ok(ReserveOutcome::Reserved(Reservation {
            hold_id: HoldId::new(""),
        }))
    }

    async fn settle(&self, _hold_id: HoldId, _outcome: Settlement) {}

    fn is_noop(&self) -> bool {
        true
    }
}

/// Wrap `inner` in a [`MeteredProvider`] unless `metering.is_noop()` is true,
/// in which case `inner` is returned untouched.
pub fn wrap_provider(inner: Box<dyn ChatProvider>, metering: Arc<dyn Metering>) -> Box<dyn ChatProvider> {
    if metering.is_noop() {
        inner
    } else {
        Box::new(MeteredProvider::new(inner, metering))
    }
}

/// `Arc`-based counterpart to [`wrap_provider`].
pub fn wrap_provider_arc(inner: Arc<dyn ChatProvider>, metering: Arc<dyn Metering>) -> Arc<dyn ChatProvider> {
    if metering.is_noop() {
        inner
    } else {
        Arc::new(MeteredProvider::new(inner, metering))
    }
}

/// [`ChatProvider`] decorator enforcing reserve/settle accounting on every
/// upstream request, strictly outside internal transport retries.
pub struct MeteredProvider<P> {
    inner: P,
    metering: Arc<dyn Metering>,
}

impl<P> MeteredProvider<P> {
    pub fn new(inner: P, metering: Arc<dyn Metering>) -> Self {
        Self { inner, metering }
    }

    pub fn inner(&self) -> &P {
        &self.inner
    }

    pub fn into_inner(self) -> P {
        self.inner
    }
}

impl<P: ChatProvider> MeteredProvider<P> {
    async fn reserve_for(
        &self,
        kind: RequestKind,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        params: &ChatParams,
    ) -> Result<HoldId, ProviderError> {
        let outcome = self
            .metering
            .reserve(ReserveRequest {
                adapter_name: self.inner.adapter_name(),
                kind,
                model: &params.model,
                messages,
                tools,
                params,
            })
            .await
            .map_err(|err| ProviderError::MeteringRefused(format!("metering unavailable: {err}")))?;

        match outcome {
            ReserveOutcome::Reserved(reservation) => Ok(reservation.hold_id),
            ReserveOutcome::Refused(refusal) => Err(ProviderError::MeteringRefused(refusal.reason)),
        }
    }
}

/// Ensures that dropping an in-flight async call (e.g. via `tokio::select!`
/// cancellation while awaiting upstream headers or non-streaming response)
/// settles the reservation as [`Settlement::Cancelled`].
struct CancelOnDrop {
    metering: Arc<dyn Metering>,
    hold_id: Option<HoldId>,
}

impl CancelOnDrop {
    fn new(metering: Arc<dyn Metering>, hold_id: HoldId) -> Self {
        Self {
            metering,
            hold_id: Some(hold_id),
        }
    }

    fn take(&mut self) -> Option<HoldId> {
        self.hold_id.take()
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(hold_id) = self.hold_id.take() {
            spawn_drop_settlement(self.metering.clone(), hold_id, Settlement::Cancelled);
        }
    }
}

fn spawn_drop_settlement(metering: Arc<dyn Metering>, hold_id: HoldId, outcome: Settlement) {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(async move {
                metering.settle(hold_id, outcome).await;
            });
        }
        Err(_) => {
            tracing::error!(
                hold_id = %hold_id.0,
                "metered request dropped without an active Tokio runtime; settlement could not be dispatched"
            );
        }
    }
}

type SettleFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

struct MeteredStream {
    inner: ChatStream,
    metering: Arc<dyn Metering>,
    hold_id: Option<HoldId>,
    latest_usage: Option<TokenUsage>,
    seen_stop: bool,
    settling: Option<SettleFuture>,
    pending_terminal: Option<Result<StreamEvent, ProviderError>>,
    finished: bool,
}

impl MeteredStream {
    fn new(inner: ChatStream, metering: Arc<dyn Metering>, hold_id: HoldId) -> Self {
        Self {
            inner,
            metering,
            hold_id: Some(hold_id),
            latest_usage: None,
            seen_stop: false,
            settling: None,
            pending_terminal: None,
            finished: false,
        }
    }

    fn begin_settle(&mut self, hold_id: HoldId) {
        let outcome = self
            .latest_usage
            .take()
            .map(Settlement::Usage)
            .unwrap_or(Settlement::NoUsage);
        let metering = self.metering.clone();
        self.settling = Some(Box::pin(async move {
            metering.settle(hold_id, outcome).await;
        }));
    }
}

impl Stream for MeteredStream {
    type Item = Result<StreamEvent, ProviderError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if this.finished {
                return Poll::Ready(None);
            }

            if let Some(fut) = this.settling.as_mut() {
                match fut.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(()) => {
                        this.settling = None;
                        if let Some(item) = this.pending_terminal.take() {
                            return Poll::Ready(Some(item));
                        }
                        this.finished = true;
                        return Poll::Ready(None);
                    }
                }
            }

            match this.inner.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(event))) => match &event {
                    StreamEvent::UsageUpdate { usage } => {
                        this.latest_usage = Some(usage.clone());
                        return Poll::Ready(Some(Ok(event)));
                    }
                    StreamEvent::Stop { usage, .. } => {
                        this.seen_stop = true;
                        if let Some(usage) = usage {
                            this.latest_usage = Some(usage.clone());
                        }
                        return Poll::Ready(Some(Ok(event)));
                    }
                    StreamEvent::Error { .. } => {
                        if let Some(hold_id) = this.hold_id.take() {
                            this.pending_terminal = Some(Ok(event));
                            this.begin_settle(hold_id);
                            continue;
                        }
                        return Poll::Ready(Some(Ok(event)));
                    }
                    _ => return Poll::Ready(Some(Ok(event))),
                },
                Poll::Ready(Some(Err(err))) => {
                    if let Some(hold_id) = this.hold_id.take() {
                        this.pending_terminal = Some(Err(err));
                        this.begin_settle(hold_id);
                        continue;
                    }
                    return Poll::Ready(Some(Err(err)));
                }
                Poll::Ready(None) => {
                    if let Some(hold_id) = this.hold_id.take() {
                        this.begin_settle(hold_id);
                        continue;
                    }
                    this.finished = true;
                    return Poll::Ready(None);
                }
            }
        }
    }
}

impl Drop for MeteredStream {
    fn drop(&mut self) {
        if let Some(fut) = self.settling.take() {
            // Settlement had already begun and returned Pending before the
            // caller dropped the stream; finish it on the runtime rather than
            // losing the settle call.
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    handle.spawn(fut);
                }
                Err(_) => {
                    tracing::error!("metered stream dropped mid-settlement without an active Tokio runtime");
                }
            }
            return;
        }
        if let Some(hold_id) = self.hold_id.take() {
            let outcome = if self.seen_stop {
                self.latest_usage
                    .take()
                    .map(Settlement::Usage)
                    .unwrap_or(Settlement::NoUsage)
            } else {
                Settlement::Cancelled
            };
            spawn_drop_settlement(self.metering.clone(), hold_id, outcome);
        }
    }
}

#[async_trait]
impl<P: ChatProvider> ChatProvider for MeteredProvider<P> {
    fn adapter_name(&self) -> &'static str {
        self.inner.adapter_name()
    }

    fn local_backend_for_test(&self) -> Option<LocalInferenceBackend> {
        self.inner.local_backend_for_test()
    }

    fn switch_impact(&self) -> SwitchImpact {
        self.inner.switch_impact()
    }

    fn capabilities(&self, model: &str) -> ProviderCapabilities {
        self.inner.capabilities(model)
    }

    async fn stream_chat_with_tools(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        params: ChatParams,
    ) -> Result<ChatStream, ProviderError> {
        let hold_id = self
            .reserve_for(RequestKind::StreamChat, &messages, &tools, &params)
            .await?;
        let mut guard = CancelOnDrop::new(self.metering.clone(), hold_id);
        match self.inner.stream_chat_with_tools(messages, tools, params).await {
            Ok(stream) => {
                let hold_id = guard.take().expect("hold_id still armed");
                Ok(Box::pin(MeteredStream::new(stream, self.metering.clone(), hold_id)))
            }
            Err(err) => {
                if let Some(hold_id) = guard.take() {
                    self.metering.settle(hold_id, Settlement::NoUsage).await;
                }
                Err(err)
            }
        }
    }

    async fn chat(&self, messages: Vec<ChatMessage>, params: ChatParams) -> Result<String, ProviderError> {
        // Route through `chat_with_tools` so even plain text requests settle
        // against reported `TokenUsage` instead of losing usage telemetry.
        let hold_id = self.reserve_for(RequestKind::Chat, &messages, &[], &params).await?;
        let mut guard = CancelOnDrop::new(self.metering.clone(), hold_id);
        match self.inner.chat_with_tools(messages, Vec::new(), params).await {
            Ok(resp) => {
                if let Some(hold_id) = guard.take() {
                    let outcome = resp.usage.clone().map(Settlement::Usage).unwrap_or(Settlement::NoUsage);
                    self.metering.settle(hold_id, outcome).await;
                }
                Ok(resp.text)
            }
            Err(err) => {
                if let Some(hold_id) = guard.take() {
                    self.metering.settle(hold_id, Settlement::NoUsage).await;
                }
                Err(err)
            }
        }
    }

    async fn chat_with_tools(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        params: ChatParams,
    ) -> Result<AgentResponse, ProviderError> {
        let hold_id = self
            .reserve_for(RequestKind::ChatWithTools, &messages, &tools, &params)
            .await?;
        let mut guard = CancelOnDrop::new(self.metering.clone(), hold_id);
        match self.inner.chat_with_tools(messages, tools, params).await {
            Ok(resp) => {
                if let Some(hold_id) = guard.take() {
                    let outcome = resp.usage.clone().map(Settlement::Usage).unwrap_or(Settlement::NoUsage);
                    self.metering.settle(hold_id, outcome).await;
                }
                Ok(resp)
            }
            Err(err) => {
                if let Some(hold_id) = guard.take() {
                    self.metering.settle(hold_id, Settlement::NoUsage).await;
                }
                Err(err)
            }
        }
    }

    async fn compact_remote(
        &self,
        messages: &[ChatMessage],
        params: &ChatParams,
    ) -> Result<RemoteCompactResult, ProviderError> {
        let hold_id = self
            .reserve_for(RequestKind::CompactRemote, messages, &[], params)
            .await?;
        let mut guard = CancelOnDrop::new(self.metering.clone(), hold_id);
        match self.inner.compact_remote(messages, params).await {
            Ok(result) => {
                if let Some(hold_id) = guard.take() {
                    self.metering
                        .settle(hold_id, Settlement::Usage(result.usage.clone()))
                        .await;
                }
                Ok(result)
            }
            Err(err) => {
                if let Some(hold_id) = guard.take() {
                    self.metering.settle(hold_id, Settlement::NoUsage).await;
                }
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use futures::StreamExt;
    use http::StatusCode;
    use tokio::sync::Notify;

    use super::*;
    use meridian_provider_sdk::client::{Request, RetryOn, RetryPolicy, TransportError, run_with_retry};

    #[derive(Default)]
    struct RecordingMetering {
        next_id: AtomicU64,
        refuse_with: Mutex<Option<String>>,
        reserves: Mutex<Vec<(HoldId, RequestKind, String)>>,
        settlements: Mutex<Vec<(HoldId, Settlement)>>,
        settled_notify: Notify,
    }

    impl RecordingMetering {
        fn refusing(reason: &str) -> Arc<Self> {
            let m = Arc::new(Self::default());
            *m.refuse_with.lock().unwrap() = Some(reason.to_string());
            m
        }

        fn settlements(&self) -> Vec<(HoldId, Settlement)> {
            self.settlements.lock().unwrap().clone()
        }

        fn reserve_count(&self) -> usize {
            self.reserves.lock().unwrap().len()
        }

        async fn wait_for_settlements(&self, count: usize) -> Vec<(HoldId, Settlement)> {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let current = self.settlements();
                    if current.len() >= count {
                        return current;
                    }
                    self.settled_notify.notified().await;
                }
            })
            .await
            .expect("timed out waiting for settlement")
        }
    }

    #[async_trait]
    impl Metering for RecordingMetering {
        async fn reserve(&self, request: ReserveRequest<'_>) -> Result<ReserveOutcome, String> {
            if let Some(reason) = self.refuse_with.lock().unwrap().clone() {
                return Ok(ReserveOutcome::Refused(Refusal { reason }));
            }
            let seq = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
            let hold_id = HoldId::new(format!("hold-{seq}"));
            self.reserves
                .lock()
                .unwrap()
                .push((hold_id.clone(), request.kind, request.model.to_string()));
            Ok(ReserveOutcome::Reserved(Reservation { hold_id }))
        }

        async fn settle(&self, hold_id: HoldId, outcome: Settlement) {
            self.settlements.lock().unwrap().push((hold_id, outcome));
            self.settled_notify.notify_waiters();
        }
    }

    fn sample_usage(prompt: i32, completion: i32) -> TokenUsage {
        TokenUsage {
            prompt_tokens: Some(prompt),
            completion_tokens: Some(completion),
            total_tokens: Some(prompt + completion),
            ..Default::default()
        }
    }

    fn sample_params() -> ChatParams {
        ChatParams {
            model: "claude-sonnet-5".into(),
            ..Default::default()
        }
    }

    type StreamFactory = Box<dyn FnMut() -> Result<ChatStream, ProviderError> + Send>;

    struct ScriptedStreamProvider {
        calls: AtomicU64,
        stream_factory: Mutex<Option<StreamFactory>>,
    }

    impl ScriptedStreamProvider {
        fn new(factory: impl FnMut() -> Result<ChatStream, ProviderError> + Send + 'static) -> Self {
            Self {
                calls: AtomicU64::new(0),
                stream_factory: Mutex::new(Some(Box::new(factory))),
            }
        }
    }

    #[async_trait]
    impl ChatProvider for ScriptedStreamProvider {
        fn adapter_name(&self) -> &'static str {
            "scripted"
        }

        async fn stream_chat_with_tools(
            &self,
            _messages: Vec<ChatMessage>,
            _tools: Vec<ToolDefinition>,
            _params: ChatParams,
        ) -> Result<ChatStream, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut guard = self.stream_factory.lock().unwrap();
            guard.as_mut().unwrap()()
        }

        async fn chat(&self, _messages: Vec<ChatMessage>, _params: ChatParams) -> Result<String, ProviderError> {
            Err(ProviderError::NotImplemented("unused".into()))
        }

        async fn chat_with_tools(
            &self,
            _messages: Vec<ChatMessage>,
            _tools: Vec<ToolDefinition>,
            _params: ChatParams,
        ) -> Result<AgentResponse, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(AgentResponse {
                text: "ok".into(),
                reasoning_content: None,
                tool_calls: Vec::new(),
                usage: Some(sample_usage(120, 45)),
                provider_state: None,
            })
        }

        async fn compact_remote(
            &self,
            _messages: &[ChatMessage],
            _params: &ChatParams,
        ) -> Result<RemoteCompactResult, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(RemoteCompactResult {
                compaction_message: ChatMessage::assistant("summary"),
                usage: sample_usage(500, 80),
            })
        }
    }

    /// Dropping an active stream before `Stop` / EOF (e.g. user Stop or network
    /// disconnect) settles as `Cancelled` (`keep_all`).
    #[tokio::test]
    async fn dropped_stream_settles_as_cancelled() {
        let metering = Arc::new(RecordingMetering::default());
        let inner = ScriptedStreamProvider::new(|| {
            let stream = futures::stream::iter(vec![
                Ok(StreamEvent::Text {
                    content: "partial ".into(),
                }),
                Ok(StreamEvent::Text {
                    content: "answer".into(),
                }),
            ])
            .chain(futures::stream::pending());
            Ok(Box::pin(stream))
        });
        let provider = MeteredProvider::new(inner, metering.clone());

        let mut stream = provider
            .stream_chat_with_tools(vec![ChatMessage::user("hi")], vec![], sample_params())
            .await
            .unwrap();
        let first = stream.next().await.unwrap().unwrap();
        assert!(matches!(first, StreamEvent::Text { .. }));
        assert!(metering.settlements().is_empty());

        drop(stream);

        let settlements = metering.wait_for_settlements(1).await;
        assert_eq!(settlements, vec![(HoldId::new("hold-1"), Settlement::Cancelled)]);
    }

    /// Upstream failure without usage settles as `NoUsage` (`refund_all`) — both
    /// before a stream opens and mid-stream (even if caller drops immediately
    /// after reading the `Err`).
    #[tokio::test]
    async fn error_without_usage_settles_as_no_usage() {
        // Case 1: pre-stream HTTP 500
        let metering = Arc::new(RecordingMetering::default());
        let inner = ScriptedStreamProvider::new(|| {
            Err(ProviderError::Api {
                status: 500,
                body: "internal error".into(),
            })
        });
        let provider = MeteredProvider::new(inner, metering.clone());
        let err = provider
            .stream_chat_with_tools(vec![ChatMessage::user("hi")], vec![], sample_params())
            .await
            .err()
            .expect("expected error");
        assert!(matches!(err, ProviderError::Api { status: 500, .. }));
        assert_eq!(
            metering.settlements(),
            vec![(HoldId::new("hold-1"), Settlement::NoUsage)]
        );

        // Case 2: mid-stream Err without prior usage; caller drops immediately.
        let metering = Arc::new(RecordingMetering::default());
        let inner = ScriptedStreamProvider::new(|| {
            let stream = futures::stream::iter(vec![
                Ok(StreamEvent::Text {
                    content: "chunk".into(),
                }),
                Err(ProviderError::Api {
                    status: 500,
                    body: "mid-stream fail".into(),
                }),
            ]);
            Ok(Box::pin(stream))
        });
        let provider = MeteredProvider::new(inner, metering.clone());
        let mut stream = provider
            .stream_chat_with_tools(vec![ChatMessage::user("hi")], vec![], sample_params())
            .await
            .unwrap();
        assert!(stream.next().await.unwrap().is_ok());
        assert!(stream.next().await.unwrap().is_err());
        // Already settled BEFORE yielding the Err, even before Drop.
        assert_eq!(
            metering.settlements(),
            vec![(HoldId::new("hold-1"), Settlement::NoUsage)]
        );
        drop(stream);
        tokio::task::yield_now().await;
        assert_eq!(metering.settlements().len(), 1);
    }

    /// Completed stream (including post-Stop `UsageUpdate`), `chat`,
    /// `chat_with_tools`, and `compact_remote` all settle with reported `Usage`.
    #[tokio::test]
    async fn completed_requests_settle_with_reported_usage() {
        let metering = Arc::new(RecordingMetering::default());
        let expected_stream_usage = sample_usage(1_000, 250);
        let usage_clone = expected_stream_usage.clone();
        let inner = ScriptedStreamProvider::new(move || {
            let u = usage_clone.clone();
            let stream = futures::stream::iter(vec![
                Ok(StreamEvent::Text {
                    content: "hello".into(),
                }),
                Ok(StreamEvent::Stop {
                    reason: "end_turn".into(),
                    usage: None,
                }),
                Ok(StreamEvent::UsageUpdate { usage: u }),
            ]);
            Ok(Box::pin(stream))
        });
        let provider = MeteredProvider::new(inner, metering.clone());

        let mut stream = provider
            .stream_chat_with_tools(vec![ChatMessage::user("hi")], vec![], sample_params())
            .await
            .unwrap();
        while let Some(item) = stream.next().await {
            item.unwrap();
        }
        assert_eq!(
            metering.settlements(),
            vec![(HoldId::new("hold-1"), Settlement::Usage(expected_stream_usage))]
        );

        let text = provider
            .chat(vec![ChatMessage::user("title")], sample_params())
            .await
            .unwrap();
        assert_eq!(text, "ok");

        let resp = provider
            .chat_with_tools(vec![ChatMessage::user("review")], vec![], sample_params())
            .await
            .unwrap();
        assert_eq!(resp.text, "ok");

        let compact = provider
            .compact_remote(&[ChatMessage::user("long")], &sample_params())
            .await
            .unwrap();
        assert_eq!(compact.usage, sample_usage(500, 80));

        assert_eq!(
            metering.settlements(),
            vec![
                (HoldId::new("hold-1"), Settlement::Usage(sample_usage(1_000, 250))),
                (HoldId::new("hold-2"), Settlement::Usage(sample_usage(120, 45))),
                (HoldId::new("hold-3"), Settlement::Usage(sample_usage(120, 45))),
                (HoldId::new("hold-4"), Settlement::Usage(sample_usage(500, 80))),
            ]
        );
    }

    /// A refusal stops the request before the inner provider is touched.
    #[tokio::test]
    async fn refusal_short_circuits_without_calling_inner_provider() {
        let metering = RecordingMetering::refusing("insufficient credits");
        let inner = ScriptedStreamProvider::new(|| unreachable!("inner must not be called"));
        let provider = MeteredProvider::new(inner, metering.clone());

        let err = provider
            .stream_chat_with_tools(vec![ChatMessage::user("hi")], vec![], sample_params())
            .await
            .err()
            .expect("expected refusal");
        assert!(matches!(err, ProviderError::MeteringRefused(ref msg) if msg == "insufficient credits"));
        assert_eq!(provider.inner().calls.load(Ordering::SeqCst), 0);
        assert!(metering.settlements().is_empty());
    }

    /// `MeteredProvider` sits outside `client::retry::run_with_retry`: transient
    /// 503 retries inside the provider consume one reservation and settle once
    /// with `Usage` — never falsely firing `Cancelled` for retried attempts.
    #[tokio::test]
    async fn internal_transport_retries_happen_under_one_hold_without_false_cancellation() {
        struct RetryingInnerProvider {
            attempts: Arc<AtomicU64>,
        }

        #[async_trait]
        impl ChatProvider for RetryingInnerProvider {
            fn adapter_name(&self) -> &'static str {
                "retrying"
            }

            async fn stream_chat_with_tools(
                &self,
                _messages: Vec<ChatMessage>,
                _tools: Vec<ToolDefinition>,
                _params: ChatParams,
            ) -> Result<ChatStream, ProviderError> {
                let attempts = self.attempts.clone();
                let policy = RetryPolicy {
                    max_attempts: 2,
                    base_delay: Duration::from_millis(1),
                    retry_on: RetryOn {
                        retry_429: false,
                        retry_5xx: true,
                        retry_transport: true,
                    },
                };
                let events = run_with_retry(
                    policy,
                    || Request::new(http::Method::POST, "https://e.invalid/v1/messages".into()),
                    |_req, _| {
                        let attempts = attempts.clone();
                        async move {
                            let seen = attempts.fetch_add(1, Ordering::SeqCst);
                            if seen < 2 {
                                Err(TransportError::Http {
                                    status: StatusCode::SERVICE_UNAVAILABLE,
                                    url: None,
                                    headers: None,
                                    body: None,
                                })
                            } else {
                                Ok(vec![
                                    Ok(StreamEvent::Text {
                                        content: "recovered".into(),
                                    }),
                                    Ok(StreamEvent::Stop {
                                        reason: "end_turn".into(),
                                        usage: Some(sample_usage(300, 50)),
                                    }),
                                ])
                            }
                        }
                    },
                )
                .await?;
                Ok(Box::pin(futures::stream::iter(events)))
            }

            async fn chat(&self, _messages: Vec<ChatMessage>, _params: ChatParams) -> Result<String, ProviderError> {
                Err(ProviderError::NotImplemented("unused".into()))
            }

            async fn chat_with_tools(
                &self,
                _messages: Vec<ChatMessage>,
                _tools: Vec<ToolDefinition>,
                _params: ChatParams,
            ) -> Result<AgentResponse, ProviderError> {
                Err(ProviderError::NotImplemented("unused".into()))
            }
        }

        let attempts = Arc::new(AtomicU64::new(0));
        let metering = Arc::new(RecordingMetering::default());
        let provider = MeteredProvider::new(
            RetryingInnerProvider {
                attempts: attempts.clone(),
            },
            metering.clone(),
        );

        let mut stream = provider
            .stream_chat_with_tools(vec![ChatMessage::user("hi")], vec![], sample_params())
            .await
            .unwrap();
        while let Some(item) = stream.next().await {
            item.unwrap();
        }

        assert_eq!(attempts.load(Ordering::SeqCst), 3, "two retries plus final success");
        assert_eq!(metering.reserve_count(), 1, "only one reservation across retries");
        assert_eq!(
            metering.settlements(),
            vec![(HoldId::new("hold-1"), Settlement::Usage(sample_usage(300, 50)))],
            "retried attempts must never trigger Cancelled"
        );
    }

    /// Dropping an in-flight non-streaming future before upstream replies
    /// settles as `Cancelled` (`keep_all`).
    #[tokio::test]
    async fn cancelled_non_streaming_future_settles_as_cancelled() {
        struct HangingProvider;

        #[async_trait]
        impl ChatProvider for HangingProvider {
            fn adapter_name(&self) -> &'static str {
                "hanging"
            }

            async fn stream_chat_with_tools(
                &self,
                _messages: Vec<ChatMessage>,
                _tools: Vec<ToolDefinition>,
                _params: ChatParams,
            ) -> Result<ChatStream, ProviderError> {
                std::future::pending().await
            }

            async fn chat(&self, _messages: Vec<ChatMessage>, _params: ChatParams) -> Result<String, ProviderError> {
                std::future::pending().await
            }

            async fn chat_with_tools(
                &self,
                _messages: Vec<ChatMessage>,
                _tools: Vec<ToolDefinition>,
                _params: ChatParams,
            ) -> Result<AgentResponse, ProviderError> {
                std::future::pending().await
            }
        }

        let metering = Arc::new(RecordingMetering::default());
        let provider = MeteredProvider::new(HangingProvider, metering.clone());

        let fut = provider.chat_with_tools(vec![ChatMessage::user("hi")], vec![], sample_params());
        let res = tokio::time::timeout(Duration::from_millis(20), fut).await;
        assert!(res.is_err(), "future should time out and be dropped");

        let settlements = metering.wait_for_settlements(1).await;
        assert_eq!(settlements, vec![(HoldId::new("hold-1"), Settlement::Cancelled)]);
    }

    #[test]
    fn noop_metering_leaves_provider_unwrapped() {
        let inner: Box<dyn ChatProvider> = Box::new(ScriptedStreamProvider::new(|| unreachable!()));
        let wrapped = wrap_provider(inner, Arc::new(NoopMetering));
        assert_eq!(wrapped.adapter_name(), "scripted");
    }
}

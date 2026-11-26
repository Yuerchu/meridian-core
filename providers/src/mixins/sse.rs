//! Shared SSE framing mixin for streaming chat protocols.

use bytes::Bytes;
use eventsource_stream::{Event, EventStreamError, Eventsource};
use futures::stream::{Stream, StreamExt};

use meridian_provider_sdk::chat::ProviderError;
use meridian_provider_sdk::client::TransportError;

/// Shared SSE byte-stream framing helper.
pub struct SseStreamMixin;

impl SseStreamMixin {
    /// Wrap a raw HTTP byte stream from `HttpTransport::stream` into a typed
    /// Server-Sent Events stream whose transport errors map to
    /// `ProviderError::Transport`.
    pub fn from_bytes<S>(bytes: S) -> impl Stream<Item = Result<Event, EventStreamError<ProviderError>>> + Send
    where
        S: Stream<Item = Result<Bytes, TransportError>> + Send + 'static,
    {
        bytes.map(|r| r.map_err(ProviderError::Transport)).eventsource()
    }
}

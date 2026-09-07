use http::HeaderMap;
use http::StatusCode;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("http {status}: {body:?}")]
    Http {
        status: StatusCode,
        url: Option<String>,
        headers: Option<HeaderMap>,
        body: Option<String>,
    },
    /// Only `run_with_retry` constructs this; kept with the rest of the ported
    /// retry surface.
    #[allow(dead_code)]
    #[error("retry limit reached")]
    RetryLimit,
    #[error("timeout")]
    Timeout,
    #[error("network error: {0}")]
    Network(String),
    #[error("request build error: {0}")]
    Build(String),
}

/// Kept for `client::sse`, the ported SSE half nothing consumes yet —
/// providers read `eventsource_stream` directly.
#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum StreamError {
    #[error("stream failed: {0}")]
    Stream(String),
    #[error("timeout")]
    Timeout,
}

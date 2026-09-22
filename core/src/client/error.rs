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
    #[error("retry limit reached")]
    RetryLimit,
    #[error("timeout")]
    Timeout,
    #[error("network error: {0}")]
    Network(String),
    #[error("request build error: {0}")]
    Build(String),
}

impl TransportError {
    /// Which of these it is, as a fixed word. For logs, where the variant's own
    /// `Display` carries a body or a reqwest chain that must not be written to
    /// a file the user can export.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Http { .. } => "http",
            Self::RetryLimit => "retry_limit",
            Self::Timeout => "timeout",
            Self::Network(_) => "network",
            Self::Build(_) => "build",
        }
    }

    /// The status, where there was a reply to read one off.
    pub(crate) fn status(&self) -> Option<u16> {
        match self {
            Self::Http { status, .. } => Some(status.as_u16()),
            _ => None,
        }
    }
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

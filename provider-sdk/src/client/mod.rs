// Ported from codex-rs/codex-client (Apache-2.0, OpenAI)
// NOTICE: This file contains code derived from the OpenAI Codex project.
// Changes: removed ChatGPT cookie handling, simplified telemetry to plain tracing.

pub mod bounded;
pub mod error;
pub mod request;
pub mod retry;
pub mod sse;
pub mod transport;

pub use error::{StreamError, TransportError};
pub use request::{PreparedRequestBody, Request, RequestBody, Response};
pub use retry::{RetryOn, RetryPolicy, backoff, run_with_retry};
pub use transport::{ByteStream, HttpTransport, ReqwestTransport, StreamResponse};

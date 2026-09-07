// Ported from codex-rs/codex-client (Apache-2.0, OpenAI)
// NOTICE: This file contains code derived from the OpenAI Codex project.
// Changes: removed ChatGPT cookie handling, simplified telemetry to plain tracing.

mod error;
mod request;
mod retry;
mod sse;
mod transport;

pub use error::TransportError;
pub use request::{Request, RequestBody, Response};
pub use retry::backoff;
pub use transport::{HttpTransport, ReqwestTransport, StreamResponse};

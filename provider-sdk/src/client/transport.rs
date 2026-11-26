use crate::client::error::TransportError;
use crate::client::request::{Request, Response};
use crate::client::retry::run_with_retry;
use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use http::HeaderMap;
use http::Method;
use http::StatusCode;
use std::sync::OnceLock;

pub type ByteStream = BoxStream<'static, Result<Bytes, TransportError>>;

pub struct StreamResponse {
    /// As on `Response`: errors carry their own status, so nothing reads these
    /// on the success path yet. Ported surface.
    #[allow(dead_code)]
    pub status: StatusCode,
    #[allow(dead_code)]
    pub headers: HeaderMap,
    pub bytes: ByteStream,
}

#[async_trait]
pub trait HttpTransport: Send + Sync {
    async fn execute(&self, req: Request) -> Result<Response, TransportError>;
    async fn stream(&self, req: Request) -> Result<StreamResponse, TransportError>;
}

#[derive(Clone, Debug)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }

    /// The process-wide client.
    ///
    /// A `reqwest::Client` *is* the connection pool. Building one per request
    /// throws away every kept-alive connection and pays DNS, TCP and the TLS
    /// handshake again — on every turn, and again on every iteration of a tool
    /// loop. Cloning is cheap; the client is an `Arc` inside.
    ///
    /// Built explicitly rather than with `Client::new()`, which panics if the
    /// TLS backend fails to initialise. Should that happen, every request will
    /// fail on its own terms instead of taking the process down.
    pub fn shared() -> Self {
        static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
        let client = CLIENT
            .get_or_init(|| reqwest::Client::builder().build().unwrap_or_default())
            .clone();
        Self::new(client)
    }

    fn build(&self, req: &Request) -> Result<reqwest::RequestBuilder, TransportError> {
        let prepared = req.prepare_body_for_send().map_err(TransportError::Build)?;

        let mut builder = self.client.request(
            Method::from_bytes(req.method.as_str().as_bytes()).unwrap_or(Method::GET),
            &req.url,
        );

        if let Some(timeout) = req.timeout {
            builder = builder.timeout(timeout);
        }

        builder = builder.headers(prepared.headers);
        if let Some(body) = prepared.body {
            builder = builder.body(body);
        }
        Ok(builder)
    }

    // Paths and upstream error codes can contain credentials too.
    fn log_http_failure(status: StatusCode, streaming: bool) {
        tracing::error!(
            status = status.as_u16(),
            kind = "http",
            streaming,
            "upstream request failed"
        );
    }

    pub fn map_error(err: reqwest::Error) -> TransportError {
        // `Timeout` renders as the single word "timeout", which cannot tell a
        // blocked proxy from a slow model. The kind is recorded here because it
        // is the last place that still knows.
        let kind = if err.is_timeout() {
            "timeout"
        } else if err.is_connect() {
            "connect"
        } else if err.is_body() || err.is_decode() {
            "body"
        } else if err.is_request() {
            "request"
        } else {
            "other"
        };
        // Not err.to_string(): reqwest's error chain can carry the full URL,
        // query string included.
        tracing::warn!(kind, "provider request could not be completed");

        if err.is_timeout() {
            TransportError::Timeout
        } else {
            TransportError::Network(kind.to_string())
        }
    }
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn execute(&self, req: Request) -> Result<Response, TransportError> {
        match req.retry.clone() {
            Some(policy) => run_with_retry(policy, || req.clone(), |req, _| self.execute_once(req)).await,
            None => self.execute_once(req).await,
        }
    }

    /// Retried like `execute`, and safe for the same reason `codex::send` gives
    /// about its own 401 retry: the status is resolved *before* a body is handed
    /// back, so a retry here cannot follow events that were already delivered
    /// downstream. A failure mid-stream is a different thing and belongs to the
    /// turn loop, which can replay the prompt.
    async fn stream(&self, req: Request) -> Result<StreamResponse, TransportError> {
        match req.retry.clone() {
            Some(policy) => run_with_retry(policy, || req.clone(), |req, _| self.stream_once(req)).await,
            None => self.stream_once(req).await,
        }
    }
}

impl ReqwestTransport {
    async fn execute_once(&self, req: Request) -> Result<Response, TransportError> {
        let url = req.url.clone();
        let builder = self.build(&req)?;
        let resp = builder.send().await.map_err(Self::map_error)?;
        let status = resp.status();
        let headers = resp.headers().clone();
        if !status.is_success() {
            let body = super::bounded::error_body(resp, super::bounded::ERROR_BYTES).await;
            Self::log_http_failure(status, false);
            return Err(TransportError::Http {
                status,
                url: Some(url),
                headers: Some(headers),
                body,
            });
        }
        let bytes = super::bounded::body(resp, super::bounded::BODY_BYTES).await?;
        Ok(Response {
            status,
            headers,
            body: bytes,
        })
    }

    async fn stream_once(&self, req: Request) -> Result<StreamResponse, TransportError> {
        let url = req.url.clone();
        let builder = self.build(&req)?;
        let resp = builder.send().await.map_err(Self::map_error)?;
        let status = resp.status();
        let headers = resp.headers().clone();
        if !status.is_success() {
            let body = super::bounded::error_body(resp, super::bounded::ERROR_BYTES).await;
            Self::log_http_failure(status, true);
            return Err(TransportError::Http {
                status,
                url: Some(url),
                headers: Some(headers),
                body,
            });
        }
        let stream = resp.bytes_stream().map(|result| result.map_err(Self::map_error));
        Ok(StreamResponse {
            status,
            headers,
            bytes: super::bounded::sse(Box::pin(stream), super::bounded::SSE_LIMITS),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn oversized_error_diagnostics_preserve_status_and_retry_headers() {
        for streaming in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 2048];
                assert!(socket.read(&mut request).await.unwrap() > 0);
                // Only a header: no large allocation or payload needed.
                let wire = format!(
                    "HTTP/1.1 503 Unavailable\r\nContent-Length: {}\r\nRetry-After: 7\r\nConnection: close\r\n\r\n",
                    super::super::bounded::ERROR_BYTES + 1
                );
                socket.write_all(wire.as_bytes()).await.unwrap();
            });
            let transport = ReqwestTransport::new(reqwest::Client::builder().no_proxy().build().unwrap());
            let mut request = Request::new(Method::GET, format!("http://{address}"));
            request.retry = None;
            request.timeout = Some(std::time::Duration::from_secs(2));
            let error = if streaming {
                transport.stream(request).await.err().unwrap()
            } else {
                transport.execute(request).await.unwrap_err()
            };
            match error {
                TransportError::Http {
                    status, headers, body, ..
                } => {
                    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
                    assert_eq!(headers.unwrap()["retry-after"], "7");
                    assert!(body.is_none());
                }
                other => panic!("lost HTTP classification: {other:?}"),
            }
            server.await.unwrap();
        }
    }
}

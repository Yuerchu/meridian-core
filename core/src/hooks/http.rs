//! The socket half: one connection, two routes, and the checks that decide
//! whether a request is allowed to cost anything.
//!
//! Every rejection here is a non-2xx, and every non-2xx is fail-open on the
//! Claude Code side. So the status code is not a signal to the caller so much
//! as a note to whoever is reading `claude --debug` — which is why each one
//! carries a sentence rather than a code.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{AUTHORIZATION, CONTENT_TYPE};
use hyper::http::request::Parts;
use hyper::{Method, Request, Response, StatusCode};

use super::protocol::{ErrorBody, Kind, ReviewRequest, StopReviewRequest};
use super::{SharedState, review};
use crate::listen_guard::constant_time_eq;

/// A plan is markdown a model wrote; a couple of megabytes is already far past
/// anything a reviewer could usefully read.
const MAX_BODY: usize = 2 * 1024 * 1024;
/// Bound arrival/idle sockets; authenticated reviews close rather than reuse a released slot.
pub(super) const MAX_CONNECTIONS: usize = 32;
/// Detached reviews are bounded separately from arrival and keepalive sockets.
pub(super) const MAX_REVIEWS: usize = 32;
/// Headers and body each get their own budget, never applied to the review.
const HEADER_TIMEOUT_SECS: u64 = 15;
const BODY_TIMEOUT_SECS: u64 = 15;

/// Called synchronously at accept, before allocating a connection task.
/// On saturation the owned socket is dropped here, not queued for a permit.
pub(super) fn admit<S>(
    stream: S,
    connections: &Arc<tokio::sync::Semaphore>,
) -> Option<(S, tokio::sync::OwnedSemaphorePermit)> {
    let permit = connections.clone().try_acquire_owned().ok()?;
    Some((stream, permit))
}

/// The connection settings, in one function so the deterministic HTTP test below
/// exercises the same assembly `serve` runs.
///
/// It did not used to be, and that is exactly how this shipped broken: hyper
/// 1.x carries no default timer, so `header_read_timeout` without one is a
/// `panic!` on every connection rather than a timeout that never fires
/// (`hyper/src/common/time.rs`). The endpoint bound its port, wrote its
/// handshake file, and reset every connection that reached it. Nothing caught
/// it because every test here called `classify` directly and none opened a
/// socket.
fn connection_builder() -> hyper::server::conn::http1::Builder {
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(Duration::from_secs(HEADER_TIMEOUT_SECS));
    builder
}

/// Shared only within one connection. Releasing is irreversible: the response
/// closes the socket and any subsequent/pipelined request is refused.
#[derive(Clone)]
struct Admission(Arc<std::sync::Mutex<Option<tokio::sync::OwnedSemaphorePermit>>>);

impl Admission {
    fn release_for_review(&self, config: &super::HookConfig) {
        if config.token.as_deref().is_some_and(|token| !token.is_empty()) {
            self.0.lock().unwrap().take();
        }
    }

    fn released(&self) -> bool {
        self.0.lock().unwrap().is_none()
    }
}

pub(crate) fn serve(
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    state: Arc<SharedState>,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    serve_with(stream, peer, permit, move |req, admission| {
        route(req, state.clone(), admission)
    });
}

fn serve_with<S, F, Fut>(
    stream: S,
    peer: SocketAddr,
    permit: tokio::sync::OwnedSemaphorePermit,
    handler: F,
) -> tokio::task::JoinHandle<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    F: Fn(Request<Incoming>, Admission) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Response<Full<Bytes>>> + Send,
{
    tokio::spawn(async move {
        // Ordinary keepalive retains admission. Only a validated authenticated
        // review releases it, and that connection can never be reused.
        let admission = Admission(Arc::new(std::sync::Mutex::new(Some(permit))));
        let io = hyper_util::rt::TokioIo::new(stream);
        let service = hyper::service::service_fn(move |req| {
            let admission = admission.clone();
            let response = if admission.released() {
                None
            } else {
                Some(handler(req, admission.clone()))
            };
            async move {
                let mut response = match response {
                    Some(response) => response.await,
                    None => fail(StatusCode::SERVICE_UNAVAILABLE, "review connection cannot be reused"),
                };
                if admission.released() {
                    response.headers_mut().insert(
                        hyper::header::CONNECTION,
                        hyper::header::HeaderValue::from_static("close"),
                    );
                }
                Ok::<_, Infallible>(response)
            }
        });
        if let Err(e) = connection_builder().serve_connection(io, service).await {
            tracing::debug!(error = %e, %peer, "hook connection ended");
        }
    })
}

fn json(status: StatusCode, body: &impl serde::Serialize) -> Response<Full<Bytes>> {
    let encoded = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(encoded)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from_static(b"{}"))))
}

fn fail(status: StatusCode, message: impl Into<String>) -> Response<Full<Bytes>> {
    let message = message.into();
    tracing::debug!(%status, %message, "hook request refused");
    json(status, &ErrorBody { error: message })
}

/// Takes no state, so the socket test can answer with the real thing rather
/// than with a stand-in that proves nothing about this file.
fn healthz() -> Response<Full<Bytes>> {
    json(
        StatusCode::OK,
        &serde_json::json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }),
    )
}

async fn route(req: Request<Incoming>, state: Arc<SharedState>, admission: Admission) -> Response<Full<Bytes>> {
    let (parts, body) = req.into_parts();

    // Which gate is being asked, decided before the body is read so an unknown
    // path costs nothing.
    let gate = match (&parts.method, parts.uri.path()) {
        (&Method::GET, "/healthz") => return healthz(),
        (&Method::POST, "/hooks/exit-plan") => Kind::Plan,
        (&Method::POST, "/hooks/stop-review") => Kind::Implementation,
        (&Method::GET, _) | (&Method::POST, _) => {
            return fail(StatusCode::NOT_FOUND, "no such hook endpoint");
        }
        _ => return fail(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
    };

    let bytes = match read_body(&parts, body, &state.config).await {
        Ok(bytes) => bytes,
        Err((status, message)) => return fail(status, message),
    };

    let job = match gate {
        Kind::Plan => classify(&parts, &bytes, &state.config).map(ReviewRequest::into_job),
        Kind::Implementation => classify_stop(&parts, &bytes, &state.config).map(StopReviewRequest::into_job),
    };
    let job = match job {
        Ok(j) => j,
        Err((status, message)) => return fail(status, message),
    };

    // Transfer without queuing before releasing arrival admission. The job owns
    // this permit even if the HTTP client disconnects while review is running.
    let review_permit = match admit_review(&state.review_slots) {
        Ok(permit) => permit,
        Err(response) => return response,
    };

    // Headers, authentication, bounded body and job validation all succeeded.
    // Do not let a minutes-long trusted review exhaust arrival admission. A
    // tokenless configuration cannot cross this boundary just by being local.
    admission.release_for_review(&state.config);

    // Spawned rather than awaited inline, so the review belongs to the app and
    // not to this socket. If the client gives up — Claude Code's own timeout, a
    // user interrupting the tool call — this handler's future is dropped, but
    // the task carries on, finishes the transcript the user reads and records
    // how the turn ended. The verdict simply has nowhere to go, and the client
    // was already failing open by then.
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _review_permit = review_permit;
        let _ = tx.send(review::run(state, job).await);
    });

    match rx.await {
        Ok(Ok(response)) => json(StatusCode::OK, &response),
        Ok(Err(review::Refused { status, message })) => fail(status, message),
        // The task died without answering — a panic, or the runtime going down.
        Err(_) => fail(StatusCode::INTERNAL_SERVER_ERROR, "the review ended without a verdict"),
    }
}

fn admit_review(
    slots: &Arc<tokio::sync::Semaphore>,
) -> Result<tokio::sync::OwnedSemaphorePermit, Response<Full<Bytes>>> {
    slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| fail(StatusCode::SERVICE_UNAVAILABLE, "review capacity is full; retry later"))
}

/// Reject headers without polling the body (including `Expect: 100-continue`).
/// Only collection has a deadline; parsing and the app-owned review do not.
async fn read_body<B>(parts: &Parts, body: B, config: &super::HookConfig) -> Result<Bytes, (StatusCode, String)>
where
    B: hyper::body::Body<Data = Bytes>,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    guard(parts, config)?;
    match tokio::time::timeout(
        Duration::from_secs(BODY_TIMEOUT_SECS),
        Limited::new(body, MAX_BODY).collect(),
    )
    .await
    {
        Ok(Ok(collected)) => Ok(collected.to_bytes()),
        Ok(Err(_)) => Err((StatusCode::PAYLOAD_TOO_LARGE, "request body too large".into())),
        Err(_) => Err((StatusCode::REQUEST_TIMEOUT, "request body timed out".into())),
    }
}

/// Everything that can be decided from the request alone.
///
/// Split out from [`route`] so it can be tested without a socket — the
/// interesting cases here are all about what gets turned away.
pub(crate) fn classify(
    parts: &Parts,
    body: &[u8],
    config: &super::HookConfig,
) -> Result<ReviewRequest, (StatusCode, String)> {
    guard(parts, config)?;

    let request: ReviewRequest =
        serde_json::from_slice(body).map_err(|e| (StatusCode::BAD_REQUEST, format!("malformed request body: {e}")))?;

    if request.plan.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "the plan is empty".into()));
    }
    who_and_where(&request.session_id, &request.cwd)?;
    Ok(request)
}

/// The same, for the route that reviews what was written rather than what was
/// proposed.
pub(crate) fn classify_stop(
    parts: &Parts,
    body: &[u8],
    config: &super::HookConfig,
) -> Result<StopReviewRequest, (StatusCode, String)> {
    guard(parts, config)?;

    let request: StopReviewRequest =
        serde_json::from_slice(body).map_err(|e| (StatusCode::BAD_REQUEST, format!("malformed request body: {e}")))?;

    // An empty diff is not an error, but it is not reviewable either, and the
    // plugin is supposed to have skipped it. Say so rather than spending a model
    // call proving there is nothing to say.
    if request.diff.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "there are no changes to review".into()));
    }
    who_and_where(&request.session_id, &request.cwd)?;
    Ok(request)
}

/// Whether this request is allowed to cost anything, before looking at what it
/// asks for.
fn guard(parts: &Parts, config: &super::HookConfig) -> Result<(), (StatusCode, String)> {
    // A page in the user's browser can POST to loopback. It cannot read the
    // reply, but it does not need to: getting here at all spends a model call
    // and puts attacker-chosen text in front of an agent that reads this
    // machine's files. Requiring application/json forces a preflight, which we
    // never answer, and refusing a cross-site fetch closes what is left.
    if parts.headers.contains_key("origin") {
        return Err((StatusCode::FORBIDDEN, "cross-origin request refused".into()));
    }
    if let Some(site) = parts.headers.get("sec-fetch-site").and_then(|v| v.to_str().ok())
        && site != "none"
        && site != "same-origin"
    {
        return Err((StatusCode::FORBIDDEN, "cross-site request refused".into()));
    }
    let content_type = parts
        .headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !content_type.starts_with("application/json") {
        return Err((StatusCode::FORBIDDEN, "expected application/json".into()));
    }

    if let Some(expected) = config.token.as_deref().filter(|t| !t.is_empty()) {
        let presented = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        if !constant_time_eq(expected.as_bytes(), presented.as_bytes()) {
            return Err((StatusCode::UNAUTHORIZED, "bad or missing token".into()));
        }
    }
    Ok(())
}

/// Both routes need to know who is asking and which repository they mean.
fn who_and_where(session_id: &str, cwd: &str) -> Result<(), (StatusCode, String)> {
    if session_id.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "sessionId is required".into()));
    }
    if cwd.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "cwd is required".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::protocol::ReviewResponse;
    use super::*;

    /// Serialised here so `ReviewResponse`'s untagged shape is pinned by a test
    /// rather than by whatever serde happens to emit.
    fn encode(response: &ReviewResponse) -> String {
        serde_json::to_string(response).unwrap()
    }

    fn config() -> super::super::HookConfig {
        super::super::HookConfig {
            token: Some("t".repeat(32)),
            ..Default::default()
        }
    }

    fn parts(headers: &[(&str, &str)]) -> Parts {
        let mut builder = Request::builder().method(Method::POST).uri("/hooks/exit-plan");
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        builder.body(()).unwrap().into_parts().0
    }

    fn authed() -> Vec<(&'static str, &'static str)> {
        vec![
            ("content-type", "application/json"),
            ("authorization", "Bearer tttttttttttttttttttttttttttttttt"),
        ]
    }

    fn review_body() -> serde_json::Value {
        serde_json::json!({
            "sessionId": "s",
            "cwd": "C:/repo",
            "conversationId": null,
            "plan": "do a thing",
            "round": 1,
            "max_rounds": null,
            "stagnant": false,
            "history": [],
        })
    }

    fn stop_body() -> serde_json::Value {
        serde_json::json!({
            "sessionId": "s",
            "cwd": "C:/repo",
            "conversationId": null,
            "diff": "+x",
            "note": null,
            "round": 1,
            "max_rounds": null,
            "stagnant": false,
            "history": [],
        })
    }

    fn encode_body(value: &serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(value).unwrap()
    }

    fn body() -> Vec<u8> {
        encode_body(&review_body())
    }

    #[tokio::test]
    async fn header_rejections_never_poll_the_body() {
        for headers in [
            vec![("content-type", "text/plain")],
            vec![("content-type", "application/json")],
            vec![("content-type", "application/json"), ("origin", "https://evil.example")],
            vec![("content-type", "application/json"), ("sec-fetch-site", "cross-site")],
        ] {
            let expected = guard(&parts(&headers), &config()).unwrap_err().0;
            let unread = http_body_util::StreamBody::new(futures::stream::poll_fn(
                |_| -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, Infallible>>> {
                    panic!("a rejected request must not poll its body")
                },
            ));
            assert_eq!(
                read_body(&parts(&headers), unread, &config()).await.unwrap_err().0,
                expected
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn body_deadline_is_independent_of_time_spent_on_headers() {
        // Simulate nearly the full header budget passing first.
        tokio::time::advance(Duration::from_secs(14)).await;
        let slow = http_body_util::StreamBody::new(futures::stream::once(async {
            tokio::time::sleep(Duration::from_secs(14)).await;
            Ok::<_, Infallible>(hyper::body::Frame::data(Bytes::from_static(b"{}")))
        }));
        assert_eq!(read_body(&parts(&authed()), slow, &config()).await.unwrap(), "{}");

        let stalled = http_body_util::StreamBody::new(futures::stream::pending::<
            Result<hyper::body::Frame<Bytes>, Infallible>,
        >());
        let started = tokio::time::Instant::now();
        assert_eq!(
            read_body(&parts(&authed()), stalled, &config()).await.unwrap_err().0,
            StatusCode::REQUEST_TIMEOUT
        );
        assert_eq!(started.elapsed(), Duration::from_secs(BODY_TIMEOUT_SECS));
    }

    #[tokio::test]
    async fn body_size_limit_still_applies() {
        let oversized = Full::new(Bytes::from(vec![b'x'; MAX_BODY + 1]));
        assert_eq!(
            read_body(&parts(&authed()), oversized, &config()).await.unwrap_err().0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[tokio::test]
    async fn saturated_admission_drops_the_socket_without_waiting() {
        use tokio::io::AsyncReadExt;

        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let (_first_client, stream) = tokio::io::duplex(4096);
        let first = admit(stream, &slots).unwrap();

        let (mut excess, stream) = tokio::io::duplex(4096);
        assert!(admit(stream, &slots).is_none());
        // Rejection closes the accepted socket while the first still owns its
        // permit; no waiting/spawned task can retain the excess connection.
        let closed = tokio::time::timeout(Duration::from_secs(2), excess.read_u8())
            .await
            .unwrap();
        assert!(closed.is_err());
        assert_eq!(slots.available_permits(), 0);

        drop(first);
        let (_next_client, stream) = tokio::io::duplex(4096);
        assert!(admit(stream, &slots).is_some());
        assert_eq!(slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn connection_slot_survives_keepalive_and_is_released_on_disconnect() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        const TEST_QUOTA: usize = 2;
        let slots = Arc::new(tokio::sync::Semaphore::new(TEST_QUOTA));
        let mut held: Vec<_> = (0..TEST_QUOTA)
            .map(|_| slots.clone().try_acquire_owned().unwrap())
            .collect();
        assert!(slots.clone().try_acquire_owned().is_err());

        let (mut client, stream) = tokio::io::duplex(8192);
        let peer = "127.0.0.1:12345".parse().unwrap();
        let task = serve_with(stream, peer, held.pop().unwrap(), |_, _| async { healthz() });
        for _ in 0..2 {
            client
                .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();
            // Read a complete response, leaving the socket open for reuse.
            let mut response = Vec::new();
            loop {
                let byte = tokio::time::timeout(Duration::from_secs(2), client.read_u8())
                    .await
                    .unwrap()
                    .unwrap();
                response.push(byte);
                if response.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let headers = String::from_utf8(response).unwrap();
            assert!(headers.starts_with("HTTP/1.1 200 OK"), "{headers}");
            let len: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .unwrap()
                .parse()
                .unwrap();
            client.read_exact(&mut vec![0; len]).await.unwrap();
            assert!(slots.clone().try_acquire_owned().is_err());
        }
        drop(client);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(slots.available_permits(), 1);
        drop(held);
        assert_eq!(slots.available_permits(), TEST_QUOTA);
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_handler_is_not_subject_to_arrival_deadlines() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let (mut client, stream) = tokio::io::duplex(8192);
        let peer = "127.0.0.1:12345".parse().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started_tx = std::sync::Mutex::new(Some(started_tx));
        let task = serve_with(stream, peer, slots.clone().try_acquire_owned().unwrap(), move |_, _| {
            started_tx.lock().unwrap().take().unwrap().send(()).unwrap();
            async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                healthz()
            }
        });
        client
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        started_rx.await.unwrap();
        tokio::time::advance(Duration::from_secs(31)).await;
        assert!(!task.is_finished());
        assert_eq!(slots.available_permits(), 0);
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        task.await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert_eq!(slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn detached_review_keeps_its_bounded_slot_after_client_disconnect() {
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = admit_review(&slots).unwrap();
        let (finish, finished) = tokio::sync::oneshot::channel();
        let (response, client) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            let _permit = permit;
            finished.await.unwrap();
            let _ = response.send(());
        });
        drop(client);
        assert_eq!(
            admit_review(&slots).unwrap_err().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(slots.available_permits(), 0);
        finish.send(()).unwrap();
        worker.await.unwrap();
        assert_eq!(slots.available_permits(), 1);
        assert!(admit_review(&slots).is_ok());
    }

    #[test]
    fn tokenless_reviews_cannot_release_admission() {
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let admission = Admission(Arc::new(std::sync::Mutex::new(Some(
            slots.clone().try_acquire_owned().unwrap(),
        ))));
        let mut config = config();
        config.token = None;
        admission.release_for_review(&config);
        assert!(!admission.released());
        assert_eq!(slots.available_permits(), 0);
        drop(admission);
        assert_eq!(slots.available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn authenticated_review_releases_admission_but_cannot_reuse_connection() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let (mut client, stream) = tokio::io::duplex(8192);
        let peer = "127.0.0.1:12345".parse().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started_tx = std::sync::Mutex::new(Some(started_tx));
        let task = serve_with(
            stream,
            peer,
            slots.clone().try_acquire_owned().unwrap(),
            move |req, admission| {
                let started_tx = started_tx
                    .lock()
                    .unwrap()
                    .take()
                    .expect("connection must not dispatch a second request");
                async move {
                    let (parts, body) = req.into_parts();
                    let config = config();
                    let bytes = read_body(&parts, body, &config).await.unwrap();
                    classify(&parts, &bytes, &config).unwrap();
                    admission.release_for_review(&config);
                    started_tx.send(()).unwrap();
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    healthz()
                }
            },
        );
        let body = body();
        client.write_all(format!(
            "POST /hooks/exit-plan HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n",
            "t".repeat(32), body.len(),
        ).as_bytes()).await.unwrap();
        client.write_all(&body).await.unwrap();
        started_rx.await.unwrap();
        tokio::time::advance(Duration::from_secs(31)).await;
        assert!(!task.is_finished(), "arrival deadline must not terminate review");
        assert_eq!(slots.available_permits(), 1, "review must not retain arrival permit");
        let _other_arrival = slots.clone().try_acquire_owned().unwrap();
        client
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        task.await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains("connection: close\r\n"), "{response}");
        assert_eq!(
            response.matches("HTTP/1.1").count(),
            1,
            "no second response on a released connection"
        );
    }

    #[test]
    fn a_well_formed_request_is_accepted() {
        let request = classify(&parts(&authed()), &body(), &config()).unwrap();
        assert_eq!(request.session_id, "s");
        assert_eq!(request.round, 1);
    }

    #[test]
    fn request_and_nested_history_reject_unknown_fields() {
        let mut top_level = review_body();
        top_level["futureField"] = true.into();
        let mut nested = review_body();
        nested["history"] = serde_json::json!([{
            "round": 1,
            "verdict": "revise",
            "summary": "x",
            "futureField": true,
        }]);

        for body in [encode_body(&top_level), encode_body(&nested)] {
            assert_eq!(
                classify(&parts(&authed()), &body, &config()).unwrap_err().0,
                StatusCode::BAD_REQUEST
            );
        }

        let mut stop = stop_body();
        stop["futureField"] = true.into();
        assert_eq!(
            classify_stop(&parts(&authed()), &encode_body(&stop), &config())
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn nullable_fields_accept_explicit_null() {
        let request = classify(&parts(&authed()), &body(), &config()).unwrap();
        assert_eq!(request.conversation_id, None);

        let stop = classify_stop(&parts(&authed()), &encode_body(&stop_body()), &config()).unwrap();
        assert_eq!(stop.conversation_id, None);
        assert_eq!(stop.note, None);
    }

    #[test]
    fn a_conversation_id_is_carried_through() {
        let mut with_id = review_body();
        with_id["conversationId"] = "c-1".into();
        let request = classify(&parts(&authed()), &encode_body(&with_id), &config()).unwrap();
        assert_eq!(request.conversation_id.as_deref(), Some("c-1"));
    }

    #[test]
    fn every_review_request_key_must_be_present() {
        for key in [
            "sessionId",
            "cwd",
            "conversationId",
            "plan",
            "round",
            "max_rounds",
            "stagnant",
            "history",
        ] {
            let mut incomplete = review_body();
            incomplete.as_object_mut().unwrap().remove(key);
            let error = classify(&parts(&authed()), &encode_body(&incomplete), &config()).unwrap_err();
            assert_eq!(error.0, StatusCode::BAD_REQUEST, "missing {key}");
            assert!(error.1.contains("missing field"), "missing {key}: {}", error.1);
        }
    }

    #[test]
    fn every_stop_request_key_must_be_present() {
        for key in [
            "sessionId",
            "cwd",
            "conversationId",
            "diff",
            "note",
            "round",
            "max_rounds",
            "stagnant",
            "history",
        ] {
            let mut incomplete = stop_body();
            incomplete.as_object_mut().unwrap().remove(key);
            let error = classify_stop(&parts(&authed()), &encode_body(&incomplete), &config()).unwrap_err();
            assert_eq!(error.0, StatusCode::BAD_REQUEST, "missing {key}");
            assert!(error.1.contains("missing field"), "missing {key}: {}", error.1);
        }
    }

    #[test]
    fn every_history_key_must_be_present_and_verdict_is_closed() {
        for key in ["round", "verdict", "summary"] {
            let mut history = serde_json::json!({
                "round": 1,
                "verdict": "revise",
                "summary": "x",
            });
            history.as_object_mut().unwrap().remove(key);
            let mut incomplete = review_body();
            incomplete["history"] = serde_json::json!([history]);
            let error = classify(&parts(&authed()), &encode_body(&incomplete), &config()).unwrap_err();
            assert_eq!(error.0, StatusCode::BAD_REQUEST, "missing history.{key}");
            assert!(error.1.contains("missing field"), "missing history.{key}: {}", error.1);
        }

        let mut unknown = review_body();
        unknown["history"] = serde_json::json!([{
            "round": 1,
            "verdict": "accepted_in_future",
            "summary": "x",
        }]);
        assert_eq!(
            classify(&parts(&authed()), &encode_body(&unknown), &config())
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn a_browser_origin_is_refused() {
        let mut headers = authed();
        headers.push(("origin", "https://evil.example"));
        let err = classify(&parts(&headers), &body(), &config()).unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }

    #[test]
    fn a_cross_site_fetch_is_refused() {
        let mut headers = authed();
        headers.push(("sec-fetch-site", "cross-site"));
        let err = classify(&parts(&headers), &body(), &config()).unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }

    /// The address bar and curl both send `none`; a same-origin XHR from our
    /// own page sends `same-origin`. Neither is the attack.
    #[test]
    fn a_same_origin_or_direct_request_passes() {
        for site in ["none", "same-origin"] {
            let mut headers = authed();
            headers.push(("sec-fetch-site", site));
            assert!(classify(&parts(&headers), &body(), &config()).is_ok(), "{site}");
        }
    }

    #[test]
    fn a_form_post_is_refused_before_the_token_is_even_checked() {
        let headers = vec![("content-type", "text/plain")];
        let err = classify(&parts(&headers), &body(), &config()).unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }

    #[test]
    fn a_wrong_or_missing_token_is_unauthorized() {
        let wrong = vec![("content-type", "application/json"), ("authorization", "Bearer nope")];
        assert_eq!(
            classify(&parts(&wrong), &body(), &config()).unwrap_err().0,
            StatusCode::UNAUTHORIZED
        );
        let none = vec![("content-type", "application/json")];
        assert_eq!(
            classify(&parts(&none), &body(), &config()).unwrap_err().0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn an_empty_plan_is_nothing_to_review() {
        let mut empty = review_body();
        empty["plan"] = "   ".into();
        assert_eq!(
            classify(&parts(&authed()), &encode_body(&empty), &config())
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn a_missing_cwd_is_refused_because_the_reviewer_needs_a_repository() {
        let mut no_cwd = review_body();
        no_cwd["cwd"] = "".into();
        assert_eq!(
            classify(&parts(&authed()), &encode_body(&no_cwd), &config())
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
    }

    /// Exercise the production Hyper builder with actual HTTP bytes. Duplex
    /// avoids Windows socket-buffer exhaustion (10055), not protocol failures.
    /// Removing `.timer(...)` still panics, just as on a real TCP connection.
    #[tokio::test]
    async fn a_duplex_connection_gets_a_real_http_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut client, stream) = tokio::io::duplex(8192);
        let task = tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let service = hyper::service::service_fn(|_req| async { Ok::<_, Infallible>(healthz()) });
            let _ = connection_builder().serve_connection(io, service).await;
        });

        // Raw HTTP bytes rather than a client crate's interpretation.
        client
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        task.await.unwrap();
        let response = String::from_utf8_lossy(&response);

        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains(r#""ok":true"#), "{response}");
    }

    /// The plugin distinguishes the three responses by shape alone, so the
    /// shapes are part of the contract rather than an implementation detail.
    #[test]
    fn the_wire_shapes_are_what_the_plugin_expects() {
        let approve = encode(&ReviewResponse::approve("fine".into(), "r-1".into(), "c-1".into()));
        assert!(approve.contains(r#""verdict":"approve""#), "{approve}");
        assert!(!approve.contains("message"), "{approve}");
        // The continuation the client has to store; without it round 2 starts
        // a reviewer with no memory of round 1.
        assert!(approve.contains(r#""conversationId":"c-1""#), "{approve}");

        let revise = encode(&ReviewResponse::revise(
            "bad".into(),
            "fix it".into(),
            "r-2".into(),
            "c-1".into(),
        ));
        assert!(revise.contains(r#""verdict":"revise""#), "{revise}");
        assert!(revise.contains(r#""message":"fix it""#), "{revise}");
        assert!(revise.contains(r#""conversationId":"c-1""#), "{revise}");

        // No verdict, but a transcript was still written — the next round
        // continues in it, so this shape carries the conversation too.
        let inconclusive = encode(&ReviewResponse::inconclusive("no idea", "c-1".into()));
        assert_eq!(inconclusive, r#"{"systemMessage":"no idea","conversationId":"c-1"}"#);
    }
}

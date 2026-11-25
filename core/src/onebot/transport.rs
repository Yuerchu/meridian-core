//! The adapter reverse-connects here; this is not an outbound client.
use std::sync::{Arc, atomic::Ordering};
use std::time::Duration;

use axum::{
    Router,
    extract::{OriginalUri, State, WebSocketUpgrade, ws::rejection::WebSocketUpgradeRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use tokio::{net::TcpListener, sync::Semaphore, task::JoinSet};

use super::SharedState;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_PENDING: usize = 32;
const MAX_ACTIVE: usize = 32;
const MAX_MESSAGE: usize = 1024 * 1024;

struct ConnectionGuard(Arc<SharedState>);
impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.connected_clients.fetch_sub(1, Ordering::Relaxed);
        self.0.conn_closed.send_modify(|n| *n += 1);
    }
}

#[derive(Clone)]
struct UpgradeState {
    shared: Arc<SharedState>,
    active: Arc<Semaphore>,
    sequence: Arc<std::sync::atomic::AtomicU64>,
}

fn close_response(response: impl IntoResponse) -> Response {
    let mut res = response.into_response();
    res.headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    res
}

fn authorize(headers: &HeaderMap, uri: &axum::http::Uri, expected: &str) -> Result<(), StatusCode> {
    if headers.contains_key(axum::http::header::ORIGIN) {
        return Err(StatusCode::FORBIDDEN);
    }
    if !matches!(uri.path(), "/" | "/ws" | "/onebot/v11/ws") {
        return Err(StatusCode::NOT_FOUND);
    }
    if headers.get_all(axum::http::header::AUTHORIZATION).iter().count() > 1 {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let auth = match headers.get(axum::http::header::AUTHORIZATION) {
        Some(value) => Some(value.to_str().map_err(|_| StatusCode::UNAUTHORIZED)?),
        None => None,
    };
    if !super::token_matches(expected, auth, uri.query()) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(())
}

async fn upgrade(
    State(state): State<UpgradeState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let expected = state.shared.config.access_token.as_deref().unwrap_or("");
    if let Err(status) = authorize(&headers, &uri, expected) {
        return close_response(status);
    }
    let ws = match ws {
        Ok(ws) => ws,
        Err(rejection) => return close_response(rejection),
    };
    if *state.shared.shutdown.borrow() {
        return close_response(StatusCode::SERVICE_UNAVAILABLE);
    }
    let Ok(permit) = state.active.clone().try_acquire_owned() else {
        return close_response(StatusCode::SERVICE_UNAVAILABLE);
    };
    let conn_id = state.sequence.fetch_add(1, Ordering::Relaxed);
    state.shared.connected_clients.fetch_add(1, Ordering::Relaxed);
    let guard = ConnectionGuard(state.shared.clone());
    ws.max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            let _guard = guard;
            super::handle_connection(socket, conn_id, state.shared.clone()).await;
        })
}

pub(super) async fn serve(listener: TcpListener, shared: Arc<SharedState>) {
    let shutdown = shared.shutdown.subscribe();
    let app = Router::new().fallback(get(upgrade)).with_state(UpgradeState {
        shared,
        active: Arc::new(Semaphore::new(MAX_ACTIVE)),
        sequence: Arc::new(std::sync::atomic::AtomicU64::new(1)),
    });
    serve_http(listener, app, shutdown, MAX_PENDING).await;
}

async fn serve_http(
    listener: TcpListener,
    app: Router,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    max_pending: usize,
) {
    let pending = Arc::new(Semaphore::new(max_pending));
    let mut tasks = JoinSet::new();
    loop {
        if *shutdown.borrow() {
            break;
        }
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                // No task is spawned without admission. Excess sockets close immediately.
                let Ok(permit) = pending.clone().try_acquire_owned() else { continue };
                let service = TowerToHyperService::new(app.clone());
                tasks.spawn(async move {
                    let _permit = permit;
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    // Do NOT set `.keep_alive(false)`: hyper 1.x strips `Connection: upgrade`
                    // from `101 Switching Protocols` responses when keep-alive is disabled,
                    // breaking RFC 6455 handshakes. Non-upgrade responses set `Connection: close`
                    // explicitly via `close_response`.
                    builder
                        .timer(TokioTimer::new())
                        .header_read_timeout(HANDSHAKE_TIMEOUT)
                        .max_buf_size(16 * 1024);
                    // Never log the request or hyper error: either can contain a query token.
                    let _ = tokio::time::timeout(HANDSHAKE_TIMEOUT,
                        builder.serve_connection(TokioIo::new(stream), service).with_upgrades()).await;
                });
            }
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn pending_admission_and_shutdown_close_real_sockets() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, receiver) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(serve_http(listener, Router::new(), receiver, 2));
        let mut sockets = Vec::new();
        for _ in 0..2 {
            let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
            socket
                .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n")
                .await
                .unwrap();
            sockets.push(socket);
        }
        let mut excess = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut byte = [0u8; 1];
        let result = tokio::time::timeout(Duration::from_secs(2), excess.read(&mut byte))
            .await
            .unwrap();
        assert!(
            matches!(result, Ok(0) | Err(_)),
            "the first socket over the test quota must close"
        );
        shutdown.send_replace(true);
        server.await.unwrap();
        for mut socket in sockets {
            let result = tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte))
                .await
                .unwrap();
            assert!(
                matches!(result, Ok(0) | Err(_)),
                "shutdown must close partial handshakes"
            );
        }
    }

    #[tokio::test]
    async fn incomplete_headers_expire_at_handshake_deadline() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, receiver) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(serve_http(listener, Router::new(), receiver, 2));
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(
            HANDSHAKE_TIMEOUT + Duration::from_secs(3),
            socket.read_to_end(&mut bytes),
        )
        .await
        .expect("header deadline must close the socket")
        .ok();
        shutdown.send_replace(true);
        server.await.unwrap();
    }

    #[test]
    fn upgrade_boundary() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer mock-token".parse().unwrap());
        assert!(authorize(&headers, &"/".parse().unwrap(), "mock-token").is_ok());
        assert!(authorize(&headers, &"/ws".parse().unwrap(), "mock-token").is_ok());
        assert!(authorize(&headers, &"/onebot/v11/ws".parse().unwrap(), "mock-token").is_ok());
        assert!(authorize(&headers, &"/unexpected".parse().unwrap(), "mock-token").is_err());
        assert!(authorize(&headers, &"/%6fnebot/v11/ws".parse().unwrap(), "mock-token").is_err());
        assert!(authorize(&headers, &"/%77s".parse().unwrap(), "mock-token").is_err());
        assert!(
            authorize(
                &headers,
                &"/onebot/v11/ws?access_token=wrong".parse().unwrap(),
                "mock-token"
            )
            .is_err()
        );
        assert!(authorize(&headers, &"/".parse().unwrap(), "").is_err());
        headers.insert("origin", "https://example.invalid".parse().unwrap());
        assert_eq!(
            authorize(&headers, &"/".parse().unwrap(), "mock-token"),
            Err(StatusCode::FORBIDDEN)
        );
        headers.remove("origin");
        headers.append("authorization", "Bearer mock-token".parse().unwrap());
        assert!(authorize(&headers, &"/".parse().unwrap(), "mock-token").is_err());
    }

    #[tokio::test]
    async fn real_websocket_handshake_and_idle_ping_pong_succeed() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, receiver) = tokio::sync::watch::channel(false);

        // Minimal upgrade handler mirroring `upgrade` + `handle_connection` split-socket Ping flush
        let app = Router::new().fallback(get(
            |OriginalUri(uri): OriginalUri,
             headers: HeaderMap,
             ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>| async move {
                if let Err(status) = authorize(&headers, &uri, "mock-token") {
                    return close_response(status);
                }
                let ws = match ws {
                    Ok(ws) => ws,
                    Err(rejection) => return close_response(rejection),
                };
                ws.max_message_size(MAX_MESSAGE)
                    .max_frame_size(MAX_MESSAGE)
                    .on_upgrade(|socket| async move {
                        use axum::extract::ws::Message;
                        use futures::{SinkExt, StreamExt};
                        let (mut write, mut read) = socket.split();
                        while let Some(Ok(msg)) = read.next().await {
                            if matches!(msg, Message::Ping(_)) {
                                if write.flush().await.is_err() {
                                    break;
                                }
                            } else if matches!(msg, Message::Close(_)) {
                                break;
                            }
                        }
                    })
            },
        ));
        let server = tokio::spawn(serve_http(listener, app, receiver, 8));

        // 1. All three supported paths must return 101 Switching Protocols WITH `Connection: upgrade`
        for path in ["/", "/ws", "/onebot/v11/ws"] {
            let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
            let req = format!(
                "GET {path} HTTP/1.1\r\n\
                 Host: 127.0.0.1\r\n\
                 Upgrade: websocket\r\n\
                 Connection: Upgrade\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                 Sec-WebSocket-Version: 13\r\n\
                 Authorization: Bearer mock-token\r\n\r\n"
            );
            stream.write_all(req.as_bytes()).await.unwrap();

            let mut buf = vec![0u8; 1024];
            let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
                .await
                .expect("handshake response timed out")
                .unwrap();
            let head = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
            assert!(
                head.starts_with("http/1.1 101 "),
                "{path}: expected 101 Switching Protocols, got:\n{head}"
            );
            assert!(
                head.contains("\r\nconnection: upgrade\r\n"),
                "{path}: missing `Connection: upgrade` header:\n{head}"
            );
            assert!(
                head.contains("\r\nupgrade: websocket\r\n"),
                "{path}: missing `Upgrade: websocket` header:\n{head}"
            );
            assert!(
                head.contains("\r\nsec-websocket-accept: s3pplmbitxaq9kygzzhzrbk+xoo=\r\n"),
                "{path}: missing valid `Sec-WebSocket-Accept` header:\n{head}"
            );

            // Send a masked client Ping frame (payload "hi", mask 0x11223344) while server is idle
            let ping_frame: [u8; 8] = [
                0x89, // FIN + Ping opcode (0x9)
                0x82, // MASK + len 2
                0x11,
                0x22,
                0x33,
                0x44, // mask key
                b'h' ^ 0x11,
                b'i' ^ 0x22,
            ];
            stream.write_all(&ping_frame).await.unwrap();

            let mut pong = [0u8; 16];
            let pn = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut pong))
                .await
                .expect("idle server must flush Pong to client Ping")
                .unwrap();
            assert_eq!(&pong[..pn], &[0x8a, 0x02, b'h', b'i'], "{path}: expected Pong('hi')");
        }

        // 2. Rejected requests must return `Connection: close` and close the TCP socket
        let mut rejected = tokio::net::TcpStream::connect(address).await.unwrap();
        rejected
            .write_all(b"GET /unexpected HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer mock-token\r\n\r\n")
            .await
            .unwrap();
        let mut raw = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), rejected.read_to_end(&mut raw))
            .await
            .expect("rejected connection must close immediately")
            .unwrap();
        let head = String::from_utf8_lossy(&raw).to_ascii_lowercase();
        assert!(head.starts_with("http/1.1 404 "), "expected 404, got:\n{head}");
        assert!(
            head.contains("\r\nconnection: close\r\n"),
            "expected `Connection: close` on 404:\n{head}"
        );

        shutdown.send_replace(true);
        server.await.unwrap();
    }
}

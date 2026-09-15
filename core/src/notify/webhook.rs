//! Putting an alert on the wire.
//!
//! **Only `generic` is our contract.** The other four formats belong to
//! DingTalk, Feishu, WeCom and Slack, and each of them differs from ours *and
//! from each other* in the two places it matters: how a request is signed, and
//! how failure is reported. Three of the four answer a rejected message with
//! HTTP 200 and an error code in the body, so "2xx" is not success here — a
//! delivery judged on the status alone reports every one of those as sent.
//!
//! The signature covers the exact bytes that are sent. The body is serialized
//! once, signed, and handed to the transport as a raw payload rather than as a
//! `serde_json::Value` it would serialize again: a re-serialization that
//! differs by one byte produces a signature the receiver rejects, and nothing
//! on either side says why.

use std::time::{Duration, Instant};

use base64::Engine;
use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::Sha256;

use crate::client::{HttpTransport, Request, RequestBody, ReqwestTransport, TransportError, backoff};
use crate::db::models::notification::{NotificationEventKind, NotificationFormat, NotificationWebhookRow};

use super::alert::{Alert, AlertDetail, BalanceAlert, TestAlert, UsageAlert};

/// The version of the `generic` contract. Bumped only when a receiver would
/// have to change; new optional members do not move it.
const SPEC_VERSION: &str = "1";

const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ATTEMPTS: u32 = 3;
const RETRY_BASE: Duration = Duration::from_millis(500);

/// How much of an upstream's answer is kept.
///
/// Enough to read `{"errcode":310000,"errmsg":"..."}`, not enough to turn a
/// list of endpoints into a log dump.
const EXCERPT_CHARS: usize = 300;

pub fn webhook_secret_name(id: &str) -> String {
    format!("NOTIFY_WEBHOOK_SECRET_{}", id.replace('-', "_").to_uppercase())
}

/// What one delivery did. `error: None` is the only success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryReport {
    pub status: Option<u16>,
    pub duration_ms: u64,
    pub attempts: u32,
    pub error: Option<String>,
    pub response_excerpt: Option<String>,
}

impl DeliveryReport {
    pub fn is_success(&self) -> bool {
        self.error.is_none()
    }
}

/// The `generic` wire shape, which is the only one this app defines.
///
/// `balance` and `usage` are mutually exclusive and both are always present as
/// members, one of them null. Absent-versus-null is a distinction a strict
/// receiver has to handle, and there is no reason to make it deal with it.
#[derive(Debug, Serialize)]
struct GenericPayload<'a> {
    spec_version: &'a str,
    delivery_id: &'a str,
    event: NotificationEventKind,
    raised_at: i64,
    alert_key: &'a str,
    title: &'a str,
    summary: &'a str,
    balance: Option<&'a BalanceAlert>,
    usage: Option<&'a UsageAlert>,
    test: Option<&'a TestAlert>,
}

impl<'a> GenericPayload<'a> {
    fn new(alert: &'a Alert, delivery_id: &'a str) -> Self {
        let (balance, usage, test) = match &alert.detail {
            AlertDetail::Balance(detail) => (Some(detail), None, None),
            AlertDetail::Usage(detail) => (None, Some(detail), None),
            AlertDetail::Test(detail) => (None, None, Some(detail)),
        };
        Self {
            spec_version: SPEC_VERSION,
            delivery_id,
            event: alert.event,
            raised_at: alert.raised_at,
            alert_key: &alert.alert_key,
            title: &alert.title,
            summary: &alert.summary,
            balance,
            usage,
            test,
        }
    }
}

/// A request built and signed, ready to be sent as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedDelivery {
    pub url: String,
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
    /// Whether the body has to be inspected for a vendor error code, or the
    /// status is the whole answer.
    pub reads_body_for_errors: bool,
}

/// Build the exact bytes for one endpoint.
///
/// `now_ms` is a parameter rather than read here so a signature is reproducible
/// in a test — a signing scheme nobody can pin is one nobody finds out is wrong
/// until a vendor silently drops every message.
pub fn prepare(
    format: NotificationFormat,
    url: &str,
    secret: Option<&str>,
    template: Option<&serde_json::Value>,
    alert: &Alert,
    delivery_id: &str,
    now_ms: i64,
) -> Result<PreparedDelivery, String> {
    let text = render_text(alert);
    match format {
        NotificationFormat::Generic => {
            let body = to_bytes(&GenericPayload::new(alert, delivery_id))?;
            let mut headers = vec![
                ("content-type".into(), "application/json".into()),
                ("x-meridian-event".into(), alert.event.as_str().into()),
                ("x-meridian-delivery".into(), delivery_id.into()),
                ("x-meridian-timestamp".into(), now_ms.to_string()),
            ];
            // No secret, no signature header at all. An HMAC over an empty key
            // is a well-formed signature that verifies against an empty key,
            // which reads to a receiver as a signed request.
            if let Some(secret) = secret.filter(|secret| !secret.is_empty()) {
                let signed = format!("{now_ms}.{}", String::from_utf8_lossy(&body));
                headers.push((
                    "x-meridian-signature".into(),
                    format!("sha256={}", hmac_hex(secret.as_bytes(), signed.as_bytes())),
                ));
            }
            Ok(PreparedDelivery {
                url: url.to_string(),
                body,
                headers,
                reads_body_for_errors: false,
            })
        }
        NotificationFormat::Dingtalk => {
            // DingTalk signs the *URL*: the timestamp and signature are query
            // parameters, and the signed material is `<timestamp>\n<secret>` —
            // the message body is not covered at all.
            let url = match secret.filter(|secret| !secret.is_empty()) {
                None => url.to_string(),
                Some(secret) => {
                    let signature = hmac_base64(secret.as_bytes(), format!("{now_ms}\n{secret}").as_bytes());
                    let escaped = percent_encoding::utf8_percent_encode(&signature, percent_encoding::NON_ALPHANUMERIC)
                        .to_string();
                    let separator = if url.contains('?') { '&' } else { '?' };
                    format!("{url}{separator}timestamp={now_ms}&sign={escaped}")
                }
            };
            let body = to_bytes(&serde_json::json!({
                "msgtype": "markdown",
                "markdown": { "title": alert.title, "text": text },
            }))?;
            Ok(PreparedDelivery {
                url,
                body,
                headers: vec![("content-type".into(), "application/json".into())],
                reads_body_for_errors: true,
            })
        }
        NotificationFormat::Feishu => {
            // Feishu signs in the *body*, in seconds rather than milliseconds,
            // and inverts the HMAC: the key is `<timestamp>\n<secret>` and the
            // signed message is empty.
            let seconds = now_ms / 1000;
            let mut payload = serde_json::json!({
                "msg_type": "text",
                "content": { "text": format!("{}\n{}", alert.title, text) },
            });
            if let Some(secret) = secret.filter(|secret| !secret.is_empty()) {
                let signature = hmac_base64(format!("{seconds}\n{secret}").as_bytes(), b"");
                payload["timestamp"] = serde_json::Value::String(seconds.to_string());
                payload["sign"] = serde_json::Value::String(signature);
            }
            Ok(PreparedDelivery {
                url: url.to_string(),
                body: to_bytes(&payload)?,
                headers: vec![("content-type".into(), "application/json".into())],
                reads_body_for_errors: true,
            })
        }
        // WeCom authenticates with the key already in the URL and offers no
        // signing of its own. A secret configured here would sign nothing.
        NotificationFormat::Wecom => {
            let body = to_bytes(&serde_json::json!({
                "msgtype": "markdown",
                "markdown": { "content": format!("**{}**\n{}", alert.title, text) },
            }))?;
            Ok(PreparedDelivery {
                url: url.to_string(),
                body,
                headers: vec![("content-type".into(), "application/json".into())],
                reads_body_for_errors: true,
            })
        }
        NotificationFormat::Slack => {
            let body = to_bytes(&serde_json::json!({
                "text": format!("*{}*\n{}", alert.title, text),
            }))?;
            Ok(PreparedDelivery {
                url: url.to_string(),
                body,
                headers: vec![("content-type".into(), "application/json".into())],
                // Slack answers a rejected payload with a non-2xx and a plain
                // text reason, so the status really is the whole answer.
                reads_body_for_errors: false,
            })
        }
        NotificationFormat::Custom => {
            let template = template
                .ok_or_else(|| "a `custom` endpoint has no body template; there is nothing to post".to_string())?;
            let body = to_bytes(&super::template::render(template, alert, delivery_id))?;
            let mut headers = vec![("content-type".into(), "application/json".into())];
            // The stored secret is a bearer token for this format. No secret is
            // an unauthenticated POST rather than an empty `Bearer `, which a
            // receiver would reject with a message about the token being
            // malformed rather than about there being none.
            if let Some(secret) = secret.filter(|secret| !secret.is_empty()) {
                headers.push(("authorization".into(), format!("Bearer {secret}")));
            }
            Ok(PreparedDelivery {
                url: url.to_string(),
                body,
                headers,
                // Whatever this endpoint is, it is not one of the three that
                // report failure inside a 200 — and guessing at an error shape
                // we have never seen would turn a delivered alert into a
                // duplicate on the next tick.
                reads_body_for_errors: false,
            })
        }
    }
}

/// The human-readable half every vendor template wraps.
fn render_text(alert: &Alert) -> String {
    alert.summary.clone()
}

fn to_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    serde_json::to_vec(value).map_err(|error| format!("could not encode the notification payload: {error}"))
}

fn hmac_bytes(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

fn hmac_hex(key: &[u8], message: &[u8]) -> String {
    hmac_bytes(key, message)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn hmac_base64(key: &[u8], message: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(hmac_bytes(key, message))
}

/// Whether a vendor that answers 200 for everything actually accepted this.
///
/// One rule rather than four, keyed on the field names the three of them use.
/// A body with none of those fields is taken at its status: these APIs have
/// more than one generation of response shape between them, and refusing an
/// answer we do not recognise would report a delivered message as failed —
/// which is the direction that causes a duplicate on the next tick.
pub fn vendor_error(body: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    for field in ["errcode", "code", "StatusCode"] {
        if let Some(code) = parsed.get(field).and_then(serde_json::Value::as_i64) {
            if code == 0 {
                return None;
            }
            let message = ["errmsg", "msg", "StatusMessage"]
                .iter()
                .find_map(|name| parsed.get(*name).and_then(serde_json::Value::as_str))
                .unwrap_or("no message");
            return Some(format!("upstream rejected the message: {field}={code} ({message})"));
        }
    }
    None
}

/// Send one alert to one endpoint.
pub async fn deliver(endpoint: &NotificationWebhookRow, secret: Option<&str>, alert: &Alert) -> DeliveryReport {
    let started = Instant::now();
    let delivery_id = uuid::Uuid::new_v4().to_string();
    let format = match endpoint.format() {
        Ok(format) => format,
        Err(error) => return failed(started, 1, None, error, None),
    };
    // Decoded here rather than at render time so a malformed template fails the
    // delivery with its own message, instead of posting a document nobody meant.
    let template = match endpoint.body_template() {
        Ok(template) => template,
        Err(error) => return failed(started, 1, None, error, None),
    };
    let prepared = match prepare(
        format,
        &endpoint.url,
        secret,
        template.as_ref(),
        alert,
        &delivery_id,
        crate::util::now_ms(),
    ) {
        Ok(prepared) => prepared,
        Err(error) => return failed(started, 1, None, error, None),
    };

    let transport = ReqwestTransport::shared();
    let mut attempts = 0;
    loop {
        attempts += 1;
        let mut request = Request::new(http::Method::POST, prepared.url.clone());
        request.timeout = Some(TIMEOUT);
        request.body = Some(RequestBody::Raw(prepared.body.clone().into()));
        for (name, value) in &prepared.headers {
            match (
                http::HeaderName::try_from(name.as_str()),
                http::HeaderValue::from_str(value),
            ) {
                (Ok(name), Ok(value)) => {
                    request.headers.insert(name, value);
                }
                // A header this app builds should always be valid; if one is
                // not, the delivery is wrong rather than merely unsigned.
                _ => return failed(started, attempts, None, format!("invalid header `{name}`"), None),
            }
        }

        match transport.execute(request).await {
            Ok(response) => {
                let status = response.status.as_u16();
                let body = String::from_utf8_lossy(&response.body).to_string();
                let excerpt = excerpt(&body);
                if prepared.reads_body_for_errors
                    && let Some(error) = vendor_error(&body)
                {
                    // A vendor rejection is about the message, not the
                    // connection: sending it again produces the same answer.
                    return failed(started, attempts, Some(status), error, excerpt);
                }
                return DeliveryReport {
                    status: Some(status),
                    duration_ms: elapsed(started),
                    attempts,
                    error: None,
                    response_excerpt: excerpt,
                };
            }
            Err(error) => {
                let status = http_status(&error);
                if !retryable(&error) || attempts >= MAX_ATTEMPTS {
                    let excerpt = http_body(&error).and_then(|body| excerpt(&body));
                    return failed(started, attempts, status, error.to_string(), excerpt);
                }
                tokio::time::sleep(backoff(RETRY_BASE, u64::from(attempts))).await;
            }
        }
    }
}

/// A 4xx is the endpoint saying this request is wrong, and repeating it
/// unchanged wastes the vendor's rate limit to get the same answer. 429 and 5xx
/// are the two that mean "later".
fn retryable(error: &TransportError) -> bool {
    match error {
        TransportError::Http { status, .. } => status.as_u16() == 429 || status.is_server_error(),
        TransportError::Timeout | TransportError::Network(_) => true,
        TransportError::Build(_) | TransportError::RetryLimit => false,
    }
}

fn http_status(error: &TransportError) -> Option<u16> {
    match error {
        TransportError::Http { status, .. } => Some(status.as_u16()),
        _ => None,
    }
}

fn http_body(error: &TransportError) -> Option<String> {
    match error {
        TransportError::Http { body, .. } => body.clone(),
        _ => None,
    }
}

fn excerpt(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= EXCERPT_CHARS {
        return Some(trimmed.to_string());
    }
    Some(trimmed.chars().take(EXCERPT_CHARS).collect::<String>() + "…")
}

fn elapsed(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn failed(
    started: Instant,
    attempts: u32,
    status: Option<u16>,
    error: String,
    excerpt: Option<String>,
) -> DeliveryReport {
    DeliveryReport {
        status,
        duration_ms: elapsed(started),
        attempts,
        error: Some(error),
        response_excerpt: excerpt,
    }
}

/// `deliver` against a real socket.
///
/// Everything above is tested a piece at a time — what the body should be, what
/// counts as a vendor rejection, what is worth retrying. None of that says the
/// pieces are *wired together*, and the one property that cannot be checked any
/// other way is that the bytes covered by the signature are the bytes that
/// leave: `prepare` can only show what it intended to send.
///
/// `hyper` is a desktop-only dependency, which is the only reason this is
/// gated. Nothing about the delivery path is.
#[cfg(all(test, not(target_os = "android")))]
mod wire_tests {
    use std::convert::Infallible;
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};

    use super::tests::test_alert;
    use super::*;

    struct Seen {
        uri: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    /// A server that answers from a script and remembers what it was asked.
    ///
    /// One scripted answer per request, so a retry gets the *next* one — which
    /// is what makes "retried once, then succeeded" observable rather than
    /// inferred from a count this code produced itself.
    struct Recorder {
        url: String,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    async fn recorder(script: Vec<(u16, &'static str)>) -> Recorder {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let remaining = Arc::new(Mutex::new(
            script.into_iter().collect::<std::collections::VecDeque<_>>(),
        ));

        let served = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let io = hyper_util::rt::TokioIo::new(stream);
                let served = served.clone();
                let remaining = remaining.clone();
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                        let served = served.clone();
                        let remaining = remaining.clone();
                        async move {
                            let uri = request.uri().to_string();
                            let headers = request
                                .headers()
                                .iter()
                                .map(|(name, value)| {
                                    (name.as_str().to_string(), value.to_str().unwrap_or("").to_string())
                                })
                                .collect();
                            let body = request.into_body().collect().await.unwrap().to_bytes();
                            served.lock().unwrap().push(Seen {
                                uri,
                                headers,
                                body: String::from_utf8_lossy(&body).to_string(),
                            });
                            let (status, answer) = remaining
                                .lock()
                                .unwrap()
                                .pop_front()
                                .unwrap_or((200, r#"{"errcode":0}"#));
                            Ok::<_, Infallible>(
                                hyper::Response::builder()
                                    .status(status)
                                    .body(Full::new(Bytes::from(answer)))
                                    .unwrap(),
                            )
                        }
                    });
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.timer(hyper_util::rt::TokioTimer::new());
                    let _ = builder.serve_connection(io, service).await;
                });
            }
        });

        Recorder {
            url: format!("http://{addr}/hook"),
            seen,
        }
    }

    fn endpoint(url: &str, format: NotificationFormat) -> NotificationWebhookRow {
        NotificationWebhookRow {
            id: "w1".into(),
            name: "test".into(),
            url: url.into(),
            format: format.as_str().into(),
            events: r#"["test"]"#.into(),
            is_enabled: 1,
            body_template: None,
            last_attempt_at: None,
            last_success_at: None,
            last_error: None,
            consecutive_failures: 0,
            created_at: 1,
            updated_at: 1,
        }
    }

    /// The property no unit test can reach: the signature on the wire verifies
    /// against the bytes that arrived. A body re-serialized anywhere between
    /// signing and sending breaks this and nothing else notices — the receiver
    /// just drops every message.
    #[tokio::test]
    async fn the_signature_covers_the_bytes_that_actually_arrive() {
        let server = recorder(vec![(200, "ok")]).await;
        let report = deliver(
            &endpoint(&server.url, NotificationFormat::Generic),
            Some("s3cret"),
            &test_alert(),
        )
        .await;
        assert!(report.is_success(), "{report:?}");

        let seen = server.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let header = |name: &str| {
            seen[0]
                .headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        let timestamp = header("x-meridian-timestamp").expect("a timestamp header");
        let signature = header("x-meridian-signature").expect("a signature header");
        assert_eq!(
            signature,
            format!(
                "sha256={}",
                hmac_hex(b"s3cret", format!("{timestamp}.{}", seen[0].body).as_bytes())
            ),
            "the signature must verify against the received body"
        );
        assert_eq!(header("x-meridian-event").as_deref(), Some("test"));
        assert_eq!(header("content-type").as_deref(), Some("application/json"));
        assert!(header("x-meridian-delivery").is_some());
        // The body really is JSON on the wire, not a debug rendering of it.
        let parsed: serde_json::Value = serde_json::from_str(&seen[0].body).expect("a JSON body");
        assert_eq!(parsed["spec_version"], "1");
    }

    /// The trap the whole `reads_body_for_errors` flag exists for.
    #[tokio::test]
    async fn a_vendor_rejection_inside_a_200_is_a_failed_delivery() {
        let server = recorder(vec![(200, r#"{"errcode":310000,"errmsg":"sign not match"}"#)]).await;
        let report = deliver(
            &endpoint(&server.url, NotificationFormat::Dingtalk),
            Some("k"),
            &test_alert(),
        )
        .await;

        assert!(!report.is_success());
        assert_eq!(report.status, Some(200));
        assert_eq!(report.attempts, 1, "the same message gets the same rejection");
        assert!(report.error.as_deref().unwrap().contains("310000"), "{report:?}");
        assert!(report.response_excerpt.as_deref().unwrap().contains("sign not match"));

        // And the signing really did reach the URL rather than a header.
        let seen = server.seen.lock().unwrap();
        assert!(seen[0].uri.contains("timestamp="), "{}", seen[0].uri);
        assert!(seen[0].uri.contains("sign="), "{}", seen[0].uri);
    }

    /// The same 200 on a format that does not read its body is a success.
    /// Without this the vendor rule could be applied everywhere and no test
    /// above would notice.
    #[tokio::test]
    async fn a_generic_endpoint_does_not_read_the_body_for_errors() {
        let server = recorder(vec![(200, r#"{"errcode":310000}"#)]).await;
        let report = deliver(&endpoint(&server.url, NotificationFormat::Generic), None, &test_alert()).await;
        assert!(report.is_success(), "{report:?}");
    }

    #[tokio::test]
    async fn a_server_error_is_retried_and_a_client_error_is_not() {
        let server = recorder(vec![(503, "later"), (200, "ok")]).await;
        let report = deliver(&endpoint(&server.url, NotificationFormat::Slack), None, &test_alert()).await;
        assert!(report.is_success(), "{report:?}");
        assert_eq!(report.attempts, 2);
        assert_eq!(server.seen.lock().unwrap().len(), 2, "the retry reached the server");

        let server = recorder(vec![(400, "no such robot")]).await;
        let report = deliver(&endpoint(&server.url, NotificationFormat::Slack), None, &test_alert()).await;
        assert!(!report.is_success());
        assert_eq!(report.status, Some(400));
        assert_eq!(report.attempts, 1, "repeating a 400 gets the same 400");
        assert_eq!(server.seen.lock().unwrap().len(), 1);
    }

    /// A company's own alert pipe, end to end: its schema in the body, its
    /// bearer token in the header, and neither of ours anywhere.
    #[tokio::test]
    async fn a_custom_endpoint_sends_its_own_schema_with_a_bearer_token() {
        let server = recorder(vec![(200, "{}")]).await;
        let mut row = endpoint(&server.url, NotificationFormat::Custom);
        row.body_template = Some(
            r#"{
                "title": "{{title}}",
                "message": "{{summary}}",
                "service": "billing",
                "requestId": "{{delivery_id}}",
                "timestamp": "{{raised_at_iso}}"
            }"#
            .into(),
        );

        let report = deliver(&row, Some("pipe-token"), &test_alert()).await;
        assert!(report.is_success(), "{report:?}");

        let seen = server.seen.lock().unwrap();
        let header = |name: &str| {
            seen[0]
                .headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(header("authorization").as_deref(), Some("Bearer pipe-token"));
        // Ours must not leak into somebody else's contract.
        assert!(
            !seen[0].headers.iter().any(|(key, _)| key.starts_with("x-meridian")),
            "our headers sign nothing this endpoint checks"
        );

        let body: serde_json::Value = serde_json::from_str(&seen[0].body).expect("a JSON body");
        assert_eq!(body["service"], "billing");
        assert_eq!(body["title"], "Meridian 通知测试");
        assert!(body["timestamp"].as_str().unwrap().ends_with('Z'), "{body}");
        assert!(body.get("spec_version").is_none(), "not our envelope: {body}");
    }

    /// Without a token it is an unauthenticated POST, not an empty `Bearer `,
    /// which a receiver rejects with a message about a malformed token rather
    /// than about there being none.
    #[tokio::test]
    async fn a_custom_endpoint_with_no_token_sends_no_authorization_header() {
        let server = recorder(vec![(200, "{}")]).await;
        let mut row = endpoint(&server.url, NotificationFormat::Custom);
        row.body_template = Some(r#"{"title": "{{title}}"}"#.into());

        assert!(deliver(&row, None, &test_alert()).await.is_success());
        let seen = server.seen.lock().unwrap();
        assert!(!seen[0].headers.iter().any(|(key, _)| key == "authorization"));
    }

    /// A `custom` row with no template would post nothing and be recorded as
    /// delivered — the worst outcome available, because it looks like it worked.
    #[tokio::test]
    async fn a_custom_endpoint_without_a_template_never_reaches_the_wire() {
        let server = recorder(vec![(200, "{}")]).await;
        let row = endpoint(&server.url, NotificationFormat::Custom);
        let report = deliver(&row, Some("t"), &test_alert()).await;

        assert!(!report.is_success());
        assert!(report.error.as_deref().unwrap().contains("body template"), "{report:?}");
        assert!(server.seen.lock().unwrap().is_empty());

        // The same for a template that will not parse: it fails rather than
        // becoming `{}`.
        let mut broken = endpoint(&server.url, NotificationFormat::Custom);
        broken.body_template = Some("{not json".into());
        let report = deliver(&broken, Some("t"), &test_alert()).await;
        assert!(!report.is_success());
        assert!(server.seen.lock().unwrap().is_empty());
    }

    /// A row whose stored format is not one of the five never reaches the wire.
    #[tokio::test]
    async fn an_unreadable_format_fails_before_any_request() {
        let server = recorder(vec![(200, "ok")]).await;
        let mut row = endpoint(&server.url, NotificationFormat::Generic);
        row.format = "teams".into();
        let report = deliver(&row, None, &test_alert()).await;
        assert!(!report.is_success());
        assert!(report.error.as_deref().unwrap().contains("teams"), "{report:?}");
        assert!(server.seen.lock().unwrap().is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::alert::AlertDetail;

    pub(super) fn test_alert() -> Alert {
        Alert {
            event: NotificationEventKind::Test,
            raised_at: 1_700_000_000_000,
            alert_key: "test".into(),
            title: "Meridian 通知测试".into(),
            summary: "如果你看到这条消息，端点配置是对的。".into(),
            detail: AlertDetail::Test(TestAlert {
                format: NotificationFormat::Generic,
            }),
        }
    }

    fn body_json(prepared: &PreparedDelivery) -> serde_json::Value {
        serde_json::from_slice(&prepared.body).expect("a JSON body")
    }

    /// A fixed vector. Without one, a change to what is signed — the separator,
    /// the timestamp, the body — passes every other test in this file and only
    /// shows up as a receiver silently dropping messages.
    #[test]
    fn the_generic_signature_is_a_fixed_vector() {
        let prepared = prepare(
            NotificationFormat::Generic,
            "https://example.invalid/hook",
            Some("s3cret"),
            None,
            &test_alert(),
            "delivery-1",
            1_700_000_000_000,
        )
        .unwrap();

        let signature = prepared
            .headers
            .iter()
            .find(|(name, _)| name == "x-meridian-signature")
            .map(|(_, value)| value.clone())
            .expect("a signature header");
        let expected = format!(
            "sha256={}",
            hmac_hex(
                b"s3cret",
                format!("1700000000000.{}", String::from_utf8(prepared.body.clone()).unwrap()).as_bytes()
            )
        );
        assert_eq!(signature, expected);
        // The timestamp is inside the signed material, not merely beside it —
        // otherwise a captured request replays for ever.
        assert!(
            hmac_hex(b"s3cret", String::from_utf8(prepared.body.clone()).unwrap().as_bytes()) != signature,
            "the body alone must not produce the same signature"
        );
    }

    /// An HMAC over an empty key is a valid signature that verifies against an
    /// empty key. Sending one says "this is signed" when it is not.
    #[test]
    fn no_secret_means_no_signature_header_at_all() {
        for secret in [None, Some("")] {
            let prepared = prepare(
                NotificationFormat::Generic,
                "https://example.invalid/hook",
                secret,
                None,
                &test_alert(),
                "d",
                1,
            )
            .unwrap();
            assert!(
                !prepared.headers.iter().any(|(name, _)| name == "x-meridian-signature"),
                "secret {secret:?} must not produce a signature"
            );
        }
    }

    #[test]
    fn the_generic_payload_carries_both_slots_with_one_of_them_null() {
        let prepared = prepare(
            NotificationFormat::Generic,
            "https://example.invalid/hook",
            None,
            None,
            &test_alert(),
            "d",
            1,
        )
        .unwrap();
        let body = body_json(&prepared);
        assert_eq!(body["spec_version"], "1");
        assert_eq!(body["event"], "test");
        assert!(body["balance"].is_null());
        assert!(body["usage"].is_null());
        assert!(body.get("test").is_some());
    }

    /// One naming convention per document, all the way down.
    ///
    /// This is the defect a smoke test found and every unit test here missed:
    /// the outer object was `camelCase` while the nested balance accounts —
    /// `BalanceAccount`, whose serialization belongs to `provider::balance` —
    /// stayed `snake_case`. Both halves were internally consistent, so every
    /// assertion written beside them agreed, and the mixed document only showed
    /// up on the wire. `snake_case` is the whole app's convention (`types.ts`
    /// says `is_available`, `total_balance`), so that is what this is.
    ///
    /// Walked recursively rather than asserted key by key, because the way this
    /// breaks is a *nested* type nobody thought about.
    #[test]
    fn every_key_in_the_payload_is_snake_case_at_every_depth() {
        fn walk(value: &serde_json::Value, path: &str) {
            match value {
                serde_json::Value::Object(map) => {
                    for (key, child) in map {
                        assert!(
                            key.chars()
                                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_'),
                            "`{path}{key}` is not snake_case; one document must not mix conventions"
                        );
                        walk(child, &format!("{path}{key}."));
                    }
                }
                serde_json::Value::Array(items) => {
                    for item in items {
                        walk(item, path);
                    }
                }
                _ => {}
            }
        }

        // Each variant in turn: the nested types differ per variant, and it was
        // exactly a nested one that drifted.
        let balance = Alert {
            event: NotificationEventKind::BalanceLow,
            raised_at: 1,
            alert_key: "balance:p1".into(),
            title: "t".into(),
            summary: "s".into(),
            detail: AlertDetail::Balance(crate::notify::BalanceAlert {
                provider_id: "p1".into(),
                provider_name: "Acme".into(),
                is_available: true,
                threshold: "5".parse().unwrap(),
                accounts: vec![crate::provider::balance::BalanceAccount {
                    currency: "CNY".into(),
                    total_balance: "3.2".parse().unwrap(),
                    granted_balance: Some("0".parse().unwrap()),
                    topped_up_balance: Some("3.2".parse().unwrap()),
                }],
            }),
        };
        let usage = Alert {
            event: NotificationEventKind::UsageSurge,
            raised_at: 1,
            alert_key: "usage_surge".into(),
            title: "t".into(),
            summary: "s".into(),
            detail: AlertDetail::Usage(crate::notify::UsageAlert {
                window_hours: 1,
                baseline_days: 7,
                multiplier: "3".parse().unwrap(),
                window_start_ms: 0,
                window_cost: "1".parse().unwrap(),
                baseline_cost: "1".parse().unwrap(),
                baseline_windows: 168,
                is_lower_bound: false,
                top_providers: vec![crate::notify::UsageSlice {
                    key: "Acme".into(),
                    label: None,
                    cost: "1".parse().unwrap(),
                    messages: 2,
                }],
                top_conversations: vec![],
            }),
        };

        for alert in [balance, usage, test_alert()] {
            let prepared = prepare(
                NotificationFormat::Generic,
                "https://example.invalid/hook",
                None,
                None,
                &alert,
                "d",
                1,
            )
            .unwrap();
            walk(&body_json(&prepared), "");
        }
    }

    /// DingTalk signs the URL, and the signed material is the timestamp and the
    /// secret — not the message.
    #[test]
    fn dingtalk_signs_the_url_with_its_own_scheme() {
        let prepared = prepare(
            NotificationFormat::Dingtalk,
            "https://oapi.dingtalk.com/robot/send?access_token=abc",
            Some("SECdead"),
            None,
            &test_alert(),
            "d",
            1_700_000_000_000,
        )
        .unwrap();

        let expected = hmac_base64(b"SECdead", b"1700000000000\nSECdead");
        let escaped = percent_encoding::utf8_percent_encode(&expected, percent_encoding::NON_ALPHANUMERIC).to_string();
        assert_eq!(
            prepared.url,
            format!("https://oapi.dingtalk.com/robot/send?access_token=abc&timestamp=1700000000000&sign={escaped}")
        );
        assert!(
            !prepared.headers.iter().any(|(name, _)| name.starts_with("x-meridian")),
            "our own headers sign nothing DingTalk checks"
        );
        assert_eq!(body_json(&prepared)["msgtype"], "markdown");
        assert!(prepared.reads_body_for_errors);
    }

    /// A URL with no query yet has to get a `?`, not a second `&`.
    #[test]
    fn dingtalk_appends_to_whichever_url_it_was_given() {
        let prepared = prepare(
            NotificationFormat::Dingtalk,
            "https://oapi.dingtalk.com/robot/send",
            Some("k"),
            None,
            &test_alert(),
            "d",
            7,
        )
        .unwrap();
        assert!(prepared.url.contains("send?timestamp=7&sign="), "{}", prepared.url);
    }

    /// Feishu inverts the HMAC — key and message swap places — and counts in
    /// seconds. Getting either wrong is a signature the robot rejects.
    #[test]
    fn feishu_signs_in_the_body_with_the_timestamp_as_the_key() {
        let prepared = prepare(
            NotificationFormat::Feishu,
            "https://open.feishu.cn/open-apis/bot/v2/hook/x",
            Some("SEC1"),
            None,
            &test_alert(),
            "d",
            1_700_000_000_999,
        )
        .unwrap();
        let body = body_json(&prepared);
        assert_eq!(body["timestamp"], "1700000000", "seconds, not milliseconds");
        assert_eq!(body["sign"], hmac_base64(b"1700000000\nSEC1", b""));
        assert_eq!(prepared.url, "https://open.feishu.cn/open-apis/bot/v2/hook/x");
    }

    #[test]
    fn an_unsigned_feishu_message_carries_neither_field() {
        let prepared = prepare(
            NotificationFormat::Feishu,
            "https://open.feishu.cn/open-apis/bot/v2/hook/x",
            None,
            None,
            &test_alert(),
            "d",
            1,
        )
        .unwrap();
        let body = body_json(&prepared);
        assert!(body.get("sign").is_none());
        assert!(body.get("timestamp").is_none());
    }

    #[test]
    fn wecom_and_slack_carry_the_text_and_no_signature() {
        let wecom = prepare(
            NotificationFormat::Wecom,
            "https://qyapi.weixin.qq.com/x?key=k",
            Some("ignored"),
            None,
            &test_alert(),
            "d",
            1,
        )
        .unwrap();
        assert_eq!(wecom.url, "https://qyapi.weixin.qq.com/x?key=k");
        assert!(
            body_json(&wecom)["markdown"]["content"]
                .as_str()
                .unwrap()
                .contains("Meridian 通知测试")
        );
        assert!(wecom.reads_body_for_errors);

        let slack = prepare(
            NotificationFormat::Slack,
            "https://hooks.slack.com/services/x",
            None,
            None,
            &test_alert(),
            "d",
            1,
        )
        .unwrap();
        assert!(
            body_json(&slack)["text"]
                .as_str()
                .unwrap()
                .contains("Meridian 通知测试")
        );
        assert!(
            !slack.reads_body_for_errors,
            "Slack refuses with a status, so the status is the answer"
        );
    }

    /// The trap this rule exists for: these three answer a rejected message
    /// with HTTP 200. Judged on the status, every rejection reads as sent.
    #[test]
    fn a_two_hundred_carrying_an_error_code_is_a_failure() {
        assert!(vendor_error(r#"{"errcode":310000,"errmsg":"sign not match"}"#).is_some());
        assert!(vendor_error(r#"{"code":19021,"msg":"sign match fail"}"#).is_some());
        assert!(vendor_error(r#"{"StatusCode":1,"StatusMessage":"nope"}"#).is_some());

        assert!(vendor_error(r#"{"errcode":0,"errmsg":"ok"}"#).is_none());
        assert!(vendor_error(r#"{"code":0}"#).is_none());
        assert!(vendor_error(r#"{"StatusCode":0,"StatusMessage":"success"}"#).is_none());
    }

    /// An answer in none of the shapes we know is left to its status code.
    /// Reading it as a failure would report a delivered message as undelivered,
    /// and the next tick would send it again.
    #[test]
    fn an_unrecognised_body_is_left_to_the_status_code() {
        assert!(vendor_error("ok").is_none());
        assert!(vendor_error("").is_none());
        assert!(vendor_error(r#"{"result":"queued"}"#).is_none());
        assert!(
            vendor_error(r#"{"errcode":"310000"}"#).is_none(),
            "a code that is not a number is not a code we can read"
        );
    }

    #[test]
    fn only_later_is_retried() {
        use http::StatusCode;
        let http = |code: u16| TransportError::Http {
            status: StatusCode::from_u16(code).unwrap(),
            url: None,
            headers: None,
            body: None,
        };
        assert!(retryable(&http(429)));
        assert!(retryable(&http(503)));
        assert!(retryable(&TransportError::Timeout));
        assert!(retryable(&TransportError::Network("reset".into())));
        assert!(!retryable(&http(400)), "the same request gets the same answer");
        assert!(!retryable(&http(404)));
        assert!(!retryable(&TransportError::Build("bad url".into())));
    }

    #[test]
    fn the_secret_name_is_derived_the_way_provider_keys_are() {
        assert_eq!(webhook_secret_name("2b0f-9c1a"), "NOTIFY_WEBHOOK_SECRET_2B0F_9C1A");
    }

    #[test]
    fn an_excerpt_is_cut_on_a_character_boundary() {
        assert_eq!(excerpt("  "), None);
        assert_eq!(excerpt(" ok "), Some("ok".into()));
        let long = "错".repeat(400);
        let cut = excerpt(&long).unwrap();
        assert_eq!(cut.chars().count(), EXCERPT_CHARS + 1);
    }
}

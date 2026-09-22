use crate::client::error::TransportError;
use crate::client::request::Request;
use rand::Rng;
use std::future::Future;
use std::time::Duration;
use tokio::time::sleep;

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u64,
    pub base_delay: Duration,
    pub retry_on: RetryOn,
}

impl Default for RetryPolicy {
    /// What a request retries on unless its builder says otherwise.
    ///
    /// **429 is deliberately absent.** A rate limit arrives with the server's
    /// own advice about when to come back — `try again in 20s` — and the only
    /// thing that reads it is `agent::stream::parse_retry_after`, one layer up.
    /// Retrying here with a sub-second backoff would spend the whole budget
    /// inside the window the server asked us to wait out, and then hand the turn
    /// loop a failure it can no longer date.
    ///
    /// Two retries rather than Codex's four (`DEFAULT_REQUEST_MAX_RETRIES`),
    /// because this budget multiplies with the turn loop's five rather than
    /// replacing it. Three attempts covers the case it exists for — a gateway
    /// that drops one connection — without a wedged upstream costing twenty
    /// requests before anyone is told.
    fn default() -> Self {
        Self {
            max_attempts: 2,
            base_delay: Duration::from_millis(400),
            retry_on: RetryOn {
                retry_429: false,
                retry_5xx: true,
                retry_transport: true,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct RetryOn {
    pub retry_429: bool,
    pub retry_5xx: bool,
    pub retry_transport: bool,
}

impl RetryOn {
    pub fn should_retry(&self, err: &TransportError, attempt: u64, max_attempts: u64) -> bool {
        if attempt >= max_attempts {
            return false;
        }
        match err {
            TransportError::Http { status, .. } => {
                (self.retry_429 && status.as_u16() == 429) || (self.retry_5xx && status.is_server_error())
            }
            TransportError::Timeout | TransportError::Network(_) => self.retry_transport,
            _ => false,
        }
    }
}

pub fn backoff(base: Duration, attempt: u64) -> Duration {
    if attempt == 0 {
        return base;
    }
    let exp = 2u64.saturating_pow(attempt as u32 - 1);
    let millis = base.as_millis() as u64;
    let raw = millis.saturating_mul(exp);
    let jitter: f64 = rand::rng().random_range(0.9..1.1);
    Duration::from_millis((raw as f64 * jitter) as u64)
}

/// The transport's own retry, for failures that never produced a reply.
///
/// **This nests inside the turn loop's five**, which is deliberate and is why
/// the budget here is small. The two layers answer different questions: the turn
/// loop re-sends a prompt after reading a *stream* that failed, and parses the
/// server's `retry-after` out of it; this one covers the request that never got
/// as far as a stream — a refused connection, a gateway's 502 — where the
/// failure is local, the answer arrives in milliseconds, and tearing down the
/// whole turn's request setup to ask again is pure latency.
///
/// It is also the only retry the non-streaming calls have ever had. The
/// summariser, the title generator and the automatic reviewer go through
/// `execute`, which sat outside the turn loop entirely: one 502 from a gateway
/// and a compaction that had already been paid for was simply lost.
pub async fn run_with_retry<T, F, Fut>(
    policy: RetryPolicy,
    mut make_req: impl FnMut() -> Request,
    op: F,
) -> Result<T, TransportError>
where
    F: Fn(Request, u64) -> Fut,
    Fut: Future<Output = Result<T, TransportError>>,
{
    for attempt in 0..=policy.max_attempts {
        let req = make_req();
        match op(req, attempt).await {
            Ok(resp) => return Ok(resp),
            Err(err) if policy.retry_on.should_retry(&err, attempt, policy.max_attempts) => {
                let delay = backoff(policy.base_delay, attempt + 1);
                // Named, never the body: a provider's error body quotes the
                // request back, and this goes to a file the user can export.
                tracing::warn!(
                    attempt = attempt + 1,
                    max_attempts = policy.max_attempts,
                    delay_ms = delay.as_millis() as u64,
                    kind = err.kind(),
                    status = err.status(),
                    "request failed before a reply; retrying"
                );
                sleep(delay).await;
            }
            Err(err) => return Err(err),
        }
    }
    Err(TransportError::RetryLimit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::StatusCode;

    #[test]
    fn test_backoff_zero() {
        let d = backoff(Duration::from_millis(100), 0);
        assert_eq!(d, Duration::from_millis(100));
    }

    #[test]
    fn test_backoff_one() {
        let d = backoff(Duration::from_millis(100), 1);
        let ms = d.as_millis() as u64;
        assert!((90..=110).contains(&ms), "expected ~100ms, got {ms}ms");
    }

    #[test]
    fn test_backoff_two() {
        let d = backoff(Duration::from_millis(100), 2);
        let ms = d.as_millis() as u64;
        assert!((180..=220).contains(&ms), "expected ~200ms, got {ms}ms");
    }

    #[test]
    fn test_backoff_no_overflow() {
        let d = backoff(Duration::from_millis(100), 64);
        assert!(d.as_millis() > 0);
    }

    fn make_retry_on(retry_429: bool, retry_5xx: bool, retry_transport: bool) -> RetryOn {
        RetryOn {
            retry_429,
            retry_5xx,
            retry_transport,
        }
    }

    fn http_err(status: u16) -> TransportError {
        TransportError::Http {
            status: StatusCode::from_u16(status).unwrap(),
            url: None,
            headers: None,
            body: None,
        }
    }

    #[test]
    fn test_should_retry_429() {
        let r = make_retry_on(true, false, false);
        assert!(r.should_retry(&http_err(429), 0, 3));
        assert!(!r.should_retry(&http_err(500), 0, 3));
    }

    #[test]
    fn test_should_retry_5xx() {
        let r = make_retry_on(false, true, false);
        assert!(r.should_retry(&http_err(500), 0, 3));
        assert!(r.should_retry(&http_err(503), 0, 3));
        assert!(!r.should_retry(&http_err(400), 0, 3));
        assert!(!r.should_retry(&http_err(429), 0, 3));
    }

    #[test]
    fn test_should_retry_transport() {
        let r = make_retry_on(false, false, true);
        assert!(r.should_retry(&TransportError::Timeout, 0, 3));
        assert!(r.should_retry(&TransportError::Network("err".into()), 0, 3));
        assert!(!r.should_retry(&http_err(500), 0, 3));
    }

    #[test]
    fn test_no_retry_max_attempts() {
        let r = make_retry_on(true, true, true);
        assert!(!r.should_retry(&http_err(429), 3, 3));
        assert!(!r.should_retry(&TransportError::Timeout, 5, 3));
    }

    #[test]
    fn test_no_retry_build_error() {
        let r = make_retry_on(true, true, true);
        assert!(!r.should_retry(&TransportError::Build("err".into()), 0, 3));
    }

    fn policy(max_attempts: u64, retry_on: RetryOn) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            base_delay: Duration::from_millis(1),
            retry_on,
        }
    }

    /// The loop actually re-issues, and the request is rebuilt each time rather
    /// than moved into the first attempt. Nothing covered this while the file
    /// was dead code.
    #[tokio::test]
    async fn a_transient_failure_is_asked_again_and_can_succeed() {
        let attempts = std::sync::atomic::AtomicU64::new(0);
        let out: Result<&str, _> = run_with_retry(
            policy(2, make_retry_on(false, true, true)),
            || Request::new(http::Method::POST, "https://e.invalid/responses".into()),
            |req, _| {
                assert_eq!(req.url, "https://e.invalid/responses", "rebuilt every attempt");
                let seen = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move { if seen < 2 { Err(http_err(503)) } else { Ok("answered") } }
            },
        )
        .await;
        assert_eq!(out.unwrap(), "answered");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    /// A rate limit is not this layer's to retry: the server's own
    /// `try again in 20s` is read one layer up, and spending three sub-second
    /// attempts inside that window loses the advice and the turn with it.
    #[tokio::test]
    async fn a_rate_limit_is_handed_straight_back() {
        let attempts = std::sync::atomic::AtomicU64::new(0);
        let out: Result<&str, _> = run_with_retry(
            policy(2, make_retry_on(false, true, true)),
            || Request::new(http::Method::POST, "https://e.invalid/responses".into()),
            |_, _| {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Err(http_err(429)) }
            },
        )
        .await;
        assert!(matches!(out, Err(TransportError::Http { .. })));
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "asked once and reported, so the delay the server named is still readable"
        );
    }

    /// **An exhausted budget hands back the last real failure, not
    /// `RetryLimit`**, and that is load-bearing rather than incidental.
    /// `should_retry` is false on the final attempt, so the `Err(err) =>`
    /// arm returns before the loop ends and the `RetryLimit` tail is
    /// unreachable — Codex's own loop has the same shape.
    ///
    /// It has to stay that way: the layer above classifies on the status in
    /// the message (`agent::stream::is_retryable_stream_error`), and
    /// `RetryLimit` renders as "retry limit reached" with no status and no
    /// network wording in it — which reads as *terminal*. Collapsing three
    /// failed 502s into that would end the turn instead of letting the turn
    /// loop replay the prompt, which is the exact shape of the bug this whole
    /// change is about.
    #[tokio::test]
    async fn an_exhausted_budget_still_reports_what_actually_failed() {
        let attempts = std::sync::atomic::AtomicU64::new(0);
        let out: Result<&str, _> = run_with_retry(
            policy(2, make_retry_on(false, true, true)),
            || Request::new(http::Method::POST, "https://e.invalid/responses".into()),
            |_, _| {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Err(http_err(502)) }
            },
        )
        .await;
        match out {
            Err(err @ TransportError::Http { .. }) => {
                assert_eq!(err.status(), Some(502));
                assert!(
                    crate::agent::is_retryable_stream_error(&err.to_string()),
                    "the status has to survive, or the turn loop stops replaying: {err}"
                );
            }
            other => panic!("expected the last failure itself, got {other:?}"),
        }
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
    }
}

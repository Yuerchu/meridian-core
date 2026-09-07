use crate::client::error::TransportError;
use crate::client::request::Request;
use rand::Rng;
use std::future::Future;
use std::time::Duration;
use tokio::time::sleep;

/// Ported policy surface nothing drives yet: providers hand-roll their loops
/// around bare `backoff`, which is the one live export of this file.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u64,
    pub base_delay: Duration,
    pub retry_on: RetryOn,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RetryOn {
    pub retry_429: bool,
    pub retry_5xx: bool,
    pub retry_transport: bool,
}

impl RetryOn {
    #[allow(dead_code)]
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

#[allow(dead_code)]
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
                sleep(backoff(policy.base_delay, attempt + 1)).await;
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
}

// parse_retry_after derived from codex-rs/codex-api/src/sse/responses.rs (Apache-2.0, OpenAI)
// NOTICE: This file contains code derived from the OpenAI Codex project.
// Changes: gate on error-string markers instead of a typed error code; clamp to 60s.

use crate::provider;

pub(crate) struct StreamResult {
    pub(crate) text: String,
    pub(crate) reasoning: String,
    pub(crate) provider_state: Option<provider::state::ProviderState>,
    pub(crate) tool_calls: Vec<provider::ToolCall>,
    pub(crate) usage: Option<provider::TokenUsage>,
    pub(crate) finish_reason: Option<String>,
    /// Whether the stream ran out on its own rather than being abandoned.
    ///
    /// `Ok` is not the same as finished. A cancelled read stops mid-answer and
    /// still returns everything it had, because that partial answer is worth
    /// keeping — so the one caller that needs to know the model actually got to
    /// the end of what it was given cannot tell from the result alone.
    ///
    /// That caller is the interrupted-turn notice: it is retired only by a
    /// reply the model finished producing, and a user pressing Stop two hundred
    /// milliseconds in is not one. `finish_reason` will not do instead — plenty
    /// of providers close the stream without ever sending a stop event.
    pub(crate) ran_to_completion: bool,
}

pub(crate) const MAX_STREAM_RETRIES: u32 = 5;
pub(crate) const STREAM_RETRY_BASE: std::time::Duration = std::time::Duration::from_millis(200);

pub(crate) fn is_context_window_error(err: &str) -> bool {
    let e = err.to_lowercase();
    e.contains("context_length_exceeded")
        || e.contains("context window")
        || e.contains("maximum context length")
        || e.contains("too many tokens")
        || e.contains("exceeds the model")
        || e.contains("status: 413")
        || e.contains("http 413")
        || e.contains("request_too_large")
        || e.contains("content_too_large")
}

pub(crate) fn is_retryable_stream_error(err: &str) -> bool {
    if is_context_window_error(err) {
        return false;
    }
    let e = err.to_lowercase();
    e.contains("timeout") || e.contains("network") || e.contains("connection")
        || e.contains("status: 429") || e.contains("status: 5")
        || e.contains("http 429") || e.contains("http 5")
        || e.contains("api error 429") || e.contains("api error 5")
        || e.contains("idle timeout")
        // A stream that stopped before it finished, said in the words the
        // gateway happens to use. Ours reaches this as a network error, but a
        // compatible provider can hand back the status and the sentence
        // directly -- and 408 read literally is a timeout that the word
        // "timeout" above only catches when the reason phrase comes with it.
        || e.contains("status: 408") || e.contains("http 408")
        || e.contains("api error 408")
        || e.contains("disconnected")
}

/// Extract a server-suggested retry delay ("try again in 20s") from a rate
/// limit error message. Only consulted for errors that look rate-limited; the
/// value comes from an untrusted response body, so it is clamped to 60s.
pub(crate) fn parse_retry_after(err: &str) -> Option<std::time::Duration> {
    const MAX_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

    let e = err.to_lowercase();
    if !(e.contains("429") || e.contains("rate limit") || e.contains("rate_limit")) {
        return None;
    }

    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?i)try again in\s*(\d+(?:\.\d+)?)\s*(s|ms|seconds?)").expect("static regex")
    });

    let captures = re.captures(err)?;
    let value = captures.get(1)?.as_str().parse::<f64>().ok()?;
    let unit = captures.get(2)?.as_str().to_ascii_lowercase();

    let delay = if unit == "s" || unit.starts_with("second") {
        std::time::Duration::from_secs_f64(value)
    } else if unit == "ms" {
        std::time::Duration::from_millis(value as u64)
    } else {
        return None;
    };
    Some(delay.min(MAX_RETRY_AFTER))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn parse_retry_after_seconds() {
        let err = "http 429: \"Rate limit reached. Please try again in 20s.\"";
        assert_eq!(parse_retry_after(err), Some(Duration::from_secs(20)));
    }

    #[test]
    fn parse_retry_after_fractional_seconds() {
        let err = "status: 429, rate limit exceeded, try again in 1.5 seconds";
        assert_eq!(parse_retry_after(err), Some(Duration::from_secs_f64(1.5)));
    }

    #[test]
    fn parse_retry_after_millis() {
        let err = "429 Too Many Requests: try again in 250 ms";
        assert_eq!(parse_retry_after(err), Some(Duration::from_millis(250)));
    }

    #[test]
    fn parse_retry_after_requires_rate_limit_marker() {
        assert_eq!(parse_retry_after("server error, try again in 20s"), None);
    }

    #[test]
    fn parse_retry_after_clamps_large_values() {
        let err = "rate limit: try again in 86400s";
        assert_eq!(parse_retry_after(err), Some(Duration::from_secs(60)));
    }

    #[test]
    fn http_status_display_classifies() {
        assert!(is_retryable_stream_error("transport: http 429: Some(\"slow down\")"));
        assert!(is_retryable_stream_error("transport: http 503: Some(\"overloaded\")"));
        assert!(is_context_window_error(
            "transport: http 413: Some(\"payload too large\")"
        ));
        assert!(!is_retryable_stream_error(
            "transport: http 413: Some(\"payload too large\")"
        ));
        assert!(!is_retryable_stream_error("http 400: bad request"));
    }

    /// A stream cut short. Our own transport reports it as a network error and
    /// was covered already; a compatible gateway hands back the status and the
    /// sentence, and "408" on its own carries no word this used to look for.
    #[test]
    fn a_stream_that_stopped_early_is_retryable_however_it_is_worded() {
        assert!(is_retryable_stream_error("http 408 stream disconnected"));
        assert!(is_retryable_stream_error(
            "api error 408: {\"message\":\"gateway gave up\"}"
        ));
        assert!(is_retryable_stream_error("status: 408"));
        assert!(is_retryable_stream_error(
            "stream error: stream disconnected before completion"
        ));
        // And still not the answers that would say the same thing every time.
        assert!(!is_retryable_stream_error("http 401: invalid api key"));
        assert!(!is_retryable_stream_error("http 404: no such model"));
    }
}

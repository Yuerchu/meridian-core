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
    /// The model the upstream reported it used. `None` when the provider did
    /// not say.
    pub(crate) response_model: Option<String>,
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

impl StreamResult {
    /// A round given up before anything was read: Stop pressed during a retry
    /// wait.
    pub(crate) fn abandoned() -> Self {
        Self {
            text: String::new(),
            reasoning: String::new(),
            provider_state: None,
            tool_calls: Vec::new(),
            usage: None,
            finish_reason: None,
            response_model: None,
            ran_to_completion: false,
        }
    }
}

pub(crate) const MAX_STREAM_RETRIES: u32 = 5;
pub(crate) const STREAM_RETRY_BASE: std::time::Duration = std::time::Duration::from_millis(200);

/// How many times, and how far apart, a retryable failure is tried again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RetryLadder {
    pub(crate) max_retries: u32,
    base: std::time::Duration,
}

impl RetryLadder {
    /// The turn loop's ladder for a dropped connection or a gateway's 502:
    /// failures that clear in milliseconds.
    pub(crate) const STREAM: Self = Self {
        max_retries: MAX_STREAM_RETRIES,
        base: STREAM_RETRY_BASE,
    };

    /// An upstream that says it is overloaded stays overloaded for tens of
    /// seconds to minutes. The ordinary ladder spent its five retries in about
    /// six seconds — measured against `server_is_overloaded`, every one of
    /// them landed inside the same overload and the turn failed anyway. This
    /// one waits 2, 4, 8, 16, 32, then 60 seconds three times: about four
    /// minutes, each wait announced to the window and cut short by Stop.
    pub(crate) const OVERLOAD: Self = Self {
        max_retries: 8,
        base: std::time::Duration::from_secs(2),
    };

    const MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

    pub(crate) const fn new(max_retries: u32, base: std::time::Duration) -> Self {
        Self { max_retries, base }
    }

    /// `self` unless the failure is an overload, which gets the long ladder.
    pub(crate) fn for_error(self, err: &str) -> Self {
        if is_overload_error(err) { Self::OVERLOAD } else { self }
    }

    /// The wait before retry number `attempt`, counted from 1.
    pub(crate) fn delay(&self, attempt: u32) -> std::time::Duration {
        let nominal = self.nominal_delay(attempt);
        // A turn test that waits out an overload would otherwise take four
        // real minutes, and tokio's paused clock is no way round it: it
        // auto-advances while SQLite works on a blocking thread and fires the
        // pool's acquire timeout instead. The schedule itself is pinned
        // against `nominal_delay`.
        if cfg!(test) { nominal / 1000 } else { nominal }
    }

    fn nominal_delay(&self, attempt: u32) -> std::time::Duration {
        crate::client::backoff(self.base, attempt as u64).min(Self::MAX_WAIT)
    }
}

/// The server blaming its own capacity: 503, Anthropic's 529, or the word
/// itself from a relay that reports it some other way.
pub(crate) fn is_overload_error(err: &str) -> bool {
    matches!(status_in(err), Some(503 | 529)) || err.to_lowercase().contains("overloaded")
}

pub(crate) fn is_context_window_error(err: &str) -> bool {
    // 413 however it is worded, including `API error 413`, which the Responses
    // adapter now produces for `context_length_exceeded` and which the two
    // literals below do not match.
    if status_in(err) == Some(413) {
        return true;
    }
    let e = err.to_lowercase();
    e.contains("context_length_exceeded")
        || e.contains("context window")
        || e.contains("maximum context length")
        || e.contains("too many tokens")
        || e.contains("exceeds the model")
        || e.contains("request_too_large")
        || e.contains("content_too_large")
}

/// The status an error string carries, whatever wording put it there.
///
/// Three producers write one into their `Display` and they agree on nothing but
/// the number: `ProviderError::Api` says `API error 503: …`,
/// `TransportError::Http` says `transport: http 503: …`, and a gateway that
/// echoes its own reason line says `status: 503`. Enumerating each as a literal
/// substring is what this replaces, and the enumeration was the defect rather
/// than an untidiness: the list held 408, 429 and `5` as a *prefix*, so it
/// happened to cover 5xx and would have missed anything else nobody had thought
/// to type. What actually escaped through it was a status this adapter was
/// inventing — a `server_is_overloaded` reported as 400 — and no amount of
/// adding literals to that list would have caught it, because 400 was in the
/// string on purpose.
///
/// Anchored on the three prefixes rather than matching a bare three-digit run:
/// an error body quotes the request back, and `{"max_output_tokens": 512}` has
/// a number in it that is not a status.
fn status_in(err: &str) -> Option<u16> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"(?i)(?:api error|http|status:)\s*(\d{3})").expect("static regex"));
    re.captures(err)?.get(1)?.as_str().parse().ok()
}

pub(crate) fn is_retryable_stream_error(err: &str) -> bool {
    if is_context_window_error(err) {
        return false;
    }
    // A status settles it on its own. The rule is Codex's
    // (`ext/guardian-reviewer/src/retry.rs`): a timeout, a rate limit, or
    // anything the server blames on itself. Everything else -- 401 on a stale
    // key, 404 on a model that is not there, 400 on a body we composed wrongly
    // -- returns the identical answer to the identical request, so the retry
    // budget buys nothing but a longer wait before the same red bubble.
    if let Some(status) = status_in(err) {
        return matches!(status, 408 | 429 | 500..=599);
    }
    // No status: the failure never reached a response. Our own transport
    // reports a stream that stopped early as a network error; a gateway may
    // say it in words instead.
    let e = err.to_lowercase();
    e.contains("timeout") || e.contains("network") || e.contains("connection") || e.contains("disconnected")
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
    fn an_overload_gets_the_long_ladder_and_nothing_else_does() {
        let overloaded = "API error 503: server_is_overloaded: Our servers are currently overloaded.";
        assert_eq!(RetryLadder::STREAM.for_error(overloaded), RetryLadder::OVERLOAD);
        assert_eq!(
            RetryLadder::STREAM.for_error("transport: http 529: busy"),
            RetryLadder::OVERLOAD
        );
        assert_eq!(
            RetryLadder::STREAM.for_error("API error 502: bad gateway"),
            RetryLadder::STREAM
        );
        assert_eq!(
            RetryLadder::STREAM.for_error("network: connection reset"),
            RetryLadder::STREAM
        );
    }

    #[test]
    fn the_overload_ladder_outlasts_minutes_and_no_wait_exceeds_one() {
        let waits: Vec<_> = (1..=RetryLadder::OVERLOAD.max_retries)
            .map(|a| RetryLadder::OVERLOAD.nominal_delay(a))
            .collect();
        let total: Duration = waits.iter().sum();
        assert!(total >= Duration::from_secs(180), "{total:?}");
        assert!(waits.iter().all(|w| *w <= Duration::from_secs(60)), "{waits:?}");
    }

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

    /// The failure this classifier was rewritten for. An upstream saying it is
    /// overloaded reaches here through the Responses adapter, which used to
    /// stamp every `response.failed` with 400 -- and 400 is precisely the
    /// status that means "asking again cannot help", so the turn ended on its
    /// first attempt with a message blaming our own request.
    #[test]
    fn an_overloaded_upstream_is_retried_rather_than_read_as_our_mistake() {
        assert!(is_retryable_stream_error(
            "API error 503: server_is_overloaded: Our servers are currently overloaded. Please try again later."
        ));
        // And the codes that share its shape but not its answer: the same body
        // sent again gets the same refusal, so the budget must not be spent.
        for terminal in [
            "API error 400: invalid_request: Unsupported parameter: max_output_tokens",
            "API error 402: insufficient_quota: You exceeded your current quota",
            "API error 400: cyber_policy: flagged",
        ] {
            assert!(!is_retryable_stream_error(terminal), "{terminal}");
        }
    }

    /// A status settles it, and the number is read rather than looked up in a
    /// list of spellings. 502 and 504 were never in that list; nothing but the
    /// accident of `5` as a prefix covered them.
    #[test]
    fn a_status_is_read_out_of_every_wording_it_arrives_in() {
        assert_eq!(status_in("API error 503: x"), Some(503));
        assert_eq!(status_in("transport: http 502: Some(\"bad gateway\")"), Some(502));
        assert_eq!(status_in("status: 504"), Some(504));
        assert_eq!(status_in("stream disconnected before completion"), None);
        // A body quotes the request back, and a number in it is not a status.
        assert_eq!(status_in("API error 429: {\"max_output_tokens\":512}"), Some(429));
        assert_eq!(status_in("{\"max_output_tokens\":512}"), None);
    }

    /// 413 now arrives as `API error 413` as well, which the two literals this
    /// replaced did not match -- and reading it as an ordinary failure would
    /// retry an oversized prompt five times instead of compacting it once.
    #[test]
    fn an_overflowing_prompt_is_recognised_through_the_adapter_status_too() {
        let err = "API error 413: context_length_exceeded: too long";
        assert!(is_context_window_error(err));
        assert!(!is_retryable_stream_error(err));
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

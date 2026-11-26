//! HTTP transport surface re-exported from `meridian-provider-sdk`.

pub use meridian_provider_sdk::client::*;

#[cfg(test)]
mod retry_integration_tests {
    use super::*;
    use http::StatusCode;

    /// Ensure an exhausted transport retry error still formats in a way the
    /// agent turn loop classifies as retryable.
    #[test]
    fn transport_502_display_stays_retryable_by_turn_loop() {
        let err = TransportError::Http {
            status: StatusCode::BAD_GATEWAY,
            url: None,
            headers: None,
            body: None,
        };
        assert!(
            crate::agent::is_retryable_stream_error(&err.to_string()),
            "the status has to survive, or the turn loop stops replaying: {err}"
        );
    }
}

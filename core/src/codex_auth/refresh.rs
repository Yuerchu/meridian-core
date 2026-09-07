//! Exchanging a refresh token for a fresh session.
//!
//! Ported from `codex-rs/login/src/auth/manager.rs` (Apache-2.0, OpenAI).
//!
//! The classification here is the part that matters. A refresh can fail because
//! the network is unhappy, in which case trying again later is right; or because
//! the token is finished, in which case trying again is not only useless but
//! actively harmful — a reused refresh token is how a backend decides a session
//! has been stolen. Telling those apart is what keeps a flaky connection from
//! logging the user out of their CLI.

use serde::{Deserialize, Serialize};

/// The OAuth client the Codex CLI is registered as.
///
/// See the note in `super`: an app-side login can only be made as this client,
/// because the redirect URIs and the client id are fixed on the server. It is
/// overridable for the same reason the CLI makes it overridable — a staging
/// backend has a different one.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CLIENT_ID_ENV: &str = "CODEX_APP_SERVER_LOGIN_CLIENT_ID";

pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const TOKEN_URL_ENV: &str = "CODEX_REFRESH_TOKEN_URL_OVERRIDE";

pub fn client_id() -> String {
    std::env::var(CLIENT_ID_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| CLIENT_ID.to_string())
}

pub fn token_url() -> String {
    std::env::var(TOKEN_URL_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| TOKEN_URL.to_string())
}

/// JSON, not form encoding — the refresh endpoint differs from the authorization
/// code exchange in exactly this way, and sending the wrong one is a 400 that
/// says nothing useful.
#[derive(Debug, Serialize)]
pub struct RefreshRequest {
    pub client_id: String,
    pub grant_type: &'static str,
    pub refresh_token: String,
}

impl RefreshRequest {
    pub fn new(refresh_token: String) -> Self {
        Self {
            client_id: client_id(),
            grant_type: "refresh_token",
            refresh_token,
        }
    }
}

/// Every field is optional because the backend may rotate only some of them —
/// a response that keeps the existing refresh token simply omits it, and
/// overwriting it with an empty string would end the session.
#[derive(Debug, Default, Deserialize)]
pub struct RefreshResponse {
    pub id_token: Option<String>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
}

/// Why a refresh failed, and therefore what to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshFailure {
    /// The session is over. Nothing but a new login will fix it, and retrying
    /// makes things worse.
    Permanent(PermanentReason),
    /// Something went wrong that says nothing about the token. Try later.
    Transient(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermanentReason {
    Expired,
    /// Presented twice. Backends treat this as evidence of a stolen token and
    /// invalidate the whole chain, which is why a blind retry is dangerous —
    /// see the guarded reload in `super`.
    Reused,
    Invalidated,
    /// A 401 with a code we do not recognise. Still permanent: the backend has
    /// refused this credential, and the honest reading of that is that it is
    /// finished.
    Unknown,
}

impl RefreshFailure {
    pub fn message(&self) -> String {
        match self {
            Self::Permanent(PermanentReason::Expired) => "The Codex login has expired. Run `codex login` again.".into(),
            Self::Permanent(PermanentReason::Reused) => {
                "The Codex login could not be renewed and has been ended for safety. \
                 Run `codex login` again."
                    .into()
            }
            Self::Permanent(PermanentReason::Invalidated) => {
                "The Codex login was revoked. Run `codex login` again.".into()
            }
            Self::Permanent(PermanentReason::Unknown) => "The Codex login was refused. Run `codex login` again.".into(),
            Self::Transient(detail) => format!("Could not reach the login service: {detail}"),
        }
    }

    pub fn is_permanent(&self) -> bool {
        matches!(self, Self::Permanent(_))
    }
}

/// Read the failure out of a response.
///
/// A recognised code is permanent whatever the status; an unrecognised one is
/// permanent only on a 401. That asymmetry is deliberate — a 500 with an
/// unfamiliar body is the backend having a bad day, not the token being spent,
/// and treating it as permanent would sign the user out over a blip.
pub fn classify(status: u16, body: &str) -> RefreshFailure {
    match error_code(body).as_deref() {
        Some("refresh_token_expired") => RefreshFailure::Permanent(PermanentReason::Expired),
        Some("refresh_token_reused") => RefreshFailure::Permanent(PermanentReason::Reused),
        Some("refresh_token_invalidated") => RefreshFailure::Permanent(PermanentReason::Invalidated),
        _ if status == 401 => RefreshFailure::Permanent(PermanentReason::Unknown),
        _ => RefreshFailure::Transient(format!("HTTP {status}")),
    }
}

/// The error code, from either shape the endpoint uses: `{"error": "code"}` and
/// `{"error": {"code": "code"}}`.
fn error_code(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let error = value.get("error")?;
    let code = match error {
        serde_json::Value::String(code) => code.clone(),
        serde_json::Value::Object(map) => map.get("code")?.as_str()?.to_string(),
        _ => return None,
    };
    Some(code.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both shapes the endpoint has been seen to use, and case-insensitively —
    /// a classification that misses turns a dead session into an endless retry.
    #[test]
    fn a_known_code_is_permanent_in_either_shape() {
        for body in [
            r#"{"error":"refresh_token_expired"}"#,
            r#"{"error":{"code":"refresh_token_expired"}}"#,
            r#"{"error":{"code":"REFRESH_TOKEN_EXPIRED"}}"#,
        ] {
            assert_eq!(
                classify(400, body),
                RefreshFailure::Permanent(PermanentReason::Expired),
                "{body}"
            );
        }
    }

    #[test]
    fn the_three_permanent_reasons_are_told_apart() {
        let cases = [
            ("refresh_token_expired", PermanentReason::Expired),
            ("refresh_token_reused", PermanentReason::Reused),
            ("refresh_token_invalidated", PermanentReason::Invalidated),
        ];
        for (code, reason) in cases {
            let body = format!(r#"{{"error":{{"code":"{code}"}}}}"#);
            assert_eq!(classify(400, &body), RefreshFailure::Permanent(reason));
        }
    }

    /// A server having a bad day must not end the session. This is the case that
    /// decides whether a flaky connection logs the user out of their CLI.
    #[test]
    fn an_unfamiliar_server_error_is_transient() {
        for status in [500, 502, 503, 429] {
            assert!(
                matches!(classify(status, "upstream unavailable"), RefreshFailure::Transient(_)),
                "HTTP {status} should be worth retrying"
            );
        }
    }

    /// A 401 is the backend refusing this credential. Retrying cannot help, and
    /// with a reused token it actively harms.
    #[test]
    fn an_unrecognised_refusal_is_still_permanent() {
        assert_eq!(classify(401, "{}"), RefreshFailure::Permanent(PermanentReason::Unknown));
        assert_eq!(
            classify(401, "not json at all"),
            RefreshFailure::Permanent(PermanentReason::Unknown)
        );
    }

    #[test]
    fn a_body_with_no_code_yields_nothing_to_classify_on() {
        assert_eq!(error_code(""), None);
        assert_eq!(error_code("plain text"), None);
        assert_eq!(error_code(r#"{"message":"nope"}"#), None);
        assert_eq!(error_code(r#"{"error":123}"#), None);
    }

    /// Every permanent failure has to tell the user how to fix it, and no
    /// message may leak the token.
    #[test]
    fn permanent_failures_say_how_to_recover() {
        for reason in [
            PermanentReason::Expired,
            PermanentReason::Reused,
            PermanentReason::Invalidated,
            PermanentReason::Unknown,
        ] {
            let failure = RefreshFailure::Permanent(reason);
            assert!(failure.is_permanent());
            assert!(failure.message().contains("codex login"), "{reason:?}");
        }
        assert!(!RefreshFailure::Transient("x".into()).is_permanent());
    }

    /// The request is JSON with these three fields — the refresh endpoint
    /// differs from the code exchange here, and getting it wrong is an
    /// unhelpful 400.
    #[test]
    fn the_request_carries_the_grant_type_and_client() {
        let body = serde_json::to_value(RefreshRequest::new("refresh-1".into())).unwrap();
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["refresh_token"], "refresh-1");
        assert_eq!(body["client_id"], client_id());
    }

    /// A response that rotates only some fields must not blank the others.
    #[test]
    fn a_partial_response_leaves_the_rest_alone() {
        let parsed: RefreshResponse = serde_json::from_str(r#"{"access_token":"new"}"#).unwrap();
        assert_eq!(parsed.access_token.as_deref(), Some("new"));
        assert_eq!(parsed.refresh_token, None, "absent means unchanged, not empty");
        assert_eq!(parsed.id_token, None);
    }
}

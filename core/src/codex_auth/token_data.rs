//! The ChatGPT tokens `codex login` leaves on disk, and the claims inside them.
//!
//! Ported from `codex-rs/login/src/token_data.rs` (Apache-2.0, OpenAI), trimmed
//! to what this app reads. The one substantive simplification is `PlanType`:
//! upstream models it as a closed enum of known plans plus an unknown arm, and
//! everything we do with it is print it, so it stays a `String` here.
//!
//! **Signatures are not verified, and must not be relied on.** These claims say
//! what the token *claims* about itself; the only thing that decides whether it
//! works is the upstream accepting it. They are read for display — which account
//! is this, what plan is it on — and to know when to refresh.

use base64::Engine;
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// What `auth.json` holds under `tokens`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TokenData {
    /// On disk this is the raw JWT string; in memory it is the parsed claims.
    /// The custom pair below is what bridges the two, and `raw_jwt` is what
    /// gets written back — so a round trip through this type is byte-identical.
    #[serde(deserialize_with = "deserialize_id_token", serialize_with = "serialize_id_token")]
    pub id_token: IdTokenInfo,
    /// Also a JWT, and the one that actually authorises a request.
    pub access_token: String,
    pub refresh_token: String,
    /// Which workspace the tokens are for. Absent on older logins, where
    /// `id_token.chatgpt_account_id` is the fallback.
    pub account_id: Option<String>,
}

/// The claims worth keeping out of the id token.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct IdTokenInfo {
    pub email: Option<String>,
    /// `plus`, `pro`, `team`… whatever the backend called it. Kept verbatim
    /// rather than mapped to an enum: this is shown to a person, and a plan
    /// name we have not heard of should display rather than vanish.
    pub chatgpt_plan_type: Option<String>,
    pub chatgpt_user_id: Option<String>,
    pub chatgpt_account_id: Option<String>,
    /// Whether this workspace must be reached through the FedRAMP edge, which
    /// is an extra request header rather than a different endpoint.
    #[serde(default)]
    pub chatgpt_account_is_fedramp: bool,
    /// The token exactly as it was on disk. Writing anything else back would
    /// invalidate it.
    pub raw_jwt: String,
}

#[derive(Debug, thiserror::Error)]
pub enum JwtError {
    #[error("not a JWT: expected three non-empty dot-separated parts")]
    InvalidFormat,
    #[error("JWT payload is not valid base64: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("JWT payload is not the JSON we expect: {0}")]
    Json(#[from] serde_json::Error),
}

/// The claims that matter live under namespaced keys rather than at the top
/// level — that is how Auth0-style custom claims are addressed, and the URL is a
/// name, not something that gets fetched. The literals are spelled out in the
/// `rename` attributes because serde requires them there.
#[derive(Deserialize)]
struct IdClaims {
    #[serde(default)]
    email: Option<String>,
    #[serde(rename = "https://api.openai.com/profile", default)]
    profile: Option<ProfileClaims>,
    #[serde(rename = "https://api.openai.com/auth", default)]
    auth: Option<AuthClaims>,
}

#[derive(Deserialize)]
struct ProfileClaims {
    #[serde(default)]
    email: Option<String>,
}

#[derive(Deserialize)]
struct AuthClaims {
    #[serde(default)]
    chatgpt_plan_type: Option<String>,
    #[serde(default)]
    chatgpt_user_id: Option<String>,
    /// The older spelling. Read as a fallback so a login from an earlier CLI
    /// still identifies its user.
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default)]
    chatgpt_account_is_fedramp: bool,
}

#[derive(Deserialize)]
struct ExpiryClaims {
    #[serde(default)]
    exp: Option<i64>,
}

/// Read a JWT's payload without checking its signature.
///
/// Deliberate: we are not the party the token is presented to, and have no key
/// to check it with. See the module header.
fn decode_payload<T: DeserializeOwned>(jwt: &str) -> Result<T, JwtError> {
    let mut parts = jwt.split('.');
    let payload = match (parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s)) if !h.is_empty() && !p.is_empty() && !s.is_empty() => p,
        _ => return Err(JwtError::InvalidFormat),
    };
    // URL-safe and unpadded, which is what JWT specifies — the standard
    // alphabet would reject a token containing `-` or `_`.
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload)?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn parse_id_token(jwt: &str) -> Result<IdTokenInfo, JwtError> {
    let claims: IdClaims = decode_payload(jwt)?;
    // The profile claim is where the email lives on newer tokens; the top-level
    // one is the older place. Either is fine and neither is required.
    let email = claims.email.or_else(|| claims.profile.and_then(|p| p.email));
    let Some(auth) = claims.auth else {
        // A token with no auth claim is still usable — it just tells us nothing
        // about the account, which is a thinner status card and not an error.
        return Ok(IdTokenInfo {
            email,
            raw_jwt: jwt.to_string(),
            ..Default::default()
        });
    };
    Ok(IdTokenInfo {
        email,
        raw_jwt: jwt.to_string(),
        chatgpt_plan_type: auth.chatgpt_plan_type,
        chatgpt_user_id: auth.chatgpt_user_id.or(auth.user_id),
        chatgpt_account_id: auth.chatgpt_account_id,
        chatgpt_account_is_fedramp: auth.chatgpt_account_is_fedramp,
    })
}

/// When a token stops being accepted, if it says.
///
/// `Ok(None)` means the token carries no `exp`, which is not an error and not a
/// licence to treat it as fresh — the caller falls back to an age check.
pub fn expires_at(jwt: &str) -> Result<Option<DateTime<Utc>>, JwtError> {
    let claims: ExpiryClaims = decode_payload(jwt)?;
    Ok(claims.exp.and_then(|exp| DateTime::<Utc>::from_timestamp(exp, 0)))
}

fn deserialize_id_token<'de, D>(deserializer: D) -> Result<IdTokenInfo, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    parse_id_token(&raw).map_err(serde::de::Error::custom)
}

fn serialize_id_token<S>(info: &IdTokenInfo, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&info.raw_jwt)
}

impl TokenData {
    /// Which workspace to bill against, preferring the value stored beside the
    /// tokens over the one inside the id token — a login that names it
    /// explicitly is the more recent statement.
    pub fn account_id(&self) -> Option<&str> {
        self.account_id
            .as_deref()
            .or(self.id_token.chatgpt_account_id.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    /// The same keys the `rename` attributes above use. Kept beside the tests
    /// that build fixtures with them so a change to either is visibly a change
    /// to both.
    const AUTH_CLAIM: &str = "https://api.openai.com/auth";
    const PROFILE_CLAIM: &str = "https://api.openai.com/profile";

    /// A JWT with the given payload and a signature that is never looked at.
    fn jwt(payload: serde_json::Value) -> String {
        let head = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let body = URL_SAFE_NO_PAD.encode(payload.to_string());
        format!("{head}.{body}.not-a-real-signature")
    }

    #[test]
    fn claims_come_out_of_the_namespaced_key() {
        let token = jwt(serde_json::json!({
            AUTH_CLAIM: {
                "chatgpt_plan_type": "pro",
                "chatgpt_user_id": "user-1",
                "chatgpt_account_id": "acct-1",
                "chatgpt_account_is_fedramp": true,
            },
            PROFILE_CLAIM: { "email": "someone@example.com" },
        }));
        let info = parse_id_token(&token).unwrap();
        assert_eq!(info.chatgpt_plan_type.as_deref(), Some("pro"));
        assert_eq!(info.chatgpt_user_id.as_deref(), Some("user-1"));
        assert_eq!(info.chatgpt_account_id.as_deref(), Some("acct-1"));
        assert!(info.chatgpt_account_is_fedramp);
        assert_eq!(info.email.as_deref(), Some("someone@example.com"));
        assert_eq!(info.raw_jwt, token, "the token is kept verbatim for writing back");
    }

    /// A plan name we have not heard of has to survive: it is shown to a person,
    /// and mapping it to an enum would turn a new plan into a blank field.
    #[test]
    fn an_unknown_plan_is_kept_as_written() {
        let token = jwt(serde_json::json!({ AUTH_CLAIM: { "chatgpt_plan_type": "some_new_tier" } }));
        assert_eq!(
            parse_id_token(&token).unwrap().chatgpt_plan_type.as_deref(),
            Some("some_new_tier")
        );
    }

    /// The older spelling still identifies its user.
    #[test]
    fn the_legacy_user_id_is_a_fallback() {
        let token = jwt(serde_json::json!({ AUTH_CLAIM: { "user_id": "legacy-1" } }));
        assert_eq!(
            parse_id_token(&token).unwrap().chatgpt_user_id.as_deref(),
            Some("legacy-1")
        );
    }

    /// Top-level email, for tokens issued before the profile claim existed.
    #[test]
    fn a_top_level_email_is_read_too() {
        let token = jwt(serde_json::json!({ "email": "old@example.com" }));
        assert_eq!(
            parse_id_token(&token).unwrap().email.as_deref(),
            Some("old@example.com")
        );
    }

    /// No auth claim is a thinner status card, not a failure — the token may
    /// still be perfectly usable.
    #[test]
    fn a_token_with_no_claims_still_parses() {
        let info = parse_id_token(&jwt(serde_json::json!({}))).unwrap();
        assert_eq!(info.chatgpt_plan_type, None);
        assert!(!info.chatgpt_account_is_fedramp);
    }

    #[test]
    fn junk_is_rejected_rather_than_guessed_at() {
        for bad in ["", "one.two", "...", "a.b", "not-a-jwt"] {
            assert!(parse_id_token(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn expiry_is_read_when_present_and_absent_otherwise() {
        let with = jwt(serde_json::json!({ "exp": 1_700_000_000 }));
        assert_eq!(expires_at(&with).unwrap().unwrap().timestamp(), 1_700_000_000);
        assert_eq!(expires_at(&jwt(serde_json::json!({}))).unwrap(), None);
    }

    /// The round trip is what protects the user's login: the id token goes back
    /// to disk as the string it arrived as, not as a re-encoding of the claims
    /// we happened to understand.
    #[test]
    fn the_id_token_is_written_back_verbatim() {
        let raw = jwt(serde_json::json!({ AUTH_CLAIM: { "chatgpt_plan_type": "plus" } }));
        let stored = serde_json::json!({
            "id_token": raw,
            "access_token": "access",
            "refresh_token": "refresh",
            "account_id": "acct-1",
        });
        let parsed: TokenData = serde_json::from_value(stored.clone()).unwrap();
        assert_eq!(parsed.id_token.chatgpt_plan_type.as_deref(), Some("plus"));
        assert_eq!(serde_json::to_value(&parsed).unwrap(), stored);
    }

    /// The stored account wins over the one inside the token, and the token is
    /// the fallback for logins that predate the field.
    #[test]
    fn the_account_beside_the_tokens_wins() {
        let raw = jwt(serde_json::json!({ AUTH_CLAIM: { "chatgpt_account_id": "from-token" } }));
        let mut data = TokenData {
            id_token: parse_id_token(&raw).unwrap(),
            access_token: "a".into(),
            refresh_token: "r".into(),
            account_id: Some("beside".into()),
        };
        assert_eq!(data.account_id(), Some("beside"));
        data.account_id = None;
        assert_eq!(data.account_id(), Some("from-token"));
    }
}

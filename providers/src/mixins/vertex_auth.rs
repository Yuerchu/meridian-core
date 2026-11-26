//! Reusable Google Cloud Vertex AI Service Account OAuth2 bearer auth mixin.
//!
//! Shared across Vertex-hosted protocols (Google Interactions today, and
//! Vertex Anthropic / Vertex GenerateContent as they are added) so service
//! account JWT signing and token caching live in exactly one place.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use bytes::Bytes;
use ring::digest;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use meridian_provider_sdk::chat::ProviderError;
use meridian_provider_sdk::client::{HttpTransport, Request, RequestBody, ReqwestTransport};
use meridian_provider_sdk::wire::auth_header_value;

const VERTEX_CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const DEFAULT_GOOGLE_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const VERTEX_OAUTH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VertexTokenCacheKey {
    pub token_uri: String,
    pub client_email: String,
    pub private_key_sha256: [u8; 32],
}

impl VertexTokenCacheKey {
    pub fn from_credentials(credentials: &VertexServiceAccountCredentials) -> Self {
        let token_uri = credentials
            .token_uri
            .as_deref()
            .filter(|uri| !uri.trim().is_empty())
            .unwrap_or(DEFAULT_GOOGLE_TOKEN_URI)
            .to_string();
        let digest = digest::digest(&digest::SHA256, credentials.private_key.as_bytes());
        let mut private_key_sha256 = [0u8; 32];
        private_key_sha256.copy_from_slice(digest.as_ref());
        Self {
            token_uri,
            client_email: credentials.client_email.clone(),
            private_key_sha256,
        }
    }
}

#[derive(Clone, Deserialize)]
pub struct VertexServiceAccountCredentials {
    #[serde(rename = "type")]
    pub credential_type: String,
    pub project_id: String,
    pub client_email: String,
    pub private_key: String,
    pub token_uri: Option<String>,
}

#[derive(Clone)]
struct CachedAccessToken {
    access_token: String,
    expires_at: u64,
}

#[derive(Serialize)]
struct ServiceAccountClaims<'a> {
    iss: &'a str,
    scope: &'static str,
    aud: &'a str,
    iat: u64,
    exp: u64,
}

#[derive(Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    expires_in: Option<u64>,
}

fn vertex_token_cache() -> &'static Mutex<HashMap<VertexTokenCacheKey, CachedAccessToken>> {
    static CACHE: OnceLock<Mutex<HashMap<VertexTokenCacheKey, CachedAccessToken>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn parse_vertex_service_account(raw: &str) -> Result<VertexServiceAccountCredentials, String> {
    let credentials: VertexServiceAccountCredentials =
        serde_json::from_str(raw.trim()).map_err(|error| format!("invalid Vertex Service Account JSON: {error}"))?;
    if credentials.credential_type != "service_account" {
        return Err("Vertex credential JSON must have type `service_account`".into());
    }
    if credentials.project_id.trim().is_empty() {
        return Err("Vertex Service Account JSON is missing project_id".into());
    }
    if credentials.client_email.trim().is_empty() {
        return Err("Vertex Service Account JSON is missing client_email".into());
    }
    if credentials.private_key.trim().is_empty() {
        return Err("Vertex Service Account JSON is missing private_key".into());
    }
    Ok(credentials)
}

/// Composable Vertex AI Service Account OAuth2 bearer auth mixin.
#[derive(Clone)]
pub struct VertexServiceAccountAuthMixin {
    credentials: VertexServiceAccountCredentials,
    cache_key: VertexTokenCacheKey,
}

impl VertexServiceAccountAuthMixin {
    pub fn from_json(service_account_json: &str) -> Result<Self, String> {
        let credentials = parse_vertex_service_account(service_account_json)?;
        let cache_key = VertexTokenCacheKey::from_credentials(&credentials);
        Ok(Self { credentials, cache_key })
    }

    pub fn project_id(&self) -> &str {
        &self.credentials.project_id
    }

    pub fn credentials(&self) -> &VertexServiceAccountCredentials {
        &self.credentials
    }

    pub fn cache_key(&self) -> &VertexTokenCacheKey {
        &self.cache_key
    }

    /// Mint or reuse a cached OAuth2 bearer token and attach it to `request`.
    pub async fn apply_bearer(&self, request: &mut Request) -> Result<(), ProviderError> {
        let token = self.access_token().await?;
        request.headers.insert(
            http::header::AUTHORIZATION,
            auth_header_value(&format!("Bearer {token}")),
        );
        Ok(())
    }

    pub async fn access_token(&self) -> Result<String, ProviderError> {
        let now = unix_timestamp()?;
        {
            let cached = vertex_token_cache().lock().await;
            if let Some(token) = cached.get(&self.cache_key).filter(|token| token.expires_at > now + 60) {
                return Ok(token.access_token.clone());
            }
        }

        let assertion = service_account_assertion(&self.credentials, now)?;
        let token_uri = self.cache_key.token_uri.as_str();
        let form_body = format!(
            "grant_type={}&assertion={}",
            percent_encoding::utf8_percent_encode(
                "urn:ietf:params:oauth:grant-type:jwt-bearer",
                percent_encoding::NON_ALPHANUMERIC
            ),
            percent_encoding::utf8_percent_encode(&assertion, percent_encoding::NON_ALPHANUMERIC)
        );
        let mut request = Request::new(http::Method::POST, token_uri.to_string());
        request.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        request.body = Some(RequestBody::Raw(Bytes::from(form_body)));
        request.timeout = Some(VERTEX_OAUTH_TIMEOUT);

        let response = ReqwestTransport::shared().execute(request).await?;
        let token: OAuthTokenResponse = serde_json::from_slice(&response.body)
            .map_err(|error| ProviderError::Parse(format!("invalid Vertex OAuth token response: {error}")))?;
        if token.access_token.trim().is_empty() {
            return Err(ProviderError::Parse(
                "Vertex OAuth token response had no access_token".into(),
            ));
        }
        let expires_in = token.expires_in.unwrap_or(3600);
        let expires_at = now.saturating_add(expires_in);
        let mut cached = vertex_token_cache().lock().await;
        cached.insert(
            self.cache_key.clone(),
            CachedAccessToken {
                access_token: token.access_token.clone(),
                expires_at,
            },
        );
        Ok(token.access_token)
    }
}

pub fn service_account_assertion(
    credentials: &VertexServiceAccountCredentials,
    now: u64,
) -> Result<String, ProviderError> {
    let token_uri = credentials
        .token_uri
        .as_deref()
        .filter(|uri| !uri.trim().is_empty())
        .unwrap_or(DEFAULT_GOOGLE_TOKEN_URI);
    let claims = ServiceAccountClaims {
        iss: &credentials.client_email,
        scope: VERTEX_CLOUD_PLATFORM_SCOPE,
        aud: token_uri,
        iat: now,
        exp: now.saturating_add(3600),
    };
    let json_header = serde_json::json!({ "alg": "RS256", "typ": "JWT" });
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json_header)
            .map_err(|error| ProviderError::Parse(format!("could not encode Vertex JWT header: {error}")))?,
    );
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&claims)
            .map_err(|error| ProviderError::Parse(format!("could not encode Vertex JWT claims: {error}")))?,
    );
    let signing_input = format!("{header}.{payload}");
    let private_key = decode_private_key_pem(&credentials.private_key)?;
    let key = if credentials.private_key.contains("BEGIN RSA PRIVATE KEY") {
        RsaKeyPair::from_der(&private_key)
    } else {
        RsaKeyPair::from_pkcs8(&private_key)
    }
    .map_err(|error| ProviderError::InvalidRequest(format!("invalid Vertex Service Account private_key: {error}")))?;
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        signing_input.as_bytes(),
        &mut signature,
    )
    .map_err(|error| ProviderError::InvalidRequest(format!("could not sign Vertex OAuth assertion: {error}")))?;
    Ok(format!(
        "{signing_input}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature)
    ))
}

fn unix_timestamp() -> Result<u64, ProviderError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| ProviderError::InvalidRequest(format!("system clock is before Unix epoch: {error}")))
}

#[cfg(test)]
pub(crate) async fn insert_cached_token_for_test(key: VertexTokenCacheKey, token: String, ttl_secs: u64) {
    let expires_at = unix_timestamp().unwrap().saturating_add(ttl_secs);
    vertex_token_cache().lock().await.insert(
        key,
        CachedAccessToken {
            access_token: token,
            expires_at,
        },
    );
}

#[cfg(test)]
pub(crate) async fn remove_cached_token_for_test(key: &VertexTokenCacheKey) {
    vertex_token_cache().lock().await.remove(key);
}

fn decode_private_key_pem(pem: &str) -> Result<Vec<u8>, ProviderError> {
    let body = pem
        .lines()
        .filter(|line| !line.starts_with("-----BEGIN ") && !line.starts_with("-----END "))
        .collect::<String>();
    base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|error| ProviderError::InvalidRequest(format!("invalid Vertex private_key PEM: {error}")))
}

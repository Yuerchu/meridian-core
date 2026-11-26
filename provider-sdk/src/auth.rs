//! Authentication session contracts for dynamic credentials (e.g. ChatGPT
//! Codex OAuth sessions) without coupling providers to OS keyrings.

use async_trait::async_trait;

/// A short-lived bearer token and workspace/account scope.
#[derive(Clone, PartialEq, Eq)]
pub struct BearerToken {
    pub access_token: String,
    pub account_id: String,
    pub is_fedramp: bool,
}

impl std::fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BearerToken")
            .field("access_token", &"<redacted>")
            .field("account_id", &self.account_id)
            .field("is_fedramp", &self.is_fedramp)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BearerSessionError {
    #[error("{0}")]
    Message(String),
}

/// Dynamic bearer session provider (implemented by `meridian-core::codex_auth::Manager`).
#[async_trait]
pub trait BearerSession: Send + Sync {
    async fn bearer(&self) -> Result<BearerToken, BearerSessionError>;

    async fn refresh_after_rejection(&self, failed_access_token: &str) -> Result<(), BearerSessionError>;
}

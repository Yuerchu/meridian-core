// Ported from codex-rs/secrets (Apache-2.0, OpenAI)
// NOTICE: This file contains code derived from the OpenAI Codex project.
// Changes: removed codex-git-utils dependency, renamed service constant.

use std::fmt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use sha2::Digest;
use sha2::Sha256;

mod local;
/// Also the last line of defence for the log file, which is why it is reachable
/// outside this module (`logging::redact`).
pub(crate) mod sanitizer;

pub use local::LocalSecretsBackend;

use crate::keyring::{DefaultKeyringStore, KeyringStore};

const KEYRING_SERVICE: &str = "meridian";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SecretName(String);

impl SecretName {
    pub fn new(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        anyhow::ensure!(!trimmed.is_empty(), "secret name must not be empty");
        anyhow::ensure!(
            trimmed
                .chars()
                .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_'),
            "secret name must contain only A-Z, 0-9, or _"
        );
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for SecretName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SecretScope {
    Global,
    /// Per-project secrets, ported from Codex; everything stored today is
    /// `Global`.
    #[allow(dead_code)]
    Environment(String),
}

impl SecretScope {
    #[allow(dead_code)]
    pub fn environment(environment_id: impl Into<String>) -> Result<Self> {
        let env_id = environment_id.into();
        let trimmed = env_id.trim();
        anyhow::ensure!(!trimmed.is_empty(), "environment id must not be empty");
        Ok(Self::Environment(trimmed.to_string()))
    }

    pub fn canonical_key(&self, name: &SecretName) -> String {
        match self {
            Self::Global => format!("global/{}", name.as_str()),
            Self::Environment(environment_id) => {
                format!("env/{environment_id}/{}", name.as_str())
            }
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretListEntry {
    pub scope: SecretScope,
    pub name: SecretName,
}

#[async_trait::async_trait]
pub trait SecretsBackend: Send + Sync {
    async fn set(&self, scope: &SecretScope, name: &SecretName, value: &str) -> Result<()>;
    async fn get(&self, scope: &SecretScope, name: &SecretName) -> Result<Option<String>>;
    async fn delete(&self, scope: &SecretScope, name: &SecretName) -> Result<bool>;
    /// No secrets-listing UI yet; ported store contract.
    #[allow(dead_code)]
    async fn list(&self, scope_filter: Option<&SecretScope>) -> Result<Vec<SecretListEntry>>;
}

#[derive(Clone)]
pub struct SecretsManager {
    backend: Arc<dyn SecretsBackend>,
}

impl SecretsManager {
    pub fn new(data_dir: PathBuf) -> Self {
        let keyring_store: Arc<dyn KeyringStore> = Arc::new(DefaultKeyringStore);
        let backend: Arc<dyn SecretsBackend> = Arc::new(LocalSecretsBackend::new(data_dir, keyring_store));
        Self { backend }
    }

    #[allow(dead_code)]
    pub fn new_with_keyring_store(data_dir: PathBuf, keyring_store: Arc<dyn KeyringStore>) -> Self {
        let backend: Arc<dyn SecretsBackend> = Arc::new(LocalSecretsBackend::new(data_dir, keyring_store));
        Self { backend }
    }

    /// Construct a [`SecretsManager`] backed by a caller-supplied async backend
    /// (for example, a database-backed encrypted store in a multi-tenant server).
    pub fn with_backend(backend: Arc<dyn SecretsBackend>) -> Self {
        Self { backend }
    }

    pub async fn set(&self, scope: &SecretScope, name: &SecretName, value: &str) -> Result<()> {
        self.backend.set(scope, name, value).await
    }

    pub async fn get(&self, scope: &SecretScope, name: &SecretName) -> Result<Option<String>> {
        self.backend.get(scope, name).await
    }

    pub async fn delete(&self, scope: &SecretScope, name: &SecretName) -> Result<bool> {
        self.backend.delete(scope, name).await
    }

    #[allow(dead_code)]
    pub async fn list(&self, scope_filter: Option<&SecretScope>) -> Result<Vec<SecretListEntry>> {
        self.backend.list(scope_filter).await
    }
}

/// Pairs with `SecretScope::Environment`, which nothing constructs yet.
#[allow(dead_code)]
pub fn environment_id_from_path(path: &Path) -> String {
    let canonical = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{digest:x}");
    let short = hex.get(..12).unwrap_or(hex.as_str());
    format!("env-{short}")
}

pub(crate) fn compute_keyring_account(_data_dir: &Path) -> String {
    // Use a fixed account name so that release and dev builds share the same
    // passphrase when they share the same app_data_dir. Previously this was
    // based on the canonicalized data_dir path hash, which broke when the
    // binary location differed between builds.
    "secrets|meridian".to_string()
}

pub(crate) fn keyring_service() -> &'static str {
    KEYRING_SERVICE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_secret_name_valid() {
        let name = SecretName::new("API_KEY").unwrap();
        assert_eq!(name.as_str(), "API_KEY");
    }

    #[test]
    fn test_secret_name_rejects_empty() {
        assert!(SecretName::new("").is_err());
        assert!(SecretName::new("   ").is_err());
    }

    #[test]
    fn test_secret_name_rejects_lowercase() {
        assert!(SecretName::new("api_key").is_err());
    }

    #[test]
    fn test_secret_name_trims_whitespace() {
        let name = SecretName::new("  API_KEY  ").unwrap();
        assert_eq!(name.as_str(), "API_KEY");
    }

    #[test]
    fn test_scope_canonical_key_global() {
        let name = SecretName::new("API_KEY").unwrap();
        assert_eq!(SecretScope::Global.canonical_key(&name), "global/API_KEY");
    }

    #[test]
    fn test_scope_canonical_key_env() {
        let name = SecretName::new("API_KEY").unwrap();
        let scope = SecretScope::Environment("env-abc".into());
        assert_eq!(scope.canonical_key(&name), "env/env-abc/API_KEY");
    }

    #[test]
    fn test_keyring_service_is_meridian() {
        assert_eq!(keyring_service(), "meridian");
    }

    #[tokio::test]
    async fn custom_async_backend_works_through_secrets_manager() {
        use std::collections::HashMap;
        use tokio::sync::Mutex;

        #[derive(Default)]
        struct InMemoryAsyncBackend {
            entries: Mutex<HashMap<(SecretScope, SecretName), String>>,
        }

        #[async_trait::async_trait]
        impl SecretsBackend for InMemoryAsyncBackend {
            async fn set(&self, scope: &SecretScope, name: &SecretName, value: &str) -> Result<()> {
                self.entries
                    .lock()
                    .await
                    .insert((scope.clone(), name.clone()), value.to_string());
                Ok(())
            }

            async fn get(&self, scope: &SecretScope, name: &SecretName) -> Result<Option<String>> {
                Ok(self.entries.lock().await.get(&(scope.clone(), name.clone())).cloned())
            }

            async fn delete(&self, scope: &SecretScope, name: &SecretName) -> Result<bool> {
                Ok(self
                    .entries
                    .lock()
                    .await
                    .remove(&(scope.clone(), name.clone()))
                    .is_some())
            }

            async fn list(&self, scope_filter: Option<&SecretScope>) -> Result<Vec<SecretListEntry>> {
                Ok(self
                    .entries
                    .lock()
                    .await
                    .keys()
                    .filter(|(scope, _)| scope_filter.is_none_or(|wanted| wanted == scope))
                    .map(|(scope, name)| SecretListEntry {
                        scope: scope.clone(),
                        name: name.clone(),
                    })
                    .collect())
            }
        }

        let manager = SecretsManager::with_backend(Arc::new(InMemoryAsyncBackend::default()));
        let key = SecretName::new("PROVIDER_OPENAI_KEY").unwrap();
        manager.set(&SecretScope::Global, &key, "sk-async").await.unwrap();
        assert_eq!(
            manager.get(&SecretScope::Global, &key).await.unwrap().as_deref(),
            Some("sk-async")
        );
        assert_eq!(manager.list(Some(&SecretScope::Global)).await.unwrap().len(), 1);
        assert!(manager.delete(&SecretScope::Global, &key).await.unwrap());
        assert_eq!(manager.get(&SecretScope::Global, &key).await.unwrap(), None);
    }
}

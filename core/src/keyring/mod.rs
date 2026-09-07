// Ported from codex-rs/keyring-store (Apache-2.0, OpenAI)
// NOTICE: This file contains code derived from the OpenAI Codex project.

use keyring::Entry;
use keyring::Error as KeyringError;
use std::error::Error;
use std::fmt;
use std::fmt::Debug;
use tracing::trace;

#[derive(Debug)]
pub enum CredentialStoreError {
    Other(KeyringError),
}

impl CredentialStoreError {
    pub fn new(error: KeyringError) -> Self {
        Self::Other(error)
    }

    pub fn message(&self) -> String {
        match self {
            Self::Other(error) => error.to_string(),
        }
    }
}

impl fmt::Display for CredentialStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Other(error) => write!(f, "{error}"),
        }
    }
}

impl Error for CredentialStoreError {}

pub trait KeyringStore: Debug + Send + Sync {
    fn load(&self, service: &str, account: &str) -> Result<Option<String>, CredentialStoreError>;
    fn save(&self, service: &str, account: &str, value: &str) -> Result<(), CredentialStoreError>;
    /// Nothing deletes the passphrase today; part of the ported store contract.
    #[allow(dead_code)]
    fn delete(&self, service: &str, account: &str) -> Result<bool, CredentialStoreError>;
}

#[derive(Debug)]
pub struct DefaultKeyringStore;

impl KeyringStore for DefaultKeyringStore {
    fn load(&self, service: &str, account: &str) -> Result<Option<String>, CredentialStoreError> {
        trace!("keyring.load start, service={service}, account={account}");
        let entry = Entry::new(service, account).map_err(CredentialStoreError::new)?;
        match entry.get_password() {
            Ok(password) => {
                trace!("keyring.load success, service={service}, account={account}");
                Ok(Some(password))
            }
            Err(keyring::Error::NoEntry) => {
                trace!("keyring.load no entry, service={service}, account={account}");
                Ok(None)
            }
            Err(error) => {
                tracing::warn!(service, account, %error, "keyring load failed");
                Err(CredentialStoreError::new(error))
            }
        }
    }

    fn save(&self, service: &str, account: &str, value: &str) -> Result<(), CredentialStoreError> {
        trace!(
            "keyring.save start, service={service}, account={account}, value_len={}",
            value.len()
        );
        let entry = Entry::new(service, account).map_err(CredentialStoreError::new)?;
        match entry.set_password(value) {
            Ok(()) => {
                trace!("keyring.save success, service={service}, account={account}");
                Ok(())
            }
            Err(error) => {
                tracing::warn!(service, account, %error, "keyring save failed");
                Err(CredentialStoreError::new(error))
            }
        }
    }

    fn delete(&self, service: &str, account: &str) -> Result<bool, CredentialStoreError> {
        trace!("keyring.delete start, service={service}, account={account}");
        let entry = Entry::new(service, account).map_err(CredentialStoreError::new)?;
        match entry.delete_credential() {
            Ok(()) => {
                trace!("keyring.delete success, service={service}, account={account}");
                Ok(true)
            }
            Err(keyring::Error::NoEntry) => {
                trace!("keyring.delete no entry, service={service}, account={account}");
                Ok(false)
            }
            Err(error) => {
                tracing::warn!(service, account, %error, "keyring delete failed");
                Err(CredentialStoreError::new(error))
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering as AtomicOrdering;

    #[derive(Debug, Default)]
    pub struct MockKeyringStore {
        store: Mutex<HashMap<String, String>>,
        errors: Mutex<HashMap<String, String>>,
        loads: AtomicUsize,
    }

    impl MockKeyringStore {
        pub fn new() -> Self {
            Self::default()
        }

        fn key(service: &str, account: &str) -> String {
            format!("{service}:{account}")
        }

        /// How many times the passphrase has been fetched — a stand-in for how
        /// many times the secrets file was decrypted, which is the expensive
        /// half of a lookup.
        pub fn load_count(&self) -> usize {
            self.loads.load(AtomicOrdering::SeqCst)
        }

        /// Error injection, part of the mock's contract even between users.
        #[allow(dead_code)]
        pub fn set_error(&self, service: &str, account: &str, msg: &str) {
            self.errors
                .lock()
                .unwrap()
                .insert(Self::key(service, account), msg.to_string());
        }
    }

    impl KeyringStore for MockKeyringStore {
        fn load(&self, service: &str, account: &str) -> Result<Option<String>, CredentialStoreError> {
            self.loads.fetch_add(1, AtomicOrdering::SeqCst);
            if let Some(msg) = self.errors.lock().unwrap().remove(&Self::key(service, account)) {
                return Err(CredentialStoreError::Other(KeyringError::PlatformFailure(msg.into())));
            }
            Ok(self.store.lock().unwrap().get(&Self::key(service, account)).cloned())
        }

        fn save(&self, service: &str, account: &str, value: &str) -> Result<(), CredentialStoreError> {
            if let Some(msg) = self.errors.lock().unwrap().remove(&Self::key(service, account)) {
                return Err(CredentialStoreError::Other(KeyringError::PlatformFailure(msg.into())));
            }
            self.store
                .lock()
                .unwrap()
                .insert(Self::key(service, account), value.to_string());
            Ok(())
        }

        fn delete(&self, service: &str, account: &str) -> Result<bool, CredentialStoreError> {
            if let Some(msg) = self.errors.lock().unwrap().remove(&Self::key(service, account)) {
                return Err(CredentialStoreError::Other(KeyringError::PlatformFailure(msg.into())));
            }
            Ok(self
                .store
                .lock()
                .unwrap()
                .remove(&Self::key(service, account))
                .is_some())
        }
    }
}

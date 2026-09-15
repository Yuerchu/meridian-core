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
    /// A store's own refusal, with nothing underneath it.
    ///
    /// `SuppliedPassphraseStore` is the reason this exists: "this build keeps
    /// no keychain" is not a platform failure and has no `KeyringError` to wrap,
    /// but it still has to reach the operator as an error rather than as a
    /// success that did nothing.
    Refused(String),
}

impl CredentialStoreError {
    pub fn new(error: KeyringError) -> Self {
        Self::Other(error)
    }

    pub fn refused(message: impl Into<String>) -> Self {
        Self::Refused(message.into())
    }

    pub fn message(&self) -> String {
        match self {
            Self::Other(error) => error.to_string(),
            Self::Refused(message) => message.clone(),
        }
    }
}

impl fmt::Display for CredentialStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Other(error) => write!(f, "{error}"),
            Self::Refused(message) => write!(f, "{message}"),
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

/// How short a supplied passphrase may be.
///
/// The same floor `listen_guard` puts on a remote token, for the same reason:
/// this one value is the whole boundary, and a short one is a boundary in name.
pub const MIN_SUPPLIED_PASSPHRASE: usize = 16;

/// The secrets-file passphrase, handed in from outside.
///
/// The desktop keeps it in the OS keychain, which is right wherever somebody is
/// logged in. A server has none to use — `linux-native-async-persistent` wants
/// a Secret Service, and a container has no session to run one — so the
/// deployment supplies the passphrase instead, from an environment variable or
/// a mounted file.
///
/// **A missing passphrase is an error, never an absent entry**, and that is the
/// whole reason this type is careful. `Ok(None)` is what tells
/// `LocalSecretsBackend` to *generate* a passphrase and save it; this store
/// cannot save, so the generated one would live only in this process. The
/// secrets file would be rewritten under a passphrase that dies at exit, and
/// every key in it becomes unreadable at the next start — silently, totally,
/// and only noticed later. So `load` answers with the passphrase it was built
/// with, and the absence is refused by the constructor rather than reported
/// here.
///
/// For the same reason `save` and `delete` fail loudly rather than pretending:
/// an operator whose passphrase is not actually being persisted has to find out
/// now, not at the restart that cannot read anything.
#[derive(Debug)]
pub struct SuppliedPassphraseStore {
    passphrase: String,
    /// Where it came from, so a refusal can say what to fix rather than that
    /// something unnamed is missing.
    source: String,
}

impl SuppliedPassphraseStore {
    pub fn new(passphrase: impl Into<String>, source: impl Into<String>) -> Result<Self, String> {
        let passphrase = passphrase.into();
        let source = source.into();
        // Trimmed before measuring, because the usual way this arrives is a
        // file whose last byte is a newline — and a passphrase that silently
        // includes it is one no other tool will reproduce.
        let passphrase = passphrase.trim().to_string();
        if passphrase.is_empty() {
            return Err(format!("{source} is empty; it must hold the secrets passphrase"));
        }
        if passphrase.chars().count() < MIN_SUPPLIED_PASSPHRASE {
            return Err(format!(
                "{source} must be at least {MIN_SUPPLIED_PASSPHRASE} characters; it is the only thing protecting the stored keys"
            ));
        }
        Ok(Self { passphrase, source })
    }
}

impl KeyringStore for SuppliedPassphraseStore {
    fn load(&self, service: &str, account: &str) -> Result<Option<String>, CredentialStoreError> {
        trace!("supplied passphrase load, service={service}, account={account}");
        Ok(Some(self.passphrase.clone()))
    }

    fn save(&self, _service: &str, _account: &str, _value: &str) -> Result<(), CredentialStoreError> {
        Err(CredentialStoreError::refused(format!(
            "this build keeps no keychain: the passphrase comes from {} and cannot be written back. \
             Nothing should be asking — a save here means the passphrase was read as absent.",
            self.source
        )))
    }

    fn delete(&self, _service: &str, _account: &str) -> Result<bool, CredentialStoreError> {
        Err(CredentialStoreError::refused(format!(
            "this build keeps no keychain: the passphrase comes from {}, and removing it is the deployment's business",
            self.source
        )))
    }
}

#[cfg(test)]
mod supplied_tests {
    use super::*;

    #[test]
    fn a_supplied_passphrase_answers_every_lookup() {
        let store = SuppliedPassphraseStore::new("correct horse battery staple", "$TEST_VAR").unwrap();
        assert_eq!(
            store.load("secrets.meridian", "secrets|meridian").unwrap().as_deref(),
            Some("correct horse battery staple")
        );
    }

    /// Both of these must fail rather than succeed quietly. A store that
    /// accepted a save would report success while the passphrase went nowhere,
    /// and the next start would find a secrets file it cannot decrypt.
    #[test]
    fn it_refuses_to_pretend_it_persisted_anything() {
        let store = SuppliedPassphraseStore::new("correct horse battery staple", "$TEST_VAR").unwrap();
        let error = store.save("s", "a", "v").unwrap_err().message();
        assert!(error.contains("$TEST_VAR"), "{error}");
        assert!(store.delete("s", "a").is_err());
    }

    /// A trailing newline is what a mounted secret file almost always has, and
    /// a passphrase that quietly includes it is one no other tool reproduces.
    #[test]
    fn surrounding_whitespace_is_not_part_of_the_passphrase() {
        let store = SuppliedPassphraseStore::new("  correct horse battery staple\n", "file").unwrap();
        assert_eq!(
            store.load("s", "a").unwrap().as_deref(),
            Some("correct horse battery staple")
        );
    }

    #[test]
    fn a_short_or_empty_passphrase_is_refused_at_construction() {
        assert!(SuppliedPassphraseStore::new("", "$VAR").is_err());
        assert!(SuppliedPassphraseStore::new("   \n", "$VAR").is_err());
        assert!(SuppliedPassphraseStore::new("short", "$VAR").is_err());
        assert!(
            SuppliedPassphraseStore::new("0123456789abcde", "$VAR").is_err(),
            "15 is short"
        );
        assert!(
            SuppliedPassphraseStore::new("0123456789abcdef", "$VAR").is_ok(),
            "16 is the floor"
        );
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

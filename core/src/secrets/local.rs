// Ported from codex-rs/secrets/local.rs (Apache-2.0, OpenAI)

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::Ordering;
use std::sync::atomic::compiler_fence;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use age::decrypt;
use age::encrypt;
use age::scrypt::Identity as ScryptIdentity;
use age::scrypt::Recipient as ScryptRecipient;
use age::secrecy::ExposeSecret;
use age::secrecy::SecretString;
use anyhow::Context;
use anyhow::Result;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use rand::TryRngCore;
use rand::rngs::OsRng;
use serde::Deserialize;
use serde::Serialize;
use tracing::warn;

use super::SecretListEntry;
use super::SecretName;
use super::SecretScope;
use super::SecretsBackend;
use super::compute_keyring_account;
use super::keyring_service;
use crate::keyring::KeyringStore;

const SECRETS_VERSION: u8 = 1;
const LOCAL_SECRETS_FILENAME: &str = "local.age";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct SecretsFile {
    version: u8,
    secrets: BTreeMap<String, String>,
}

impl SecretsFile {
    fn new_empty() -> Self {
        Self {
            version: SECRETS_VERSION,
            secrets: BTreeMap::new(),
        }
    }
}

/// What the cache was built from, so an edit by another process is noticed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
}

#[derive(Debug, Clone)]
pub struct LocalSecretsBackend {
    data_dir: PathBuf,
    keyring_store: Arc<dyn KeyringStore>,
    /// Decrypted contents, kept for the life of the process.
    ///
    /// age picks an scrypt work factor calibrated to take **about a second** on
    /// the machine that wrote the file — deliberately, since that is what makes
    /// a stolen file expensive to attack. Reading it per lookup made sending one
    /// message pay that several times over (the provider key, then each tool
    /// credential), which is most of the wait before an answer starts.
    ///
    /// The trade is that decrypted secrets live in process memory rather than
    /// being re-derived each time. That is a smaller change than it sounds: any
    /// key actually in use is already in memory, and this process holds no
    /// secret it would not otherwise have handled. Nothing is written back to
    /// disk, and it all goes away when the process exits.
    cache: Arc<RwLock<Option<(FileStamp, SecretsFile)>>>,
}

impl LocalSecretsBackend {
    pub fn new(data_dir: PathBuf, keyring_store: Arc<dyn KeyringStore>) -> Self {
        Self {
            data_dir,
            keyring_store,
            cache: Arc::new(RwLock::new(None)),
        }
    }

    fn stamp(path: &Path) -> Option<FileStamp> {
        let meta = fs::metadata(path).ok()?;
        Some(FileStamp {
            modified: meta.modified().ok(),
            len: meta.len(),
        })
    }

    fn secrets_dir(&self) -> PathBuf {
        self.data_dir.join("secrets")
    }

    fn secrets_path(&self) -> PathBuf {
        self.secrets_dir().join(LOCAL_SECRETS_FILENAME)
    }

    fn load_file(&self) -> Result<SecretsFile> {
        let path = self.secrets_path();
        if !path.exists() {
            return Ok(SecretsFile::new_empty());
        }

        // Stamped before the read: a write landing in between leaves the cache
        // looking older than the file, which costs one wasted decrypt. Stamping
        // after would let it look current while holding the previous contents.
        let stamp = Self::stamp(&path);
        if let Some(stamp) = &stamp
            && let Ok(cache) = self.cache.read()
            && let Some((cached_stamp, file)) = cache.as_ref()
            && cached_stamp == stamp
        {
            return Ok(file.clone());
        }

        let ciphertext =
            fs::read(&path).with_context(|| format!("failed to read secrets file at {}", path.display()))?;
        let passphrase = self.load_or_create_passphrase()?;
        // On decrypt failure, surface an error instead of silently discarding the
        // file: the keyring entry may have been lost/rotated, and destroying the
        // ciphertext would make the secrets unrecoverable once the keyring is fixed.
        let plaintext = decrypt_with_passphrase(&ciphertext, &passphrase).with_context(|| {
            format!(
                "failed to decrypt secrets file at {}: the keyring passphrase does not match. \
                 The encrypted file is preserved unchanged — restore the keyring entry to recover.",
                path.display()
            )
        })?;
        let mut parsed: SecretsFile = serde_json::from_slice(&plaintext)
            .with_context(|| format!("failed to deserialize decrypted secrets file at {}", path.display()))?;
        if parsed.version == 0 {
            parsed.version = SECRETS_VERSION;
        }
        anyhow::ensure!(
            parsed.version <= SECRETS_VERSION,
            "secrets file version {} is newer than supported version {}",
            parsed.version,
            SECRETS_VERSION
        );
        if let Some(stamp) = stamp
            && let Ok(mut cache) = self.cache.write()
        {
            *cache = Some((stamp, parsed.clone()));
        }
        Ok(parsed)
    }

    fn save_file(&self, file: &SecretsFile) -> Result<()> {
        let dir = self.secrets_dir();
        fs::create_dir_all(&dir).with_context(|| format!("failed to create secrets dir {}", dir.display()))?;

        let passphrase = self.load_or_create_passphrase()?;
        let plaintext = serde_json::to_vec(file).context("failed to serialize secrets file")?;
        let ciphertext = encrypt_with_passphrase(&plaintext, &passphrase)?;
        let path = self.secrets_path();
        write_file_atomically(&path, &ciphertext)?;
        // Restamped from what actually landed, so the next read trusts it.
        if let Ok(mut cache) = self.cache.write() {
            *cache = Self::stamp(&path).map(|stamp| (stamp, file.clone()));
        }
        Ok(())
    }

    fn load_or_create_passphrase(&self) -> Result<SecretString> {
        let account = compute_keyring_account(&self.data_dir);
        let loaded = self
            .keyring_store
            .load(keyring_service(), &account)
            .map_err(|err| anyhow::anyhow!(err.message()))
            .with_context(|| format!("failed to load secrets key from keyring for {account}"))?;
        match loaded {
            Some(existing) => Ok(SecretString::from(existing)),
            None => {
                let generated = generate_passphrase()?;
                self.keyring_store
                    .save(keyring_service(), &account, generated.expose_secret())
                    .map_err(|err| anyhow::anyhow!(err.message()))
                    .context("failed to persist secrets key in keyring")?;
                Ok(generated)
            }
        }
    }
}

impl SecretsBackend for LocalSecretsBackend {
    fn set(&self, scope: &SecretScope, name: &SecretName, value: &str) -> Result<()> {
        anyhow::ensure!(!value.is_empty(), "secret value must not be empty");
        let canonical_key = scope.canonical_key(name);
        let mut file = self.load_file()?;
        file.secrets.insert(canonical_key, value.to_string());
        self.save_file(&file)
    }

    fn get(&self, scope: &SecretScope, name: &SecretName) -> Result<Option<String>> {
        let canonical_key = scope.canonical_key(name);
        let file = self.load_file()?;
        Ok(file.secrets.get(&canonical_key).cloned())
    }

    fn delete(&self, scope: &SecretScope, name: &SecretName) -> Result<bool> {
        let canonical_key = scope.canonical_key(name);
        let mut file = self.load_file()?;
        let removed = file.secrets.remove(&canonical_key).is_some();
        if removed {
            self.save_file(&file)?;
        }
        Ok(removed)
    }

    fn list(&self, scope_filter: Option<&SecretScope>) -> Result<Vec<SecretListEntry>> {
        let file = self.load_file()?;
        let mut entries = Vec::new();
        for canonical_key in file.secrets.keys() {
            let Some(entry) = parse_canonical_key(canonical_key) else {
                warn!("skipping invalid canonical secret key: {canonical_key}");
                continue;
            };
            if let Some(scope) = scope_filter
                && entry.scope != *scope
            {
                continue;
            }
            entries.push(entry);
        }
        Ok(entries)
    }
}

fn write_file_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().with_context(|| {
        format!(
            "failed to compute parent directory for secrets file at {}",
            path.display()
        )
    })?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let tmp_path = dir.join(format!(".{LOCAL_SECRETS_FILENAME}.tmp-{}-{nonce}", std::process::id()));

    {
        let mut tmp_file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp_path)
            .with_context(|| format!("failed to create temp secrets file at {}", tmp_path.display()))?;
        tmp_file
            .write_all(contents)
            .with_context(|| format!("failed to write temp secrets file at {}", tmp_path.display()))?;
        tmp_file
            .sync_all()
            .with_context(|| format!("failed to sync temp secrets file at {}", tmp_path.display()))?;
    }

    match fs::rename(&tmp_path, path) {
        Ok(()) => Ok(()),
        Err(initial_error) => {
            #[cfg(target_os = "windows")]
            {
                if path.exists() {
                    fs::remove_file(path).with_context(|| {
                        format!(
                            "failed to remove existing secrets file at {} before replace",
                            path.display()
                        )
                    })?;
                    fs::rename(&tmp_path, path).with_context(|| {
                        format!(
                            "failed to replace secrets file at {} with {}",
                            path.display(),
                            tmp_path.display()
                        )
                    })?;
                    return Ok(());
                }
            }

            let _ = fs::remove_file(&tmp_path);
            Err(initial_error).with_context(|| {
                format!(
                    "failed to atomically replace secrets file at {} with {}",
                    path.display(),
                    tmp_path.display()
                )
            })
        }
    }
}

fn generate_passphrase() -> Result<SecretString> {
    let mut bytes = [0_u8; 32];
    let mut rng = OsRng;
    rng.try_fill_bytes(&mut bytes)
        .context("failed to generate random secrets key")?;
    let encoded = BASE64_STANDARD.encode(bytes);
    wipe_bytes(&mut bytes);
    Ok(SecretString::from(encoded))
}

fn wipe_bytes(bytes: &mut [u8]) {
    for byte in bytes {
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}

fn encrypt_with_passphrase(plaintext: &[u8], passphrase: &SecretString) -> Result<Vec<u8>> {
    let recipient = ScryptRecipient::new(passphrase.clone());
    encrypt(&recipient, plaintext).context("failed to encrypt secrets file")
}

fn decrypt_with_passphrase(ciphertext: &[u8], passphrase: &SecretString) -> Result<Vec<u8>> {
    let identity = ScryptIdentity::new(passphrase.clone());
    decrypt(&identity, ciphertext).context("failed to decrypt secrets file")
}

/// Read side of `SecretScope::canonical_key`, used by the unconsumed `list`.
#[allow(dead_code)]
fn parse_canonical_key(canonical_key: &str) -> Option<SecretListEntry> {
    let mut parts = canonical_key.split('/');
    let scope_kind = parts.next()?;
    match scope_kind {
        "global" => {
            let name = parts.next()?;
            if parts.next().is_some() {
                return None;
            }
            let name = SecretName::new(name).ok()?;
            Some(SecretListEntry {
                scope: SecretScope::Global,
                name,
            })
        }
        "env" => {
            let environment_id = parts.next()?;
            let name = parts.next()?;
            if parts.next().is_some() {
                return None;
            }
            let name = SecretName::new(name).ok()?;
            let scope = SecretScope::environment(environment_id.to_string()).ok()?;
            Some(SecretListEntry { scope, name })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyring::test_support::MockKeyringStore;
    use std::sync::Arc;

    fn make_backend(dir: &std::path::Path) -> LocalSecretsBackend {
        let keyring = Arc::new(MockKeyringStore::new());
        LocalSecretsBackend::new(dir.to_path_buf(), keyring)
    }

    fn make_counting_backend(dir: &std::path::Path) -> (LocalSecretsBackend, Arc<MockKeyringStore>) {
        let keyring = Arc::new(MockKeyringStore::new());
        (LocalSecretsBackend::new(dir.to_path_buf(), keyring.clone()), keyring)
    }

    /// age calibrates its scrypt work factor to take about a second on the
    /// machine that wrote the file. Decrypting per lookup made one turn pay it
    /// several times over — the provider key, then each tool credential.
    #[test]
    fn test_repeat_reads_decrypt_once() {
        let dir = tempfile::tempdir().unwrap();
        let (backend, keyring) = make_counting_backend(dir.path());
        let scope = SecretScope::Global;
        let name = SecretName::new("API_KEY").unwrap();
        backend.set(&scope, &name, "value").unwrap();

        let after_write = keyring.load_count();
        for _ in 0..5 {
            assert_eq!(backend.get(&scope, &name).unwrap(), Some("value".into()));
        }
        assert_eq!(keyring.load_count(), after_write, "reads went back to disk");
    }

    /// The cache is stamped with the file it was built from, so a write that
    /// went around this backend is not served stale.
    #[test]
    fn test_external_write_invalidates_cache() {
        let dir = tempfile::tempdir().unwrap();
        let keyring = Arc::new(MockKeyringStore::new());
        let backend = LocalSecretsBackend::new(dir.path().to_path_buf(), keyring.clone());
        // Same directory and same keyring entry, but its own cache — as a second
        // window would be.
        let other = LocalSecretsBackend::new(dir.path().to_path_buf(), keyring);
        let scope = SecretScope::Global;
        let name = SecretName::new("API_KEY").unwrap();

        backend.set(&scope, &name, "first").unwrap();
        assert_eq!(backend.get(&scope, &name).unwrap(), Some("first".into()));

        // Long enough that the ciphertext changes length: two writes in the same
        // filesystem timestamp tick would otherwise be told apart by size alone.
        other.set(&scope, &name, "second-value-considerably-longer").unwrap();
        assert_eq!(
            backend.get(&scope, &name).unwrap(),
            Some("second-value-considerably-longer".into()),
        );
    }

    #[test]
    fn test_set_get_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let backend = make_backend(dir.path());
        let scope = SecretScope::Global;
        let name = SecretName::new("API_KEY").unwrap();

        backend.set(&scope, &name, "secret_value_123").unwrap();
        let got = backend.get(&scope, &name).unwrap();
        assert_eq!(got, Some("secret_value_123".to_string()));
    }

    #[test]
    fn test_delete_secret() {
        let dir = tempfile::tempdir().unwrap();
        let backend = make_backend(dir.path());
        let scope = SecretScope::Global;
        let name = SecretName::new("TO_DELETE").unwrap();

        backend.set(&scope, &name, "temp").unwrap();
        assert!(backend.delete(&scope, &name).unwrap());
        assert_eq!(backend.get(&scope, &name).unwrap(), None);
        assert!(!backend.delete(&scope, &name).unwrap());
    }

    #[test]
    fn test_no_temp_files_left() {
        let dir = tempfile::tempdir().unwrap();
        let backend = make_backend(dir.path());
        let scope = SecretScope::Global;
        let name = SecretName::new("KEY_A").unwrap();

        backend.set(&scope, &name, "val1").unwrap();
        backend.set(&scope, &name, "val2").unwrap();

        let secrets_dir = dir.path().join("secrets");
        let entries: Vec<_> = fs::read_dir(&secrets_dir).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].file_name().to_str().unwrap(), LOCAL_SECRETS_FILENAME);
    }

    #[test]
    fn test_set_empty_value_fails() {
        let dir = tempfile::tempdir().unwrap();
        let backend = make_backend(dir.path());
        let scope = SecretScope::Global;
        let name = SecretName::new("EMPTY").unwrap();
        assert!(backend.set(&scope, &name, "").is_err());
    }

    #[test]
    fn test_list_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let backend = make_backend(dir.path());
        let global = SecretScope::Global;
        let env = SecretScope::Environment("test-env".into());

        backend.set(&global, &SecretName::new("KEY_A").unwrap(), "a").unwrap();
        backend.set(&env, &SecretName::new("KEY_B").unwrap(), "b").unwrap();

        let all = backend.list(None).unwrap();
        assert_eq!(all.len(), 2);

        let global_only = backend.list(Some(&global)).unwrap();
        assert_eq!(global_only.len(), 1);
        assert_eq!(global_only[0].name.as_str(), "KEY_A");
    }

    #[test]
    fn test_parse_canonical_key_valid() {
        let entry = parse_canonical_key("global/API_KEY").unwrap();
        assert_eq!(entry.scope, SecretScope::Global);
        assert_eq!(entry.name.as_str(), "API_KEY");

        let entry = parse_canonical_key("env/my-env/SECRET").unwrap();
        assert_eq!(entry.scope, SecretScope::Environment("my-env".into()));
        assert_eq!(entry.name.as_str(), "SECRET");
    }

    #[test]
    fn test_parse_canonical_key_invalid() {
        assert!(parse_canonical_key("invalid").is_none());
        assert!(parse_canonical_key("global/too/many").is_none());
        assert!(parse_canonical_key("unknown/KEY").is_none());
    }
}

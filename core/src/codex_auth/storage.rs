//! Reading and writing the Codex CLI's `auth.json`.
//!
//! Ported from `codex-rs/login/src/auth/storage.rs` (Apache-2.0, OpenAI), with
//! two deliberate departures noted at their call sites: the write is atomic, and
//! unknown fields survive a round trip.
//!
//! **This file is somebody else's.** It belongs to the Codex CLI, which is
//! running its own sessions against it, and we are a second writer. Everything
//! here is shaped by that: we keep what we do not understand, we replace the
//! file rather than truncating it, and we read it fresh rather than trusting a
//! copy. See [`super::mod`] for the locking that goes with it.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::token_data::TokenData;

/// The shape of `auth.json`.
///
/// Every field this app does not use is still declared, and anything not
/// declared at all is caught by `extra`. **Dropping a field on write would
/// damage the user's CLI login** — `agent_identity` and the rest are how the CLI
/// authenticates in modes we know nothing about, and a rewrite that loses them
/// is indistinguishable from a corrupted file.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AuthDotJson {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<String>,
    /// Screaming case on the wire because it mirrors the environment variable.
    #[serde(rename = "OPENAI_API_KEY", default, skip_serializing_if = "Option::is_none")]
    pub openai_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenData>,
    /// When the tokens were last exchanged. The fallback staleness check when a
    /// token carries no `exp`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<DateTime<Utc>>,
    /// Everything else the file holds, carried through untouched.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl AuthDotJson {
    /// Whether this is a ChatGPT login rather than an API key pasted into the
    /// CLI. Only the first is usable here — an API key in `auth.json` belongs to
    /// an ordinary OpenAI provider, configured as one.
    pub fn is_chatgpt_login(&self) -> bool {
        self.tokens.is_some()
    }
}

/// Where the Codex CLI keeps its state.
///
/// `CODEX_HOME` first, because that is what the CLI itself honours — and a GUI
/// process does not necessarily inherit the environment a terminal has, which is
/// the single most common reason for "I am logged in but Meridian says I am
/// not". Whatever this resolves to is shown in the settings panel for exactly
/// that reason.
pub fn find_codex_home() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CODEX_HOME")
        && !dir.trim().is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    dirs::home_dir().map(|home| home.join(".codex"))
}

pub fn auth_file(codex_home: &Path) -> PathBuf {
    codex_home.join("auth.json")
}

/// Which store a login was read from, so it can be written back to the same one.
///
/// Carried rather than re-decided: reading from the keyring and then writing to
/// a file would leave two logins on the machine, and the CLI would keep using
/// the one we did not update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthBackend {
    File,
    Keyring,
}

/// The keyring service name the CLI files its credentials under.
pub const KEYRING_SERVICE: &str = "Codex Auth";

/// The account name within that service.
///
/// Derived from the canonical home path so that two Codex installations do not
/// collide, and hashed because the path is not something to leave in a
/// credential-manager listing. Truncated to 16 hex characters, matching the CLI
/// — this has to agree with it exactly or we look at an empty slot.
pub fn keyring_account(codex_home: &Path) -> String {
    use sha2::{Digest, Sha256};
    let canonical = std::fs::canonicalize(codex_home).unwrap_or_else(|_| codex_home.to_path_buf());
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    format!("cli|{}", &hex(&digest)[..16])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("auth.json is not valid JSON: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("{0}")]
    Keyring(String),
}

/// Read the login, from wherever it is.
///
/// Tries the file first and the keyring second, and reports which answered. A
/// missing file is `Ok(None)` rather than an error — not being logged in is an
/// ordinary state that the caller turns into an instruction.
///
/// The keyring is consulted **even when the file is absent**, because the CLI
/// deletes the file when it moves a login into the keyring: "no auth.json" and
/// "not logged in" are not the same thing, and treating them as one tells a
/// logged-in user to log in again.
pub fn load(
    codex_home: &Path,
    keyring: &dyn crate::keyring::KeyringStore,
) -> Result<Option<(AuthDotJson, AuthBackend)>, StorageError> {
    match std::fs::read_to_string(auth_file(codex_home)) {
        Ok(text) => {
            let parsed: AuthDotJson = serde_json::from_str(&text)?;
            return Ok(Some((parsed, AuthBackend::File)));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }

    match keyring.load(KEYRING_SERVICE, &keyring_account(codex_home)) {
        Ok(Some(text)) => Ok(Some((serde_json::from_str(&text)?, AuthBackend::Keyring))),
        Ok(None) => Ok(None),
        // A keyring that cannot be reached is not the same as an empty one, and
        // saying so beats reporting a logged-in user as logged out.
        Err(e) => Err(StorageError::Keyring(e.message())),
    }
}

/// Write the login back to the store it came from.
pub fn save(
    codex_home: &Path,
    backend: AuthBackend,
    auth: &AuthDotJson,
    keyring: &dyn crate::keyring::KeyringStore,
) -> Result<(), StorageError> {
    match backend {
        AuthBackend::File => write_file_atomically(codex_home, auth),
        AuthBackend::Keyring => {
            let json = serde_json::to_string(auth)?;
            keyring
                .save(KEYRING_SERVICE, &keyring_account(codex_home), &json)
                .map_err(|e| StorageError::Keyring(e.message()))
        }
    }
}

/// Replace `auth.json` in one step.
///
/// **Upstream truncates and rewrites in place; this does not.** That window —
/// file emptied, new contents not yet written — is one where the Codex CLI, or
/// this app on another thread, reads a half-file and concludes the user is
/// logged out. A refresh happening while the CLI starts up is exactly when both
/// are touching it. Writing a sibling and renaming means a reader sees either
/// the old login or the new one.
///
/// `fs::rename` replaces an existing destination on both platforms (Windows goes
/// through `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`). It can still fail if
/// another process holds the file open without sharing — which is a visible
/// error, and leaves the original intact.
fn write_file_atomically(codex_home: &Path, auth: &AuthDotJson) -> Result<(), StorageError> {
    std::fs::create_dir_all(codex_home)?;
    let target = auth_file(codex_home);
    // Beside the target, so the rename stays within one filesystem — across
    // mount points it is a copy, and no longer atomic.
    let temp = codex_home.join(format!("auth.json.{}.tmp", std::process::id()));

    let json = serde_json::to_string_pretty(auth)?;
    write_private(&temp, json.as_bytes())?;

    match std::fs::rename(&temp, &target) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Do not leave a stray temp file holding a copy of the tokens.
            let _ = std::fs::remove_file(&temp);
            Err(e.into())
        }
    }
}

/// Create a file only the owner can read, and write it.
///
/// The mode is set at creation rather than afterwards: a chmod after the fact
/// leaves the tokens world-readable for the moment in between.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    // Reach the disk before the rename, so a crash cannot leave the new name
    // pointing at an empty file.
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyring::KeyringStore;
    use crate::keyring::test_support::MockKeyringStore;

    fn sample_json() -> serde_json::Value {
        serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": serde_json::Value::Null,
            "tokens": {
                "id_token": "eyJhbGciOiJub25lIn0.e30.sig",
                "access_token": "access-1",
                "refresh_token": "refresh-1",
                "account_id": "acct-1"
            },
            "last_refresh": "2026-01-01T00:00:00Z",
            "agent_identity": { "something": "we do not model" },
            "personal_access_token": "pat-1"
        })
    }

    fn write(dir: &Path, value: &serde_json::Value) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(auth_file(dir), serde_json::to_string_pretty(value).unwrap()).unwrap();
    }

    /// The one that protects the user's CLI login: fields we do not model are
    /// still there after we write the file back. Losing `agent_identity` would
    /// break a login mode this app knows nothing about.
    #[test]
    fn unknown_fields_survive_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let keyring = MockKeyringStore::new();
        write(dir.path(), &sample_json());

        let (auth, backend) = load(dir.path(), &keyring).unwrap().unwrap();
        assert_eq!(backend, AuthBackend::File);
        assert!(auth.extra.contains_key("agent_identity"));
        assert!(auth.extra.contains_key("personal_access_token"));

        save(dir.path(), backend, &auth, &keyring).unwrap();
        let back: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(auth_file(dir.path())).unwrap()).unwrap();
        assert_eq!(back["agent_identity"]["something"], "we do not model");
        assert_eq!(back["personal_access_token"], "pat-1");
        assert_eq!(back["tokens"]["id_token"], sample_json()["tokens"]["id_token"]);
    }

    /// Not being logged in is an ordinary answer, not an error.
    #[test]
    fn an_absent_login_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path(), &MockKeyringStore::new()).unwrap().is_none());
    }

    /// The CLI removes the file when it moves a login into the keyring, so an
    /// absent file must not be reported as "not logged in".
    #[test]
    fn the_keyring_is_consulted_when_there_is_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let keyring = MockKeyringStore::new();
        keyring
            .save(
                KEYRING_SERVICE,
                &keyring_account(dir.path()),
                &sample_json().to_string(),
            )
            .unwrap();

        let (auth, backend) = load(dir.path(), &keyring).unwrap().unwrap();
        assert_eq!(backend, AuthBackend::Keyring);
        assert!(auth.is_chatgpt_login());
    }

    /// From the keyring, back to the keyring. Writing to the file instead would
    /// leave two logins on the machine with the CLI using the stale one.
    #[test]
    fn a_keyring_login_is_written_back_to_the_keyring() {
        let dir = tempfile::tempdir().unwrap();
        let keyring = MockKeyringStore::new();
        keyring
            .save(
                KEYRING_SERVICE,
                &keyring_account(dir.path()),
                &sample_json().to_string(),
            )
            .unwrap();

        let (auth, backend) = load(dir.path(), &keyring).unwrap().unwrap();
        save(dir.path(), backend, &auth, &keyring).unwrap();

        assert!(!auth_file(dir.path()).exists(), "no second copy on disk");
        assert!(
            keyring
                .load(KEYRING_SERVICE, &keyring_account(dir.path()))
                .unwrap()
                .is_some()
        );
    }

    /// An unreachable keyring is not an empty one. Reporting a logged-in user as
    /// logged out sends them to run `codex login` again for no reason.
    #[test]
    fn an_unreachable_keyring_is_reported_rather_than_read_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let keyring = MockKeyringStore::new();
        keyring.set_error(KEYRING_SERVICE, &keyring_account(dir.path()), "locked");
        assert!(matches!(load(dir.path(), &keyring), Err(StorageError::Keyring(_))));
    }

    /// Overwriting an existing file has to work — it is the common case, and on
    /// Windows a rename onto an existing name is the part that is easy to get
    /// wrong.
    #[test]
    fn saving_replaces_an_existing_file_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let keyring = MockKeyringStore::new();
        write(dir.path(), &sample_json());

        let (mut auth, backend) = load(dir.path(), &keyring).unwrap().unwrap();
        auth.tokens.as_mut().unwrap().access_token = "rotated".into();
        save(dir.path(), backend, &auth, &keyring).unwrap();

        let (reloaded, _) = load(dir.path(), &keyring).unwrap().unwrap();
        assert_eq!(reloaded.tokens.unwrap().access_token, "rotated");

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left a temp file behind: {leftovers:?}");
    }

    /// An API key in auth.json is not a ChatGPT session. Treating it as one
    /// would send a key to an endpoint that does not take keys.
    #[test]
    fn an_api_key_login_is_not_a_chatgpt_login() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            &serde_json::json!({ "auth_mode": "apikey", "OPENAI_API_KEY": "sk-test" }),
        );
        let (auth, _) = load(dir.path(), &MockKeyringStore::new()).unwrap().unwrap();
        assert!(!auth.is_chatgpt_login());
        assert_eq!(auth.openai_api_key.as_deref(), Some("sk-test"));
    }

    /// Malformed JSON is reported rather than silently treated as absent — the
    /// difference between "log in" and "your login file is damaged".
    #[test]
    fn a_damaged_file_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(auth_file(dir.path()), "{not json").unwrap();
        assert!(matches!(
            load(dir.path(), &MockKeyringStore::new()),
            Err(StorageError::Malformed(_))
        ));
    }

    /// Two installations must not share a slot, and the name must not be the
    /// path itself.
    #[test]
    fn the_keyring_account_is_derived_and_scoped() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let name = keyring_account(a.path());
        assert_ne!(name, keyring_account(b.path()));
        assert!(name.starts_with("cli|"));
        assert_eq!(name.len(), "cli|".len() + 16);
        assert!(!name.contains(&*a.path().to_string_lossy()));
    }
}

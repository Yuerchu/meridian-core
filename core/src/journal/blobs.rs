//! Content-addressed snapshot storage: the bytes exist before the row does.
//!
//! The voice corpus's shape, minus the fencing. Fencing there guards two
//! owners publishing *different* bytes under one claim; here the sha is the
//! address, so two concurrent writers of one sha are writing byte-identical
//! content and either rename winning is correct. What is kept: a staging file
//! cleaned by `Drop` (the common path forgets), an atomic rename into place,
//! and a full re-hash on load — length only catches truncation, and for an
//! attribution record a wrong snapshot is worse than a missing one.

use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// What `store` hands back: everything `journal_blobs` needs for its row.
#[derive(Debug, Clone)]
pub struct StoredBlob {
    pub sha256: String,
    pub byte_len: i64,
    pub line_count: i32,
}

pub fn sha256_of(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn line_count_of(content: &str) -> i32 {
    content.lines().count() as i32
}

/// `blobs/<sha[..2]>/<sha>` under the journal root — two-level fan-out so one
/// directory never holds every snapshot ever taken.
pub fn blob_path(journal_root: &Path, sha: &str) -> PathBuf {
    let prefix = sha.get(..2).unwrap_or("00");
    journal_root.join("blobs").join(prefix).join(sha)
}

fn staging_dir(journal_root: &Path) -> PathBuf {
    journal_root.join(".staging")
}

/// A staging file that removes itself unless it was renamed into place.
///
/// `Drop` only ever touches the staging path, never the final one — the
/// common exit is "the blob already existed", and that path is not thinking
/// about the temp file it created.
struct Staging {
    path: PathBuf,
    published: bool,
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.published {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Write `content` into the store, idempotently. Returns the row-shaped facts.
///
/// An existing file under this sha is re-hashed rather than trusted: a crash
/// mid-write or a bad sector leaves the name right and the bytes wrong, and
/// the fix is cheap here (rewrite) and expensive later (blame built on a wrong
/// snapshot).
pub fn store(journal_root: &Path, content: &str) -> io::Result<StoredBlob> {
    let sha = sha256_of(content);
    let stored = StoredBlob {
        sha256: sha.clone(),
        byte_len: content.len() as i64,
        line_count: line_count_of(content),
    };

    let target = blob_path(journal_root, &sha);
    if let Ok(existing) = std::fs::read_to_string(&target)
        && sha256_of(&existing) == sha
    {
        return Ok(stored);
    }

    std::fs::create_dir_all(staging_dir(journal_root))?;
    std::fs::create_dir_all(target.parent().expect("blob path has a parent"))?;

    let mut staging = Staging {
        path: staging_dir(journal_root).join(format!("{}.part", uuid::Uuid::new_v4())),
        published: false,
    };
    {
        // Written and synced before the rename: the database row this receipt
        // becomes is committed durably, and a power cut between that commit
        // and these bytes reaching the platter would leave a version pointing
        // at a blob that never existed — the inverse of bytes-before-rows.
        use std::io::Write;
        let mut f = std::fs::File::create(&staging.path)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
    }

    let outcome = match std::fs::rename(&staging.path, &target) {
        Ok(()) => {
            staging.published = true;
            Ok(stored)
        }
        // Belt and braces for a lost race: `rename` replaces an existing file
        // on every platform this ships on (Windows included — the corruption
        // test above passes on Windows because it does), but a target held
        // open by another process can still fail the rename. If what is there
        // is already these bytes, whoever put them there won and that is fine.
        Err(_) if matches!(std::fs::read_to_string(&target), Ok(t) if sha256_of(&t) == sha) => Ok(stored),
        Err(e) => Err(e),
    };

    // The directory entry needs its own sync for the rename to be durable.
    // Unix only: std has no way to open a directory for fsync on Windows, and
    // NTFS metadata journaling covers most of the distance anyway.
    #[cfg(unix)]
    if outcome.is_ok()
        && let Some(parent) = target.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }

    outcome
}

/// Read a blob back, verifying the bytes still hash to their name.
pub fn load(journal_root: &Path, sha: &str) -> io::Result<String> {
    let content = std::fs::read_to_string(blob_path(journal_root, sha))?;
    if sha256_of(&content) != sha {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("blob {sha} does not hash to its name"),
        ));
    }
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_then_load_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let stored = store(dir.path(), "fn main() {}\n").unwrap();
        assert_eq!(stored.line_count, 1);
        assert_eq!(load(dir.path(), &stored.sha256).unwrap(), "fn main() {}\n");
    }

    #[test]
    fn store_is_idempotent_and_leaves_no_staging_debris() {
        let dir = tempfile::tempdir().unwrap();
        let a = store(dir.path(), "same").unwrap();
        let b = store(dir.path(), "same").unwrap();
        assert_eq!(a.sha256, b.sha256);

        let staging: Vec<_> = std::fs::read_dir(staging_dir(dir.path()))
            .map(|rd| rd.collect())
            .unwrap_or_default();
        assert!(staging.is_empty(), "staging should be empty after publishes");
    }

    /// A blob whose bytes rotted under its name must be refused, not served —
    /// blame built on it would attribute lines nobody wrote.
    #[test]
    fn a_corrupted_blob_is_refused_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let stored = store(dir.path(), "original").unwrap();
        std::fs::write(blob_path(dir.path(), &stored.sha256), "tampered").unwrap();

        assert!(load(dir.path(), &stored.sha256).is_err());
    }

    /// And `store` heals it rather than trusting the name.
    #[test]
    fn store_rewrites_a_corrupted_existing_blob() {
        let dir = tempfile::tempdir().unwrap();
        let stored = store(dir.path(), "original").unwrap();
        std::fs::write(blob_path(dir.path(), &stored.sha256), "tampered").unwrap();

        store(dir.path(), "original").unwrap();
        assert_eq!(load(dir.path(), &stored.sha256).unwrap(), "original");
    }
}

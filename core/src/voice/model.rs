//! Model file management: install status, local-archive import, deletion.
//!
//! An install is only ever judged by "all five files present and non-empty",
//! so a cancelled download or a botched import can never masquerade as an
//! installed model.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::sync::Mutex;

use super::{MODEL_FILES, MODEL_ID, model_dir, models_dir};

pub(super) const MAX_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Clone, Copy)]
struct Limits {
    compressed: u64,
    expanded: u64,
    file: u64,
    entries: usize,
}

const LIMITS: Limits = Limits {
    compressed: MAX_ARCHIVE_BYTES,
    expanded: 1024 * 1024 * 1024,
    file: 512 * 1024 * 1024,
    entries: 12, // Five files, optional root and GNU long-name records.
};

static INSTALL_LOCK: Mutex<()> = Mutex::new(());

#[derive(serde::Serialize, Clone)]
pub struct ModelStatus {
    pub installed: bool,
    pub path: Option<String>,
    pub size_bytes: u64,
    pub downloading: bool,
}

/// Check the install without touching the download slot; the command layer
/// fills in `downloading`.
pub fn status(app_data_dir: &Path) -> ModelStatus {
    let dir = model_dir(app_data_dir);
    let mut size = 0u64;
    let mut complete = true;
    for name in MODEL_FILES {
        match fs::symlink_metadata(dir.join(name)) {
            Ok(m) if m.is_file() && m.len() > 0 => size += m.len(),
            _ => {
                complete = false;
                break;
            }
        }
    }
    ModelStatus {
        installed: complete,
        path: complete.then(|| dir.to_string_lossy().into_owned()),
        size_bytes: if complete { size } else { 0 },
        downloading: false,
    }
}

/// Import a locally downloaded `.tar.bz2` archive (the same one the download
/// would fetch). Blocking — run inside `spawn_blocking`.
pub fn import_archive(app_data_dir: &Path, archive_path: &Path) -> Result<(), String> {
    let file = File::open(archive_path).map_err(|e| format!("Cannot open archive: {e}"))?;
    install(app_data_dir, file, LIMITS).map_err(|e| format!("Cannot install model: {e}"))
}

fn invalid(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

fn install(app_data_dir: &Path, file: File, limits: Limits) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limits.compressed {
        return Err(invalid("Expected a model archive within the compressed size limit"));
    }
    let parent = models_dir(app_data_dir);
    fs::create_dir_all(&parent)?;
    let scratch = parent.join(format!("install-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&scratch)?;
    let result = extract(file, &scratch, limits).and_then(|()| {
        let _guard = INSTALL_LOCK
            .lock()
            .map_err(|_| io::Error::other("Model install lock poisoned"))?;
        replace_model(&scratch, &model_dir(app_data_dir))
    });
    if result.is_err() {
        let _ = fs::remove_dir_all(scratch);
    }
    result
}

fn extract(file: File, scratch: &Path, limits: Limits) -> io::Result<()> {
    let input = BufReader::new(file.take(limits.compressed + 1));
    let decoder = bzip2::bufread::BzDecoder::new(input);
    let mut archive = tar::Archive::new(decoder.take(limits.expanded + 1));
    let mut seen = [false; MODEL_FILES.len()];
    let mut nested = None;
    let mut root_seen = false;
    let mut long_name = None;
    // Raw iteration exposes metadata before tar can allocate it or expand sparse records.
    for (index, entry) in archive.entries()?.raw(true).enumerate() {
        if index >= limits.entries {
            return Err(invalid("Too many model archive entries"));
        }
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_gnu_longname() && long_name.is_none() && entry.size() <= 256 {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            if bytes.pop() != Some(0) {
                return Err(invalid("Invalid GNU long name"));
            }
            long_name = Some(bytes);
            continue;
        }
        let path = long_name
            .take()
            .unwrap_or_else(|| entry.header().path_bytes().into_owned());
        let path = std::str::from_utf8(&path).map_err(|_| invalid("Invalid model path"))?;
        if kind.is_dir() && !root_seen && path.strip_suffix('/').unwrap_or(path) == MODEL_ID && entry.size() == 0 {
            if nested == Some(false) {
                return Err(invalid("Mixed model roots"));
            }
            root_seen = true;
            nested = Some(true);
            continue;
        }
        let name = path.strip_prefix(MODEL_ID).and_then(|p| p.strip_prefix('/'));
        if nested.replace(name.is_some()).is_some_and(|old| old != name.is_some()) {
            return Err(invalid("Mixed model roots"));
        }
        let idx = MODEL_FILES
            .iter()
            .position(|&f| f == name.unwrap_or(path))
            .ok_or_else(|| invalid("Unexpected model file"))?;
        let size = entry.size();
        if !kind.is_file() || seen[idx] || size == 0 || size > limits.file {
            return Err(invalid("Invalid model entry type, size, or duplicate name"));
        }
        seen[idx] = true;
        let mut output = File::create_new(scratch.join(MODEL_FILES[idx]))?;
        if io::copy(&mut entry, &mut output)? != size {
            return Err(invalid("Truncated model entry"));
        }
    }
    if long_name.is_some() || !seen.into_iter().all(|present| present) {
        return Err(invalid("Incomplete model manifest"));
    }
    // Account for the complete stream, including padding after the first end marker.
    let mut remaining = archive.into_inner();
    let mut buf = [0; 8192];
    loop {
        let n = remaining.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if buf[..n].iter().any(|&b| b != 0) {
            return Err(invalid("Unexpected data after tar end"));
        }
    }
    if remaining.limit() == 0 {
        return Err(invalid("Expanded model archive exceeds limit"));
    }
    let decoder = remaining.into_inner();
    if decoder.total_in() > limits.compressed || !decoder.into_inner().fill_buf()?.is_empty() {
        return Err(invalid("Unexpected compressed archive length"));
    }
    Ok(())
}

fn replace_model(scratch: &Path, dest: &Path) -> io::Result<()> {
    let backup = dest.with_extension(format!("previous-{}", uuid::Uuid::new_v4()));
    match fs::rename(dest, &backup) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return fs::rename(scratch, dest),
        other => other?,
    }
    if let Err(e) = fs::rename(scratch, dest) {
        fs::rename(&backup, dest).map_err(|restore| {
            io::Error::other(format!(
                "{e}; restore failed: {restore}; previous model retained at {}",
                backup.display()
            ))
        })?;
        return Err(e);
    }
    // Promotion has committed; a cleanup failure does not undo the new installation.
    if let Err(error) = fs::remove_dir_all(backup) {
        tracing::warn!(error = %error, "Could not remove previous voice model");
    }
    Ok(())
}

/// Remove the installed model. The caller must also drop the cached engine.
pub fn delete(app_data_dir: &Path) -> Result<(), String> {
    let _guard = INSTALL_LOCK.lock().map_err(|_| "Model install lock poisoned")?;
    let dir = model_dir(app_data_dir);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("Cannot delete model: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_model_files(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        for name in MODEL_FILES {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
    }

    #[test]
    fn status_requires_all_files() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!status(tmp.path()).installed);

        write_model_files(&model_dir(tmp.path()));
        assert!(status(tmp.path()).installed);

        std::fs::remove_file(model_dir(tmp.path()).join("tokens.txt")).unwrap();
        assert!(!status(tmp.path()).installed);
    }

    #[test]
    fn status_rejects_empty_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_model_files(&model_dir(tmp.path()));
        std::fs::write(model_dir(tmp.path()).join("bpe.model"), b"").unwrap();
        assert!(!status(tmp.path()).installed);
    }

    #[test]
    fn import_rejects_garbage() {
        let tmp = tempfile::tempdir().unwrap();
        let bogus = tmp.path().join("model.tar.bz2");
        std::fs::write(&bogus, b"<html>not a tarball</html>").unwrap();
        assert!(import_archive(tmp.path(), &bogus).is_err());
        // No half-installed leftovers.
        assert!(!model_dir(tmp.path()).exists());
        assert!(!status(tmp.path()).installed);
    }

    #[test]
    fn import_accepts_nested_archive() {
        let tmp = tempfile::tempdir().unwrap();

        // Build a tarball shaped like the upstream release (nested dir).
        let archive_path = tmp.path().join("model.tar.bz2");
        let file = std::fs::File::create(&archive_path).unwrap();
        let encoder = bzip2::write::BzEncoder::new(file, bzip2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for name in MODEL_FILES {
            let mut header = tar::Header::new_gnu();
            header.set_size(1);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, format!("{}/{}", super::super::MODEL_ID, name), &b"x"[..])
                .unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();

        import_archive(tmp.path(), &archive_path).unwrap();
        assert!(status(tmp.path()).installed);
        // Scratch dir cleaned up.
        assert!(!model_dir(tmp.path()).with_extension("tmp").exists());
    }

    fn fixture(path: &Path, extra: Option<(&str, tar::EntryType)>) -> (u64, u64) {
        use std::io::Write;
        let mut builder = tar::Builder::new(Vec::new());
        for (name, kind) in MODEL_FILES
            .into_iter()
            .map(|f| (f, tar::EntryType::Regular))
            .chain(extra)
        {
            let mut header = tar::Header::new_gnu();
            header.set_size(3);
            header.set_mode(0o600);
            header.set_entry_type(kind);
            builder.append_data(&mut header, name, &b"new"[..]).unwrap();
        }
        let tar = builder.into_inner().unwrap();
        let mut encoder = bzip2::write::BzEncoder::new(File::create(path).unwrap(), bzip2::Compression::fast());
        encoder.write_all(&tar).unwrap();
        (encoder.finish().unwrap().metadata().unwrap().len(), tar.len() as u64)
    }

    #[test]
    fn every_budget_is_enforced_before_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("archive");
        let (compressed, expanded) = fixture(&path, None);
        let exact = Limits {
            compressed,
            expanded,
            file: 3,
            entries: 5,
        };
        write_model_files(&model_dir(tmp.path()));
        for limits in [
            Limits {
                compressed: compressed - 1,
                ..exact
            },
            Limits {
                expanded: expanded - 1,
                ..exact
            },
            Limits { file: 2, ..exact },
            Limits { entries: 4, ..exact },
        ] {
            assert!(install(tmp.path(), File::open(&path).unwrap(), limits).is_err());
            assert_eq!(fs::read(model_dir(tmp.path()).join(MODEL_FILES[0])).unwrap(), b"x");
            assert_eq!(fs::read_dir(models_dir(tmp.path())).unwrap().count(), 1);
        }
        install(tmp.path(), File::open(path).unwrap(), exact).unwrap();
        assert_eq!(fs::read(model_dir(tmp.path()).join(MODEL_FILES[0])).unwrap(), b"new");
    }

    #[test]
    fn rejects_duplicate_extra_and_mixed_root() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("archive");
        for extra in [MODEL_FILES[0], "extra", &format!("{MODEL_ID}/tokens.txt")] {
            fixture(&path, Some((extra, tar::EntryType::Regular)));
            assert!(import_archive(tmp.path(), &path).is_err());
            assert!(!model_dir(tmp.path()).exists());
        }
    }

    #[test]
    fn regular_file_check_rejects_non_manifest_types() {
        let tmp = tempfile::tempdir().unwrap();
        let scratch = tmp.path().join("scratch");
        fs::create_dir(&scratch).unwrap();
        for kind in [
            tar::EntryType::Symlink,
            tar::EntryType::Link,
            tar::EntryType::Char,
            tar::EntryType::Block,
            tar::EntryType::Fifo,
            tar::EntryType::GNUSparse,
            tar::EntryType::XHeader,
        ] {
            let path = tmp.path().join("archive");
            let mut builder = tar::Builder::new(bzip2::write::BzEncoder::new(
                File::create(&path).unwrap(),
                bzip2::Compression::fast(),
            ));
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(kind);
            header.set_size(1);
            builder.append_data(&mut header, MODEL_FILES[0], &b"x"[..]).unwrap();
            builder.into_inner().unwrap().finish().unwrap();
            assert!(extract(File::open(path).unwrap(), &scratch, LIMITS).is_err());
            assert!(!scratch.join(MODEL_FILES[0]).exists());
        }
    }

    #[test]
    fn failed_promotion_restores_previous_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = model_dir(tmp.path());
        write_model_files(&dest);
        assert!(replace_model(&tmp.path().join("missing"), &dest).is_err());
        assert!(status(tmp.path()).installed);
        assert_eq!(fs::read_dir(models_dir(tmp.path())).unwrap().count(), 1);
    }

    #[test]
    fn delete_clears_install() {
        let tmp = tempfile::tempdir().unwrap();
        write_model_files(&model_dir(tmp.path()));
        delete(tmp.path()).unwrap();
        assert!(!status(tmp.path()).installed);
    }
}

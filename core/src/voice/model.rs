//! Model file management: install status, local-archive import, deletion.
//!
//! An install is only ever judged by "all five files present and non-empty",
//! so a cancelled download or a botched import can never masquerade as an
//! installed model.

use std::path::Path;

use super::{MODEL_FILES, model_dir};

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
        match std::fs::metadata(dir.join(name)) {
            Ok(m) if m.len() > 0 => size += m.len(),
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
    let file = std::fs::File::open(archive_path).map_err(|e| format!("Cannot open archive: {e}"))?;
    let decoder = bzip2::read::BzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    unpack_and_install(&mut archive, app_data_dir)
}

/// Shared tail of import and download: unpack into a scratch dir, verify the
/// file manifest, then swap it into place atomically.
pub fn unpack_and_install<R: std::io::Read>(archive: &mut tar::Archive<R>, app_data_dir: &Path) -> Result<(), String> {
    let dest = model_dir(app_data_dir);
    let scratch = dest.with_extension("tmp");
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).map_err(|e| format!("Cannot create directory: {e}"))?;

    let result = (|| {
        archive
            .unpack(&scratch)
            .map_err(|e| format!("Not a valid model archive: {e}"))?;

        // The upstream tarball nests everything under the model-id directory;
        // a hand-rolled mirror might not. Accept either shape.
        let root = if scratch.join(MODEL_FILES[0]).is_file() {
            scratch.clone()
        } else {
            find_model_root(&scratch).ok_or("Archive does not contain the expected model files")?
        };
        for name in MODEL_FILES {
            let ok = std::fs::metadata(root.join(name)).is_ok_and(|m| m.len() > 0);
            if !ok {
                return Err(format!("Archive is missing {name}"));
            }
        }

        let _ = std::fs::remove_dir_all(&dest);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("Cannot create directory: {e}"))?;
        }
        std::fs::rename(&root, &dest).map_err(|e| format!("Cannot move model into place: {e}"))?;
        Ok(())
    })();

    let _ = std::fs::remove_dir_all(&scratch);
    result
}

fn find_model_root(scratch: &Path) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(scratch).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && path.join(MODEL_FILES[0]).is_file() {
            return Some(path);
        }
    }
    None
}

/// Remove the installed model. The caller must also drop the cached engine.
pub fn delete(app_data_dir: &Path) -> Result<(), String> {
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

    #[test]
    fn delete_clears_install() {
        let tmp = tempfile::tempdir().unwrap();
        write_model_files(&model_dir(tmp.path()));
        delete(tmp.path()).unwrap();
        assert!(!status(tmp.path()).installed);
    }
}

//! Managed clean-original storage for AI-generated images and videos.
//!
//! Layout: `<data_dir>/media_generations/<generation_id>/<index>.<ext>`
//!
//! Strictly separated from user uploads (`files/`), so AI-generated originals
//! carry independent lifecycle, provenance, and future compliance rules.

use std::path::{Component, Path, PathBuf};

pub const MEDIA_GENERATIONS_DIR: &str = "media_generations";

/// Owns the `<data_dir>/media_generations/` directory tree.
#[derive(Debug, Clone)]
pub struct MediaAssetStore {
    root: PathBuf,
}

impl MediaAssetStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join(MEDIA_GENERATIONS_DIR),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Persist one clean original binary into
    /// `<root>/<generation_id>/<index>.<ext>`.
    ///
    /// Returns `(absolute_path, relative_path, file_uri)`.
    pub async fn write_original(
        &self,
        generation_id: &str,
        index: usize,
        mime_type: &str,
        bytes: &[u8],
    ) -> Result<(PathBuf, String, String), String> {
        validate_id_segment(generation_id)?;
        let ext = extension_for_mime(mime_type);
        let gen_dir = self.root.join(generation_id);
        tokio::fs::create_dir_all(&gen_dir)
            .await
            .map_err(|err| format!("failed to create media generation directory: {err}"))?;

        let file_name = format!("{index:02}.{ext}");
        let abs_path = gen_dir.join(&file_name);
        tokio::fs::write(&abs_path, bytes)
            .await
            .map_err(|err| format!("failed to write generated media original: {err}"))?;

        let relative_path = format!("{generation_id}/{file_name}");
        let file_uri = path_to_file_uri(&abs_path);
        Ok((abs_path, relative_path, file_uri))
    }

    /// Resolve a stored relative path back to an absolute path under `root`,
    /// rejecting any path traversal outside `media_generations/`.
    pub fn resolve_relative(&self, relative_path: &str) -> Result<PathBuf, String> {
        let rel = Path::new(relative_path);
        if rel.is_absolute() {
            return Err("media relative path must not be absolute".into());
        }
        for comp in rel.components() {
            match comp {
                Component::Normal(_) => {}
                _ => return Err("media relative path contains disallowed traversal segments".into()),
            }
        }
        Ok(self.root.join(rel))
    }

    /// Remove the managed directory for `generation_id` if present.
    pub async fn delete_generation(&self, generation_id: &str) -> Result<(), String> {
        validate_id_segment(generation_id)?;
        let gen_dir = self.root.join(generation_id);
        if gen_dir.exists() {
            tokio::fs::remove_dir_all(&gen_dir)
                .await
                .map_err(|err| format!("failed to delete media generation files: {err}"))?;
        }
        Ok(())
    }
}

fn validate_id_segment(id: &str) -> Result<(), String> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(format!("invalid generation id segment `{id}`"));
    }
    Ok(())
}

pub fn extension_for_mime(mime: &str) -> &'static str {
    match mime.to_ascii_lowercase().as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        _ => "bin",
    }
}

fn path_to_file_uri(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    if raw.starts_with('/') {
        format!("file://{raw}")
    } else {
        format!("file:///{raw}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_resolves_and_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaAssetStore::new(dir.path());
        let (abs, rel, uri) = store
            .write_original("gen-123", 0, "image/png", b"png-bytes")
            .await
            .unwrap();
        assert!(abs.is_file());
        assert_eq!(rel, "gen-123/00.png");
        assert!(uri.starts_with("file:///") || uri.starts_with("file://"));
        assert_eq!(store.resolve_relative(&rel).unwrap(), abs);
        assert!(store.resolve_relative("../secrets.bin").is_err());
        assert!(store.delete_generation("gen-123").await.is_ok());
        assert!(!abs.exists());
    }
}

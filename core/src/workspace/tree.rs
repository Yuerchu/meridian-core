//! One level of the project tree at a time.
//!
//! On-demand rather than a whole-tree walk: a monorepo's full listing is
//! hundreds of thousands of entries, and the panel only ever shows the levels
//! somebody opened. `ignore::WalkBuilder` does the gitignore reading — the same
//! crate `glob_files` already leans on — with `parents(true)` so a subdirectory
//! listing still honours the `.gitignore` levels above it.

use std::path::Path;

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct TreeEntry {
    pub name: String,
    /// Relative to the workspace root, `/`-separated on every platform so the
    /// frontend never parses `\`.
    pub rel_path: String,
    pub is_dir: bool,
}

/// List the immediate children of `rel_dir` under `root`.
///
/// `rel_dir` empty or `None`-like means the root itself. The directory is
/// verified against the root before it is read — a panel request is nobody
/// asking permission, so the boundary has to hold here, not in the UI.
pub fn list_dir(root: &Path, rel_dir: &str) -> Result<Vec<TreeEntry>, String> {
    let joined = if rel_dir.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel_dir)
    };
    let dir = crate::tools::verified::verify_path(&joined, Some(root)).map_err(|e| e.message())?;

    let mut entries: Vec<TreeEntry> = Vec::new();
    let walker = ignore::WalkBuilder::new(&dir)
        .max_depth(Some(1))
        // Hidden files are shown — dotfiles are project files here — but what
        // gitignore hides stays hidden, matching what the journal snapshots.
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .build();

    for item in walker {
        let item = item.map_err(|e| e.to_string())?;
        // Depth 0 is the directory being listed.
        if item.depth() == 0 {
            continue;
        }
        let name = item.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        // Through `metadata()` rather than the entry's own file type: the
        // entry reports a symlink as a symlink, and a link to a directory
        // would be drawn as a file with no way to expand it. Following the
        // link here only decides the icon and the affordance — expanding it
        // still goes through `verify_path`, which refuses targets outside
        // the root.
        let is_dir = item
            .path()
            .metadata()
            .map(|m| m.is_dir())
            .unwrap_or_else(|_| item.file_type().is_some_and(|t| t.is_dir()));
        let rel_path = if rel_dir.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", rel_dir.trim_end_matches('/'), name)
        };
        entries.push(TreeEntry { name, rel_path, is_dir });
    }

    // Directories first, then names, which is what every file browser does and
    // therefore what reads as "sorted" at all.
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x").unwrap();
    }

    #[test]
    fn lists_one_level_dirs_first() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::tools::verified::resolve_root(dir.path()).unwrap();
        touch(&root.join("b.txt"));
        touch(&root.join("sub/inner.txt"));
        touch(&root.join(".hidden"));

        let entries = list_dir(&root, "").unwrap();
        let names: Vec<(&str, bool)> = entries.iter().map(|e| (e.name.as_str(), e.is_dir)).collect();
        assert_eq!(names, vec![("sub", true), (".hidden", false), ("b.txt", false)]);
        // One level only: sub/inner.txt is not in this listing.
        assert!(entries.iter().all(|e| !e.rel_path.contains("inner")));
    }

    /// The gitignore of a *parent* level must apply when a subdirectory is
    /// listed — `parents(true)` is load-bearing, and this pins it.
    #[test]
    fn parent_gitignore_reaches_a_subdirectory_listing() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::tools::verified::resolve_root(dir.path()).unwrap();
        // WalkBuilder only reads ignore files inside a repository.
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        touch(&root.join("sub/keep.txt"));
        touch(&root.join("sub/noise.log"));

        let entries = list_dir(&root, "sub").unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["keep.txt"], "noise.log should be gitignored");
        assert_eq!(entries[0].rel_path, "sub/keep.txt");
    }

    #[test]
    fn escaping_the_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::tools::verified::resolve_root(dir.path()).unwrap();
        assert!(list_dir(&root, "../..").is_err());
    }
}

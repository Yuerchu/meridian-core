//! The shadow file journal: who changed which file, recorded outside git.
//!
//! Modelled on Claude Code's file-history — a local sidecar store the
//! repository never sees — and grown into an attribution record: every capture
//! path appends `(observed old, new, who)` to a per-file version chain, and
//! blame walks the chain to say which conversation wrote which line. The two
//! disciplines everything here serves:
//!
//! - **Never misattribute.** A change the journal did not see becomes an
//!   `external` version with no conversation on it, inserted by the append
//!   itself whenever the observed old content does not match the chain head.
//!   Every failure direction is "attribute less", never "attribute wrong".
//! - **The repository stays clean.** No trailers, no notes, nothing on disk
//!   inside the project. The journal lives under `{app_data_dir}/file-journal/`
//!   and outlives conversations — deleting a chat must not unwrite who made a
//!   change, so cleanup is its own, explicit operation.
//!
//! Blob storage copies the voice corpus shape (content-addressed, bytes before
//! rows, staging + atomic rename) minus the fencing — see `blobs.rs` for why
//! content addressing makes fencing unnecessary here.

pub mod blame;
pub mod blobs;
pub mod capture;

use std::path::Path;

/// The journal's spelling of a path.
///
/// This is not `project::normalize_path`. That one compares working
/// directories people typed, so it trims whitespace a picker or another
/// program's cwd might have grown. A journal key is a filename: on Unix,
/// `file` and `file ` (trailing space) are two files, and folding them
/// merges two chains — misattribution. Windows still folds case and
/// separators because the filesystem does, and a trailing slash is stripped
/// so a directory is one key whether or not it was written with one.
///
/// `None` for a path that is not valid UTF-8: a lossy conversion can map two
/// *distinct* non-UTF-8 names onto one string, which would merge their chains
/// and attribute one file's lines to the other's editors. Such a file is not
/// journalled at all — the same treatment non-UTF-8 *content* gets, and the
/// same direction every journal failure takes: attribute less, never wrong.
pub fn norm_path(path: &Path) -> Option<String> {
    Some(normalize_file_key(path.to_str()?))
}

pub(crate) fn normalize_file_key(path: &str) -> String {
    if cfg!(windows) {
        path.replace('\\', "/").trim_end_matches('/').to_lowercase()
    } else {
        path.trim_end_matches('/').to_string()
    }
}

/// Where the journal keeps its blobs, under the app's data directory.
///
/// Deliberately not under `files/<conversation_id>/`: deleting a conversation
/// `remove_dir_all`s that tree, and the journal is exactly the record that
/// must survive it.
pub fn journal_root(app_data_dir: &Path) -> std::path::PathBuf {
    app_data_dir.join("file-journal")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_trailing_space_is_a_distinct_unix_key() {
        use std::path::Path;
        assert_ne!(normalize_file_key("/tmp/file"), normalize_file_key("/tmp/file "));
        assert_ne!(
            norm_path(Path::new("/tmp/file")).unwrap(),
            norm_path(Path::new("/tmp/file ")).unwrap()
        );
    }

    #[test]
    fn a_trailing_slash_is_the_same_key() {
        assert_eq!(normalize_file_key("/tmp/dir"), normalize_file_key("/tmp/dir/"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_keys_fold_case_and_separators() {
        assert_eq!(normalize_file_key(r"C:\Repo\A.rs"), normalize_file_key("c:/repo/a.rs"));
    }
}

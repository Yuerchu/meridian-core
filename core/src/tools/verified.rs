//! Opening a file and confirming what we actually opened.
//!
//! Every file tool has to answer one question before it does anything: is this
//! inside the project? The tempting way to answer it is to look at the path the
//! model handed us. That does not work, and it cannot be made to work. The same
//! file can be spelled as a symlink, a junction, an 8.3 short name
//! (`PROGRA~1`), a `\\?\` or `\\.\` prefixed path, an alternate data stream, a
//! UNC or `\\wsl$\` share, a device name, a drive-relative path (`C:foo`), with
//! trailing dots and spaces that Win32 strips on the way in, in any casing, or
//! through a hard link. Recognising all of those is a blocklist, and the list
//! has no end.
//!
//! So this module does not read the path string to decide. It opens the path,
//! asks the kernel which object the handle landed on, and compares *that*. Every
//! spelling above collapses because the kernel already collapsed it; nothing
//! here has to know any of them exist. The handle then stays open and does the
//! I/O, so there is no second resolution that could land somewhere else.
//!
//! One thing this deliberately does not defend against: hard links. A hard link
//! inside the project pointing at a file outside it is indistinguishable from
//! the real thing at the filesystem level — both names are equally the file.
//! Codex hit the same wall and drew the same conclusion (`core/src/safety.rs`):
//! a path check decides whether something may skip approval, it is not the
//! security boundary. The boundary is the OS sandbox.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};

/// Why a path was refused. Kept separate from the message so callers can tell a
/// policy refusal from a missing file without matching on strings.
#[derive(Debug)]
pub enum AccessError {
    /// The path resolved to something outside the allowed area.
    Outside {
        requested: PathBuf,
        real: PathBuf,
    },
    /// `..` climbed above the filesystem root.
    EscapesRoot {
        requested: PathBuf,
    },
    Io {
        path: PathBuf,
        source: io::Error,
    },
}

impl AccessError {
    /// What the model is told. Deliberately does not include the resolved real
    /// path on refusal: if a symlink points somewhere sensitive, echoing the
    /// target back is itself the leak the refusal was meant to prevent.
    pub fn message(&self) -> String {
        match self {
            Self::Outside { requested, .. } => format!(
                "Access denied: '{}' resolves to a location outside the project directory",
                requested.display()
            ),
            Self::EscapesRoot { requested } => {
                format!("Access denied: '{}' escapes the filesystem root", requested.display())
            }
            Self::Io { path, source } => format!("cannot access '{}': {}", path.display(), source),
        }
    }
}

impl From<AccessError> for String {
    fn from(e: AccessError) -> String {
        e.message()
    }
}

/// Fold `.` and `..` away without touching the filesystem.
///
/// This runs *before* the path is opened, for the one case the kernel cannot
/// help with: a path that does not exist yet. A write creating
/// `subdir/../../../elsewhere/new.txt` has nothing to open, so the lexical form
/// is the only form there is. Returns None when `..` climbs past the root.
pub fn lexical_normalize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    let mut depth = 0usize;
    for comp in path.components() {
        match comp {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    return None;
                }
                out.pop();
                depth -= 1;
            }
            Component::Normal(seg) => {
                out.push(seg);
                depth += 1;
            }
        }
    }
    Some(out)
}

/// A file that has been opened and confirmed to sit inside the allowed area.
///
/// Holding the `File` is the point: the check applies to this handle, and the
/// caller does its I/O through this handle, so nothing can be swapped in
/// between. Handing back a `PathBuf` instead would reopen the question.
#[derive(Debug)]
pub struct VerifiedFile {
    file: File,
    real: PathBuf,
    /// Whether the file already existed when the handle was opened. Carried
    /// from `verified_anchor`'s missing-components walk, because it cannot be
    /// recovered afterwards: a `create(true)` open looks identical over a file
    /// it just made and an empty file that was already there — and the journal
    /// records those as different histories (no old state vs an empty one).
    pre_existed: bool,
}

impl VerifiedFile {
    /// The handle plus the path as the OS reports it — canonical casing, links
    /// followed, aliases collapsed. That path is for logging and for messages
    /// back to the model; the string that was requested is not what was opened.
    pub fn into_parts(self) -> (File, PathBuf) {
        (self.file, self.real)
    }

    /// Whether the open found the file already there. See the field note.
    pub fn pre_existed(&self) -> bool {
        self.pre_existed
    }
}

/// Open an existing file for reading, confirming it lands inside `within`.
///
/// `within` is the project directory, already resolved through this same
/// machinery by the caller; `None` means no directory restriction applies (the
/// session is not bound to a project, in which case the tool asked for approval
/// instead).
pub fn open_read(requested: &Path, within: Option<&Path>) -> Result<VerifiedFile, AccessError> {
    let lexical = lexical_normalize(requested).ok_or_else(|| AccessError::EscapesRoot {
        requested: requested.to_path_buf(),
    })?;
    // Opened readable rather than query-only, because this handle is the one
    // that will do the reading. Verifying one handle and reading through
    // another would put the resolution back in play.
    let file = open_readable(&lexical).map_err(|e| AccessError::Io {
        path: lexical.clone(),
        source: e,
    })?;
    let real = real_path_of(&file).map_err(|e| AccessError::Io {
        path: lexical.clone(),
        source: e,
    })?;
    confirm_within(requested, &real, within)?;
    Ok(VerifiedFile {
        file,
        real,
        pre_existed: true,
    })
}

/// Open a file for writing, creating it if absent, confirming it lands inside
/// `within` *before* any content is destroyed.
///
/// The order matters. The file is opened without `O_TRUNC`, verified, and only
/// then truncated — so a path that turns out to be a link pointing outside the
/// project is refused with the target still intact. Truncating first and
/// checking after would destroy the file we were trying to protect.
pub fn open_write(requested: &Path, within: Option<&Path>) -> Result<VerifiedFile, AccessError> {
    let lexical = lexical_normalize(requested).ok_or_else(|| AccessError::EscapesRoot {
        requested: requested.to_path_buf(),
    })?;

    // Anchor first, then build downward from what the OS confirmed. Creating
    // the directories before checking would already have written into whatever
    // the parent turned out to be — the refusal would come too late to matter.
    let (anchor, missing) = verified_anchor(requested, &lexical, within)?;
    let mut target = anchor;
    for seg in &missing {
        target.push(seg);
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AccessError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }

    let file = open_for_write_untruncated(&target).map_err(|e| AccessError::Io {
        path: target.clone(),
        source: e,
    })?;
    let real = real_path_of(&file).map_err(|e| AccessError::Io {
        path: target.clone(),
        source: e,
    })?;
    // Re-checked against the handle rather than trusting the parent check: the
    // name may have been a link to somewhere else entirely, and between the two
    // opens the parent could have been replaced.
    confirm_within(requested, &real, within)?;
    Ok(VerifiedFile {
        file,
        real,
        // The anchor walk found every component present exactly when the file
        // was already there; `create(true)` then erases the distinction.
        pre_existed: missing.is_empty(),
    })
}

/// Walk up to the deepest ancestor that exists, confirm *that* is inside the
/// allowed area, and hand back the components below it that do not exist yet.
///
/// This is what makes a not-yet-created file checkable at all. The kernel can
/// only answer about things that exist, so we ask it about the deepest thing
/// that does. The leftover components are safe to append because
/// `lexical_normalize` has already removed every `.` and `..` — they are plain
/// names, and a plain name cannot climb back out.
fn verified_anchor(
    requested: &Path,
    lexical: &Path,
    within: Option<&Path>,
) -> Result<(PathBuf, Vec<std::ffi::OsString>), AccessError> {
    let mut missing = Vec::new();
    let mut cur = lexical.to_path_buf();
    loop {
        match open_for_query(&cur) {
            Ok(handle) => {
                let real = real_path_of(&handle).map_err(|e| AccessError::Io {
                    path: cur.clone(),
                    source: e,
                })?;
                confirm_within(requested, &real, within)?;
                missing.reverse();
                return Ok((real, missing));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let Some(name) = cur.file_name() else {
                    return Err(AccessError::Io { path: cur, source: e });
                };
                missing.push(name.to_os_string());
                let Some(parent) = cur.parent().map(Path::to_path_buf) else {
                    return Err(AccessError::Io { path: cur, source: e });
                };
                if parent.as_os_str().is_empty() {
                    return Err(AccessError::Io { path: cur, source: e });
                }
                cur = parent;
            }
            Err(e) => return Err(AccessError::Io { path: cur, source: e }),
        }
    }
}

fn confirm_within(requested: &Path, real: &Path, within: Option<&Path>) -> Result<(), AccessError> {
    let Some(root) = within else { return Ok(()) };
    if real.starts_with(root) {
        return Ok(());
    }
    // The only record that a traversal was attempted at all. The real path is
    // logged but never returned to the model.
    tracing::warn!(
        requested = %requested.display(),
        resolved_outside = true,
        "file access denied"
    );
    Err(AccessError::Outside {
        requested: requested.to_path_buf(),
        real: real.to_path_buf(),
    })
}

/// Create a file, failing if anything is already there.
///
/// The exclusive create is what makes "add" mean add. Checking for absence and
/// then creating are two steps, and a file appearing between them turns the
/// check into approval for overwriting something it never saw. It also settles
/// the symlink case for free: an exclusive create refuses to follow one.
pub fn open_create_new(requested: &Path, within: Option<&Path>) -> Result<VerifiedFile, AccessError> {
    let lexical = lexical_normalize(requested).ok_or_else(|| AccessError::EscapesRoot {
        requested: requested.to_path_buf(),
    })?;
    let (anchor, missing) = verified_anchor(requested, &lexical, within)?;
    let mut target = anchor;
    for seg in &missing {
        target.push(seg);
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AccessError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }
    let file = open_exclusive(&target).map_err(|e| AccessError::Io {
        path: target.clone(),
        source: e,
    })?;
    let real = real_path_of(&file).map_err(|e| AccessError::Io {
        path: target,
        source: e,
    })?;
    confirm_within(requested, &real, within)?;
    Ok(VerifiedFile {
        file,
        real,
        // An exclusive create that succeeded made the file; anything already
        // there would have refused the open.
        pre_existed: false,
    })
}

/// Open an existing file for reading and writing, refusing to create it.
///
/// For edits, which are only meaningful against contents that already exist. If
/// this created the file the way `open_write` does, an edit whose `old_string`
/// does not match would fail *after* having left an empty file behind — a
/// refusal that still changed the workspace.
pub fn open_edit(requested: &Path, within: Option<&Path>) -> Result<VerifiedFile, AccessError> {
    let lexical = lexical_normalize(requested).ok_or_else(|| AccessError::EscapesRoot {
        requested: requested.to_path_buf(),
    })?;
    let file = open_existing_rw(&lexical).map_err(|e| AccessError::Io {
        path: lexical.clone(),
        source: e,
    })?;
    let real = real_path_of(&file).map_err(|e| AccessError::Io {
        path: lexical.clone(),
        source: e,
    })?;
    confirm_within(requested, &real, within)?;
    Ok(VerifiedFile {
        file,
        real,
        pre_existed: true,
    })
}

/// Verify a path and return what the OS says it really is, without keeping the
/// handle open.
///
/// For the operations that have no handle-based form in std — `unlink`,
/// `rename`, reading a directory. There the check and the call are necessarily
/// two separate resolutions, so a window exists between them. That is tolerable
/// only because every one of those operations requires approval: a person is
/// looking at the window. Anything that may skip approval must use `open_read`
/// or `open_write`, which do the I/O through the handle they checked.
pub fn verify_path(requested: &Path, within: Option<&Path>) -> Result<PathBuf, AccessError> {
    let lexical = lexical_normalize(requested).ok_or_else(|| AccessError::EscapesRoot {
        requested: requested.to_path_buf(),
    })?;
    let (anchor, missing) = verified_anchor(requested, &lexical, within)?;
    let mut out = anchor;
    for seg in &missing {
        out.push(seg);
    }
    Ok(out)
}

/// Resolve a directory the same way, for use as the `within` root. Returns the
/// OS's own spelling so later comparisons are against like for like.
pub fn resolve_root(dir: &Path) -> Result<PathBuf, AccessError> {
    let lexical = lexical_normalize(dir).ok_or_else(|| AccessError::EscapesRoot {
        requested: dir.to_path_buf(),
    })?;
    let handle = open_for_query(&lexical).map_err(|e| AccessError::Io {
        path: lexical.clone(),
        source: e,
    })?;
    real_path_of(&handle).map_err(|e| AccessError::Io {
        path: lexical,
        source: e,
    })
}

// ---------------------------------------------------------------------------
// Platform layer: open a handle, and ask the OS what it is.
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    // Lets the same call open directories as well as files. Without it,
    // CreateFileW refuses directories outright.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    const VOLUME_NAME_DOS: u32 = 0x0;

    /// Open purely to ask about it. `access_mode(0)` requests no read or write
    /// rights at all, which still permits the metadata query and works on files
    /// the user cannot otherwise open.
    pub fn open_for_query(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .access_mode(0)
            .share_mode(SHARE_ALL)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
    }

    pub fn open_readable(path: &Path) -> io::Result<File> {
        OpenOptions::new().read(true).share_mode(SHARE_ALL).open(path)
    }

    pub fn open_existing_rw(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(SHARE_ALL)
            .open(path)
    }

    pub fn open_exclusive(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(SHARE_ALL)
            .open(path)
    }

    pub fn open_for_write_untruncated(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .share_mode(SHARE_ALL)
            .open(path)
    }

    pub fn real_path_of(file: &File) -> io::Result<PathBuf> {
        use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
        let handle = file.as_raw_handle() as isize;
        // Called twice on purpose: the first call reports the buffer size it
        // needs (including the terminator), the second fills it.
        let needed = unsafe { GetFinalPathNameByHandleW(handle, std::ptr::null_mut(), 0, VOLUME_NAME_DOS) };
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u16; needed as usize];
        let written = unsafe { GetFinalPathNameByHandleW(handle, buf.as_mut_ptr(), buf.len() as u32, VOLUME_NAME_DOS) };
        if written == 0 || written as usize >= buf.len() {
            return Err(io::Error::last_os_error());
        }
        buf.truncate(written as usize);
        Ok(strip_verbatim(PathBuf::from(std::ffi::OsString::from_wide(&buf))))
    }

    /// GetFinalPathNameByHandle always answers in `\\?\` form. Keeping that
    /// prefix would leak into every message the model sees, so drop it where
    /// dropping it is lossless — which is everywhere except paths long enough
    /// that the prefix is what makes them usable.
    fn strip_verbatim(p: PathBuf) -> PathBuf {
        const MAX_COMFORTABLE: usize = 240;
        let s = p.to_string_lossy();
        let stripped = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{rest}")
        } else if let Some(rest) = s.strip_prefix(r"\\?\") {
            rest.to_string()
        } else {
            return p;
        };
        if stripped.len() <= MAX_COMFORTABLE {
            PathBuf::from(stripped)
        } else {
            p
        }
    }
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    pub fn open_for_query(path: &Path) -> io::Result<File> {
        // O_PATH opens the object without reading it, so this works on files
        // the user has no read permission for and on directories alike. macOS
        // has no equivalent, where a plain read-only open covers both.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            OpenOptions::new().read(true).custom_flags(libc::O_PATH).open(path)
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            OpenOptions::new().read(true).open(path)
        }
    }

    pub fn open_readable(path: &Path) -> io::Result<File> {
        OpenOptions::new().read(true).open(path)
    }

    pub fn open_existing_rw(path: &Path) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).open(path)
    }

    pub fn open_exclusive(path: &Path) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).create_new(true).open(path)
    }

    pub fn open_for_write_untruncated(path: &Path) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).create(true).open(path)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn real_path_of(file: &File) -> io::Result<PathBuf> {
        std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
    }

    #[cfg(target_os = "macos")]
    pub fn real_path_of(file: &File) -> io::Result<PathBuf> {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        const F_GETPATH: libc::c_int = 50;
        let mut buf = vec![0u8; libc::PATH_MAX as usize];
        let rc = unsafe { libc::fcntl(file.as_raw_fd(), F_GETPATH, buf.as_mut_ptr()) };
        if rc == -1 {
            return Err(io::Error::last_os_error());
        }
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        buf.truncate(len);
        Ok(PathBuf::from(OsString::from_vec(buf)))
    }
}

use imp::{open_exclusive, open_existing_rw, open_for_query, open_for_write_untruncated, open_readable, real_path_of};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_folds_dot_and_parent() {
        let p = lexical_normalize(Path::new("/a/b/../c/./d")).unwrap();
        assert_eq!(p, PathBuf::from("/a/c/d"));
    }

    #[test]
    fn lexical_refuses_to_climb_past_root() {
        assert!(lexical_normalize(Path::new("/a/../../etc/passwd")).is_none());
    }

    #[test]
    fn read_inside_root_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let root = resolve_root(dir.path()).unwrap();
        let target = dir.path().join("note.txt");
        std::fs::write(&target, "hi").unwrap();

        let (_, real) = open_read(&target, Some(&root)).unwrap().into_parts();
        assert!(real.starts_with(&root));
    }

    #[test]
    fn read_outside_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = resolve_root(dir.path()).unwrap();
        let target = outside.path().join("secret.txt");
        std::fs::write(&target, "s").unwrap();

        let err = open_read(&target, Some(&root)).unwrap_err();
        assert!(matches!(err, AccessError::Outside { .. }));
    }

    #[test]
    fn parent_traversal_is_refused_even_through_a_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = resolve_root(dir.path()).unwrap();
        // The middle component does not exist, which is exactly the case a
        // canonicalize-then-compare check gets wrong: there is nothing to
        // canonicalize, so the `..` survives into the comparison.
        let sneaky = dir.path().join("ghost/../../escaped.txt");
        let err = open_write(&sneaky, Some(&root)).unwrap_err();
        assert!(matches!(err, AccessError::Outside { .. }), "got {err:?}");
    }

    #[test]
    fn write_refuses_before_truncating() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = resolve_root(dir.path()).unwrap();
        let victim = outside.path().join("important.txt");
        std::fs::write(&victim, "must survive").unwrap();

        let _ = open_write(&victim, Some(&root)).unwrap_err();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "must survive");
    }

    #[test]
    fn a_symlink_out_of_the_project_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = resolve_root(dir.path()).unwrap();
        let secret = outside.path().join("id_rsa");
        std::fs::write(&secret, "PRIVATE KEY").unwrap();

        let link = dir.path().join("innocent.txt");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&secret, &link).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(&secret, &link).is_ok();
        if !made {
            // Windows needs developer mode or elevation to create symlinks.
            return;
        }

        let err = open_read(&link, Some(&root)).unwrap_err();
        assert!(matches!(err, AccessError::Outside { .. }));
    }

    /// Symlink creation needs developer mode or elevation, so the test above
    /// quietly skips on a stock Windows box. Junctions need no privilege at
    /// all, which makes them the redirection a caller could actually reach for
    /// — this one always runs.
    #[cfg(windows)]
    #[test]
    fn a_junction_out_of_the_project_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = resolve_root(dir.path()).unwrap();
        std::fs::write(outside.path().join("secret.txt"), "PRIVATE KEY").unwrap();

        let junction = dir.path().join("data");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(outside.path())
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J did not succeed");

        let err = open_read(&junction.join("secret.txt"), Some(&root)).unwrap_err();
        assert!(matches!(err, AccessError::Outside { .. }), "got {err:?}");
    }
}

//! Asking git what changed, as a subprocess.
//!
//! A subprocess rather than `git2`/`gix`: status and diff are a handful of
//! porcelain calls, the machine that has a project in git has git, and the
//! machine that does not needs the graceful "no git view" path either way —
//! which a library dependency would not remove. The invocation pattern follows
//! `container.rs`: piped stdio, `kill_on_drop`, a timeout, and `CREATE_NO_WINDOW`
//! so nothing flashes a console.
//!
//! Output is read with a hard cap instead of `output()`: a diff can be
//! arbitrarily large, and the timeout alone would still let it allocate all of
//! it before being cut off.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::OnceCell;

/// The command's own deadline. Status and diff on a warm repo are milliseconds;
/// ten seconds is "the repo is on a dead network share", at which point an
/// error beats a spinner.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// How much of stdout is kept. Beyond this the panel would truncate anyway,
/// so the rest is never read off the pipe.
const MAX_OUTPUT_BYTES: u64 = 1024 * 1024;

/// What the working tree looks like, or why it cannot be asked.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GitStatus {
    Ok {
        branch: Option<String>,
        files: Vec<StatusEntry>,
    },
    /// The machine has no `git`. The tree and the viewer work without one.
    NoGit,
    NotRepo,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusEntry {
    pub path: String,
    pub status: GitFileStatus,
    pub renamed_from: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GitFileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

#[derive(Debug, Clone, Serialize)]
pub struct GitDiffResult {
    pub diff_text: String,
    pub truncated: bool,
}

struct GitOutput {
    status: i32,
    stdout: String,
    stderr: String,
    truncated: bool,
}

/// Whether `git` answers at all, asked of the OS once per process.
///
/// Cached because every panel refresh asks, and a missing binary fails with the
/// same answer every time until somebody installs one — at which point they
/// will restart or we can live with the stale `false` until then.
pub async fn git_available() -> bool {
    static AVAILABLE: OnceCell<bool> = OnceCell::const_new();
    *AVAILABLE
        .get_or_init(|| async { matches!(run_in(Path::new("."), &["--version"]).await, Ok(o) if o.status == 0) })
        .await
}

/// Whether `root` sits inside a git work tree. A project directory that is a
/// subdirectory of a repository counts, which is what diff-against-HEAD needs.
pub async fn is_repo(root: &Path) -> bool {
    match run_in(root, &["rev-parse", "--is-inside-work-tree"]).await {
        Ok(out) => out.status == 0 && out.stdout.trim() == "true",
        Err(_) => false,
    }
}

pub async fn status(root: &Path) -> Result<GitStatus, String> {
    if !git_available().await {
        return Ok(GitStatus::NoGit);
    }
    // Scoped with a pathspec: the workspace may be a subdirectory of a larger
    // work tree, and unscoped status would report siblings of the workspace —
    // paths the panel's own tree cannot show, from directories the caller was
    // never granted. `.` is resolved against `-C root`, so it means "this
    // workspace" wherever the repository's own root is.
    let out = run_in(root, &["status", "--porcelain=v2", "--branch", "-z", "--", "."]).await?;
    if out.status != 0 {
        // "not a git repository" is a state the panel draws, not an error.
        if out.stderr.contains("not a git repository") {
            return Ok(GitStatus::NotRepo);
        }
        return Err(format!("git status failed: {}", out.stderr));
    }
    // Porcelain paths are repo-root-relative even under a pathspec, and the
    // panel's world is workspace-relative. `--show-prefix` is the difference.
    let prefix = workspace_prefix(root).await?;
    let (branch, files) = parse_porcelain_v2(&out.stdout);
    let files = files.into_iter().filter_map(|e| relativize(e, &prefix)).collect();
    Ok(GitStatus::Ok { branch, files })
}

/// Where the workspace sits inside its repository (`""` when it is the root),
/// as git spells it: `/`-separated with a trailing slash.
async fn workspace_prefix(root: &Path) -> Result<String, String> {
    let out = run_in(root, &["rev-parse", "--show-prefix"]).await?;
    if out.status != 0 {
        return Err(format!("git rev-parse failed: {}", out.stderr));
    }
    Ok(out.stdout.trim().to_string())
}

/// Rebase one status entry from repo-root-relative onto workspace-relative.
///
/// An entry outside the prefix should not exist under the pathspec; if one
/// appears anyway it is dropped rather than shown under a wrong name — a
/// rename whose *source* is outside the workspace keeps the entry and loses
/// only the origin, which is the true story from the workspace's viewpoint.
fn relativize(mut e: StatusEntry, prefix: &str) -> Option<StatusEntry> {
    e.path = e.path.strip_prefix(prefix)?.to_string();
    e.renamed_from = e
        .renamed_from
        .take()
        .and_then(|f| Some(f.strip_prefix(prefix)?.to_string()));
    Some(e)
}

/// The unified diff for one file, or for everything tracked.
///
/// Against `HEAD`, so staged and unstaged changes read as one delta — the
/// panel answers "what is different from the last commit", not "what is in the
/// index". `--relative` plus the pathspec keep both the coverage and the
/// printed paths inside the workspace, for the same reason `status` is scoped.
pub async fn diff(root: &Path, rel_path: Option<&str>) -> Result<GitDiffResult, String> {
    if let Some(rel) = rel_path {
        // The one caller-supplied path in this module, and `--no-index` below
        // will read whatever it names — so it is confined to the root *here*,
        // before any git invocation, not merely by what the index contains.
        // An absolute path or a `..` escape is refused with the file untouched.
        crate::tools::verified::verify_path(&root.join(rel), Some(root)).map_err(|e| e.message())?;
        if !is_tracked(root, rel).await? {
            return untracked_diff(root, rel).await;
        }
    }
    let mut args = vec!["diff", "--no-color", "--relative", "HEAD"];
    args.push("--");
    args.push(rel_path.unwrap_or("."));
    let out = run_in(root, &args).await?;
    if out.status != 0 {
        // No commits yet, so no HEAD. The "before" of everything is the empty
        // tree, whose id depends on the repository's hash algorithm — asked of
        // git rather than hardcoded, so a sha256 repository answers too.
        // Diffing the work tree against it covers staged and unstaged alike;
        // the index-vs-worktree form would silently drop freshly staged files.
        let empty = empty_tree(root).await?;
        let mut fallback = vec!["diff", "--no-color", "--relative", empty.as_str()];
        fallback.push("--");
        fallback.push(rel_path.unwrap_or("."));
        let second = run_in(root, &fallback).await?;
        if second.status != 0 {
            return Err(format!("git diff failed: {}", out.stderr));
        }
        return Ok(GitDiffResult {
            diff_text: second.stdout,
            truncated: second.truncated,
        });
    }
    Ok(GitDiffResult {
        diff_text: out.stdout,
        truncated: out.truncated,
    })
}

/// The empty tree's object id in this repository's hash algorithm. Computed
/// (`hash-object` over empty stdin, which `run_in` provides) rather than the
/// well-known sha1 constant, which is wrong in a sha256 repository.
async fn empty_tree(root: &Path) -> Result<String, String> {
    let out = run_in(root, &["hash-object", "-t", "tree", "--stdin"]).await?;
    if out.status != 0 {
        return Err(format!("git hash-object failed: {}", out.stderr));
    }
    Ok(out.stdout.trim().to_string())
}

async fn is_tracked(root: &Path, rel: &str) -> Result<bool, String> {
    let out = run_in(root, &["ls-files", "--error-unmatch", "--", rel]).await?;
    Ok(out.status == 0)
}

/// An untracked file drawn as an all-additions diff, through the same renderer
/// every other file uses. Git accepts `/dev/null` as the null side on Windows
/// too — it treats the name specially rather than opening it. Exit code 1 is
/// "there were differences", which for a diff against nothing is success.
///
/// `rel` was verified against the root by `diff` before this is reached;
/// `--no-index` reads straight from the filesystem, so that check is the only
/// thing standing between a remote caller and any file on the host.
async fn untracked_diff(root: &Path, rel: &str) -> Result<GitDiffResult, String> {
    let out = run_in(root, &["diff", "--no-color", "--no-index", "--", "/dev/null", rel]).await?;
    if out.status != 0 && out.status != 1 {
        return Err(format!("git diff --no-index failed: {}", out.stderr));
    }
    Ok(GitDiffResult {
        diff_text: out.stdout,
        truncated: out.truncated,
    })
}

async fn run_in(root: &Path, args: &[&str]) -> Result<GitOutput, String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW: without it every git call flashes a console.
        cmd.creation_flags(0x08000000);
    }

    let run = async {
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("could not run `git`: {e}. Is git installed?"))?;

        // Stdout is capped, stderr is small (error prose) and read whole.
        let mut stderr = Vec::new();
        let out_pipe = child.stdout.take().expect("stdout piped");
        let mut err_pipe = child.stderr.take().expect("stderr piped");
        let (out_read, err_read, waited) = tokio::join!(
            read_capped(out_pipe, MAX_OUTPUT_BYTES as usize),
            err_pipe.read_to_end(&mut stderr),
            child.wait(),
        );
        let (stdout, truncated) = out_read.map_err(|e| format!("reading git output: {e}"))?;
        err_read.map_err(|e| format!("reading git stderr: {e}"))?;
        let status = waited.map_err(|e| format!("waiting for git: {e}"))?;

        Ok(GitOutput {
            status: status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).trim().to_string(),
            truncated,
        })
    };

    tokio::time::timeout(GIT_TIMEOUT, run)
        .await
        .map_err(|_| "`git` did not answer within 10s".to_string())?
}

/// Read a pipe to EOF, keeping only the first `cap` bytes.
///
/// The pipe is drained past the cap rather than abandoned at it: a reader that
/// stops mid-stream leaves git blocked on a full pipe, its exit never comes,
/// and the promised `truncated` result turns into a timeout.
async fn read_capped(mut pipe: impl tokio::io::AsyncRead + Unpin, cap: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = pipe.read(&mut buf).await?;
        if n == 0 {
            return Ok((kept, truncated));
        }
        if kept.len() < cap {
            let take = n.min(cap - kept.len());
            kept.extend_from_slice(&buf[..take]);
            if take < n {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }
}

/// Parse `status --porcelain=v2 --branch -z` output.
///
/// With `-z` every record ends in NUL, and a rename record (`2 …`) is followed
/// by one extra NUL-terminated field: the path it came *from*. That trailing
/// field is the part a line-oriented parser silently mistakes for a second,
/// unchanged file — which is why this is a pure function with canned-string
/// tests rather than a few `split` calls at the call site.
fn parse_porcelain_v2(raw: &str) -> (Option<String>, Vec<StatusEntry>) {
    let mut branch = None;
    let mut files = Vec::new();
    let mut fields = raw.split('\0');

    while let Some(record) = fields.next() {
        if record.is_empty() {
            continue;
        }
        if let Some(rest) = record.strip_prefix("# ") {
            if let Some(head) = rest.strip_prefix("branch.head ") {
                // "(detached)" is git's own spelling; pass it through.
                branch = Some(head.to_string());
            }
            continue;
        }

        let mut parts = record.splitn(2, ' ');
        let kind = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("");
        match kind {
            "1" => {
                // 1 XY sub mH mI mW hH hI path — path is field 8 of `rest`.
                if let Some((xy, path)) = split_fields(rest, 7) {
                    files.push(StatusEntry {
                        path: path.to_string(),
                        status: status_of(xy),
                        renamed_from: None,
                    });
                }
            }
            "2" => {
                // 2 XY sub mH mI mW hH hI Xscore path NUL origPath
                if let Some((_, path)) = split_fields(rest, 8) {
                    let renamed_from = fields.next().map(str::to_string);
                    files.push(StatusEntry {
                        path: path.to_string(),
                        status: GitFileStatus::Renamed,
                        renamed_from,
                    });
                }
            }
            "u" => {
                // u XY sub m1 m2 m3 mW h1 h2 h3 path
                if let Some((_, path)) = split_fields(rest, 9) {
                    files.push(StatusEntry {
                        path: path.to_string(),
                        status: GitFileStatus::Conflicted,
                        renamed_from: None,
                    });
                }
            }
            "?" => files.push(StatusEntry {
                path: rest.to_string(),
                status: GitFileStatus::Untracked,
                renamed_from: None,
            }),
            // "!" (ignored) is not requested; headers were handled above.
            _ => {}
        }
    }
    (branch, files)
}

/// The first field and everything after `skip` space-separated fields.
///
/// Only the leading fields are structured; the final field is the path and may
/// itself contain spaces, so it must be taken as "the rest" rather than split.
fn split_fields(rest: &str, skip: usize) -> Option<(&str, &str)> {
    let mut remainder = rest;
    let mut first = None;
    for i in 0..skip {
        let (field, tail) = remainder.split_once(' ')?;
        if i == 0 {
            first = Some(field);
        }
        remainder = tail;
    }
    Some((first?, remainder))
}

/// One word for an XY pair. The panel does not distinguish staged from
/// unstaged — the diff is against HEAD, so neither does the letter.
fn status_of(xy: &str) -> GitFileStatus {
    let has = |c: char| xy.contains(c);
    if has('D') {
        GitFileStatus::Deleted
    } else if has('A') {
        GitFileStatus::Added
    } else {
        GitFileStatus::Modified
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_status_uses_the_closed_wire_vocabulary() {
        assert_eq!(
            serde_json::to_string(&GitFileStatus::Conflicted).unwrap(),
            r#""conflicted""#
        );
    }

    /// A modified file, an added one, a deletion and an untracked one, as git
    /// actually emits them with `-z`.
    #[test]
    fn ordinary_entries_parse() {
        let raw = concat!(
            "# branch.oid 1234\0",
            "# branch.head main\0",
            "1 .M N... 100644 100644 100644 aaaa bbbb src/lib.rs\0",
            "1 A. N... 000000 100644 100644 0000 cccc src/new.rs\0",
            "1 D. N... 100644 000000 000000 dddd 0000 gone.txt\0",
            "? notes.md\0",
        );
        let (branch, files) = parse_porcelain_v2(raw);
        assert_eq!(branch.as_deref(), Some("main"));
        let got: Vec<(&str, GitFileStatus)> = files.iter().map(|f| (f.path.as_str(), f.status)).collect();
        assert_eq!(
            got,
            vec![
                ("src/lib.rs", GitFileStatus::Modified),
                ("src/new.rs", GitFileStatus::Added),
                ("gone.txt", GitFileStatus::Deleted),
                ("notes.md", GitFileStatus::Untracked),
            ]
        );
    }

    /// The rename record's origin path arrives as a separate NUL field. Read
    /// line-wise it becomes a phantom second entry; read here it becomes
    /// `renamed_from`, and the entry after it is still parsed.
    #[test]
    fn a_rename_consumes_its_origin_field() {
        let raw = concat!(
            "2 R. N... 100644 100644 100644 aaaa aaaa R100 new/name.rs\0",
            "old/name.rs\0",
            "? after.md\0",
        );
        let (_, files) = parse_porcelain_v2(raw);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "new/name.rs");
        assert_eq!(files[0].status, GitFileStatus::Renamed);
        assert_eq!(files[0].renamed_from.as_deref(), Some("old/name.rs"));
        assert_eq!(files[1].path, "after.md");
    }

    /// Paths with spaces are the reason the tail is "the rest", not a field.
    #[test]
    fn a_path_with_spaces_survives() {
        let raw = "1 .M N... 100644 100644 100644 aaaa bbbb my docs/read me.txt\0";
        let (_, files) = parse_porcelain_v2(raw);
        assert_eq!(files[0].path, "my docs/read me.txt");
    }

    /// Repo-root-relative entries become workspace-relative, entries outside
    /// the workspace are dropped, and a rename in from outside keeps the entry
    /// while losing only the origin.
    #[test]
    fn relativize_rebases_and_drops() {
        let entry = |path: &str, from: Option<&str>| StatusEntry {
            path: path.into(),
            status: GitFileStatus::Modified,
            renamed_from: from.map(String::from),
        };
        let inside = relativize(entry("sub/dir/a.rs", None), "sub/dir/").unwrap();
        assert_eq!(inside.path, "a.rs");

        assert!(relativize(entry("elsewhere/b.rs", None), "sub/dir/").is_none());

        let moved_in = relativize(entry("sub/dir/new.rs", Some("other/old.rs")), "sub/dir/").unwrap();
        assert_eq!(moved_in.path, "new.rs");
        assert_eq!(
            moved_in.renamed_from, None,
            "an origin outside the workspace is dropped"
        );

        // A workspace at the repository root has an empty prefix and changes nothing.
        let at_root = relativize(entry("a.rs", Some("b.rs")), "").unwrap();
        assert_eq!(
            (at_root.path.as_str(), at_root.renamed_from.as_deref()),
            ("a.rs", Some("b.rs"))
        );
    }

    /// The caller-supplied path is confined before any git invocation:
    /// `--no-index` would otherwise read whatever an absolute or escaping
    /// path names, remotely. Neither case needs git installed to be refused.
    #[tokio::test]
    async fn diff_refuses_paths_outside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = crate::tools::verified::resolve_root(dir.path()).unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "s").unwrap();

        assert!(diff(&root, Some(secret.to_str().unwrap())).await.is_err());
        assert!(diff(&root, Some("../../etc/hosts")).await.is_err());
    }

    /// The end-to-end shape against a real repository, skipped when the
    /// machine has no git. One repo, workspace = a subdirectory: status and
    /// diff must stay inside it and speak workspace-relative paths; the
    /// unborn-branch fallback must include a freshly *staged* file, which the
    /// index-vs-worktree form silently dropped.
    #[tokio::test]
    async fn against_a_real_repository() {
        if !git_available().await {
            eprintln!("skipping: git not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::tools::verified::resolve_root(dir.path()).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                // The developer's global config must not reach into a test
                // repository. `commit.gpgsign=true` in particular turns every
                // suite run into a pinentry prompt waiting for a hardware key
                // that times out after a minute when nobody touches it — the
                // "unreproducible" failure this suite carried until the cause
                // was caught red-handed. The test's commits are throwaway; the
                // user's own commits keep their signing untouched.
                .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::create_dir_all(repo.join("ws")).unwrap();
        std::fs::write(repo.join("ws/inside.txt"), "one\n").unwrap();
        std::fs::write(repo.join("sibling.txt"), "secret\n").unwrap();
        git(&["add", "-A"]);

        // Unborn branch, everything staged: the empty-tree fallback must
        // still produce content for the staged file.
        let ws = repo.join("ws");
        let whole = diff(&ws, None).await.unwrap();
        assert!(
            whole.diff_text.contains("inside.txt"),
            "staged file missing: {}",
            whole.diff_text
        );
        assert!(
            !whole.diff_text.contains("sibling"),
            "sibling leaked into a scoped diff"
        );

        git(&["commit", "-qm", "init"]);
        std::fs::write(repo.join("ws/inside.txt"), "one\ntwo\n").unwrap();
        std::fs::write(repo.join("sibling.txt"), "changed\n").unwrap();

        // Status scoped to the workspace: workspace-relative paths, no siblings.
        let status = status(&ws).await.unwrap();
        let GitStatus::Ok { files, .. } = status else {
            panic!("expected Ok, got {status:?}")
        };
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["inside.txt"], "got {paths:?}");

        let scoped = diff(&ws, None).await.unwrap();
        assert!(scoped.diff_text.contains("inside.txt"));
        assert!(!scoped.diff_text.contains("sibling"));

        // An untracked file inside the workspace renders as an all-add diff.
        std::fs::write(repo.join("ws/fresh.txt"), "new stuff\n").unwrap();
        let fresh = diff(&ws, Some("fresh.txt")).await.unwrap();
        assert!(fresh.diff_text.contains("new stuff"));
    }

    #[test]
    fn conflicts_are_named() {
        let raw = "u UU N... 100644 100644 100644 100644 a b c both.rs\0";
        let (_, files) = parse_porcelain_v2(raw);
        assert_eq!(files[0].status, GitFileStatus::Conflicted);
        assert_eq!(files[0].path, "both.rs");
    }
}

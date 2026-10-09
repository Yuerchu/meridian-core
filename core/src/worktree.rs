//! Git worktrees for the agent board: one per card, beside the repository.
//!
//! The board gives every task its own checkout so agents working at once do not
//! write into each other's files. A worktree lives in `<repo>.worktrees/`, next
//! to the repository rather than inside it, so it never shows in the
//! repository's own status, search or editor tree.
//!
//! A worktree is added **detached**, at the repository's current `HEAD`, and
//! that commit is only a place to stand. The branch — its name *and* what it
//! starts from — is the agent's to choose once it knows what the work is. A
//! name from a title would be a guess the person lives with in every log, and
//! the default branch is not every project's base: a dev/test/main project
//! defaults to main and starts work from dev. Git refuses a name that is taken
//! and the agent picks another, so no collision handling is needed here.
//!
//! Removal is a person's act. Unless forced it refuses a worktree with
//! uncommitted changes, and one whose `HEAD` holds commits no branch or remote
//! does — an agent that committed before branching — since removing that
//! would leave the work reachable from nothing. A branch the agent made is
//! never deleted with the worktree.
//!
//! These are the writes `workspace::git` deliberately does not make; they go
//! through the same subprocess runner with a longer deadline, since adding a
//! worktree checks out the whole tree.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::tools::verified;
use crate::workspace::git::{self, GitOutput};

/// Checking out or deleting a whole tree, on a large repository.
const WRITE_TIMEOUT: Duration = Duration::from_secs(120);
const READ_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WorktreeError {
    #[error("git is not installed")]
    NoGit,
    #[error("{0} is not inside a git repository")]
    NotRepo(PathBuf),
    #[error("the repository is at the root of a drive, so there is no directory beside it for worktrees")]
    NoParent,
    #[error("{0} already exists")]
    PathExists(PathBuf),
    #[error("worktree name {0:?} may only use letters, digits, '-' and '_'")]
    BadName(String),
    #[error("the worktree has {files} uncommitted change(s)")]
    Dirty { files: usize },
    #[error("the worktree has {commits} commit(s) that are on no branch")]
    Unbranched { commits: usize },
    #[error("git: {0}")]
    Git(String),
}

/// A worktree that was just added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// The worktree's own top level.
    pub root: PathBuf,
    /// Where the project lives inside it: `root` when the project is the whole
    /// repository, a subdirectory when the project is one. This is the
    /// directory an agent works in.
    pub dir: PathBuf,
    /// The commit it was checked out at.
    pub head: String,
}

/// One entry of `git worktree list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub head: Option<String>,
    /// `None` for a detached worktree.
    pub branch: Option<String>,
    /// The repository's own checkout, listed first by git.
    pub is_main: bool,
}

async fn run(root: &Path, args: &[&str], timeout: Duration) -> Result<GitOutput, WorktreeError> {
    git::run_in_with(root, args, timeout).await.map_err(WorktreeError::Git)
}

async fn run_ok(root: &Path, args: &[&str], timeout: Duration) -> Result<String, WorktreeError> {
    let out = run(root, args, timeout).await?;
    if out.status != 0 {
        return Err(WorktreeError::Git(out.stderr));
    }
    Ok(out.stdout.trim_end().to_string())
}

async fn ensure_repo(dir: &Path) -> Result<(), WorktreeError> {
    if !git::git_available().await {
        return Err(WorktreeError::NoGit);
    }
    if !git::is_repo(dir).await {
        return Err(WorktreeError::NotRepo(dir.to_path_buf()));
    }
    Ok(())
}

/// `<repo>.worktrees`, beside the repository's top level.
pub fn worktrees_dir(repo_top: &Path) -> Result<PathBuf, WorktreeError> {
    let (Some(parent), Some(name)) = (repo_top.parent(), repo_top.file_name()) else {
        return Err(WorktreeError::NoParent);
    };
    let mut dir_name = name.to_os_string();
    dir_name.push(".worktrees");
    Ok(parent.join(dir_name))
}

fn check_name(name: &str) -> Result<(), WorktreeError> {
    let ok = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(WorktreeError::BadName(name.to_string()))
    }
}

/// Add a detached worktree named `name` for the repository `project_dir` is in,
/// at `base` (the current `HEAD` when `None`).
///
/// `project_dir` may be a subdirectory of the repository; the returned `dir` is
/// the same subdirectory inside the new worktree, which is where the agent's
/// project is.
pub async fn add_detached(project_dir: &Path, name: &str, base: Option<&str>) -> Result<Worktree, WorktreeError> {
    check_name(name)?;
    ensure_repo(project_dir).await?;
    let top = PathBuf::from(run_ok(project_dir, &["rev-parse", "--show-toplevel"], READ_TIMEOUT).await?);
    let prefix = run_ok(project_dir, &["rev-parse", "--show-prefix"], READ_TIMEOUT).await?;
    let parent = worktrees_dir(&top)?;
    let root = parent.join(name);
    if root.exists() {
        return Err(WorktreeError::PathExists(root));
    }
    std::fs::create_dir_all(&parent).map_err(|e| WorktreeError::Git(format!("creating {}: {e}", parent.display())))?;

    let root_arg = root.to_string_lossy().into_owned();
    let base = base.unwrap_or("HEAD");
    run_ok(&top, &["worktree", "add", "--detach", &root_arg, base], WRITE_TIMEOUT).await?;

    // The spelling the rest of the app compares against: what the OS calls it.
    let root = verified::resolve_root(&root).map_err(|e| WorktreeError::Git(format!("{e:?}")))?;
    let dir = if prefix.is_empty() {
        root.clone()
    } else {
        root.join(&prefix)
    };
    let head = run_ok(&root, &["rev-parse", "HEAD"], READ_TIMEOUT).await?;
    Ok(Worktree { root, dir, head })
}

/// Every worktree of the repository `dir` is in, the main checkout first.
pub async fn list(dir: &Path) -> Result<Vec<WorktreeEntry>, WorktreeError> {
    ensure_repo(dir).await?;
    let out = run_ok(dir, &["worktree", "list", "--porcelain"], READ_TIMEOUT).await?;
    Ok(parse_list(&out))
}

fn parse_list(porcelain: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    for block in porcelain.split("\n\n").filter(|b| !b.trim().is_empty()) {
        let mut path = None;
        let mut head = None;
        let mut branch = None;
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let Some(h) = line.strip_prefix("HEAD ") {
                head = Some(h.to_string());
            } else if let Some(b) = line.strip_prefix("branch ") {
                branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
            }
        }
        if let Some(path) = path {
            let is_main = entries.is_empty();
            entries.push(WorktreeEntry {
                path,
                head,
                branch,
                is_main,
            });
        }
    }
    entries
}

/// The branch checked out in `dir`, or `None` while it is detached — before
/// the agent has named one.
pub async fn current_branch(dir: &Path) -> Result<Option<String>, WorktreeError> {
    let out = run(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"], READ_TIMEOUT).await?;
    match out.status {
        0 => Ok(Some(out.stdout.trim().to_string())),
        // `--quiet`: exit 1 with nothing on stderr is "not a symbolic ref".
        1 if out.stderr.is_empty() => Ok(None),
        _ => Err(WorktreeError::Git(out.stderr)),
    }
}

/// How many commits `HEAD` in `dir` holds that no local branch and no remote
/// does: work an agent committed before it branched. Zero once it has, whatever
/// it branched from — which is why this, and not a count beyond the commit the
/// worktree was added at, is what decides.
pub async fn unbranched_commits(dir: &Path) -> Result<usize, WorktreeError> {
    let count = run_ok(
        dir,
        &["rev-list", "--count", "HEAD", "--not", "--branches", "--remotes"],
        READ_TIMEOUT,
    )
    .await?;
    count
        .trim()
        .parse()
        .map_err(|_| WorktreeError::Git(format!("unexpected rev-list output: {count:?}")))
}

/// How many paths in `dir`'s worktree are changed or untracked.
pub async fn dirty_files(dir: &Path) -> Result<usize, WorktreeError> {
    let out = run_ok(dir, &["status", "--porcelain"], READ_TIMEOUT).await?;
    Ok(out.lines().filter(|l| !l.is_empty()).count())
}

/// The repository's shared git directory when `dir` is in a linked worktree —
/// where a commit made there writes its objects and refs — and `None` in the
/// main checkout, whose git directory is already inside the project.
pub async fn git_common_dir(dir: &Path) -> Result<Option<PathBuf>, WorktreeError> {
    let out = run_ok(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir", "--git-dir"],
        READ_TIMEOUT,
    )
    .await?;
    let mut lines = out.lines();
    let (Some(common), Some(own)) = (lines.next(), lines.next()) else {
        return Err(WorktreeError::Git(format!("unexpected rev-parse output: {out:?}")));
    };
    if common == own {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(common)))
}

/// Remove the worktree `dir` is in. Unless `force`, refuses one with
/// uncommitted changes or with commits no branch holds; never deletes a branch.
pub async fn remove(dir: &Path, force: bool) -> Result<(), WorktreeError> {
    ensure_repo(dir).await?;
    if !force {
        let files = dirty_files(dir).await?;
        if files > 0 {
            return Err(WorktreeError::Dirty { files });
        }
        let commits = unbranched_commits(dir).await?;
        if commits > 0 {
            return Err(WorktreeError::Unbranched { commits });
        }
    }
    let top = PathBuf::from(run_ok(dir, &["rev-parse", "--show-toplevel"], READ_TIMEOUT).await?);
    let Some(common) = git_common_dir(&top).await? else {
        return Err(WorktreeError::Git(format!(
            "{} is the main checkout, not a worktree",
            top.display()
        )));
    };
    // Run in the shared git directory itself. Not inside the worktree: Windows
    // will not delete the directory a process is standing in. And not in the
    // common directory's parent, which is the main checkout only when the git
    // directory is that checkout's `.git` — a repository made with
    // `--separate-git-dir` keeps it anywhere, and its parent is no repository.
    let top_arg = top.to_string_lossy().into_owned();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&top_arg);
    run_ok(&common, &args, WRITE_TIMEOUT).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            // See `workspace::git`'s test: the developer's signing config must
            // not reach a throwaway repository.
            .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repository with one commit, inside its own temporary parent so the
    /// `<repo>.worktrees` sibling is cleaned up with it.
    fn repo() -> (tempfile::TempDir, PathBuf) {
        let parent = tempfile::tempdir().unwrap();
        let repo = parent.path().join("repo");
        std::fs::create_dir_all(repo.join("app")).unwrap();
        let repo = verified::resolve_root(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("app/main.txt"), "one\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        (parent, repo)
    }

    #[test]
    fn the_worktrees_live_beside_the_repository() {
        assert_eq!(
            worktrees_dir(Path::new("/code/meridian")).unwrap(),
            PathBuf::from("/code/meridian.worktrees")
        );
        assert_eq!(worktrees_dir(Path::new("/")), Err(WorktreeError::NoParent));
    }

    #[test]
    fn a_name_is_one_plain_path_component() {
        assert!(check_name("a1b2c3d4").is_ok());
        for bad in ["", "../x", "a/b", "a b", "a:b"] {
            assert_eq!(check_name(bad), Err(WorktreeError::BadName(bad.to_string())));
        }
    }

    #[test]
    fn list_reads_detached_and_branch_entries() {
        let porcelain =
            "worktree /r\nHEAD aaa\nbranch refs/heads/main\n\nworktree /r.worktrees/x\nHEAD bbb\ndetached\n";
        let entries = parse_list(porcelain);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_main);
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
        assert_eq!(entries[1].branch, None);
        assert_eq!(entries[1].head.as_deref(), Some("bbb"));
        assert!(!entries[1].is_main);
    }

    #[tokio::test]
    async fn a_worktree_from_add_to_remove() {
        if !git::git_available().await {
            eprintln!("skipping: git not installed");
            return;
        }
        let (_parent, repo) = repo();

        // Added from a subdirectory project: the agent's dir is that
        // subdirectory inside the new worktree, detached at HEAD.
        let wt = add_detached(&repo.join("app"), "card1", None).await.unwrap();
        assert_eq!(wt.root.parent().unwrap().file_name().unwrap(), "repo.worktrees");
        assert!(wt.dir.join("main.txt").is_file(), "{}", wt.dir.display());
        assert_eq!(current_branch(&wt.dir).await.unwrap(), None);
        assert!(
            list(&repo)
                .await
                .unwrap()
                .iter()
                .any(|e| e.branch.is_none() && !e.is_main)
        );

        // The same name again is refused before git is asked.
        assert!(matches!(
            add_detached(&repo, "card1", None).await,
            Err(WorktreeError::PathExists(_))
        ));

        // A commit there writes into the repository's shared git directory.
        let common = git_common_dir(&wt.dir).await.unwrap().unwrap();
        assert_eq!(verified::resolve_root(&common).unwrap(), repo.join(".git"));
        assert_eq!(git_common_dir(&repo).await.unwrap(), None);

        // Uncommitted work refuses removal.
        std::fs::write(wt.dir.join("main.txt"), "two\n").unwrap();
        assert_eq!(remove(&wt.dir, false).await, Err(WorktreeError::Dirty { files: 1 }));

        // Committed before branching: clean, but the commit is on no branch,
        // and removing the worktree would strand it.
        git(&wt.root, &["commit", "-q", "-am", "work"]);
        assert_eq!(unbranched_commits(&wt.dir).await.unwrap(), 1);
        assert_eq!(
            remove(&wt.dir, false).await,
            Err(WorktreeError::Unbranched { commits: 1 })
        );

        // The agent names its branch: the work is held, removal may go ahead.
        git(&wt.root, &["switch", "-q", "-c", "agent/fix-login"]);
        assert_eq!(
            current_branch(&wt.dir).await.unwrap().as_deref(),
            Some("agent/fix-login")
        );
        assert_eq!(unbranched_commits(&wt.dir).await.unwrap(), 0);

        // Clean: removed, and the branch the agent made outlives it.
        remove(&wt.dir, false).await.unwrap();
        assert!(!wt.root.exists());
        assert_eq!(list(&repo).await.unwrap().len(), 1);
        git(&repo, &["rev-parse", "--verify", "-q", "agent/fix-login"]);
    }

    /// A repository whose git directory lives elsewhere (`--separate-git-dir`):
    /// the shared git directory's parent is no checkout, and removal asked there
    /// failed with "not a git repository".
    #[tokio::test]
    async fn a_worktree_of_a_repository_with_a_separate_git_dir() {
        if !git::git_available().await {
            eprintln!("skipping: git not installed");
            return;
        }
        let parent = tempfile::tempdir().unwrap();
        let work = parent.path().join("work");
        let gitdir = parent.path().join("store").join("work.git");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(gitdir.parent().unwrap()).unwrap();
        let work = verified::resolve_root(&work).unwrap();
        git(&work, &["init", "-q", "--separate-git-dir", gitdir.to_str().unwrap()]);
        git(&work, &["config", "user.email", "t@t"]);
        git(&work, &["config", "user.name", "t"]);
        std::fs::write(work.join("a.txt"), "a\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "init"]);

        let wt = add_detached(&work, "card1", None).await.unwrap();
        assert!(wt.dir.join("a.txt").is_file());
        remove(&wt.dir, false).await.unwrap();
        assert!(!wt.root.exists());
        assert_eq!(list(&work).await.unwrap().len(), 1);
    }
}

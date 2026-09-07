//! What a turn hands the file primitives so their writes land in the journal.
//!
//! The capture point is `tools::backend`'s primitives, not the agent loop:
//! only there do the old bytes, the new bytes and the canonical path exist on
//! one verified handle, and reading them again anywhere else reopens the
//! TOCTOU window the handle design closed. What the loop contributes is this
//! context — who is writing, on whose behalf, into which project.
//!
//! Two disciplines, both inherited from the append layer and upheld here:
//!
//! - **Recording never fails the write.** `record` logs and returns; a missed
//!   entry surfaces later as an `external` version, which is under-attribution
//!   — the direction every journal failure must take. Failing a user's
//!   successful file write over a busy database would be the wrong trade in
//!   every case.
//! - **The lock spans the observation, not just the append.** The append
//!   transaction serialises database work, but the old content was read and
//!   the file mutated *before* it. Two turns interleaving there would publish
//!   in the wrong order and mint fictional `external` transitions — so each
//!   primitive takes this context's per-path lock before it reads, and holds
//!   it until the append has landed. The lock table lives on `Services` so
//!   independent conversations contend on the same path, not only a parent
//!   and its sub-agent. The gitignore cache stays on the `JournalCtx`: a
//!   process-wide matcher would miss an external `.gitignore` rewrite and
//!   keep snapshotting secrets until restart. Cross-process interleaving
//!   has no lock and is accepted: it degrades into `external` rows, again
//!   the safe direction.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::db::ops::journal::{AppendVersion, Attribution};
use crate::journal::blobs;

/// Files past this size are not journalled — the snapshot store is for source
/// files, and a generated bundle would crowd out everything else. `pub`
/// because the blame command refuses to *read* what capture refused to
/// *store*: past this size there is no chain to blame, only memory to burn.
pub const MAX_SNAPSHOT_BYTES: usize = 2 * 1024 * 1024;

/// Process-wide per-path locks every turn's journal borrows.
///
/// Two conversations writing the same file still have to serialise
/// observe→append. Gitignore answers do not live here: a matcher that
/// outlives the turn cannot see an external `.gitignore` rewrite, and
/// that is how a secret stays in the snapshot store until restart.
#[derive(Default)]
pub struct JournalShared {
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Open command-bracket windows, keyed by normalised project root.
    ///
    /// Two conversations bracketing one project concurrently cannot tell whose
    /// command changed a file — whichever settled first would claim the other's
    /// work through the per-file CAS, which is misattribution, the one output
    /// this system may never produce. Overlap therefore voids *both* windows:
    /// their observations are dropped and the changes surface as `external`.
    /// Under-attribution, no serialisation — a build should not queue behind
    /// another conversation's build for the journal's sake.
    brackets: Mutex<HashMap<String, BracketSlot>>,
}

#[derive(Default)]
struct BracketSlot {
    active: usize,
    /// Bumped whenever a second window joins. A window remembers the value it
    /// opened under; a changed value at settle means someone overlapped it —
    /// including an overlap that opened *and closed* entirely inside it.
    taint_epoch: u64,
}

type IgnoreCache = HashMap<PathBuf, Arc<Vec<ignore::gitignore::Gitignore>>>;

impl JournalShared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

/// Read a snapshot for the journal, or `None` if it should not be stored:
/// missing, too large, not UTF-8, or unreadable. Never a reason to fail the
/// file operation itself.
///
/// Opens first and reads through that handle: a `metadata` then `read` pair
/// can land on two different files, and a file that grows after the length
/// check would otherwise be slurped whole. The read itself is capped at
/// `MAX_SNAPSHOT_BYTES + 1` so growth cannot allocate without bound.
pub fn snapshot_path(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    snapshot_open_file(&mut file)
}

/// Same, through an already-open handle. Leaves the cursor wherever the
/// read stopped; the caller rewinds before writing.
pub fn snapshot_open_file(file: &mut std::fs::File) -> Option<String> {
    use std::io::{Seek, SeekFrom};
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    file.seek(SeekFrom::Start(0)).ok()?;
    snapshot_bytes(file, meta.len())
}

fn snapshot_bytes(reader: impl std::io::Read, claimed_len: u64) -> Option<String> {
    use std::io::Read;
    if claimed_len > MAX_SNAPSHOT_BYTES as u64 {
        return None;
    }
    let mut buf = Vec::new();
    reader.take(MAX_SNAPSHOT_BYTES as u64 + 1).read_to_end(&mut buf).ok()?;
    if buf.len() > MAX_SNAPSHOT_BYTES {
        return None;
    }
    String::from_utf8(buf).ok()
}

/// What kind of transition a primitive is reporting.
#[derive(Debug, Clone, Copy)]
pub enum Op {
    Write,
    Edit,
    Patch,
    Delete,
    RenameFrom,
    RenameTo,
}

impl Op {
    fn as_str(self) -> &'static str {
        match self {
            Op::Write => crate::db::models::journal::version_op::WRITE,
            Op::Edit => crate::db::models::journal::version_op::EDIT,
            Op::Patch => crate::db::models::journal::version_op::PATCH,
            Op::Delete => crate::db::models::journal::version_op::DELETE,
            Op::RenameFrom => crate::db::models::journal::version_op::RENAME_FROM,
            Op::RenameTo => crate::db::models::journal::version_op::RENAME_TO,
        }
    }
}

/// One primitive's licence to record: the turn's context plus which tool and
/// which kind of transition. Built per call by `ToolContext::journal_record`.
#[derive(Clone, Copy)]
pub struct JournalRecord<'a> {
    pub ctx: &'a JournalCtx,
    pub tool_name: &'a str,
    pub op: Op,
}

/// The turn-scoped half of the journal: identity, storage, gitignore cache,
/// and a borrow of the process-wide path locks.
pub struct JournalCtx {
    pub pool: crate::db::DbPool,
    pub blob_root: PathBuf,
    pub conversation_id: String,
    pub turn_id: String,
    /// `turns.origin` as a string — `desktop`, `sub_agent`, `onebot`, …
    pub origin: String,
    pub model_id: Option<String>,
    pub project_id: Option<String>,
    /// Where gitignore files are read from. `None` means no ignore filtering:
    /// a write outside any project is still a write worth attributing.
    /// Canonicalised at construction so it matches the handle-derived paths
    /// the primitives pass in — a symlink or Windows alias as the stored
    /// project path would otherwise fail `strip_prefix` and skip every
    /// `.gitignore` rule.
    pub project_root: Option<PathBuf>,
    shared: Arc<JournalShared>,
    /// Matcher chains for this turn. Fresh on `new` so an external
    /// `.gitignore` edit is visible to the next turn; shared across
    /// `for_turn` so a sub-agent rewriting `.gitignore` invalidates the
    /// parent it is writing beside.
    ignore: Arc<Mutex<IgnoreCache>>,
}

impl JournalCtx {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: crate::db::DbPool,
        blob_root: PathBuf,
        conversation_id: String,
        turn_id: String,
        origin: String,
        model_id: Option<String>,
        project_id: Option<String>,
        project_root: Option<PathBuf>,
        shared: Arc<JournalShared>,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            blob_root,
            conversation_id,
            turn_id,
            origin,
            model_id,
            project_id,
            project_root: project_root.map(canonical_project_root),
            shared,
            ignore: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// The same journal under another identity, for a delegated run.
    ///
    /// A sub-agent inherits its parent's `ToolContext` by struct update, and
    /// inheriting the journal *as is* would file the child's writes under the
    /// parent's conversation — misattribution by inheritance. Storage and
    /// locks are shared; so is the ignore cache, so a `.gitignore` the child
    /// records is visible to the parent in the same flight. Who is writing
    /// is not.
    pub fn for_turn(
        &self,
        conversation_id: String,
        turn_id: String,
        origin: String,
        model_id: Option<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool: self.pool.clone(),
            blob_root: self.blob_root.clone(),
            conversation_id,
            turn_id,
            origin,
            model_id,
            project_id: self.project_id.clone(),
            project_root: self.project_root.clone(),
            shared: self.shared.clone(),
            ignore: self.ignore.clone(),
        })
    }

    /// The per-path lock. Taken by a primitive *before* it reads the old
    /// content and held until `record` returns; guards ordering between two
    /// turns of this process writing one file.
    pub async fn lock_path(&self, path: &Path) -> tokio::sync::OwnedMutexGuard<()> {
        let key = crate::journal::norm_path(path).unwrap_or_else(|| path.to_string_lossy().into_owned());
        let lock = {
            let mut locks = self.shared.locks.lock().expect("journal lock table poisoned");
            locks
                .entry(key)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        lock.lock_owned().await
    }

    /// Append one observed transition. Never fails the caller; returns the
    /// inserted version's id so a rename can link its two halves.
    pub async fn record(
        &self,
        path: &Path,
        observed_old: Option<&str>,
        new: Option<&str>,
        op: Op,
        tool_name: &str,
        moved_from_version_id: Option<String>,
    ) -> Option<String> {
        let Some(norm) = crate::journal::norm_path(path) else {
            // A non-UTF-8 path cannot be keyed without risking two files
            // sharing one chain; not journalling it is the safe direction.
            tracing::debug!(
                chars = path.to_string_lossy().chars().count(),
                "journal: non-utf8 path skipped"
            );
            return None;
        };
        if is_gitignore_file(path) {
            let dir = path.parent().unwrap_or(path);
            let dir = crate::tools::verified::resolve_root(dir).unwrap_or_else(|_| dir.to_path_buf());
            self.invalidate_ignore_under(&dir);
        }
        if self.skipped(path) {
            return None;
        }
        if observed_old.is_some_and(|c| c.len() > MAX_SNAPSHOT_BYTES)
            || new.is_some_and(|c| c.len() > MAX_SNAPSHOT_BYTES)
        {
            tracing::debug!(path_chars = norm.chars().count(), "journal: oversized file skipped");
            return None;
        }

        // Everything below blocks — the blob store fsyncs, the append takes a
        // pooled connection — so the whole tail runs on a blocking thread.
        let pool = self.pool.clone();
        let blob_root = self.blob_root.clone();
        let display = path.to_string_lossy().into_owned();
        let conversation_id = self.conversation_id.clone();
        let turn_id = self.turn_id.clone();
        let origin = self.origin.clone();
        let model_id = self.model_id.clone();
        let project_id = self.project_id.clone();
        let tool = tool_name.to_string();
        let old_content = observed_old.map(str::to_string);
        let new_content = new.map(str::to_string);
        let appended = tokio::task::spawn_blocking(move || {
            // Bytes before rows: both snapshots are durably in the store
            // before the transaction that references them opens.
            let stored_old = old_content
                .map(|c| blobs::store(&blob_root, &c))
                .transpose()
                .map_err(|e| e.to_string())?;
            let stored_new = new_content
                .map(|c| blobs::store(&blob_root, &c))
                .transpose()
                .map_err(|e| e.to_string())?;

            let mut conn = pool.get().map_err(|e| e.to_string())?;
            crate::db::ops::journal::append_version(
                &mut conn,
                &norm,
                &AppendVersion {
                    display_path: &display,
                    op: op.as_str(),
                    observed_old: stored_old.as_ref(),
                    new: stored_new.as_ref(),
                    attribution: Attribution {
                        source: crate::db::models::journal::version_source::NATIVE,
                        conversation_id: Some(&conversation_id),
                        turn_id: Some(&turn_id),
                        project_id: project_id.as_deref(),
                        origin: Some(&origin),
                        model_id: model_id.as_deref(),
                        tool_name: Some(&tool),
                    },
                    moved_from_version_id: moved_from_version_id.as_deref(),
                    now: crate::util::now_ms(),
                },
            )
            .map_err(|e| e.to_string())
        })
        .await;

        match appended {
            Ok(Ok(outcome)) => Some(outcome.version_id),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "journal: append failed; entry skipped");
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "journal: append task failed; entry skipped");
                None
            }
        }
    }

    /// Whether this path is outside what the journal keeps: anything under a
    /// `.git` directory, and anything the project's gitignore stack hides —
    /// the same visibility git itself gives the file, which keeps `.env` and
    /// friends out of the snapshot store.
    fn skipped(&self, path: &Path) -> bool {
        if path_has_git_dir(path) {
            return true;
        }
        let Some(root) = self.project_root.as_deref() else {
            return false;
        };
        let Some(rel_parent) = relative_parent(root, path) else {
            // Outside the project: no gitignore applies, record it.
            return false;
        };

        let chain = self.ignore_chain(root, &rel_parent);
        let check_path = root.join(&rel_parent).join(path.file_name().unwrap_or_default());
        // Deepest .gitignore wins, which is git's own precedence; a whitelist
        // (`!kept.log`) therefore un-hides what a parent hid. Directory
        // rules (`secrets/`, `target/`) match the file's ancestors, not
        // only the file name itself.
        for gi in chain.iter().rev() {
            match gitignore_match(gi, &check_path) {
                ignore::Match::Ignore(_) => return true,
                ignore::Match::Whitelist(_) => return false,
                ignore::Match::None => {}
            }
        }
        false
    }

    fn ignore_chain(&self, root: &Path, rel_parent: &Path) -> Arc<Vec<ignore::gitignore::Gitignore>> {
        let parent = root.join(rel_parent);
        if let Some(cached) = self.ignore.lock().expect("ignore cache poisoned").get(&parent) {
            return cached.clone();
        }

        let mut chain = Vec::new();
        let mut dir = root.to_path_buf();
        push_gitignore(&mut chain, &dir);
        for comp in rel_parent.components() {
            dir.push(comp);
            push_gitignore(&mut chain, &dir);
        }
        let chain = Arc::new(chain);
        self.ignore
            .lock()
            .expect("ignore cache poisoned")
            .insert(parent, chain.clone());
        chain
    }

    fn invalidate_ignore_under(&self, dir: &Path) {
        self.ignore
            .lock()
            .expect("ignore cache poisoned")
            .retain(|cached, _| !cached.starts_with(dir));
    }
}

fn push_gitignore(chain: &mut Vec<ignore::gitignore::Gitignore>, dir: &Path) {
    let file = dir.join(".gitignore");
    if !file.is_file() {
        return;
    }
    let mut builder = ignore::gitignore::GitignoreBuilder::new(dir);
    // `add` returns a partial error after keeping the lines it could parse.
    // Refusing to `build` at all would drop the valid rules — including the
    // ones that hide secrets — because of a single bad glob.
    if let Some(err) = builder.add(&file) {
        tracing::debug!(error = %err, "journal: gitignore had unparseable lines; using the rest");
    }
    if let Ok(gi) = builder.build() {
        chain.push(gi);
    }
}

fn canonical_project_root(path: PathBuf) -> PathBuf {
    crate::tools::verified::resolve_root(&path).unwrap_or(path)
}

fn path_has_git_dir(path: &Path) -> bool {
    path.components().any(|c| is_git_dir_name(c.as_os_str()))
}

fn is_git_dir_name(name: &OsStr) -> bool {
    if cfg!(windows) {
        name.eq_ignore_ascii_case(".git")
    } else {
        name == ".git"
    }
}

fn is_gitignore_file(path: &Path) -> bool {
    path.file_name().is_some_and(|n| n.eq_ignore_ascii_case(".gitignore"))
}

/// Place `path`'s parent under `root`, resolving aliases when the spellings
/// differ (symlink, `nested/..`, Windows 8.3). `None` means outside the
/// project, so no gitignore applies.
fn relative_parent(root: &Path, path: &Path) -> Option<PathBuf> {
    let parent = path.parent().unwrap_or(path);
    if let Ok(rel) = parent.strip_prefix(root) {
        return Some(rel.to_path_buf());
    }
    let resolved = crate::tools::verified::resolve_root(parent)
        .ok()
        .or_else(|| crate::tools::verified::resolve_root(path).ok());
    resolved.and_then(|p| {
        let dir = if p.is_file() { p.parent()?.to_path_buf() } else { p };
        dir.strip_prefix(root).ok().map(|r| r.to_path_buf())
    })
}

/// Match a file against one gitignore, including directory rules such as
/// `secrets/` that only fire when an ancestor is tested as a directory.
///
/// `matched_path_or_any_parents` panics if the path is not under the matcher
/// root, and on Windows a leftover `\`-separated relative path can miss
/// `secrets/` entirely. Walking ancestors with `matched(..., is_dir)` uses
/// the same path form the file globs already accept.
fn gitignore_match<'a>(
    gi: &'a ignore::gitignore::Gitignore,
    path: &Path,
) -> ignore::Match<&'a ignore::gitignore::Glob> {
    let file = gi.matched(path, false);
    if !matches!(file, ignore::Match::None) {
        return file;
    }
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir != gi.path() && !dir.starts_with(gi.path()) {
            break;
        }
        let dir_match = gi.matched(dir, true);
        if !matches!(dir_match, ignore::Match::None) {
            return dir_match;
        }
        if dir == gi.path() {
            break;
        }
        current = dir.parent();
    }
    ignore::Match::None
}

/// A path locked and its pre-action contents read, for the destructive
/// path-based operations (`delete`, `rename`, `write_string`) that std gives
/// no handle-based form. The lock is held for the observation's lifetime, so
/// the caller's action and the eventual `commit` sit inside the same span the
/// handle-based primitives get from `lock_path`. All of these operations
/// require approval, which is what makes the path-based read tolerable at all
/// — the same argument `verify_path` makes for the operations themselves.
pub struct Observed {
    _guard: tokio::sync::OwnedMutexGuard<()>,
    pub path: PathBuf,
    pub old: Option<String>,
}

impl JournalRecord<'_> {
    /// Lock `path` and snapshot what is there now. Call before the action.
    pub async fn observe(&self, path: &Path) -> Observed {
        let guard = self.ctx.lock_path(path).await;
        Observed {
            _guard: guard,
            path: path.to_path_buf(),
            old: snapshot_path(path),
        }
    }

    /// Two paths for a rename, locked in normalised order so two opposing
    /// moves cannot deadlock each other.
    pub async fn observe_pair(&self, a: &Path, b: &Path) -> (Observed, Observed) {
        let a_key = crate::journal::norm_path(a).unwrap_or_default();
        let b_key = crate::journal::norm_path(b).unwrap_or_default();
        if a_key <= b_key {
            let first = self.observe(a).await;
            let second = self.observe(b).await;
            (first, second)
        } else {
            let second = self.observe(b).await;
            let first = self.observe(a).await;
            (first, second)
        }
    }

    /// Record the transition this record was built for. Call after the action
    /// succeeded; never fails the caller.
    pub async fn commit(&self, obs: &Observed, new: Option<&str>) -> Option<String> {
        self.commit_as(self.op, obs, new, None).await
    }

    /// Same, under a different op — a rename records two halves with two ops
    /// through one record.
    pub async fn commit_as(
        &self,
        op: Op,
        obs: &Observed,
        new: Option<&str>,
        moved_from_version_id: Option<String>,
    ) -> Option<String> {
        self.ctx
            .record(
                &obs.path,
                obs.old.as_deref(),
                new,
                op,
                self.tool_name,
                moved_from_version_id,
            )
            .await
    }
}

/// What one bracketed file looked like before the command ran.
#[derive(Debug, Clone, PartialEq)]
enum PreState {
    /// Not on disk.
    Missing,
    Content(String),
}

/// One observation of a path, distinguishing the states the settle logic has
/// to treat differently. `Unobservable` covers everything the bracket may not
/// speak about: a stat failure that is *not* NotFound (a transient EIO or a
/// permission change is not a deletion, and recording a tombstone for it would
/// corrupt the head), a symlink (following it reads whatever it points at —
/// including outside the project, or on the host when a container made it —
/// so it is refused outright), a directory where a file was, and content the
/// snapshot rules skip.
enum Probe {
    Missing,
    Content(String),
    Unobservable,
}

fn probe(path: &Path) -> Probe {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Probe::Missing,
        Err(_) => Probe::Unobservable,
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => Probe::Unobservable,
        Ok(_) => match snapshot_path(path) {
            Some(content) => Probe::Content(content),
            None => Probe::Unobservable,
        },
    }
}

struct BracketFile {
    path: PathBuf,
    norm: String,
    pre: PreState,
}

/// Decrements the open-window count on drop, so a panic between bracket and
/// settle cannot leave the slot permanently occupied — which would taint
/// every later bracket for the project until restart.
struct SlotGuard {
    shared: Arc<JournalShared>,
    key: String,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let mut slots = self.shared.brackets.lock().expect("bracket slot table poisoned");
        if let Some(slot) = slots.get_mut(&self.key) {
            slot.active = slot.active.saturating_sub(1);
            if slot.active == 0 {
                slots.remove(&self.key);
            }
        }
    }
}

/// The pre-command snapshot of every file the journal tracks under the
/// project, taken by [`JournalCtx::command_bracket`] and settled after the
/// command by [`JournalCtx::settle_command_bracket`]. What it exists for: a
/// command that runs `sed`, a formatter or codegen changes files through no
/// primitive this crate owns, and without the bracket every one of those
/// changes is `external` — real work with nobody's name on it.
///
/// The scan set is *only* what the journal already tracks — live chains and
/// tombstoned ones alike, since a file one command deleted and the next
/// recreated is not untracked. A file the journal has never seen changing
/// under a command is left to the external-labelling path; attributing it
/// would be a guess, and the journal does not guess.
pub struct CommandBracket {
    files: Vec<BracketFile>,
    /// Voids the window when another bracket overlapped it; see
    /// `JournalShared::brackets`.
    epoch_at_open: u64,
    tainted_at_open: bool,
    slot: SlotGuard,
}

/// The scan never reads more than this many tracked files or this many bytes
/// per command; past either, changes fall back to external labelling. Small
/// on purpose: this runs before and after *every* shell command.
const BRACKET_MAX_FILES: usize = 256;
const BRACKET_MAX_BYTES: usize = 8 * 1024 * 1024;

impl JournalCtx {
    /// Snapshot the tracked files before a command runs. Also pins any change
    /// somebody else made since the chain head as `external` *now* — pinned
    /// after the command instead, it would land on the command's bill.
    ///
    /// `None` when there is no project to scan under; a command with no
    /// project has no tracked set, and its effects stay external-labelled.
    pub async fn command_bracket(&self) -> Option<CommandBracket> {
        let root = self.project_root.as_ref()?;
        let mut prefix = crate::journal::norm_path(root)?;
        if !prefix.ends_with('/') {
            prefix.push('/');
        }

        // Open the window before anything is read: a second window joining at
        // any point voids both, so the registration has to cover the whole
        // observation span.
        let (epoch_at_open, tainted_at_open) = {
            let mut slots = self.shared.brackets.lock().expect("bracket slot table poisoned");
            let slot = slots.entry(prefix.clone()).or_default();
            slot.active += 1;
            if slot.active > 1 {
                slot.taint_epoch += 1;
            }
            (slot.taint_epoch, slot.active > 1)
        };
        let slot = SlotGuard {
            shared: self.shared.clone(),
            key: prefix.clone(),
        };

        let pool = self.pool.clone();
        let query_prefix = prefix.clone();
        let tracked = tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| e.to_string())?;
            crate::db::ops::journal::chains_under_prefix(&mut conn, &query_prefix, BRACKET_MAX_FILES + 1)
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|inner| inner);
        let tracked = match tracked {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "journal: bracket scan failed; command changes stay external");
                return None;
            }
        };

        let mut dropped = tracked.len().saturating_sub(BRACKET_MAX_FILES);
        let mut budget = BRACKET_MAX_BYTES;
        let mut files = Vec::new();
        for (file, head_sha) in tracked.into_iter().take(BRACKET_MAX_FILES) {
            if budget == 0 {
                dropped += 1;
                continue;
            }
            let path = PathBuf::from(&file.display_path);
            // The same visibility rule `record` enforces: a file that has
            // since been gitignored (it may hold credentials now) must not be
            // snapshotted by the scan either — nor may `.git` internals.
            if self.skipped(&path) {
                continue;
            }
            let _guard = self.lock_path(&path).await;
            let pre = match probe(&path) {
                Probe::Missing => PreState::Missing,
                Probe::Content(content) => {
                    budget = budget.saturating_sub(content.len());
                    PreState::Content(content)
                }
                Probe::Unobservable => {
                    dropped += 1;
                    continue;
                }
            };

            // Pin what somebody else already changed, under the same lock the
            // snapshot was taken under.
            let pre_sha = match &pre {
                PreState::Content(c) => Some(blobs::sha256_of(c)),
                PreState::Missing => None,
            };
            if pre_sha != head_sha {
                let pool = self.pool.clone();
                let blob_root = self.blob_root.clone();
                let norm = file.norm_path.clone();
                let content = match &pre {
                    PreState::Content(c) => Some(c.clone()),
                    PreState::Missing => None,
                };
                let outcome = tokio::task::spawn_blocking(move || -> Result<(), String> {
                    let stored = content
                        .map(|c| blobs::store(&blob_root, &c))
                        .transpose()
                        .map_err(|e| e.to_string())?;
                    let mut conn = pool.get().map_err(|e| e.to_string())?;
                    crate::db::ops::journal::reconcile_external(
                        &mut conn,
                        &norm,
                        stored.as_ref(),
                        crate::util::now_ms(),
                    )
                    .map_err(|e| e.to_string())?;
                    Ok(())
                })
                .await
                .map_err(|e| e.to_string())
                .and_then(|inner| inner);
                if let Err(e) = outcome {
                    tracing::warn!(error = %e, "journal: pre-command reconcile failed; entry skipped");
                }
                // A hand-edited `.gitignore` was just pinned: the rules the
                // rest of this scan (and the turn) run under changed with it.
                if is_gitignore_file(&path) {
                    let dir = path.parent().unwrap_or(&path);
                    self.invalidate_ignore_under(dir);
                }
            }

            files.push(BracketFile {
                path,
                norm: file.norm_path,
                pre,
            });
        }

        if dropped > 0 {
            tracing::debug!(
                dropped,
                "journal: bracket scan capped; uncovered changes will label external"
            );
        }
        Some(CommandBracket {
            files,
            epoch_at_open,
            tainted_at_open,
            slot,
        })
    }

    /// Compare the tracked set against the bracket and record what the command
    /// changed, as `command_observed` / `inferred` — attributed to this turn,
    /// and drawn by the UI with an "inferred" marker because the journal saw
    /// the window, not the write.
    pub async fn settle_command_bracket(&self, bracket: CommandBracket, tool_name: &str) {
        // A window somebody overlapped proves nothing about *whose* command
        // changed a file, and the per-file CAS cannot tell either — the first
        // settler would simply claim everything. Both windows void instead;
        // the changes surface as `external`. Checked at settle rather than
        // at open so an overlap that began after this bracket opened still
        // counts.
        let tainted = bracket.tainted_at_open || {
            let slots = self.shared.brackets.lock().expect("bracket slot table poisoned");
            slots
                .get(&bracket.slot.key)
                .is_none_or(|slot| slot.taint_epoch != bracket.epoch_at_open)
        };
        if tainted {
            tracing::info!(
                files = bracket.files.len(),
                "journal: overlapping command brackets; observations voided, changes will label external"
            );
            return;
        }

        // `.gitignore` files first: a command that adds a rule and creates the
        // file it hides must have the new rule in force before the other
        // entries are re-tested below.
        let (gitignores, others): (Vec<_>, Vec<_>) =
            bracket.files.into_iter().partition(|f| is_gitignore_file(&f.path));

        let mut budget = BRACKET_MAX_BYTES;
        for f in gitignores.into_iter().chain(others) {
            // Re-tested at settle: the command may have gitignored it.
            if self.skipped(&f.path) {
                continue;
            }
            if budget == 0 {
                tracing::debug!("journal: bracket settle budget exhausted; remaining changes label external");
                break;
            }
            let _guard = self.lock_path(&f.path).await;
            let post = match probe(&f.path) {
                Probe::Missing => PreState::Missing,
                Probe::Content(content) => {
                    budget = budget.saturating_sub(content.len());
                    PreState::Content(content)
                }
                // Became a symlink, a directory, unreadable or oversized:
                // nothing this bracket may speak about.
                Probe::Unobservable => continue,
            };
            let changed_gitignore = post != f.pre && is_gitignore_file(&f.path);
            if post == f.pre {
                continue;
            }

            let pool = self.pool.clone();
            let blob_root = self.blob_root.clone();
            let conversation_id = self.conversation_id.clone();
            let turn_id = self.turn_id.clone();
            let origin = self.origin.clone();
            let model_id = self.model_id.clone();
            let project_id = self.project_id.clone();
            let tool = tool_name.to_string();
            let norm = f.norm;
            let pre_content = match f.pre {
                PreState::Content(c) => Some(c),
                PreState::Missing => None,
            };
            let post_content = match post {
                PreState::Content(c) => Some(c),
                PreState::Missing => None,
            };
            let outcome = tokio::task::spawn_blocking(move || -> Result<_, String> {
                let stored_pre = pre_content
                    .map(|c| blobs::store(&blob_root, &c))
                    .transpose()
                    .map_err(|e| e.to_string())?;
                let stored_post = post_content
                    .map(|c| blobs::store(&blob_root, &c))
                    .transpose()
                    .map_err(|e| e.to_string())?;
                let mut conn = pool.get().map_err(|e| e.to_string())?;
                crate::db::ops::journal::append_command_observed(
                    &mut conn,
                    &norm,
                    stored_pre.as_ref(),
                    stored_post.as_ref(),
                    &Attribution {
                        source: crate::db::models::journal::version_source::INFERRED,
                        conversation_id: Some(&conversation_id),
                        turn_id: Some(&turn_id),
                        project_id: project_id.as_deref(),
                        origin: Some(&origin),
                        model_id: model_id.as_deref(),
                        tool_name: Some(&tool),
                    },
                    crate::util::now_ms(),
                )
                .map_err(|e| e.to_string())
            })
            .await;
            match outcome {
                Ok(Ok(crate::db::ops::journal::CommandObservedOutcome::Recorded)) => {
                    // The command rewrote a `.gitignore`: the matchers built
                    // under the old rules are stale for the rest of this turn,
                    // exactly as `record` invalidates for a tool edit.
                    if changed_gitignore {
                        let dir = f.path.parent().unwrap_or(&f.path);
                        self.invalidate_ignore_under(dir);
                    }
                }
                Ok(Ok(other)) => {
                    tracing::debug!(?other, "journal: bracket observation not recorded")
                }
                Ok(Err(e)) => tracing::warn!(error = %e, "journal: bracket settle failed; entry skipped"),
                Err(e) => tracing::warn!(error = %e, "journal: bracket settle task failed; entry skipped"),
            }
        }
    }
}

impl JournalCtx {
    /// Journalled files that still exist under `dir`, for a recursive delete's
    /// tombstones. Only what the journal already tracks: an untracked file's
    /// deletion is left to the external-labelling path rather than guessed at.
    pub async fn tracked_under(&self, dir: &Path) -> Vec<PathBuf> {
        let Some(mut prefix) = crate::journal::norm_path(dir) else {
            return Vec::new();
        };
        if !prefix.ends_with('/') {
            prefix.push('/');
        }
        let pool = self.pool.clone();
        let listed = tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| e.to_string())?;
            crate::db::ops::journal::tracked_files(&mut conn, &prefix, usize::MAX).map_err(|e| e.to_string())
        })
        .await;
        match listed {
            Ok(Ok(files)) => files.into_iter().map(|(f, _)| PathBuf::from(f.display_path)).collect(),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "journal: listing tracked files failed; no tombstones");
                Vec::new()
            }
            Err(e) => {
                tracing::warn!(error = %e, "journal: tracked-files task failed; no tombstones");
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    fn ctx(root: Option<&Path>) -> Arc<JournalCtx> {
        // `keep()` so the directory outlives the TempDir guard; the OS temp
        // cleaner owns it from here, which is fine for a test.
        let blob = tempfile::tempdir().unwrap().keep();
        JournalCtx::new(
            test_db(),
            blob,
            "conv".into(),
            "turn".into(),
            "desktop".into(),
            Some("gpt-test".into()),
            Some("proj".into()),
            root.map(Path::to_path_buf),
            JournalShared::new(),
        )
    }

    #[tokio::test]
    async fn a_recorded_edit_lands_with_full_attribution() {
        let ctx = ctx(None);
        let id = ctx
            .record(
                Path::new("C:/p/a.rs"),
                Some("old"),
                Some("new"),
                Op::Edit,
                "edit_file",
                None,
            )
            .await
            .expect("recorded");

        let mut conn = ctx.pool.get().unwrap();
        let file = crate::db::ops::journal::file_by_path(
            &mut conn,
            &crate::journal::norm_path(Path::new("C:/p/a.rs")).unwrap(),
        )
        .unwrap()
        .expect("file row");
        let chain = crate::db::ops::journal::chain(&mut conn, &file.id).unwrap();
        assert_eq!(chain.len(), 1);
        let v = &chain[0];
        assert_eq!(v.id, id);
        assert_eq!(v.op, "edit");
        assert_eq!(v.source, "native");
        assert_eq!(v.conversation_id.as_deref(), Some("conv"));
        assert_eq!(v.turn_id.as_deref(), Some("turn"));
        assert_eq!(v.project_id.as_deref(), Some("proj"));
        assert_eq!(v.model_id.as_deref(), Some("gpt-test"));
        assert_eq!(v.tool_name.as_deref(), Some("edit_file"));
        // And the bytes are really in the store.
        assert_eq!(
            blobs::load(&ctx.blob_root, v.new_sha.as_deref().unwrap()).unwrap(),
            "new"
        );
    }

    #[tokio::test]
    async fn gitignored_files_and_git_internals_are_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "*.log\n!kept.log\n").unwrap();
        let ctx = ctx(Some(dir.path()));

        let ignored = dir.path().join("noise.log");
        assert!(
            ctx.record(&ignored, None, Some("x"), Op::Write, "write_file", None)
                .await
                .is_none()
        );

        // A whitelist un-hides what the same file hid.
        let kept = dir.path().join("kept.log");
        assert!(
            ctx.record(&kept, None, Some("x"), Op::Write, "write_file", None)
                .await
                .is_some()
        );

        let git_internal = dir.path().join(".git/config");
        assert!(
            ctx.record(&git_internal, None, Some("x"), Op::Write, "write_file", None)
                .await
                .is_none()
        );

        // A nested .gitignore applies to its own subtree.
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/.gitignore"), "secret.txt\n").unwrap();
        let nested = dir.path().join("sub/secret.txt");
        assert!(
            ctx.record(&nested, None, Some("x"), Op::Write, "write_file", None)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_oversized_snapshot_is_skipped_not_split() {
        let ctx = ctx(None);
        let big = "x".repeat(MAX_SNAPSHOT_BYTES + 1);
        assert!(
            ctx.record(
                Path::new("C:/p/big.bin"),
                None,
                Some(&big),
                Op::Write,
                "write_file",
                None
            )
            .await
            .is_none()
        );
    }

    /// The ordering hazard the per-path lock exists for: with the lock held
    /// across observe→append, a second writer cannot slip its append between
    /// another writer's observation and publication.
    #[tokio::test]
    async fn the_path_lock_serialises_observe_to_append() {
        let ctx = ctx(None);
        let path = Path::new("C:/p/contended.rs");
        ctx.record(path, None, Some("A"), Op::Write, "write_file", None)
            .await
            .unwrap();

        // Writer 1 takes the lock, observes A, and stalls before appending.
        let guard = ctx.lock_path(path).await;
        let ctx2 = ctx.clone();
        let racer = tokio::spawn(async move {
            let _g = ctx2.lock_path(Path::new("C:/p/contended.rs")).await;
            ctx2.record(
                Path::new("C:/p/contended.rs"),
                Some("B"),
                Some("C"),
                Op::Edit,
                "edit_file",
                None,
            )
            .await
        });
        // The racer cannot proceed while the guard is held.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!racer.is_finished(), "the lock should hold the second writer");

        ctx.record(path, Some("A"), Some("B"), Op::Edit, "edit_file", None)
            .await
            .unwrap();
        drop(guard);
        racer.await.unwrap().unwrap();

        // Publication order matches lock order: A→B then B→C, no external rows.
        let mut conn = ctx.pool.get().unwrap();
        let file = crate::db::ops::journal::file_by_path(&mut conn, &crate::journal::norm_path(path).unwrap())
            .unwrap()
            .unwrap();
        let chain = crate::db::ops::journal::chain(&mut conn, &file.id).unwrap();
        assert_eq!(chain.len(), 3);
        assert!(
            chain.iter().all(|v| v.source != "external"),
            "no fictional external rows"
        );
    }

    fn ctx_sharing(root: Option<&Path>, shared: Arc<JournalShared>) -> Arc<JournalCtx> {
        let blob = tempfile::tempdir().unwrap().keep();
        JournalCtx::new(
            test_db(),
            blob,
            "conv".into(),
            "turn".into(),
            "desktop".into(),
            Some("gpt-test".into()),
            Some("proj".into()),
            root.map(Path::to_path_buf),
            shared,
        )
    }

    /// Two independent turns (two `JournalCtx::new` calls) still serialise
    /// when they share the process table — the production wiring on `Services`.
    #[tokio::test]
    async fn independent_turns_share_the_path_lock() {
        let shared = JournalShared::new();
        let a = ctx_sharing(None, shared.clone());
        let b = ctx_sharing(None, shared);
        let path = Path::new("C:/p/contended.rs");
        a.record(path, None, Some("A"), Op::Write, "write_file", None)
            .await
            .unwrap();

        let guard = a.lock_path(path).await;
        let racer = tokio::spawn({
            let b = b.clone();
            async move {
                let _g = b.lock_path(Path::new("C:/p/contended.rs")).await;
                b.record(
                    Path::new("C:/p/contended.rs"),
                    Some("B"),
                    Some("C"),
                    Op::Edit,
                    "edit_file",
                    None,
                )
                .await
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!racer.is_finished(), "independent turns must still contend");
        drop(guard);
        racer.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn directory_gitignore_rules_cover_children() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "secrets/\ntarget/\n").unwrap();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        let ctx = ctx(Some(dir.path()));

        assert!(
            ctx.record(
                &dir.path().join("secrets/token.txt"),
                None,
                Some("s3cret"),
                Op::Write,
                "write_file",
                None
            )
            .await
            .is_none()
        );
        assert!(
            ctx.record(
                &dir.path().join("target/generated.js"),
                None,
                Some("bundle"),
                Op::Write,
                "write_file",
                None
            )
            .await
            .is_none()
        );
        assert!(
            ctx.record(
                &dir.path().join("src/main.rs"),
                None,
                Some("fn main() {}"),
                Op::Write,
                "write_file",
                None
            )
            .await
            .is_some()
        );
    }

    #[tokio::test]
    async fn rewriting_gitignore_invalidates_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "").unwrap();
        let ctx = ctx(Some(dir.path()));
        let secret = dir.path().join("secret.env");

        assert!(
            ctx.record(&secret, None, Some("A"), Op::Write, "write_file", None)
                .await
                .is_some(),
            "nothing ignored yet"
        );

        std::fs::write(dir.path().join(".gitignore"), "secret.env\n").unwrap();
        ctx.record(
            &dir.path().join(".gitignore"),
            Some(""),
            Some("secret.env\n"),
            Op::Write,
            "write_file",
            None,
        )
        .await;

        assert!(
            ctx.record(&secret, None, Some("B"), Op::Write, "write_file", None)
                .await
                .is_none(),
            "the rewritten rule must apply in the same turn"
        );
    }

    #[tokio::test]
    async fn a_malformed_gitignore_line_keeps_the_valid_rules() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "*.log\n[\n.env\n").unwrap();
        let ctx = ctx(Some(dir.path()));

        assert!(
            ctx.record(
                &dir.path().join("noise.log"),
                None,
                Some("x"),
                Op::Write,
                "write_file",
                None
            )
            .await
            .is_none()
        );
        assert!(
            ctx.record(
                &dir.path().join(".env"),
                None,
                Some("SECRET=1"),
                Op::Write,
                "write_file",
                None
            )
            .await
            .is_none(),
            "a bad glob must not disable .env"
        );
        assert!(
            ctx.record(
                &dir.path().join("ok.rs"),
                None,
                Some("x"),
                Op::Write,
                "write_file",
                None
            )
            .await
            .is_some()
        );
    }

    #[tokio::test]
    async fn an_aliased_project_root_still_applies_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("proj");
        std::fs::create_dir(&proj).unwrap();
        std::fs::create_dir(proj.join("nested")).unwrap();
        std::fs::write(proj.join(".gitignore"), ".env\n").unwrap();
        let aliased = proj.join("nested").join("..");
        let ctx = ctx(Some(&aliased));

        assert!(
            ctx.record(
                &proj.join(".env"),
                None,
                Some("SECRET=1"),
                Op::Write,
                "write_file",
                None
            )
            .await
            .is_none(),
            "canonicalising the root is what makes strip_prefix see .env"
        );
    }

    #[tokio::test]
    async fn tracked_under_returns_every_live_file_not_a_page() {
        let ctx = ctx(None);
        for i in 0..257 {
            let p = PathBuf::from(format!("C:/p/f{i}.rs"));
            ctx.record(&p, None, Some("x"), Op::Write, "write_file", None)
                .await
                .unwrap();
        }
        let listed = ctx.tracked_under(Path::new("C:/p")).await;
        assert_eq!(listed.len(), 257);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_windows_dot_git_directory_is_skipped_regardless_of_case() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(Some(dir.path()));
        let git_internal = dir.path().join(".GIT").join("config");
        assert!(
            ctx.record(&git_internal, None, Some("x"), Op::Write, "write_file", None)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn snapshot_path_skips_non_utf8_and_oversized() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("a.bin");
        std::fs::write(&bin, [0xff, 0xfe, 0xfd]).unwrap();
        assert!(snapshot_path(&bin).is_none());

        let big = dir.path().join("big.bin");
        std::fs::write(&big, vec![b'x'; MAX_SNAPSHOT_BYTES + 1]).unwrap();
        assert!(snapshot_path(&big).is_none());

        let ok = dir.path().join("ok.txt");
        std::fs::write(&ok, "hi").unwrap();
        assert_eq!(snapshot_path(&ok).as_deref(), Some("hi"));
    }

    /// Stale metadata can claim a file is small while the reader has already
    /// grown past the cap. `take(MAX+1)` is what stops that from becoming an
    /// unbounded `read_to_end`.
    #[test]
    fn snapshot_bytes_caps_a_reader_that_outgrew_its_claimed_len() {
        let grown = vec![b'x'; MAX_SNAPSHOT_BYTES + 50];
        let mut cursor = std::io::Cursor::new(&grown);
        assert!(
            snapshot_bytes(&mut cursor, MAX_SNAPSHOT_BYTES as u64).is_none(),
            "claimed_len is at the cap, but the reader has more"
        );
        assert_eq!(
            cursor.position(),
            (MAX_SNAPSHOT_BYTES as u64) + 1,
            "the read must stop at the cap, not drain the reader"
        );
        let small = b"hi";
        assert_eq!(
            snapshot_bytes(std::io::Cursor::new(&small[..]), small.len() as u64).as_deref(),
            Some("hi")
        );
    }

    /// Two turns share the lock table but not the ignore cache. An external
    /// `.gitignore` rewrite (editor, `git pull`, `run_command`) is invisible
    /// to a process-wide matcher; a fresh ctx must see it.
    #[tokio::test]
    async fn a_later_turn_sees_an_external_gitignore_change() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "").unwrap();
        let env = dir.path().join(".env");
        let shared = JournalShared::new();

        let first = ctx_sharing(Some(dir.path()), shared.clone());
        assert!(
            first
                .record(&env, None, Some("SECRET=1"), Op::Write, "write_file", None)
                .await
                .is_some(),
            "nothing ignored yet"
        );

        std::fs::write(dir.path().join(".gitignore"), ".env\n").unwrap();

        let second = ctx_sharing(Some(dir.path()), shared);
        assert!(
            second
                .record(&env, None, Some("SECRET=2"), Op::Write, "write_file", None)
                .await
                .is_none(),
            "the next turn must not reuse the previous matcher"
        );
    }

    /// Outer project A and nested project B both visit the same directory.
    /// A's chain includes `repo/.gitignore`; B's must not. Each `JournalCtx`
    /// has its own ignore cache, so sharing the lock table must not mix
    /// their matchers.
    #[tokio::test]
    async fn nested_projects_do_not_share_an_ignore_chain() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let nested = repo.join("subproject");
        std::fs::create_dir_all(nested.join("src")).unwrap();
        std::fs::write(repo.join(".gitignore"), "secret.txt\n").unwrap();
        let secret = nested.join("src/secret.txt");

        let shared = JournalShared::new();
        let outer = ctx_sharing(Some(&repo), shared.clone());
        let inner = ctx_sharing(Some(&nested), shared);

        assert!(
            outer
                .record(&secret, None, Some("from-outer"), Op::Write, "write_file", None)
                .await
                .is_none(),
            "the outer gitignore hides secret.txt"
        );
        assert!(
            inner
                .record(&secret, None, Some("from-inner"), Op::Write, "write_file", None)
                .await
                .is_some(),
            "the nested project does not inherit the outer gitignore"
        );
    }

    #[tokio::test]
    async fn a_nested_project_warming_the_cache_does_not_unhide_for_the_outer() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let nested = repo.join("subproject");
        std::fs::create_dir_all(nested.join("src")).unwrap();
        std::fs::write(repo.join(".gitignore"), "secret.txt\n").unwrap();
        let secret = nested.join("src/secret.txt");

        let shared = JournalShared::new();
        let outer = ctx_sharing(Some(&repo), shared.clone());
        let inner = ctx_sharing(Some(&nested), shared);

        assert!(
            inner
                .record(&secret, None, Some("from-inner"), Op::Write, "write_file", None)
                .await
                .is_some()
        );
        assert!(
            outer
                .record(&secret, None, Some("from-outer"), Op::Write, "write_file", None)
                .await
                .is_none(),
            "the outer gitignore still applies after the nested project cached"
        );
    }

    /// The bracket's whole story: a tracked file changed by a "command"
    /// (simulated by writing the disk between bracket and settle) lands as
    /// one `command_observed` / `inferred` version attributed to the turn,
    /// with the real before and after bytes.
    #[tokio::test]
    async fn a_bracketed_command_change_is_recorded_as_inferred() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(Some(dir.path()));
        let file = dir.path().join("tracked.txt");
        std::fs::write(&file, "v1\n").unwrap();
        let real = crate::tools::verified::resolve_root(&file).unwrap();
        ctx.record(&real, None, Some("v1\n"), Op::Write, "write_file", None)
            .await
            .unwrap();

        let bracket = ctx.command_bracket().await.expect("a project has a bracket");
        std::fs::write(&file, "v1\nv2 from a script\n").unwrap();
        ctx.settle_command_bracket(bracket, "run_command").await;

        let mut conn = ctx.pool.get().unwrap();
        let row = crate::db::ops::journal::file_by_path(&mut conn, &crate::journal::norm_path(&real).unwrap())
            .unwrap()
            .unwrap();
        let chain = crate::db::ops::journal::chain(&mut conn, &row.id).unwrap();
        assert_eq!(chain.len(), 2);
        let v = &chain[1];
        assert_eq!((v.op.as_str(), v.source.as_str()), ("command_observed", "inferred"));
        assert_eq!(v.conversation_id.as_deref(), Some("conv"));
        assert_eq!(v.turn_id.as_deref(), Some("turn"));
        assert_eq!(v.tool_name.as_deref(), Some("run_command"));
        assert_eq!(
            blobs::load(&ctx.blob_root, v.new_sha.as_deref().unwrap()).unwrap(),
            "v1\nv2 from a script\n"
        );
    }

    /// A hand edit *before* the command is pinned as `external` by the
    /// bracket's opening scan, so it never lands on the command's bill — and
    /// a command that then changes nothing adds nothing.
    #[tokio::test]
    async fn a_pre_command_hand_edit_is_pinned_external_not_inferred() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(Some(dir.path()));
        let file = dir.path().join("tracked.txt");
        std::fs::write(&file, "v1\n").unwrap();
        let real = crate::tools::verified::resolve_root(&file).unwrap();
        ctx.record(&real, None, Some("v1\n"), Op::Write, "write_file", None)
            .await
            .unwrap();

        // The hand edit happens before the command.
        std::fs::write(&file, "hand edited\n").unwrap();
        let bracket = ctx.command_bracket().await.unwrap();
        // The command changes nothing.
        ctx.settle_command_bracket(bracket, "run_command").await;

        let mut conn = ctx.pool.get().unwrap();
        let row = crate::db::ops::journal::file_by_path(&mut conn, &crate::journal::norm_path(&real).unwrap())
            .unwrap()
            .unwrap();
        let chain = crate::db::ops::journal::chain(&mut conn, &row.id).unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[1].op, "external");
        assert_eq!(chain[1].conversation_id, None, "a hand edit is nobody's");
    }

    /// A file the journal has never seen is not scanned: its changes are the
    /// external path's to notice, never something the bracket guesses at.
    #[tokio::test]
    async fn an_untracked_file_is_not_bracketed() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(Some(dir.path()));
        std::fs::write(dir.path().join("stranger.txt"), "v1\n").unwrap();

        let bracket = ctx.command_bracket().await.unwrap();
        std::fs::write(dir.path().join("stranger.txt"), "v2\n").unwrap();
        ctx.settle_command_bracket(bracket, "run_command").await;

        let mut conn = ctx.pool.get().unwrap();
        let real = crate::tools::verified::resolve_root(&dir.path().join("stranger.txt")).unwrap();
        assert!(
            crate::db::ops::journal::file_by_path(&mut conn, &crate::journal::norm_path(&real).unwrap())
                .unwrap()
                .is_none()
        );
    }

    /// A command deleting a tracked file is a tombstone on the command's bill.
    #[tokio::test]
    async fn a_command_deletion_is_a_command_observed_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(Some(dir.path()));
        let file = dir.path().join("doomed.txt");
        std::fs::write(&file, "v1\n").unwrap();
        let real = crate::tools::verified::resolve_root(&file).unwrap();
        ctx.record(&real, None, Some("v1\n"), Op::Write, "write_file", None)
            .await
            .unwrap();

        let bracket = ctx.command_bracket().await.unwrap();
        std::fs::remove_file(&file).unwrap();
        ctx.settle_command_bracket(bracket, "run_command").await;

        let mut conn = ctx.pool.get().unwrap();
        let row = crate::db::ops::journal::file_by_path(&mut conn, &crate::journal::norm_path(&real).unwrap())
            .unwrap()
            .unwrap();
        let chain = crate::db::ops::journal::chain(&mut conn, &row.id).unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(
            (chain[1].op.as_str(), chain[1].new_sha.as_deref()),
            ("command_observed", None)
        );
    }

    /// A file journalled while visible and *then* gitignored (it may hold
    /// credentials now) is invisible to the bracket too: no snapshot of its
    /// contents may enter the store through the scan, before or after.
    #[tokio::test]
    async fn a_newly_gitignored_tracked_file_is_not_scanned() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(Some(dir.path()));
        let file = dir.path().join("config.txt");
        std::fs::write(&file, "harmless\n").unwrap();
        let real = crate::tools::verified::resolve_root(&file).unwrap();
        ctx.record(&real, None, Some("harmless\n"), Op::Write, "write_file", None)
            .await
            .unwrap();

        // The rule arrives after the file was journalled; a fresh turn sees
        // it. Same pool and blob store as the first turn — `ctx_sharing`
        // would mint a new database, and a bracket over an empty journal
        // proves nothing (this test sat green under mutation for exactly
        // that reason).
        std::fs::write(dir.path().join(".gitignore"), "config.txt\n").unwrap();
        let ctx2 = JournalCtx::new(
            ctx.pool.clone(),
            ctx.blob_root.clone(),
            "conv2".into(),
            "turn2".into(),
            "desktop".into(),
            None,
            None,
            Some(dir.path().to_path_buf()),
            JournalShared::new(),
        );
        std::fs::write(&file, "SECRET=1\n").unwrap();

        let bracket = ctx2.command_bracket().await.unwrap();
        std::fs::write(&file, "SECRET=2\n").unwrap();
        ctx2.settle_command_bracket(bracket, "run_command").await;

        // Neither the pre-command reconcile nor the settle may have stored the
        // secret contents.
        for secret in ["SECRET=1\n", "SECRET=2\n"] {
            let sha = blobs::sha256_of(secret);
            assert!(
                blobs::load(&ctx2.blob_root, &sha).is_err(),
                "a gitignored file's contents reached the blob store"
            );
        }
    }

    /// A command that rewrites `.gitignore` changes the rules for the rest of
    /// the turn: the settle must invalidate the cached matchers, exactly as a
    /// tool edit of the same file does.
    #[tokio::test]
    async fn a_command_edit_of_gitignore_invalidates_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "").unwrap();
        let ctx = ctx(Some(dir.path()));
        let gi = dir.path().join(".gitignore");
        let real_gi = crate::tools::verified::resolve_root(&gi).unwrap();
        ctx.record(&real_gi, None, Some(""), Op::Write, "write_file", None)
            .await
            .unwrap();
        // Warm the matcher cache with the empty rules.
        let secret = dir.path().join("secret.env");
        assert!(
            ctx.record(&secret, None, Some("A"), Op::Write, "write_file", None)
                .await
                .is_some()
        );

        let bracket = ctx.command_bracket().await.unwrap();
        std::fs::write(&gi, "secret.env\n").unwrap();
        ctx.settle_command_bracket(bracket, "run_command").await;

        assert!(
            ctx.record(&secret, None, Some("B"), Op::Write, "write_file", None)
                .await
                .is_none(),
            "the rule the command wrote must apply to the rest of the turn"
        );
    }

    /// Deleted-then-recreated across two commands: the second bracket must see
    /// the tombstoned chain (it is not untracked) and attribute the rebirth.
    #[tokio::test]
    async fn a_recreation_after_a_command_deletion_is_attributed() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(Some(dir.path()));
        let file = dir.path().join("phoenix.txt");
        std::fs::write(&file, "v1\n").unwrap();
        let real = crate::tools::verified::resolve_root(&file).unwrap();
        ctx.record(&real, None, Some("v1\n"), Op::Write, "write_file", None)
            .await
            .unwrap();

        // Command one deletes it.
        let bracket = ctx.command_bracket().await.unwrap();
        std::fs::remove_file(&file).unwrap();
        ctx.settle_command_bracket(bracket, "run_command").await;

        // Command two recreates it.
        let bracket = ctx.command_bracket().await.unwrap();
        std::fs::write(&file, "reborn\n").unwrap();
        ctx.settle_command_bracket(bracket, "run_command").await;

        let mut conn = ctx.pool.get().unwrap();
        let row = crate::db::ops::journal::file_by_path(&mut conn, &crate::journal::norm_path(&real).unwrap())
            .unwrap()
            .unwrap();
        let chain = crate::db::ops::journal::chain(&mut conn, &row.id).unwrap();
        assert_eq!(chain.len(), 3);
        let rebirth = &chain[2];
        assert_eq!(
            (rebirth.op.as_str(), rebirth.observed_old_sha.as_deref()),
            ("command_observed", None)
        );
        assert_eq!(rebirth.conversation_id.as_deref(), Some("conv"));
    }

    /// Two overlapping windows on one project void each other: neither may
    /// claim the changes, because neither can prove whose command made them —
    /// the misattribution this exists to prevent is the first settler
    /// claiming the other's work through the per-file CAS.
    #[tokio::test]
    async fn overlapping_brackets_void_both_windows() {
        let dir = tempfile::tempdir().unwrap();
        let shared = JournalShared::new();
        let a = ctx_sharing(Some(dir.path()), shared.clone());
        let b = ctx_sharing(Some(dir.path()), shared);
        let file = dir.path().join("contended.txt");
        std::fs::write(&file, "v1\n").unwrap();
        let real = crate::tools::verified::resolve_root(&file).unwrap();
        a.record(&real, None, Some("v1\n"), Op::Write, "write_file", None)
            .await
            .unwrap();

        let bracket_a = a.command_bracket().await.unwrap();
        let bracket_b = b.command_bracket().await.unwrap();
        // B's command changes the file; A settles first and must not claim it.
        std::fs::write(&file, "b's work\n").unwrap();
        a.settle_command_bracket(bracket_a, "run_command").await;
        b.settle_command_bracket(bracket_b, "run_command").await;

        let row = {
            // Scoped: the test pool is tiny, and holding a connection across
            // the next bracket starves its spawn_blocking query into a
            // timeout — a defect of this test, not of the bracket.
            let mut conn = a.pool.get().unwrap();
            let row = crate::db::ops::journal::file_by_path(&mut conn, &crate::journal::norm_path(&real).unwrap())
                .unwrap()
                .unwrap();
            let chain = crate::db::ops::journal::chain(&mut conn, &row.id).unwrap();
            assert_eq!(chain.len(), 1, "voided windows record nothing: {chain:?}");
            row
        };

        // And the window closes with its brackets: a later lone bracket works.
        std::fs::write(&file, "later solo change\n").unwrap();
        let bracket = a.command_bracket().await.unwrap();
        std::fs::write(&file, "solo command work\n").unwrap();
        a.settle_command_bracket(bracket, "run_command").await;
        let mut conn = a.pool.get().unwrap();
        let chain = crate::db::ops::journal::chain(&mut conn, &row.id).unwrap();
        assert_eq!(
            chain.last().map(|v| v.op.as_str()),
            Some("command_observed"),
            "the slot must not stay tainted after both windows closed"
        );
    }

    /// A tracked path replaced by a symlink is refused, not followed: the
    /// target may be outside the project entirely (or on the host, when a
    /// container made the link), and snapshotting it would pull foreign bytes
    /// into the store.
    #[tokio::test]
    async fn a_symlinked_path_is_not_snapshotted_at_settle() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret_target = outside.path().join("host-secret.txt");
        std::fs::write(&secret_target, "HOST SECRET\n").unwrap();

        let ctx = ctx(Some(dir.path()));
        let file = dir.path().join("tracked.txt");
        std::fs::write(&file, "v1\n").unwrap();
        let real = crate::tools::verified::resolve_root(&file).unwrap();
        ctx.record(&real, None, Some("v1\n"), Op::Write, "write_file", None)
            .await
            .unwrap();

        let bracket = ctx.command_bracket().await.unwrap();
        std::fs::remove_file(&file).unwrap();
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&secret_target, &file).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(&secret_target, &file).is_ok();
        if !made {
            // Windows without developer mode cannot create symlinks.
            return;
        }
        ctx.settle_command_bracket(bracket, "run_command").await;

        let sha = blobs::sha256_of("HOST SECRET\n");
        assert!(
            blobs::load(&ctx.blob_root, &sha).is_err(),
            "the symlink target's bytes reached the store"
        );
        let mut conn = ctx.pool.get().unwrap();
        let row = crate::db::ops::journal::file_by_path(&mut conn, &crate::journal::norm_path(&real).unwrap())
            .unwrap()
            .unwrap();
        let chain = crate::db::ops::journal::chain(&mut conn, &row.id).unwrap();
        assert_eq!(chain.len(), 1, "an unobservable path appends nothing: {chain:?}");
    }

    /// The CAS at settle: a legitimate tool write landing inside the command
    /// window moves the head, and the bracket must drop its stale observation
    /// rather than interpose a fictional external transition "undoing" it.
    #[tokio::test]
    async fn a_head_moved_during_the_window_drops_the_observation() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(Some(dir.path()));
        let file = dir.path().join("tracked.txt");
        std::fs::write(&file, "v1\n").unwrap();
        let real = crate::tools::verified::resolve_root(&file).unwrap();
        ctx.record(&real, None, Some("v1\n"), Op::Write, "write_file", None)
            .await
            .unwrap();

        let bracket = ctx.command_bracket().await.unwrap();
        // Inside the window: a legitimate tool write (another turn's edit).
        std::fs::write(&file, "tool wrote this\n").unwrap();
        ctx.record(
            &real,
            Some("v1\n"),
            Some("tool wrote this\n"),
            Op::Edit,
            "edit_file",
            None,
        )
        .await
        .unwrap();
        ctx.settle_command_bracket(bracket, "run_command").await;

        let mut conn = ctx.pool.get().unwrap();
        let row = crate::db::ops::journal::file_by_path(&mut conn, &crate::journal::norm_path(&real).unwrap())
            .unwrap()
            .unwrap();
        let chain = crate::db::ops::journal::chain(&mut conn, &row.id).unwrap();
        assert_eq!(chain.len(), 2, "the stale observation must not append");
        assert!(
            chain.iter().all(|v| v.source != "external"),
            "and must not mint external rows"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_trailing_space_is_a_different_file() {
        let ctx = ctx(None);
        ctx.record(Path::new("/tmp/file"), None, Some("a"), Op::Write, "write_file", None)
            .await
            .unwrap();
        ctx.record(Path::new("/tmp/file "), None, Some("b"), Op::Write, "write_file", None)
            .await
            .unwrap();

        let mut conn = ctx.pool.get().unwrap();
        let a = crate::db::ops::journal::file_by_path(
            &mut conn,
            &crate::journal::norm_path(Path::new("/tmp/file")).unwrap(),
        )
        .unwrap()
        .unwrap();
        let b = crate::db::ops::journal::file_by_path(
            &mut conn,
            &crate::journal::norm_path(Path::new("/tmp/file ")).unwrap(),
        )
        .unwrap()
        .unwrap();
        assert_ne!(a.id, b.id, "two Unix names must not share a chain");
    }
}

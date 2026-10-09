//! Crash-safe materialisation of durable plan revisions as app-private files.
//!
//! SQLite is the recovery truth; `plan.md` is the real file the agent reads.
//! A revision first records a pending materialisation, then this store writes
//! and fsyncs a sibling staging file before atomically renaming it.  A restart
//! can therefore distinguish DB-ahead, rename-before-ack and unexpected-file
//! windows without guessing.
//!
//! Each acknowledgement is its own write, after the file it describes is on
//! disk, so a crash between the two leaves exactly the DB-ahead window the
//! next reconcile recognises. The file work runs on the blocking pool; a
//! per-document async lock keeps two reconciles of one document from
//! interleaving across those awaits.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use crate::db::entity::plan_document;
use crate::db::entity::plan_materialization::{self, PlanMaterializationState};
use crate::db::sea::cap::Db;
use crate::db::sea::ops::plan_review::{self, PlanReviewStoreError};

#[derive(Debug, thiserror::Error)]
pub enum PlanFileError {
    #[error(transparent)]
    Store(#[from] PlanReviewStoreError),
    #[error("invalid plan file metadata: {0}")]
    InvalidPath(String),
    #[error("plan revision {revision_id} does not match its stored hash")]
    CorruptRevision { revision_id: String },
    #[error("plan file I/O failed at '{path}': {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("plan file lock was poisoned")]
    PoisonedLock,
    #[error("plan file work did not finish: {0}")]
    Blocking(String),
}

fn io(path: &Path, source: std::io::Error) -> PlanFileError {
    PlanFileError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Run file work on the blocking pool.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, PlanFileError> + Send + 'static,
) -> Result<T, PlanFileError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| PlanFileError::Blocking(error.to_string()))?
}

#[derive(Debug, Clone)]
pub struct PlanFileSnapshot {
    pub path: PathBuf,
    pub content: String,
    pub sha256: String,
}

#[derive(Debug, Default)]
pub struct PlanMaterializationReport {
    pub applied: Vec<plan_materialization::Model>,
    pub conflict: Option<plan_materialization::Model>,
}

/// Called between staging and publication; tests use it to edit the file in
/// that window.
type BeforePublish = Arc<dyn Fn(&Path) + Send + Sync>;

#[derive(Clone)]
pub struct PlanFileStore {
    files_root: PathBuf,
    locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl PlanFileStore {
    pub fn new(app_data_dir: &Path) -> Self {
        Self {
            files_root: crate::files::files_dir(app_data_dir),
            locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn document_lock(&self, document_id: &str) -> Result<Arc<tokio::sync::Mutex<()>>, PlanFileError> {
        let mut locks = self.locks.lock().map_err(|_| PlanFileError::PoisonedLock)?;
        Ok(locks
            .entry(document_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone())
    }

    pub fn path_for(&self, document: &plan_document::Model) -> Result<PathBuf, PlanFileError> {
        if !safe_id(&document.conversation_id) || !safe_id(&document.id) {
            return Err(PlanFileError::InvalidPath("unsafe conversation or document id".into()));
        }
        let expected = format!("plans/{}/plan.md", document.id);
        if document.file_rel_path.replace('\\', "/") != expected {
            return Err(PlanFileError::InvalidPath(format!(
                "unexpected relative path '{}'",
                document.file_rel_path
            )));
        }
        Ok(self
            .files_root
            .join(&document.conversation_id)
            .join("plans")
            .join(&document.id)
            .join("plan.md"))
    }

    pub async fn read_document(&self, db: &Db, document_id: &str) -> Result<Option<PlanFileSnapshot>, PlanFileError> {
        let document = plan_review::get_document(db, document_id).await?;
        if document.head_revision_id.is_none() {
            return Ok(None);
        }
        let path = self.path_for(&document)?;
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io(&path, error)),
        };
        let sha256 = bytes_sha256(&bytes);
        let content = String::from_utf8(bytes)
            .map_err(|error| io(&path, std::io::Error::new(std::io::ErrorKind::InvalidData, error)))?;
        Ok(Some(PlanFileSnapshot { path, content, sha256 }))
    }

    pub async fn reconcile_document(
        &self,
        db: &Db,
        document_id: &str,
        now: i64,
    ) -> Result<PlanMaterializationReport, PlanFileError> {
        self.reconcile_document_inner(db, document_id, now, Arc::new(|_| {}))
            .await
    }

    async fn reconcile_document_inner(
        &self,
        db: &Db,
        document_id: &str,
        now: i64,
        before_publish_check: BeforePublish,
    ) -> Result<PlanMaterializationReport, PlanFileError> {
        let lock = self.document_lock(document_id)?;
        let _guard = lock.lock().await;
        // The reads below are outside the writes that act on them: file I/O sits
        // between, and a write lock is not held across an fsync. Each write is a
        // state CAS (pending -> applied or conflict, applied -> conflict) made under
        // this document's lock, so a stale read fails the CAS rather than overwrite.
        // pool-read-before-write: see above.
        let document = plan_review::get_document(db, document_id).await?;
        let path = self.path_for(&document)?;
        let mut report = PlanMaterializationReport::default();

        // pool-read-before-write: see above.
        for materialization in plan_review::pending_materializations(db, Some(document_id)).await? {
            // pool-read-before-write: see above.
            let revision = plan_review::get_revision(db, &materialization.revision_id).await?;
            if plan_review::markdown_sha256(&revision.content_markdown) != materialization.desired_sha256
                || revision.content_sha256 != materialization.desired_sha256
            {
                return Err(PlanFileError::CorruptRevision {
                    revision_id: revision.id,
                });
            }

            let current_sha = file_sha256(&path).await?;
            if current_sha.as_deref() == Some(&materialization.desired_sha256) {
                report.applied.push(mark_applied(db, &materialization.id, now).await?);
                continue;
            }
            let expected_matches = current_sha.as_deref() == materialization.expected_sha256.as_deref();
            let initial_missing = current_sha.is_none() && materialization.expected_sha256.is_none();
            let force_replace = materialization.force_replace.get();
            if force_replace || expected_matches || initial_missing {
                let publish = {
                    let path = path.clone();
                    let content = revision.content_markdown.clone();
                    let expected = materialization.expected_sha256.clone();
                    let check = before_publish_check.clone();
                    blocking(move || {
                        if force_replace {
                            atomic_replace(&path, &content)?;
                            return Ok(PublishDecision::Publish);
                        }
                        // The first hash check happens before staging I/O. Recheck
                        // after the staging file is durable and immediately before
                        // publication so an external edit during that interval is
                        // not knowingly overwritten. This deliberately narrows the
                        // race; it is not an inter-process atomic compare-and-swap.
                        atomic_replace_guarded(&path, &content, || {
                            check(&path);
                            let observed = file_sha256_now(&path)?;
                            if observed.as_deref() == expected.as_deref() {
                                Ok(PublishDecision::Publish)
                            } else {
                                Ok(PublishDecision::Changed(observed))
                            }
                        })
                    })
                    .await?
                };
                if let PublishDecision::Changed(observed) = publish {
                    let error = format!(
                        "plan.md hash changed outside Meridian before publish (expected {:?}, found {:?})",
                        materialization.expected_sha256, observed
                    );
                    report.conflict = Some(mark_conflict(db, &materialization.id, &error, now).await?);
                    return Ok(report);
                }
                let written_sha = file_sha256(&path).await?;
                if written_sha.as_deref() != Some(materialization.desired_sha256.as_str()) {
                    return Err(PlanFileError::Io {
                        path: path.clone(),
                        source: std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "plan file hash did not match after atomic replace",
                        ),
                    });
                }
                report.applied.push(mark_applied(db, &materialization.id, now).await?);
            } else {
                let error = format!(
                    "plan.md hash changed outside Meridian (expected {:?}, found {:?})",
                    materialization.expected_sha256, current_sha
                );
                report.conflict = Some(mark_conflict(db, &materialization.id, &error, now).await?);
                return Ok(report);
            }
        }

        // Detect deletion or external modification even when no new revision is
        // pending.  The applied row becomes the durable conflict the UI can
        // offer to restore; startup never overwrites it.
        // pool-read-before-write: see above; drift is the applied -> conflict CAS.
        if let Some(latest) = plan_review::latest_materialization(db, document_id).await? {
            match latest.state {
                PlanMaterializationState::Applied => {
                    let current_sha = file_sha256(&path).await?;
                    if current_sha.as_deref() != Some(latest.desired_sha256.as_str()) {
                        let error = format!(
                            "plan.md no longer matches the applied revision (expected {}, found {:?})",
                            latest.desired_sha256, current_sha
                        );
                        let drifted = db
                            .write(async |tx| {
                                plan_review::mark_materialization_drift(tx, &latest.id, &error, now).await
                            })
                            .await?;
                        report.conflict = Some(drifted);
                    }
                }
                PlanMaterializationState::Conflict => report.conflict = Some(latest),
                PlanMaterializationState::Pending => {}
            }
        }
        Ok(report)
    }

    pub async fn reconcile_all(
        &self,
        db: &Db,
        now: i64,
    ) -> Result<Vec<(String, PlanMaterializationReport)>, PlanFileError> {
        let mut document_ids = plan_review::pending_materializations(db, None)
            .await?
            .into_iter()
            .map(|row| row.document_id)
            .collect::<HashSet<_>>();
        document_ids.extend(
            plan_review::list_active_documents(db)
                .await?
                .into_iter()
                .map(|document| document.id),
        );
        let mut document_ids = document_ids.into_iter().collect::<Vec<_>>();
        document_ids.sort();
        let mut reports = Vec::with_capacity(document_ids.len());
        let mut first_error = None;
        for document_id in document_ids {
            match self.reconcile_document(db, &document_id, now).await {
                Ok(report) => reports.push((document_id, report)),
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(reports)
    }

    /// Narrow variant for callers that only need to drain DB-ahead writes.
    pub async fn reconcile_pending(
        &self,
        db: &Db,
        now: i64,
    ) -> Result<Vec<(String, PlanMaterializationReport)>, PlanFileError> {
        let mut document_ids = plan_review::pending_materializations(db, None)
            .await?
            .into_iter()
            .map(|row| row.document_id)
            .collect::<Vec<_>>();
        document_ids.sort();
        document_ids.dedup();
        let mut reports = Vec::with_capacity(document_ids.len());
        for document_id in document_ids {
            let report = self.reconcile_document(db, &document_id, now).await?;
            reports.push((document_id, report));
        }
        Ok(reports)
    }
}

async fn mark_applied(db: &Db, id: &str, now: i64) -> Result<plan_materialization::Model, PlanReviewStoreError> {
    db.write(async |tx| plan_review::mark_materialization_applied(tx, id, now).await)
        .await
}

async fn mark_conflict(
    db: &Db,
    id: &str,
    error: &str,
    now: i64,
) -> Result<plan_materialization::Model, PlanReviewStoreError> {
    db.write(async |tx| plan_review::mark_materialization_conflict(tx, id, error, now).await)
        .await
}

fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn bytes_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// The file's hash, or `None` when there is no file, read on the blocking
/// pool's terms from inside file work.
fn file_sha256_now(path: &Path) -> Result<Option<String>, PlanFileError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes_sha256(&bytes))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io(path, error)),
    }
}

async fn file_sha256(path: &Path) -> Result<Option<String>, PlanFileError> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(Some(bytes_sha256(&bytes))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io(path, error)),
    }
}

struct StagingFile {
    path: PathBuf,
    published: bool,
}

impl Drop for StagingFile {
    fn drop(&mut self) {
        if !self.published {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

enum PublishDecision {
    Publish,
    Changed(Option<String>),
}

fn atomic_replace(path: &Path, content: &str) -> Result<(), PlanFileError> {
    match atomic_replace_guarded(path, content, || Ok(PublishDecision::Publish))? {
        PublishDecision::Publish => Ok(()),
        PublishDecision::Changed(_) => unreachable!("unconditional atomic replace cannot be declined"),
    }
}

fn atomic_replace_guarded<F>(path: &Path, content: &str, before_publish: F) -> Result<PublishDecision, PlanFileError>
where
    F: FnOnce() -> Result<PublishDecision, PlanFileError>,
{
    let parent = path
        .parent()
        .ok_or_else(|| PlanFileError::InvalidPath("plan.md has no parent".into()))?;
    std::fs::create_dir_all(parent).map_err(|error| io(parent, error))?;
    let mut staging = StagingFile {
        path: parent.join(format!(".plan.{}.part", uuid::Uuid::new_v4())),
        published: false,
    };
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging.path)
            .map_err(|error| io(&staging.path, error))?;
        file.write_all(content.as_bytes())
            .map_err(|error| io(&staging.path, error))?;
        file.sync_all().map_err(|error| io(&staging.path, error))?;
    }
    let decision = before_publish()?;
    if matches!(&decision, PublishDecision::Changed(_)) {
        return Ok(decision);
    }
    publish_staging(&staging.path, path).map_err(|error| io(path, error))?;
    staging.published = true;

    #[cfg(unix)]
    if let Ok(directory) = std::fs::File::open(parent) {
        directory.sync_all().map_err(|error| io(parent, error))?;
    }
    Ok(PublishDecision::Publish)
}

#[cfg(target_os = "windows")]
fn publish_staging(staging: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW};

    let staging = staging
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // `std::fs::rename` does not replace an existing destination on Windows,
    // which would make every update after the initial plan creation fail.
    // MoveFileExW preserves the same-directory atomic publication model while
    // replacing the previous plan and asking the filesystem to flush the move.
    let moved = unsafe {
        MoveFileExW(
            staging.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "windows"))]
fn publish_staging(staging: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(staging, destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::entity::plan_document::PlanDocumentState;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    async fn pending_plan(db: &Db, conversation_id: &str, content: &str) -> plan_review::PlanRevisionAppendResult {
        db.write(async |tx| {
            crate::db::sea::ops::conversation::create_conversation(tx, conversation_id, None, None, None, 1).await?;
            let document = plan_review::create_or_resume_document(tx, conversation_id, 2).await?;
            plan_review::append_assistant_revision(
                tx,
                &plan_review::PlanRevisionAppend {
                    document_id: &document.id,
                    expected_generation: 0,
                    expected_head_sha256: None,
                    content_markdown: content,
                    patch: "*** Add File: plan.md",
                    source_message_id: None,
                    source_call_id: None,
                    responding_to_suggestion_revision_id: None,
                    now: 3,
                },
            )
            .await
        })
        .await
        .unwrap()
    }

    #[test]
    fn path_is_derived_instead_of_trusting_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let store = PlanFileStore::new(dir.path());
        let mut document = plan_document::Model {
            id: "doc-1".into(),
            conversation_id: "conv-1".into(),
            state: PlanDocumentState::Drafting,
            head_revision_id: None,
            approved_revision_id: None,
            working_generation: 0,
            file_rel_path: "plans/doc-1/plan.md".into(),
            lock_version: 0,
            created_at: 1,
            updated_at: 1,
        };
        assert_eq!(
            store.path_for(&document).unwrap(),
            dir.path()
                .join("files")
                .join("conv-1")
                .join("plans")
                .join("doc-1")
                .join("plan.md")
        );
        document.file_rel_path = "../../secret".into();
        assert!(store.path_for(&document).is_err());
    }

    #[test]
    fn atomic_replace_leaves_only_old_or_new_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plans/doc/plan.md");
        atomic_replace(&path, "first").unwrap();
        atomic_replace(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        let debris = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".part"))
            .count();
        assert_eq!(debris, 0);
    }

    #[tokio::test]
    async fn reconcile_covers_write_drift_and_explicit_restore() {
        let dir = tempfile::tempdir().unwrap();
        let store = PlanFileStore::new(dir.path());
        let db = sea_test_db().await;
        let appended = pending_plan(&db, "conv-1", "# Durable\n").await;

        let first = store.reconcile_document(&db, &appended.document.id, 4).await.unwrap();
        assert_eq!(first.applied.len(), 1);
        assert!(first.conflict.is_none());
        let snapshot = store.read_document(&db, &appended.document.id).await.unwrap().unwrap();
        assert_eq!(snapshot.content, "# Durable\n");

        std::fs::write(&snapshot.path, "external edit").unwrap();
        let drift = store.reconcile_document(&db, &appended.document.id, 5).await.unwrap();
        assert_eq!(drift.conflict.unwrap().state, PlanMaterializationState::Conflict);
        assert_eq!(std::fs::read_to_string(&snapshot.path).unwrap(), "external edit");

        db.write(async |tx| plan_review::retry_materialization_from_database(tx, &appended.document.id, 6).await)
            .await
            .unwrap();
        let restored = store.reconcile_document(&db, &appended.document.id, 7).await.unwrap();
        assert_eq!(restored.applied.len(), 1);
        assert!(restored.conflict.is_none());
        assert_eq!(std::fs::read_to_string(&snapshot.path).unwrap(), "# Durable\n");
    }

    #[tokio::test]
    async fn external_change_after_staging_becomes_a_conflict_instead_of_being_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let store = PlanFileStore::new(dir.path());
        let db = sea_test_db().await;
        let appended = pending_plan(&db, "conv-1", "# Durable\n").await;

        let report = store
            .reconcile_document_inner(
                &db,
                &appended.document.id,
                4,
                Arc::new(|path| std::fs::write(path, "external edit during staging").unwrap()),
            )
            .await
            .unwrap();

        let conflict = report.conflict.expect("the late external edit must be durable");
        assert_eq!(conflict.state, PlanMaterializationState::Conflict);
        let path = store.path_for(&appended.document).unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "external edit during staging");
    }

    #[tokio::test]
    async fn reconcile_all_recovers_later_documents_before_returning_the_first_stable_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = PlanFileStore::new(dir.path());
        let db = sea_test_db().await;
        let mut plans = [
            pending_plan(&db, "conv-1", "# One\n").await,
            pending_plan(&db, "conv-2", "# Two\n").await,
            pending_plan(&db, "conv-3", "# Three\n").await,
        ];
        plans.sort_by(|left, right| left.document.id.cmp(&right.document.id));
        let bad_revision_id = plans[0].revision.id.clone();
        execute_for_tests(
            &db,
            &format!("UPDATE plan_revisions SET content_markdown = 'corrupt bytes' WHERE id = '{bad_revision_id}'"),
        )
        .await
        .unwrap();

        let error = store.reconcile_all(&db, 4).await.unwrap_err();
        assert!(matches!(
            error,
            PlanFileError::CorruptRevision { revision_id } if revision_id == bad_revision_id
        ));
        for plan in &plans[1..] {
            let snapshot = store.read_document(&db, &plan.document.id).await.unwrap().unwrap();
            assert_eq!(snapshot.content, plan.revision.content_markdown);
            assert_eq!(
                plan_review::latest_materialization(&db, &plan.document.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                PlanMaterializationState::Applied
            );
        }

        let second_error = store.reconcile_all(&db, 5).await.unwrap_err();
        assert!(matches!(
            second_error,
            PlanFileError::CorruptRevision { revision_id } if revision_id == bad_revision_id
        ));
    }

    #[tokio::test]
    async fn desired_bytes_before_database_ack_are_only_acknowledged() {
        let dir = tempfile::tempdir().unwrap();
        let store = PlanFileStore::new(dir.path());
        let db = sea_test_db().await;
        let appended = pending_plan(&db, "conv-1", "# Already renamed\n").await;
        let path = store.path_for(&appended.document).unwrap();
        atomic_replace(&path, "# Already renamed\n").unwrap();

        let report = store.reconcile_document(&db, &appended.document.id, 4).await.unwrap();
        assert_eq!(report.applied.len(), 1);
        assert!(report.conflict.is_none());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "# Already renamed\n");
    }
}

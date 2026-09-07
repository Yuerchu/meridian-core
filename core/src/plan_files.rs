//! Crash-safe materialisation of durable plan revisions as app-private files.
//!
//! SQLite is the recovery truth; `plan.md` is the real file the agent reads.
//! A revision first records a pending materialisation, then this store writes
//! and fsyncs a sibling staging file before atomically renaming it.  A restart
//! can therefore distinguish DB-ahead, rename-before-ack and unexpected-file
//! windows without guessing.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use diesel::sqlite::SqliteConnection;
use sha2::{Digest, Sha256};

use crate::db::models::plan_review::{PlanDocumentRow, PlanMaterializationRow, PlanMaterializationState};
use crate::db::ops::plan_review::{self, PlanReviewStoreError};

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
}

fn io(path: &Path, source: std::io::Error) -> PlanFileError {
    PlanFileError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[derive(Debug, Clone)]
pub struct PlanFileSnapshot {
    pub path: PathBuf,
    pub content: String,
    pub sha256: String,
}

#[derive(Debug, Default)]
pub struct PlanMaterializationReport {
    pub applied: Vec<PlanMaterializationRow>,
    pub conflict: Option<PlanMaterializationRow>,
}

#[derive(Clone)]
pub struct PlanFileStore {
    files_root: PathBuf,
    locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl PlanFileStore {
    pub fn new(app_data_dir: &Path) -> Self {
        Self {
            files_root: crate::files::files_dir(app_data_dir),
            locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn document_lock(&self, document_id: &str) -> Result<Arc<Mutex<()>>, PlanFileError> {
        let mut locks = self.locks.lock().map_err(|_| PlanFileError::PoisonedLock)?;
        Ok(locks
            .entry(document_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone())
    }

    pub fn path_for(&self, document: &PlanDocumentRow) -> Result<PathBuf, PlanFileError> {
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

    pub fn read_document(
        &self,
        conn: &mut SqliteConnection,
        document_id: &str,
    ) -> Result<Option<PlanFileSnapshot>, PlanFileError> {
        let document = plan_review::get_document(conn, document_id)?;
        if document.head_revision_id.is_none() {
            return Ok(None);
        }
        let path = self.path_for(&document)?;
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io(&path, error)),
        };
        let sha256 = bytes_sha256(&bytes);
        let content = String::from_utf8(bytes)
            .map_err(|error| io(&path, std::io::Error::new(std::io::ErrorKind::InvalidData, error)))?;
        Ok(Some(PlanFileSnapshot { path, content, sha256 }))
    }

    pub fn reconcile_document(
        &self,
        conn: &mut SqliteConnection,
        document_id: &str,
        now: i64,
    ) -> Result<PlanMaterializationReport, PlanFileError> {
        self.reconcile_document_inner(conn, document_id, now, &|_| {})
    }

    fn reconcile_document_inner<F>(
        &self,
        conn: &mut SqliteConnection,
        document_id: &str,
        now: i64,
        before_publish_check: &F,
    ) -> Result<PlanMaterializationReport, PlanFileError>
    where
        F: Fn(&Path),
    {
        let lock = self.document_lock(document_id)?;
        let _guard = lock.lock().map_err(|_| PlanFileError::PoisonedLock)?;
        let document = plan_review::get_document(conn, document_id)?;
        let path = self.path_for(&document)?;
        let mut report = PlanMaterializationReport::default();

        for materialization in plan_review::pending_materializations(conn, Some(document_id))? {
            let revision = plan_review::get_revision(conn, &materialization.revision_id)?;
            if plan_review::markdown_sha256(&revision.content_markdown) != materialization.desired_sha256
                || revision.content_sha256 != materialization.desired_sha256
            {
                return Err(PlanFileError::CorruptRevision {
                    revision_id: revision.id,
                });
            }

            let current_sha = file_sha256(&path)?;
            if current_sha.as_deref() == Some(&materialization.desired_sha256) {
                report.applied.push(plan_review::mark_materialization_applied(
                    conn,
                    &materialization.id,
                    now,
                )?);
                continue;
            }
            let expected_matches = current_sha.as_deref() == materialization.expected_sha256.as_deref();
            let initial_missing = current_sha.is_none() && materialization.expected_sha256.is_none();
            if materialization.force_replace == 1 || expected_matches || initial_missing {
                let publish = if materialization.force_replace == 1 {
                    atomic_replace(&path, &revision.content_markdown)?;
                    PublishDecision::Publish
                } else {
                    // The first hash check happens before staging I/O. Recheck
                    // after the staging file is durable and immediately before
                    // publication so an external edit during that interval is
                    // not knowingly overwritten. This deliberately narrows the
                    // race; it is not an inter-process atomic compare-and-swap.
                    atomic_replace_guarded(&path, &revision.content_markdown, || {
                        before_publish_check(&path);
                        let observed = file_sha256(&path)?;
                        if observed.as_deref() == materialization.expected_sha256.as_deref() {
                            Ok(PublishDecision::Publish)
                        } else {
                            Ok(PublishDecision::Changed(observed))
                        }
                    })?
                };
                if let PublishDecision::Changed(observed) = publish {
                    let error = format!(
                        "plan.md hash changed outside Meridian before publish (expected {:?}, found {:?})",
                        materialization.expected_sha256, observed
                    );
                    let conflict = plan_review::mark_materialization_conflict(conn, &materialization.id, &error, now)?;
                    report.conflict = Some(conflict);
                    return Ok(report);
                }
                let written_sha = file_sha256(&path)?;
                if written_sha.as_deref() != Some(materialization.desired_sha256.as_str()) {
                    return Err(PlanFileError::Io {
                        path: path.clone(),
                        source: std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "plan file hash did not match after atomic replace",
                        ),
                    });
                }
                report.applied.push(plan_review::mark_materialization_applied(
                    conn,
                    &materialization.id,
                    now,
                )?);
            } else {
                let error = format!(
                    "plan.md hash changed outside Meridian (expected {:?}, found {:?})",
                    materialization.expected_sha256, current_sha
                );
                let conflict = plan_review::mark_materialization_conflict(conn, &materialization.id, &error, now)?;
                report.conflict = Some(conflict);
                return Ok(report);
            }
        }

        // Detect deletion or external modification even when no new revision is
        // pending.  The applied row becomes the durable conflict the UI can
        // offer to restore; startup never overwrites it.
        if let Some(latest) = plan_review::latest_materialization(conn, document_id)? {
            match latest.state().map_err(PlanReviewStoreError::from)? {
                PlanMaterializationState::Applied => {
                    let current_sha = file_sha256(&path)?;
                    if current_sha.as_deref() != Some(latest.desired_sha256.as_str()) {
                        let error = format!(
                            "plan.md no longer matches the applied revision (expected {}, found {:?})",
                            latest.desired_sha256, current_sha
                        );
                        report.conflict = Some(plan_review::mark_materialization_drift(conn, &latest.id, &error, now)?);
                    }
                }
                PlanMaterializationState::Conflict => report.conflict = Some(latest),
                PlanMaterializationState::Pending => {}
            }
        }
        Ok(report)
    }

    pub fn reconcile_all(
        &self,
        conn: &mut SqliteConnection,
        now: i64,
    ) -> Result<Vec<(String, PlanMaterializationReport)>, PlanFileError> {
        let mut document_ids = plan_review::pending_materializations(conn, None)?
            .into_iter()
            .map(|row| row.document_id)
            .collect::<HashSet<_>>();
        document_ids.extend(
            plan_review::list_active_documents(conn)?
                .into_iter()
                .map(|document| document.id),
        );
        let mut document_ids = document_ids.into_iter().collect::<Vec<_>>();
        document_ids.sort();
        let mut reports = Vec::with_capacity(document_ids.len());
        let mut first_error = None;
        for document_id in document_ids {
            match self.reconcile_document(conn, &document_id, now) {
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
    pub fn reconcile_pending(
        &self,
        conn: &mut SqliteConnection,
        now: i64,
    ) -> Result<Vec<(String, PlanMaterializationReport)>, PlanFileError> {
        let mut document_ids = plan_review::pending_materializations(conn, None)?
            .into_iter()
            .map(|row| row.document_id)
            .collect::<Vec<_>>();
        document_ids.sort();
        document_ids.dedup();
        let mut reports = Vec::with_capacity(document_ids.len());
        for document_id in document_ids {
            let report = self.reconcile_document(conn, &document_id, now)?;
            reports.push((document_id, report));
        }
        Ok(reports)
    }
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

fn file_sha256(path: &Path) -> Result<Option<String>, PlanFileError> {
    match std::fs::read(path) {
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
    use diesel::prelude::*;

    fn pending_plan(
        conn: &mut SqliteConnection,
        conversation_id: &str,
        content: &str,
    ) -> plan_review::PlanRevisionAppendResult {
        crate::db::ops::conversation::create_conversation(conn, conversation_id, None, None, None, 1).unwrap();
        let document = plan_review::create_or_resume_document(conn, conversation_id, 2).unwrap();
        plan_review::append_assistant_revision(
            conn,
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
        .unwrap()
    }

    #[test]
    fn path_is_derived_instead_of_trusting_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let store = PlanFileStore::new(dir.path());
        let mut document = PlanDocumentRow {
            id: "doc-1".into(),
            conversation_id: "conv-1".into(),
            state: "drafting".into(),
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

    #[test]
    fn reconcile_covers_write_drift_and_explicit_restore() {
        let dir = tempfile::tempdir().unwrap();
        let store = PlanFileStore::new(dir.path());
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        let appended = pending_plan(&mut conn, "conv-1", "# Durable\n");

        let first = store.reconcile_document(&mut conn, &appended.document.id, 4).unwrap();
        assert_eq!(first.applied.len(), 1);
        assert!(first.conflict.is_none());
        let snapshot = store.read_document(&mut conn, &appended.document.id).unwrap().unwrap();
        assert_eq!(snapshot.content, "# Durable\n");

        std::fs::write(&snapshot.path, "external edit").unwrap();
        let drift = store.reconcile_document(&mut conn, &appended.document.id, 5).unwrap();
        assert_eq!(
            drift.conflict.unwrap().state,
            PlanMaterializationState::Conflict.as_str()
        );
        assert_eq!(std::fs::read_to_string(&snapshot.path).unwrap(), "external edit");

        plan_review::retry_materialization_from_database(&mut conn, &appended.document.id, 6).unwrap();
        let restored = store.reconcile_document(&mut conn, &appended.document.id, 7).unwrap();
        assert_eq!(restored.applied.len(), 1);
        assert!(restored.conflict.is_none());
        assert_eq!(std::fs::read_to_string(&snapshot.path).unwrap(), "# Durable\n");
    }

    #[test]
    fn external_change_after_staging_becomes_a_conflict_instead_of_being_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let store = PlanFileStore::new(dir.path());
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        let appended = pending_plan(&mut conn, "conv-1", "# Durable\n");

        let report = store
            .reconcile_document_inner(&mut conn, &appended.document.id, 4, &|path| {
                std::fs::write(path, "external edit during staging").unwrap();
            })
            .unwrap();

        let conflict = report.conflict.expect("the late external edit must be durable");
        assert_eq!(conflict.state, PlanMaterializationState::Conflict.as_str());
        let path = store.path_for(&appended.document).unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "external edit during staging");
    }

    #[test]
    fn reconcile_all_recovers_later_documents_before_returning_the_first_stable_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = PlanFileStore::new(dir.path());
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        let mut plans = [
            pending_plan(&mut conn, "conv-1", "# One\n"),
            pending_plan(&mut conn, "conv-2", "# Two\n"),
            pending_plan(&mut conn, "conv-3", "# Three\n"),
        ];
        plans.sort_by(|left, right| left.document.id.cmp(&right.document.id));
        let bad_revision_id = plans[0].revision.id.clone();
        diesel::update(crate::db::schema::plan_revisions::table.find(&bad_revision_id))
            .set(crate::db::schema::plan_revisions::content_markdown.eq("corrupt bytes"))
            .execute(&mut conn)
            .unwrap();

        let error = store.reconcile_all(&mut conn, 4).unwrap_err();
        assert!(matches!(
            error,
            PlanFileError::CorruptRevision { revision_id } if revision_id == bad_revision_id
        ));
        for plan in &plans[1..] {
            let snapshot = store.read_document(&mut conn, &plan.document.id).unwrap().unwrap();
            assert_eq!(snapshot.content, plan.revision.content_markdown);
            assert_eq!(
                plan_review::latest_materialization(&mut conn, &plan.document.id)
                    .unwrap()
                    .unwrap()
                    .state,
                PlanMaterializationState::Applied.as_str()
            );
        }

        let second_error = store.reconcile_all(&mut conn, 5).unwrap_err();
        assert!(matches!(
            second_error,
            PlanFileError::CorruptRevision { revision_id } if revision_id == bad_revision_id
        ));
    }

    #[test]
    fn desired_bytes_before_database_ack_are_only_acknowledged() {
        let dir = tempfile::tempdir().unwrap();
        let store = PlanFileStore::new(dir.path());
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        let appended = pending_plan(&mut conn, "conv-1", "# Already renamed\n");
        let path = store.path_for(&appended.document).unwrap();
        atomic_replace(&path, "# Already renamed\n").unwrap();

        let report = store.reconcile_document(&mut conn, &appended.document.id, 4).unwrap();
        assert_eq!(report.applied.len(), 1);
        assert!(report.conflict.is_none());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "# Already renamed\n");
    }
}

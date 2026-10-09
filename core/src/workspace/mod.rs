//! What a conversation's project looks like on disk, for the panel that shows it.
//!
//! Read-only by construction: everything here answers questions — where is the
//! root, what changed, what is in this directory, what does this file say — and
//! nothing writes. The panel opens files without asking anybody, so reads go
//! through `tools::verified::open_read` and do their I/O on the handle that was
//! checked; see the `OpenedTarget` rule in `tools/mod.rs`.
//!
//! The root is resolved per conversation and nothing falls back to the process
//! cwd: `working_dir_or_current()` answers "where should a command run" and its
//! fallback is wrong for a browser, which must say "no project" rather than
//! quietly showing whatever directory the app was started from.
//!
//! Android/SAF workspaces are out of scope for now, deliberately: a SAF root is
//! a `content://` grant resolved through the storage bridge, not an OS path,
//! and every reader here is handle-based. Such a project reports `MissingDir`,
//! which is an honest degradation rather than a hole — the panel says there is
//! nothing to browse instead of browsing the wrong thing.

pub mod git;
pub mod read;
pub mod reference;
pub mod tree;

use std::path::PathBuf;

use serde::Serialize;

use crate::db::sea::DbErr;
use crate::db::sea::cap::{Db, Snapshot};
use crate::db::sea::ops as sea_ops;

/// Where a conversation's files live, or the reason there is nowhere to look.
///
/// The three empty states are deliberately distinct: "this conversation has no
/// project", "the project has no directory", and "the directory is gone" ask
/// the user for three different actions.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WorkspaceRoot {
    Ok {
        /// The OS's own spelling of the root, links followed.
        root: String,
        git_available: bool,
        is_repo: bool,
    },
    NoProject,
    NoPath,
    MissingDir {
        path: String,
    },
}

/// The directory a conversation is configured to work in, before the
/// filesystem is asked whether it exists.
///
/// A hosted conversation's agent writes wherever its session was opened, so
/// `acp_sessions.cwd` outranks the project for `agent_kind = 'claude_code'`;
/// a native conversation's tools resolve against the project's path, so the
/// panel shows the same tree the tools can reach. The `bool` says whether a
/// project row existed at all, which is what separates `NoProject` from
/// `NoPath` when the answer is `None`.
///
/// A native conversation that works an agent board card works in the card's
/// worktree, not in the project it was made from: that is what keeps agents
/// running at once out of each other's files. A card whose worktree a person
/// removed is an error rather than the project's path — there is nowhere left
/// for it to work, and falling back would set it loose in the main checkout.
/// (A hosted card needs nothing here: its session was opened in the worktree,
/// so `acp_sessions.cwd` already says so.)
pub async fn configured_dir_in(db: &impl Snapshot, conversation_id: &str) -> Result<(Option<PathBuf>, bool), String> {
    let conv = sea_ops::conversation::get_conversation(db, conversation_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("conversation {conversation_id} not found"))?;

    if conv.agent_kind.as_deref() == Some("claude_code") {
        let cwd = sea_ops::acp_session::get(db, conversation_id)
            .await
            .map_err(|e| e.to_string())?
            .map(|row| PathBuf::from(row.cwd));
        return Ok((cwd, false));
    }

    if let Some(card) = sea_ops::board_task::for_conversation(db, conversation_id)
        .await
        .map_err(|e| e.to_string())?
    {
        return match card.worktree_path {
            Some(worktree) => Ok((Some(PathBuf::from(worktree)), true)),
            None => Err(format!(
                "this conversation works board card \"{}\", whose worktree was removed; it has nowhere to work",
                card.title
            )),
        };
    }

    match conv.project_id.as_deref() {
        Some(pid) => {
            let path = sea_ops::project::get_project(db, pid)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("project {pid} not found"))?
                .path
                .map(PathBuf::from);
            Ok((path, true))
        }
        None => Ok((None, false)),
    }
}

/// [`configured_dir_in`] in a snapshot of its own.
async fn configured_dir(db: &Db, conversation_id: &str) -> Result<(Option<PathBuf>, bool), String> {
    db.read(async |tx| Ok::<_, DbErr>(configured_dir_in(tx, conversation_id).await))
        .await
        .map_err(|e| e.to_string())?
}

/// The configured directory alone, for callers that resolve it themselves.
pub async fn resolve_workspace_dir(db: &Db, conversation_id: &str) -> Result<Option<PathBuf>, String> {
    configured_dir(db, conversation_id).await.map(|(dir, _)| dir)
}

/// Resolve the configured directory against the filesystem.
///
/// `Err` from `resolve_root` collapses to `MissingDir` on purpose: whether the
/// directory is absent, unreadable or a file, the panel's answer is the same —
/// there is nothing to browse, and here is the path that was tried.
///
/// `git_available` / `is_repo` come back `false` from here: git is a
/// subprocess, so the command layer fills them in. The filesystem check runs
/// on the blocking pool.
pub async fn resolve_workspace_root(db: &Db, conversation_id: &str) -> Result<WorkspaceRoot, String> {
    let (configured, has_project) = configured_dir(db, conversation_id).await?;
    tokio::task::spawn_blocking(move || root_of(configured, has_project))
        .await
        .map_err(|e| e.to_string())
}

/// A configured directory, checked against the filesystem. Blocking.
fn root_of(configured: Option<PathBuf>, has_project: bool) -> WorkspaceRoot {
    let Some(configured) = configured else {
        return if has_project {
            WorkspaceRoot::NoPath
        } else {
            WorkspaceRoot::NoProject
        };
    };

    match crate::tools::verified::resolve_root(&configured) {
        // `resolve_root` opens any filesystem object; a project whose path
        // names a regular file would otherwise report `Ok` and then fail
        // strangely on every listing. Not-a-directory is the same answer as
        // not-there: nothing to browse, and here is the path that was tried.
        Ok(real) if real.is_dir() => WorkspaceRoot::Ok {
            root: real.to_string_lossy().into_owned(),
            git_available: false,
            is_repo: false,
        },
        _ => WorkspaceRoot::MissingDir {
            path: configured.to_string_lossy().into_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    /// The four answers: no project, a project without a path, a path that is
    /// not there, and one that is. A hosted conversation answers from its
    /// session's directory, not the project's.
    #[tokio::test]
    async fn the_root_comes_from_the_project_or_the_hosted_session() {
        let db = sea_test_db().await;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().to_string_lossy().replace('\'', "''");
        let gone = dir.path().join("gone").to_string_lossy().replace('\'', "''");
        execute_for_tests(
            &db,
            &format!(
                "INSERT INTO projects (id, name, path, created_at, updated_at) VALUES
                     ('p-real', 'P', '{real}', 1, 1), ('p-none', 'P', NULL, 1, 1), ('p-gone', 'P', '{gone}', 1, 1);
                 INSERT INTO conversations (id, project_id, agent_kind, created_at, updated_at) VALUES
                     ('loose', NULL, NULL, 1, 1), ('none', 'p-none', NULL, 1, 1),
                     ('gone', 'p-gone', NULL, 1, 1), ('real', 'p-real', NULL, 1, 1),
                     ('hosted', 'p-gone', 'claude_code', 1, 1);
                 INSERT INTO acp_sessions (conversation_id, cwd, created_at, updated_at)
                     VALUES ('hosted', '{real}', 1, 1)"
            ),
        )
        .await
        .unwrap();

        assert!(matches!(
            resolve_workspace_root(&db, "loose").await.unwrap(),
            WorkspaceRoot::NoProject
        ));
        assert!(matches!(
            resolve_workspace_root(&db, "none").await.unwrap(),
            WorkspaceRoot::NoPath
        ));
        assert!(matches!(
            resolve_workspace_root(&db, "gone").await.unwrap(),
            WorkspaceRoot::MissingDir { .. }
        ));
        assert!(matches!(
            resolve_workspace_root(&db, "real").await.unwrap(),
            WorkspaceRoot::Ok { .. }
        ));
        assert!(
            matches!(
                resolve_workspace_root(&db, "hosted").await.unwrap(),
                WorkspaceRoot::Ok { .. }
            ),
            "the session's directory outranks the project's"
        );
        assert_eq!(
            resolve_workspace_dir(&db, "hosted").await.unwrap(),
            Some(PathBuf::from(dir.path()))
        );
        assert!(resolve_workspace_dir(&db, "nope").await.is_err());
    }

    /// A conversation working a board card answers from the card's worktree,
    /// not the project — and once a person removed the worktree, refuses
    /// rather than falling back to the main checkout.
    #[tokio::test]
    async fn a_board_card_works_in_its_worktree_and_nowhere_once_removed() {
        let db = sea_test_db().await;
        let project = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let project_path = project.path().to_string_lossy().replace('\'', "''");
        let worktree_path = worktree.path().to_string_lossy().replace('\'', "''");
        execute_for_tests(
            &db,
            &format!(
                "INSERT INTO projects (id, name, path, created_at, updated_at) VALUES ('p', 'P', '{project_path}', 1, 1);
                 INSERT INTO conversations (id, project_id, created_at, updated_at) VALUES
                     ('card', 'p', 1, 1), ('plain', 'p', 1, 1), ('removed', 'p', 1, 1);
                 INSERT INTO board_tasks (id, project_id, conversation_id, source, title, stage, position,
                                          agent_kind, worktree_path, created_at, updated_at, worktree_removed_at)
                 VALUES ('t1', 'p', 'card', 'local', 'fix', 'running', 0, 'native', '{worktree_path}', 1, 1, NULL),
                        ('t2', 'p', 'removed', 'local', 'old', 'done', 0, 'native', NULL, 1, 1, 5)"
            ),
        )
        .await
        .unwrap();

        assert_eq!(
            resolve_workspace_dir(&db, "card").await.unwrap(),
            Some(PathBuf::from(worktree.path()))
        );
        assert_eq!(
            resolve_workspace_dir(&db, "plain").await.unwrap(),
            Some(PathBuf::from(project.path())),
            "a conversation that is not a card keeps the project's directory"
        );
        let refused = resolve_workspace_dir(&db, "removed").await.unwrap_err();
        assert!(refused.contains("worktree was removed"), "{refused}");
        assert!(resolve_workspace_root(&db, "removed").await.is_err());
    }
}

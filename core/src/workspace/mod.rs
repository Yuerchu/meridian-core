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

use diesel::sqlite::SqliteConnection;
use serde::Serialize;

use crate::db;

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
fn configured_dir(conn: &mut SqliteConnection, conversation_id: &str) -> Result<(Option<PathBuf>, bool), String> {
    let conv = db::ops::conversation::get_conversation(conn, conversation_id).map_err(|e| e.to_string())?;

    if conv.agent_kind.as_deref() == Some("claude_code") {
        let cwd = db::ops::acp_session::get(conn, conversation_id)
            .map_err(|e| e.to_string())?
            .map(|row| PathBuf::from(row.cwd));
        return Ok((cwd, false));
    }

    match conv.project_id.as_deref() {
        Some(pid) => {
            let path = db::ops::project::get_project(conn, pid)
                .map_err(|e| e.to_string())?
                .path
                .map(PathBuf::from);
            Ok((path, true))
        }
        None => Ok((None, false)),
    }
}

/// The configured directory alone, for callers that resolve it themselves.
pub fn resolve_workspace_dir(conn: &mut SqliteConnection, conversation_id: &str) -> Result<Option<PathBuf>, String> {
    configured_dir(conn, conversation_id).map(|(dir, _)| dir)
}

/// Resolve the configured directory against the filesystem.
///
/// `Err` from `resolve_root` collapses to `MissingDir` on purpose: whether the
/// directory is absent, unreadable or a file, the panel's answer is the same —
/// there is nothing to browse, and here is the path that was tried.
///
/// `git_available` / `is_repo` come back `false` from here: this runs under
/// `spawn_blocking` and git is a subprocess, so the command layer fills them in.
pub fn resolve_workspace_root(conn: &mut SqliteConnection, conversation_id: &str) -> Result<WorkspaceRoot, String> {
    let (configured, has_project) = configured_dir(conn, conversation_id)?;
    let Some(configured) = configured else {
        return Ok(if has_project {
            WorkspaceRoot::NoPath
        } else {
            WorkspaceRoot::NoProject
        });
    };

    match crate::tools::verified::resolve_root(&configured) {
        // `resolve_root` opens any filesystem object; a project whose path
        // names a regular file would otherwise report `Ok` and then fail
        // strangely on every listing. Not-a-directory is the same answer as
        // not-there: nothing to browse, and here is the path that was tried.
        Ok(real) if real.is_dir() => Ok(WorkspaceRoot::Ok {
            root: real.to_string_lossy().into_owned(),
            git_available: false,
            is_repo: false,
        }),
        _ => Ok(WorkspaceRoot::MissingDir {
            path: configured.to_string_lossy().into_owned(),
        }),
    }
}

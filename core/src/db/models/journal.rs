//! The shadow file journal's rows: file identity, content blobs, and the
//! version chain. Why three tables, and why the chain invariant is the
//! writer's job, is argued in migration 42.

use diesel::prelude::*;
use serde::Serialize;

use crate::db::schema::{journal_blobs, journal_files, journal_versions};

/// What a version row says happened. Strings rather than integers for the
/// voice-corpus reason: they appear in exports and logs, and `external` reads
/// while `6` does not.
pub mod version_op {
    pub const WRITE: &str = "write";
    pub const EDIT: &str = "edit";
    pub const PATCH: &str = "patch";
    pub const DELETE: &str = "delete";
    pub const RENAME_FROM: &str = "rename_from";
    pub const RENAME_TO: &str = "rename_to";
    /// Observed across a `run_command` bracket rather than performed by a
    /// file tool; attributed to the turn, drawn as "inferred".
    pub const COMMAND_OBSERVED: &str = "command_observed";
    /// The chain head did not match what was observed: somebody else changed
    /// the file. Carries no conversation by construction (CHECK in the DDL).
    pub const EXTERNAL: &str = "external";
    pub const REWIND: &str = "rewind";
}

/// Which capture path wrote the row.
pub mod version_source {
    pub const NATIVE: &str = "native";
    pub const HOSTED: &str = "hosted";
    pub const INFERRED: &str = "inferred";
    pub const EXTERNAL: &str = "external";
    pub const REWIND: &str = "rewind";
}

#[derive(Debug, Clone, Queryable, Selectable, Identifiable, Serialize)]
#[diesel(table_name = journal_files)]
pub struct JournalFileRow {
    pub id: String,
    /// Canonical, normalised, case-folded on Windows — a matching key, not a
    /// display string.
    pub norm_path: String,
    pub display_path: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = journal_files)]
pub struct JournalFileInsert<'a> {
    pub id: &'a str,
    pub norm_path: &'a str,
    pub display_path: &'a str,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = journal_blobs)]
pub struct JournalBlobRow {
    pub sha256: String,
    pub byte_len: i64,
    pub line_count: i32,
    pub created_at: i64,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = journal_blobs)]
pub struct JournalBlobInsert<'a> {
    pub sha256: &'a str,
    pub byte_len: i64,
    pub line_count: i32,
    pub created_at: i64,
}

#[derive(Debug, Clone, Queryable, Selectable, Identifiable, Serialize)]
#[diesel(table_name = journal_versions)]
pub struct JournalVersionRow {
    pub id: String,
    pub file_id: String,
    pub seq: i64,
    pub op: String,
    /// `None` = the file did not exist when the writer looked.
    pub observed_old_sha: Option<String>,
    /// `None` = the write was a deletion.
    pub new_sha: Option<String>,
    pub source: String,
    pub conversation_id: Option<String>,
    pub turn_id: Option<String>,
    /// Which project the file belonged to at write time — a snapshot, so a
    /// conversation moving projects or being deleted cannot rewrite history.
    pub project_id: Option<String>,
    pub origin: Option<String>,
    pub model_id: Option<String>,
    pub tool_name: Option<String>,
    /// For `rename_to`: the exact `rename_from` *version* the content came
    /// from — a version rather than a file, because the old path can be
    /// recreated later and a chain-level pointer would let blame wander into
    /// an unrelated incarnation.
    pub moved_from_version_id: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = journal_versions)]
pub struct JournalVersionInsert<'a> {
    pub id: &'a str,
    pub file_id: &'a str,
    pub seq: i64,
    pub op: &'a str,
    pub observed_old_sha: Option<&'a str>,
    pub new_sha: Option<&'a str>,
    pub source: &'a str,
    pub conversation_id: Option<&'a str>,
    pub turn_id: Option<&'a str>,
    pub project_id: Option<&'a str>,
    pub origin: Option<&'a str>,
    pub model_id: Option<&'a str>,
    pub tool_name: Option<&'a str>,
    pub moved_from_version_id: Option<&'a str>,
    pub created_at: i64,
}

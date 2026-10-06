// backend: sqlite-only
//! The registered raw SQL: every native statement the SeaORM side runs, in
//! one place, so `docs/backend-neutrality.md` can list what a second backend
//! has to answer for. Nothing outside this module can build a [`ReadOnly`],
//! and the model-contract checker refuses `Statement::from_*` anywhere else
//! outside the bridge and baseline files.
//!
//! What is SQLite's here: the `?` positional placeholders and the backend the
//! statement is prepared for. `LIKE … ESCAPE '\'` and `NOT EXISTS` are
//! standard SQL. SQLite's own `?1` numbering is deliberately not used — the
//! values are passed in the order the placeholders appear.
//!
//! No write-side helper yet: nothing needs one.

use sea_orm::{ConnectionTrait, DbBackend, DbErr, QueryResult, Statement, Value};

use crate::db::sea::cap::Read;

/// A statement that only reads. The constructor is private to this module:
/// a value of this type is a statement reviewed and registered here.
pub struct ReadOnly(&'static str);

/// Every chain under a path prefix with its head sha — `NULL` for a dead head
/// — newest-updated first, capped. Values: the escaped `LIKE` pattern, then
/// the limit.
///
/// One statement rather than a per-candidate head query: this runs before and
/// after every shell command, and a project with a long journal history would
/// otherwise turn each command into thousands of queries.
pub const JOURNAL_CHAINS_UNDER_PREFIX: ReadOnly = ReadOnly(
    "SELECT f.id, f.norm_path, f.display_path, f.created_at, f.updated_at, v.new_sha AS head_sha
     FROM journal_files f
     JOIN journal_versions v ON v.file_id = f.id
      AND v.seq = (SELECT MAX(seq) FROM journal_versions v2 WHERE v2.file_id = f.id)
     WHERE f.norm_path LIKE ? ESCAPE '\\'
     ORDER BY f.updated_at DESC
     LIMIT ?",
);

/// The live subset of [`JOURNAL_CHAINS_UNDER_PREFIX`], same values. The
/// liveness test is in SQL so the cap counts live files — applied afterwards,
/// a window of freshly deleted chains would evict the tracked files a
/// tombstone scan exists to find.
pub const JOURNAL_LIVE_CHAINS_UNDER_PREFIX: ReadOnly = ReadOnly(
    "SELECT f.id, f.norm_path, f.display_path, f.created_at, f.updated_at, v.new_sha AS head_sha
     FROM journal_files f
     JOIN journal_versions v ON v.file_id = f.id
      AND v.seq = (SELECT MAX(seq) FROM journal_versions v2 WHERE v2.file_id = f.id)
     WHERE f.norm_path LIKE ? ESCAPE '\\' AND v.new_sha IS NOT NULL
     ORDER BY f.updated_at DESC
     LIMIT ?",
);

/// Blob shas no version references any more, as `sha`. No values.
pub const JOURNAL_UNREFERENCED_BLOBS: ReadOnly = ReadOnly(
    "SELECT b.sha256 AS sha FROM journal_blobs b
     WHERE NOT EXISTS (
         SELECT 1 FROM journal_versions v
         WHERE v.observed_old_sha = b.sha256 OR v.new_sha = b.sha256
     )",
);

/// Runs a registered read-only statement with its values, in placeholder order.
pub async fn query_all(db: &impl Read, sql: ReadOnly, values: Vec<Value>) -> Result<Vec<QueryResult>, DbErr> {
    db.conn()?
        .query_all_raw(Statement::from_sql_and_values(DbBackend::Sqlite, sql.0, values))
        .await
}

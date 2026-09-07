//! Appending to and reading the file journal's version chains.
//!
//! The one writer, `append_version`, owns the chain invariant: for `seq > 1`,
//! `observed_old_sha` equals the previous version's `new_sha`. It holds by
//! construction — when the caller's observation disagrees with the chain head,
//! an `op = 'external'` version is inserted *first*, carrying the change nobody
//! here made and naming no conversation. That ordering is the "never
//! misattribute" rule made mechanical: the unexplained delta lands on
//! `external`, and only the delta the caller actually performed lands on the
//! caller's turn.
//!
//! Everything runs inside `immediate_transaction`: the head is read and the
//! next `seq` chosen under the write lock, so two conversations appending to
//! one file serialise instead of racing the unique `(file_id, seq)` index.

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::journal::{
    JournalBlobInsert, JournalFileInsert, JournalFileRow, JournalVersionInsert, JournalVersionRow,
};
use crate::db::schema::{journal_blobs, journal_files, journal_versions};
use crate::journal::blobs::StoredBlob;

/// Who did it, copied onto the row at write time.
#[derive(Debug, Clone, Default)]
pub struct Attribution<'a> {
    pub source: &'a str,
    pub conversation_id: Option<&'a str>,
    pub turn_id: Option<&'a str>,
    /// The project at write time — a snapshot, since the conversation can
    /// move projects or be deleted, and per-project cleanup selects on this.
    pub project_id: Option<&'a str>,
    pub origin: Option<&'a str>,
    pub model_id: Option<&'a str>,
    pub tool_name: Option<&'a str>,
}

/// One observed transition, ready to append. Blobs are already on disk —
/// "bytes before rows" — and `StoredBlob` is the receipt.
#[derive(Debug)]
pub struct AppendVersion<'a> {
    /// The canonical OS spelling, for display; the matching key is derived.
    pub display_path: &'a str,
    pub op: &'a str,
    /// What the writer saw before acting; `None` = the file did not exist.
    pub observed_old: Option<&'a StoredBlob>,
    /// What it left behind; `None` = deleted.
    pub new: Option<&'a StoredBlob>,
    pub attribution: Attribution<'a>,
    /// For `rename_to`: the exact `rename_from` version the content came from.
    pub moved_from_version_id: Option<&'a str>,
    pub now: i64,
}

#[derive(Debug, PartialEq)]
pub struct AppendOutcome {
    pub file_id: String,
    /// The id of the *caller's* row (never the interposed external one) — a
    /// rename's `rename_to` half links back to this.
    pub version_id: String,
    pub seq: i64,
    /// Whether an `external` version was interposed because the observation
    /// disagreed with the chain head.
    pub external_inserted: bool,
}

/// Materialise an out-of-band change now, without appending anything else.
///
/// The run_command bracket calls this *before* a command runs: whatever the
/// disk says that the chain head does not is somebody else's edit, and pinning
/// it as `external` first is what keeps it off the command's bill. Only files
/// the journal already tracks get a row — `false` for an unknown path, because
/// starting a chain on a reconcile would claim a first-observation the bracket
/// never made.
pub fn reconcile_external(
    conn: &mut SqliteConnection,
    norm_path: &str,
    observed: Option<&StoredBlob>,
    now: i64,
) -> QueryResult<bool> {
    conn.immediate_transaction(|conn| {
        let Some(file) = file_by_path(conn, norm_path)? else {
            return Ok(false);
        };
        let head = head_version(conn, &file.id)?;
        let seq = head.as_ref().map(|h| h.seq + 1).unwrap_or(1);
        let head_sha = head.as_ref().and_then(|h| h.new_sha.as_deref());
        let observed_sha = observed.map(|b| b.sha256.as_str());
        if head_sha == observed_sha {
            return Ok(false);
        }
        if let Some(blob) = observed {
            ensure_blob(conn, blob, now)?;
        }
        let row = JournalVersionInsert {
            id: &uuid::Uuid::new_v4().to_string(),
            file_id: &file.id,
            seq,
            op: crate::db::models::journal::version_op::EXTERNAL,
            observed_old_sha: head_sha,
            new_sha: observed_sha,
            source: crate::db::models::journal::version_source::EXTERNAL,
            conversation_id: None,
            turn_id: None,
            project_id: None,
            origin: None,
            model_id: None,
            tool_name: None,
            moved_from_version_id: None,
            created_at: now,
        };
        diesel::insert_into(journal_versions::table)
            .values(&row)
            .execute(conn)?;
        diesel::update(journal_files::table.find(&file.id))
            .set(journal_files::updated_at.eq(now))
            .execute(conn)?;
        Ok(true)
    })
}

/// What became of one bracketed file at settle time.
#[derive(Debug, PartialEq)]
pub enum CommandObservedOutcome {
    Recorded,
    /// The chain moved while the command ran — a real tool write from some
    /// turn landed in the window. Recording against the stale pre-state would
    /// interpose a fictional `external` transition "undoing" that legitimate
    /// write, so the bracket's observation is dropped instead; whatever the
    /// disk now says beyond the head surfaces as `external` on the next
    /// observation. Under-attribution, the permitted direction.
    HeadMoved,
    /// The chain vanished mid-window (cleanup); nothing to append to.
    NoChain,
}

/// Append a command-observed transition, if and only if the chain head still
/// equals the bracketed pre-state. The check and the insert share one
/// transaction — done as two calls, the head can move between them and the
/// fiction this exists to prevent comes back.
pub fn append_command_observed(
    conn: &mut SqliteConnection,
    norm_path: &str,
    pre: Option<&StoredBlob>,
    post: Option<&StoredBlob>,
    attribution: &Attribution,
    now: i64,
) -> QueryResult<CommandObservedOutcome> {
    conn.immediate_transaction(|conn| {
        let Some(file) = file_by_path(conn, norm_path)? else {
            return Ok(CommandObservedOutcome::NoChain);
        };
        let head = head_version(conn, &file.id)?;
        let seq = head.as_ref().map(|h| h.seq + 1).unwrap_or(1);
        let head_sha = head.as_ref().and_then(|h| h.new_sha.as_deref());
        if head_sha != pre.map(|b| b.sha256.as_str()) {
            return Ok(CommandObservedOutcome::HeadMoved);
        }
        for blob in [pre, post].into_iter().flatten() {
            ensure_blob(conn, blob, now)?;
        }
        let row = JournalVersionInsert {
            id: &uuid::Uuid::new_v4().to_string(),
            file_id: &file.id,
            seq,
            op: crate::db::models::journal::version_op::COMMAND_OBSERVED,
            observed_old_sha: pre.map(|b| b.sha256.as_str()),
            new_sha: post.map(|b| b.sha256.as_str()),
            source: crate::db::models::journal::version_source::INFERRED,
            conversation_id: attribution.conversation_id,
            turn_id: attribution.turn_id,
            project_id: attribution.project_id,
            origin: attribution.origin,
            model_id: attribution.model_id,
            tool_name: attribution.tool_name,
            moved_from_version_id: None,
            created_at: now,
        };
        diesel::insert_into(journal_versions::table)
            .values(&row)
            .execute(conn)?;
        diesel::update(journal_files::table.find(&file.id))
            .set(journal_files::updated_at.eq(now))
            .execute(conn)?;
        Ok(CommandObservedOutcome::Recorded)
    })
}

pub fn append_version(conn: &mut SqliteConnection, norm_path: &str, v: &AppendVersion) -> QueryResult<AppendOutcome> {
    conn.immediate_transaction(|conn| {
        for blob in [v.observed_old, v.new].into_iter().flatten() {
            ensure_blob(conn, blob, v.now)?;
        }

        let file = ensure_file(conn, norm_path, v.display_path, v.now)?;
        let head = head_version(conn, &file.id)?;

        let observed = v.observed_old.map(|b| b.sha256.as_str());
        let mut seq = head.as_ref().map(|h| h.seq + 1).unwrap_or(1);
        let mut external_inserted = false;

        if let Some(head) = &head
            && head.new_sha.as_deref() != observed
        {
            // The chain head is not what the writer found: someone changed the
            // file outside every capture path. That change gets its own
            // version, attributed to nobody — inserting it *before* the real
            // row is what keeps the real row's delta exactly the delta its
            // conversation performed.
            let external = JournalVersionInsert {
                id: &uuid::Uuid::new_v4().to_string(),
                file_id: &file.id,
                seq,
                op: crate::db::models::journal::version_op::EXTERNAL,
                observed_old_sha: head.new_sha.as_deref(),
                new_sha: observed,
                source: crate::db::models::journal::version_source::EXTERNAL,
                conversation_id: None,
                turn_id: None,
                project_id: None,
                origin: None,
                model_id: None,
                tool_name: None,
                moved_from_version_id: None,
                created_at: v.now,
            };
            diesel::insert_into(journal_versions::table)
                .values(&external)
                .execute(conn)?;
            seq += 1;
            external_inserted = true;
        }

        let version_id = uuid::Uuid::new_v4().to_string();
        let row = JournalVersionInsert {
            id: &version_id,
            file_id: &file.id,
            seq,
            op: v.op,
            observed_old_sha: observed,
            new_sha: v.new.map(|b| b.sha256.as_str()),
            source: v.attribution.source,
            conversation_id: v.attribution.conversation_id,
            turn_id: v.attribution.turn_id,
            project_id: v.attribution.project_id,
            origin: v.attribution.origin,
            model_id: v.attribution.model_id,
            tool_name: v.attribution.tool_name,
            moved_from_version_id: v.moved_from_version_id,
            created_at: v.now,
        };
        diesel::insert_into(journal_versions::table)
            .values(&row)
            .execute(conn)?;

        diesel::update(journal_files::table.find(&file.id))
            .set(journal_files::updated_at.eq(v.now))
            .execute(conn)?;

        Ok(AppendOutcome {
            file_id: file.id,
            version_id,
            seq,
            external_inserted,
        })
    })
}

fn ensure_blob(conn: &mut SqliteConnection, blob: &StoredBlob, now: i64) -> QueryResult<()> {
    diesel::insert_or_ignore_into(journal_blobs::table)
        .values(&JournalBlobInsert {
            sha256: &blob.sha256,
            byte_len: blob.byte_len,
            line_count: blob.line_count,
            created_at: now,
        })
        .execute(conn)?;
    Ok(())
}

fn ensure_file(
    conn: &mut SqliteConnection,
    norm_path: &str,
    display_path: &str,
    now: i64,
) -> QueryResult<JournalFileRow> {
    if let Some(existing) = file_by_path(conn, norm_path)? {
        return Ok(existing);
    }
    let row = JournalFileInsert {
        id: &uuid::Uuid::new_v4().to_string(),
        norm_path,
        display_path,
        created_at: now,
        updated_at: now,
    };
    diesel::insert_into(journal_files::table).values(&row).execute(conn)?;
    journal_files::table.find(row.id).first(conn)
}

pub fn file_by_path(conn: &mut SqliteConnection, norm_path: &str) -> QueryResult<Option<JournalFileRow>> {
    journal_files::table
        .filter(journal_files::norm_path.eq(norm_path))
        .select(JournalFileRow::as_select())
        .first(conn)
        .optional()
}

/// The newest version of a file's chain, if the file has one.
pub fn head_version(conn: &mut SqliteConnection, file_id: &str) -> QueryResult<Option<JournalVersionRow>> {
    journal_versions::table
        .filter(journal_versions::file_id.eq(file_id))
        .order(journal_versions::seq.desc())
        .select(JournalVersionRow::as_select())
        .first(conn)
        .optional()
}

/// The whole chain, oldest first — the order blame walks it.
pub fn chain(conn: &mut SqliteConnection, file_id: &str) -> QueryResult<Vec<JournalVersionRow>> {
    journal_versions::table
        .filter(journal_versions::file_id.eq(file_id))
        .order(journal_versions::seq.asc())
        .select(JournalVersionRow::as_select())
        .load(conn)
}

/// One version by id — how blame follows a `moved_from_version_id` into the
/// chain a rename came from.
pub fn version_by_id(conn: &mut SqliteConnection, id: &str) -> QueryResult<Option<JournalVersionRow>> {
    journal_versions::table
        .find(id)
        .select(JournalVersionRow::as_select())
        .first(conn)
        .optional()
}

/// Every chain under `prefix` with its head sha, dead heads included, capped.
///
/// One SQL statement rather than a per-candidate head query: this runs before
/// and after every shell command, and a project with a long journal history
/// would otherwise turn each command into thousands of queries. Dead heads
/// (`None`) are part of the answer on purpose — a file one command deleted and
/// the next recreated is not untracked, and a scan that cannot see its chain
/// leaves the recreation unattributed for ever.
///
/// `LIKE` is still not a path prefix test — ASCII-case-insensitive, and its
/// `\` inert without the `ESCAPE` clause — so it only pre-narrows; the real
/// test is `starts_with` on the exact key, applied after.
pub fn chains_under_prefix(
    conn: &mut SqliteConnection,
    prefix: &str,
    limit: usize,
) -> QueryResult<Vec<(JournalFileRow, Option<String>)>> {
    chains_query(conn, prefix, limit, false)
}

fn chains_query(
    conn: &mut SqliteConnection,
    prefix: &str,
    limit: usize,
    live_only: bool,
) -> QueryResult<Vec<(JournalFileRow, Option<String>)>> {
    #[derive(diesel::QueryableByName)]
    struct JournalChainRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        id: String,
        #[diesel(sql_type = diesel::sql_types::Text)]
        norm_path: String,
        #[diesel(sql_type = diesel::sql_types::Text)]
        display_path: String,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        created_at: i64,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        updated_at: i64,
        #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
        head_sha: Option<String>,
    }
    // `live_only` narrows in SQL so the cap counts live files — applied
    // afterwards, a window of freshly deleted chains would evict the tracked
    // files a tombstone scan exists to find (the round-one review finding,
    // kept fixed through the single-query rewrite).
    let sql = if live_only {
        "SELECT f.id, f.norm_path, f.display_path, f.created_at, f.updated_at, v.new_sha AS head_sha
         FROM journal_files f
         JOIN journal_versions v ON v.file_id = f.id
          AND v.seq = (SELECT MAX(seq) FROM journal_versions v2 WHERE v2.file_id = f.id)
         WHERE f.norm_path LIKE ?1 ESCAPE '\\' AND v.new_sha IS NOT NULL
         ORDER BY f.updated_at DESC
         LIMIT ?2"
    } else {
        "SELECT f.id, f.norm_path, f.display_path, f.created_at, f.updated_at, v.new_sha AS head_sha
         FROM journal_files f
         JOIN journal_versions v ON v.file_id = f.id
          AND v.seq = (SELECT MAX(seq) FROM journal_versions v2 WHERE v2.file_id = f.id)
         WHERE f.norm_path LIKE ?1 ESCAPE '\\'
         ORDER BY f.updated_at DESC
         LIMIT ?2"
    };
    let rows: Vec<JournalChainRow> = diesel::sql_query(sql)
        .bind::<diesel::sql_types::Text, _>(format!("{}%", like_escape(prefix)))
        // Clamped: `usize::MAX as i64` is -1, which SQLite reads as LIMIT
        // *removed* — accidentally the intent, but not a spelling to rely on.
        .bind::<diesel::sql_types::BigInt, _>(limit.min(i64::MAX as usize) as i64)
        .load(conn)?;

    Ok(rows
        .into_iter()
        .filter(|r| r.norm_path.starts_with(prefix))
        .map(|r| {
            (
                JournalFileRow {
                    id: r.id,
                    norm_path: r.norm_path,
                    display_path: r.display_path,
                    created_at: r.created_at,
                    updated_at: r.updated_at,
                },
                r.head_sha,
            )
        })
        .collect())
}

/// The live subset of [`chains_under_prefix`]: files whose chain head says
/// they still exist, with the cap counting live files. What a recursive
/// delete's tombstones scan over.
pub fn tracked_files(
    conn: &mut SqliteConnection,
    prefix: &str,
    limit: usize,
) -> QueryResult<Vec<(JournalFileRow, String)>> {
    Ok(chains_query(conn, prefix, limit, true)?
        .into_iter()
        .filter_map(|(f, head)| head.map(|sha| (f, sha)))
        .collect())
}

/// Everything a turn wrote, for rewind previews and the turn's own summary.
pub fn versions_of_turn(
    conn: &mut SqliteConnection,
    turn_id: &str,
) -> QueryResult<Vec<(JournalFileRow, JournalVersionRow)>> {
    journal_versions::table
        .inner_join(journal_files::table)
        .filter(journal_versions::turn_id.eq(turn_id))
        .order((journal_files::norm_path.asc(), journal_versions::seq.asc()))
        .select((JournalFileRow::as_select(), JournalVersionRow::as_select()))
        .load(conn)
}

/// Blob shas no version references any more — deletable, rows first, files
/// after (the reverse order would leave rows naming missing bytes).
pub fn unreferenced_blobs(conn: &mut SqliteConnection) -> QueryResult<Vec<String>> {
    diesel::sql_query(
        "SELECT b.sha256 AS sha FROM journal_blobs b
         WHERE NOT EXISTS (
             SELECT 1 FROM journal_versions v
             WHERE v.observed_old_sha = b.sha256 OR v.new_sha = b.sha256
         )",
    )
    .load::<ShaRow>(conn)
    .map(|rows| rows.into_iter().map(|r| r.sha).collect())
}

pub fn delete_blob_rows(conn: &mut SqliteConnection, shas: &[String]) -> QueryResult<usize> {
    diesel::delete(journal_blobs::table.filter(journal_blobs::sha256.eq_any(shas))).execute(conn)
}

#[derive(diesel::QueryableByName)]
struct ShaRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    sha: String,
}

/// LIKE special characters escaped so a path containing `%` or `_` cannot
/// widen a prefix scan. The escape character goes first: on Unix a `\` is an
/// ordinary path character, and left bare it would escape whatever follows it
/// in the pattern — a project under `/tmp/a\b` would match nothing at all.
fn like_escape(s: &str) -> String {
    s.replace('\\', r"\\").replace('%', r"\%").replace('_', r"\_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;
    use crate::journal::blobs::StoredBlob;

    fn blob(content: &str) -> StoredBlob {
        StoredBlob {
            sha256: crate::journal::blobs::sha256_of(content),
            byte_len: content.len() as i64,
            line_count: content.lines().count() as i32,
        }
    }

    fn append(
        conn: &mut SqliteConnection,
        path: &str,
        old: Option<&StoredBlob>,
        new: Option<&StoredBlob>,
        conversation: Option<&str>,
        now: i64,
    ) -> AppendOutcome {
        append_version(
            conn,
            path,
            &AppendVersion {
                display_path: path,
                op: "edit",
                observed_old: old,
                new,
                attribution: Attribution {
                    source: "native",
                    conversation_id: conversation,
                    turn_id: conversation.map(|_| "t1"),
                    project_id: conversation.map(|_| "proj"),
                    origin: Some("desktop"),
                    model_id: None,
                    tool_name: Some("edit_file"),
                },
                moved_from_version_id: None,
                now,
            },
        )
        .unwrap()
    }

    #[test]
    fn a_matching_observation_appends_without_an_external_row() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let (a, b) = (blob("v1"), blob("v2"));

        let first = append(&mut conn, "c:/p/a.rs", None, Some(&a), Some("conv"), 1);
        assert_eq!((first.seq, first.external_inserted), (1, false));

        let second = append(&mut conn, "c:/p/a.rs", Some(&a), Some(&b), Some("conv"), 2);
        assert_eq!((second.seq, second.external_inserted), (2, false));
    }

    /// The core discipline: an observation that disagrees with the head makes
    /// an `external` version *first*, attributed to nobody, and the real row
    /// lands after it — so the chain invariant holds and the conversation is
    /// only credited with the delta it performed.
    #[test]
    fn a_mismatched_observation_interposes_an_external_version() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let (a, hand_edit, b) = (blob("v1"), blob("hand-edited"), blob("v2"));

        append(&mut conn, "c:/p/a.rs", None, Some(&a), Some("conv"), 1);
        // The conversation's tool observed `hand_edit`, not `a`: somebody
        // touched the file in between.
        let out = append(&mut conn, "c:/p/a.rs", Some(&hand_edit), Some(&b), Some("conv"), 2);
        assert!(out.external_inserted);
        assert_eq!(out.seq, 3, "external took seq 2, the real row took 3");

        let rows = chain(&mut conn, &out.file_id).unwrap();
        assert_eq!(rows.len(), 3);
        let ext = &rows[1];
        assert_eq!(ext.op, "external");
        assert_eq!(ext.source, "external");
        assert_eq!(ext.conversation_id, None, "external rows name nobody");
        assert_eq!(ext.observed_old_sha.as_deref(), Some(a.sha256.as_str()));
        assert_eq!(ext.new_sha.as_deref(), Some(hand_edit.sha256.as_str()));
        // Chain invariant: every row's observed_old equals its predecessor's new.
        for pair in rows.windows(2) {
            assert_eq!(pair[1].observed_old_sha, pair[0].new_sha);
        }
    }

    /// The CHECK is the second lock on the same door: even a buggy writer
    /// cannot record an external change under a conversation's name.
    #[test]
    fn the_schema_refuses_an_attributed_external_row() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let a = blob("v1");
        let out = append(&mut conn, "c:/p/a.rs", None, Some(&a), Some("conv"), 1);

        let bad = JournalVersionInsert {
            id: "bad",
            file_id: &out.file_id,
            seq: 99,
            op: "external",
            observed_old_sha: None,
            new_sha: None,
            source: "external",
            conversation_id: Some("conv"),
            turn_id: None,
            project_id: None,
            origin: None,
            model_id: None,
            tool_name: None,
            moved_from_version_id: None,
            created_at: 9,
        };
        assert!(
            diesel::insert_into(journal_versions::table)
                .values(&bad)
                .execute(&mut conn)
                .is_err()
        );
    }

    #[test]
    fn deletion_and_recreation_stay_on_one_chain() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let (a, b) = (blob("v1"), blob("v2"));

        append(&mut conn, "c:/p/a.rs", None, Some(&a), Some("conv"), 1);
        let gone = append(&mut conn, "c:/p/a.rs", Some(&a), None, Some("conv"), 2);
        assert!(!gone.external_inserted);
        // Recreated: the writer observed "no file", which matches the head.
        let back = append(&mut conn, "c:/p/a.rs", None, Some(&b), Some("conv"), 3);
        assert!(!back.external_inserted);
        assert_eq!(back.seq, 3);
    }

    /// Two connections appending to one file serialise on BEGIN IMMEDIATE
    /// rather than racing the `(file_id, seq)` unique index.
    #[test]
    fn concurrent_appends_do_not_collide_on_seq() {
        let pool = test_db();
        {
            let mut conn = pool.get().unwrap();
            let a = blob("v1");
            append(&mut conn, "c:/p/a.rs", None, Some(&a), Some("conv"), 1);
        }

        let handles: Vec<_> = (0..2)
            .map(|i| {
                let pool = pool.clone();
                std::thread::spawn(move || {
                    let mut conn = pool.get().unwrap();
                    let a = blob("v1");
                    let next = blob(&format!("writer-{i}"));
                    append(&mut conn, "c:/p/a.rs", Some(&a), Some(&next), Some("conv"), 10 + i)
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let mut conn = pool.get().unwrap();
        let file = file_by_path(&mut conn, "c:/p/a.rs").unwrap().unwrap();
        let rows = chain(&mut conn, &file.id).unwrap();
        let seqs: Vec<i64> = rows.iter().map(|r| r.seq).collect();
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(seqs.len(), sorted.len(), "no duplicate seq: {seqs:?}");
        for pair in rows.windows(2) {
            assert_eq!(pair[1].observed_old_sha, pair[0].new_sha);
        }
    }

    #[test]
    fn tracked_files_reports_only_living_heads_under_the_prefix() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let a = blob("v1");

        append(&mut conn, "c:/p/alive.rs", None, Some(&a), Some("conv"), 1);
        append(&mut conn, "c:/p/dead.rs", None, Some(&a), Some("conv"), 2);
        append(&mut conn, "c:/p/dead.rs", Some(&a), None, Some("conv"), 3);
        append(&mut conn, "c:/elsewhere/other.rs", None, Some(&a), Some("conv"), 4);

        let got = tracked_files(&mut conn, "c:/p/", 100).unwrap();
        let paths: Vec<&str> = got.iter().map(|(f, _)| f.norm_path.as_str()).collect();
        assert_eq!(paths, vec!["c:/p/alive.rs"]);
    }

    /// SQLite's LIKE is not a path prefix test: `_` is a wildcard without an
    /// ESCAPE clause, and matching is ASCII-case-insensitive. Both would make
    /// the run_command bracket scan the wrong files.
    #[test]
    fn tracked_files_prefix_is_literal_and_case_exact() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let a = blob("v1");

        // `p_x` as a LIKE pattern would match `pyx`; as a literal it must not.
        append(&mut conn, "c:/p_x/one.rs", None, Some(&a), Some("conv"), 1);
        append(&mut conn, "c:/pyx/two.rs", None, Some(&a), Some("conv"), 2);
        let got = tracked_files(&mut conn, "c:/p_x/", 100).unwrap();
        let paths: Vec<&str> = got.iter().map(|(f, _)| f.norm_path.as_str()).collect();
        assert_eq!(paths, vec!["c:/p_x/one.rs"]);

        // LIKE is case-insensitive; the journal's keys are case-exact (they
        // are already case-folded per platform before they get here).
        append(&mut conn, "c:/repo/lower.rs", None, Some(&a), Some("conv"), 3);
        append(&mut conn, "c:/Repo/upper.rs", None, Some(&a), Some("conv"), 4);
        let got = tracked_files(&mut conn, "c:/repo/", 100).unwrap();
        let paths: Vec<&str> = got.iter().map(|(f, _)| f.norm_path.as_str()).collect();
        assert_eq!(paths, vec!["c:/repo/lower.rs"]);
    }

    /// The cap counts live files: a window's worth of recently deleted chains
    /// must not evict the tracked file the scan exists to watch.
    #[test]
    fn tracked_files_cap_is_applied_after_the_liveness_filter() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let a = blob("v1");

        // Oldest: one live file.
        append(&mut conn, "c:/p/alive.rs", None, Some(&a), Some("conv"), 1);
        // Newer: three deleted chains that would fill a cap of 3 on their own.
        for i in 0..3 {
            let path = format!("c:/p/dead{i}.rs");
            append(&mut conn, &path, None, Some(&a), Some("conv"), 10 + i);
            append(&mut conn, &path, Some(&a), None, Some("conv"), 20 + i);
        }

        let got = tracked_files(&mut conn, "c:/p/", 3).unwrap();
        let paths: Vec<&str> = got.iter().map(|(f, _)| f.norm_path.as_str()).collect();
        assert_eq!(paths, vec!["c:/p/alive.rs"]);
    }

    #[test]
    fn unreferenced_blobs_spares_everything_a_version_names() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let (a, b) = (blob("v1"), blob("v2"));
        append(&mut conn, "c:/p/a.rs", None, Some(&a), Some("conv"), 1);
        // `b` gets a row but no version referencing it.
        diesel::insert_or_ignore_into(journal_blobs::table)
            .values(&JournalBlobInsert {
                sha256: &b.sha256,
                byte_len: b.byte_len,
                line_count: b.line_count,
                created_at: 5,
            })
            .execute(&mut conn)
            .unwrap();

        let orphans = unreferenced_blobs(&mut conn).unwrap();
        assert_eq!(orphans, vec![b.sha256.clone()]);
        assert_eq!(delete_blob_rows(&mut conn, &orphans).unwrap(), 1);
    }
}

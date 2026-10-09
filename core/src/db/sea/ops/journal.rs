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
//! No function here opens a transaction of its own: a write takes the
//! caller's `WriteTx`, and the caller's `Db::write` is the `BEGIN IMMEDIATE`.
//! The head is therefore read and the next `seq` chosen under the write lock,
//! so two conversations appending to one file serialise instead of racing the
//! unique `(file_id, seq)` index.

use sea_orm::ActiveValue::{Set, Unchanged};
use sea_orm::sea_query::OnConflict;
use sea_orm::{ColumnTrait, DbErr, EntityTrait, FromQueryResult, IntoActiveModel, QueryFilter, QueryOrder};

use crate::db::entity::journal_version::{VersionOp, VersionSource};
use crate::db::entity::{journal_blob, journal_file, journal_version};
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::sql;
use crate::db::types::EpochMs;
use crate::journal::blobs::StoredBlob;

/// Who did it, copied onto the row at write time.
#[derive(Debug, Clone)]
pub struct Attribution<'a> {
    pub source: VersionSource,
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
    pub op: VersionOp,
    /// What the writer saw before acting; `None` = the file did not exist.
    pub observed_old: Option<&'a StoredBlob>,
    /// What it left behind; `None` = deleted.
    pub new: Option<&'a StoredBlob>,
    pub attribution: Attribution<'a>,
    /// For `rename_to`: the exact `rename_from` version the content came from.
    pub moved_from_version_id: Option<&'a str>,
    pub now: EpochMs,
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

/// The row an `external` transition gets: the delta from the chain head to
/// what was observed, attributed to nobody.
fn external_row(
    file_id: &str,
    seq: i64,
    head_sha: Option<&str>,
    observed: Option<&str>,
    now: EpochMs,
) -> journal_version::ActiveModel {
    journal_version::ActiveModel {
        id: Set(uuid::Uuid::new_v4().to_string()),
        file_id: Set(file_id.to_owned()),
        seq: Set(seq),
        op: Set(VersionOp::External),
        observed_old_sha: Set(head_sha.map(str::to_owned)),
        new_sha: Set(observed.map(str::to_owned)),
        source: Set(VersionSource::External),
        conversation_id: Set(None),
        turn_id: Set(None),
        project_id: Set(None),
        origin: Set(None),
        model_id: Set(None),
        tool_name: Set(None),
        moved_from_version_id: Set(None),
        created_at: Set(now),
    }
}

/// The row a caller's transition gets, carrying its attribution.
#[allow(clippy::too_many_arguments)]
fn attributed_row(
    id: String,
    file_id: &str,
    seq: i64,
    op: VersionOp,
    observed_old_sha: Option<&str>,
    new_sha: Option<&str>,
    attribution: &Attribution<'_>,
    moved_from_version_id: Option<&str>,
    now: EpochMs,
) -> journal_version::ActiveModel {
    journal_version::ActiveModel {
        id: Set(id),
        file_id: Set(file_id.to_owned()),
        seq: Set(seq),
        op: Set(op),
        observed_old_sha: Set(observed_old_sha.map(str::to_owned)),
        new_sha: Set(new_sha.map(str::to_owned)),
        source: Set(attribution.source),
        conversation_id: Set(attribution.conversation_id.map(str::to_owned)),
        turn_id: Set(attribution.turn_id.map(str::to_owned)),
        project_id: Set(attribution.project_id.map(str::to_owned)),
        origin: Set(attribution.origin.map(str::to_owned)),
        model_id: Set(attribution.model_id.map(str::to_owned)),
        tool_name: Set(attribution.tool_name.map(str::to_owned)),
        moved_from_version_id: Set(moved_from_version_id.map(str::to_owned)),
        created_at: Set(now),
    }
}

async fn insert_version(tx: &WriteTx, row: journal_version::ActiveModel) -> Result<(), DbErr> {
    journal_version::Entity::insert(row)
        .exec_without_returning(tx.conn()?)
        .await?;
    Ok(())
}

async fn touch_file(tx: &WriteTx, file_id: &str, now: EpochMs) -> Result<(), DbErr> {
    let row = journal_file::ActiveModel {
        id: Unchanged(file_id.to_owned()),
        updated_at: Set(now),
        ..Default::default()
    };
    journal_file::Entity::update(row)
        .exec_without_returning(tx.conn()?)
        .await?;
    Ok(())
}

/// Materialise an out-of-band change now, without appending anything else.
///
/// The run_command bracket calls this *before* a command runs: whatever the
/// disk says that the chain head does not is somebody else's edit, and pinning
/// it as `external` first is what keeps it off the command's bill. Only files
/// the journal already tracks get a row — `false` for an unknown path, because
/// starting a chain on a reconcile would claim a first-observation the bracket
/// never made.
pub async fn reconcile_external(
    tx: &WriteTx,
    norm_path: &str,
    observed: Option<&StoredBlob>,
    now: EpochMs,
) -> Result<bool, DbErr> {
    let Some(file) = file_by_path(tx, norm_path).await? else {
        return Ok(false);
    };
    let head = head_version(tx, &file.id).await?;
    let seq = head.as_ref().map(|h| h.seq + 1).unwrap_or(1);
    let head_sha = head.as_ref().and_then(|h| h.new_sha.as_deref());
    let observed_sha = observed.map(|b| b.sha256.as_str());
    if head_sha == observed_sha {
        return Ok(false);
    }
    if let Some(blob) = observed {
        ensure_blob(tx, blob, now).await?;
    }
    insert_version(tx, external_row(&file.id, seq, head_sha, observed_sha, now)).await?;
    touch_file(tx, &file.id, now).await?;
    Ok(true)
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
pub async fn append_command_observed(
    tx: &WriteTx,
    norm_path: &str,
    pre: Option<&StoredBlob>,
    post: Option<&StoredBlob>,
    attribution: &Attribution<'_>,
    now: EpochMs,
) -> Result<CommandObservedOutcome, DbErr> {
    let Some(file) = file_by_path(tx, norm_path).await? else {
        return Ok(CommandObservedOutcome::NoChain);
    };
    let head = head_version(tx, &file.id).await?;
    let seq = head.as_ref().map(|h| h.seq + 1).unwrap_or(1);
    let head_sha = head.as_ref().and_then(|h| h.new_sha.as_deref());
    if head_sha != pre.map(|b| b.sha256.as_str()) {
        return Ok(CommandObservedOutcome::HeadMoved);
    }
    for blob in [pre, post].into_iter().flatten() {
        ensure_blob(tx, blob, now).await?;
    }
    let row = attributed_row(
        uuid::Uuid::new_v4().to_string(),
        &file.id,
        seq,
        VersionOp::CommandObserved,
        pre.map(|b| b.sha256.as_str()),
        post.map(|b| b.sha256.as_str()),
        attribution,
        None,
        now,
    );
    insert_version(tx, row).await?;
    touch_file(tx, &file.id, now).await?;
    Ok(CommandObservedOutcome::Recorded)
}

pub async fn append_version(tx: &WriteTx, norm_path: &str, v: &AppendVersion<'_>) -> Result<AppendOutcome, DbErr> {
    for blob in [v.observed_old, v.new].into_iter().flatten() {
        ensure_blob(tx, blob, v.now).await?;
    }

    let file = ensure_file(tx, norm_path, v.display_path, v.now).await?;
    let head = head_version(tx, &file.id).await?;

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
        insert_version(
            tx,
            external_row(&file.id, seq, head.new_sha.as_deref(), observed, v.now),
        )
        .await?;
        seq += 1;
        external_inserted = true;
    }

    let version_id = uuid::Uuid::new_v4().to_string();
    let row = attributed_row(
        version_id.clone(),
        &file.id,
        seq,
        v.op,
        observed,
        v.new.map(|b| b.sha256.as_str()),
        &v.attribution,
        v.moved_from_version_id,
        v.now,
    );
    insert_version(tx, row).await?;
    touch_file(tx, &file.id, v.now).await?;

    Ok(AppendOutcome {
        file_id: file.id,
        version_id,
        seq,
        external_inserted,
    })
}

/// The blob's row, if it has none yet. Content-addressed, so a row that
/// already exists describes the same bytes and is left alone.
async fn ensure_blob(tx: &WriteTx, blob: &StoredBlob, now: EpochMs) -> Result<(), DbErr> {
    let row = journal_blob::ActiveModel {
        sha256: Set(blob.sha256.clone()),
        byte_len: Set(blob.byte_len),
        line_count: Set(blob.line_count),
        created_at: Set(now),
    };
    journal_blob::Entity::insert(row)
        .on_conflict(OnConflict::column(journal_blob::Column::Sha256).do_nothing().to_owned())
        .exec_without_returning(tx.conn()?)
        .await?;
    Ok(())
}

async fn ensure_file(
    tx: &WriteTx,
    norm_path: &str,
    display_path: &str,
    now: EpochMs,
) -> Result<journal_file::Model, DbErr> {
    if let Some(existing) = file_by_path(tx, norm_path).await? {
        return Ok(existing);
    }
    let row = journal_file::Model {
        id: uuid::Uuid::new_v4().to_string(),
        norm_path: norm_path.to_owned(),
        display_path: display_path.to_owned(),
        created_at: now,
        updated_at: now,
    };
    journal_file::Entity::insert(row.clone().into_active_model())
        .exec_without_returning(tx.conn()?)
        .await?;
    Ok(row)
}

pub async fn file_by_path(db: &impl Read, norm_path: &str) -> Result<Option<journal_file::Model>, DbErr> {
    journal_file::Entity::find()
        .filter(journal_file::Column::NormPath.eq(norm_path))
        .one(db.conn()?)
        .await
}

/// The newest version of a file's chain, if the file has one.
pub async fn head_version(db: &impl Read, file_id: &str) -> Result<Option<journal_version::Model>, DbErr> {
    journal_version::Entity::find()
        .filter(journal_version::Column::FileId.eq(file_id))
        .order_by_desc(journal_version::Column::Seq)
        .one(db.conn()?)
        .await
}

/// The whole chain, oldest first — the order blame walks it.
pub async fn chain(db: &impl Read, file_id: &str) -> Result<Vec<journal_version::Model>, DbErr> {
    journal_version::Entity::find()
        .filter(journal_version::Column::FileId.eq(file_id))
        .order_by_asc(journal_version::Column::Seq)
        .all(db.conn()?)
        .await
}

/// One version by id — how blame follows a `moved_from_version_id` into the
/// chain a rename came from.
pub async fn version_by_id(db: &impl Read, id: &str) -> Result<Option<journal_version::Model>, DbErr> {
    journal_version::Entity::find_by_id(id).one(db.conn()?).await
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
pub async fn chains_under_prefix(
    db: &impl Read,
    prefix: &str,
    limit: usize,
) -> Result<Vec<(journal_file::Model, Option<String>)>, DbErr> {
    chains_query(db, prefix, limit, false).await
}

async fn chains_query(
    db: &impl Read,
    prefix: &str,
    limit: usize,
    live_only: bool,
) -> Result<Vec<(journal_file::Model, Option<String>)>, DbErr> {
    #[derive(FromQueryResult)]
    struct JournalChainRow {
        id: String,
        norm_path: String,
        display_path: String,
        created_at: i64,
        updated_at: i64,
        head_sha: Option<String>,
    }
    // `live_only` narrows in SQL so the cap counts live files — applied
    // afterwards, a window of freshly deleted chains would evict the tracked
    // files a tombstone scan exists to find (the round-one review finding,
    // kept fixed through the single-query rewrite).
    let statement = if live_only {
        sql::JOURNAL_LIVE_CHAINS_UNDER_PREFIX
    } else {
        sql::JOURNAL_CHAINS_UNDER_PREFIX
    };
    let rows = sql::query_all(
        db,
        statement,
        vec![
            format!("{}%", like_escape(prefix)).into(),
            // Clamped: `usize::MAX as i64` is -1, which SQLite reads as LIMIT
            // *removed* — accidentally the intent, but not a spelling to rely on.
            (limit.min(i64::MAX as usize) as i64).into(),
        ],
    )
    .await?;

    let rows: Vec<JournalChainRow> = rows
        .iter()
        .map(|row| JournalChainRow::from_query_result(row, ""))
        .collect::<Result<_, _>>()?;

    Ok(rows
        .into_iter()
        .filter(|r| r.norm_path.starts_with(prefix))
        .map(|r| {
            (
                journal_file::Model {
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
pub async fn tracked_files(
    db: &impl Read,
    prefix: &str,
    limit: usize,
) -> Result<Vec<(journal_file::Model, String)>, DbErr> {
    Ok(chains_query(db, prefix, limit, true)
        .await?
        .into_iter()
        .filter_map(|(f, head)| head.map(|sha| (f, sha)))
        .collect())
}

/// Everything a turn wrote, for rewind previews and the turn's own summary.
pub async fn versions_of_turn(
    db: &impl Read,
    turn_id: &str,
) -> Result<Vec<(journal_file::Model, journal_version::Model)>, DbErr> {
    journal_version::Entity::find()
        .find_also_related(journal_file::Entity)
        .filter(journal_version::Column::TurnId.eq(turn_id))
        .order_by_asc(journal_file::Column::NormPath)
        .order_by_asc(journal_version::Column::Seq)
        .all(db.conn()?)
        .await?
        .into_iter()
        .map(|(version, file)| {
            // `file_id` is NOT NULL with a cascading key, so a version without
            // its file is a broken database, not an empty answer.
            file.map(|file| (file, version.clone())).ok_or_else(|| {
                DbErr::RecordNotFound(format!(
                    "journal_files row `{}` of version `{}`",
                    version.file_id, version.id
                ))
            })
        })
        .collect()
}

/// Blob shas no version references any more — deletable, rows first, files
/// after (the reverse order would leave rows naming missing bytes).
pub async fn unreferenced_blobs(db: &impl Read) -> Result<Vec<String>, DbErr> {
    sql::query_all(db, sql::JOURNAL_UNREFERENCED_BLOBS, Vec::new())
        .await?
        .iter()
        .map(|row| row.try_get("", "sha"))
        .collect()
}

pub async fn delete_blob_rows(tx: &WriteTx, shas: &[String]) -> Result<u64, DbErr> {
    Ok(journal_blob::Entity::delete_many()
        .filter(journal_blob::Column::Sha256.is_in(shas.iter().map(String::as_str)))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
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
    use crate::db::sea::cap::Db;
    use crate::db::sea::{file_test_db, sea_test_db};
    use crate::journal::blobs::StoredBlob;

    fn blob(content: &str) -> StoredBlob {
        StoredBlob {
            sha256: crate::journal::blobs::sha256_of(content),
            byte_len: content.len() as i64,
            line_count: content.lines().count() as i32,
        }
    }

    async fn append(
        db: &Db,
        path: &str,
        old: Option<&StoredBlob>,
        new: Option<&StoredBlob>,
        conversation: Option<&str>,
        now: i64,
    ) -> AppendOutcome {
        db.write(async |tx| {
            append_version(
                tx,
                path,
                &AppendVersion {
                    display_path: path,
                    op: VersionOp::Edit,
                    observed_old: old,
                    new,
                    attribution: Attribution {
                        source: VersionSource::Native,
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
            .await
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_matching_observation_appends_without_an_external_row() {
        let db = sea_test_db().await;
        let (a, b) = (blob("v1"), blob("v2"));

        let first = append(&db, "c:/p/a.rs", None, Some(&a), Some("conv"), 1).await;
        assert_eq!((first.seq, first.external_inserted), (1, false));

        let second = append(&db, "c:/p/a.rs", Some(&a), Some(&b), Some("conv"), 2).await;
        assert_eq!((second.seq, second.external_inserted), (2, false));
    }

    /// The core discipline: an observation that disagrees with the head makes
    /// an `external` version *first*, attributed to nobody, and the real row
    /// lands after it — so the chain invariant holds and the conversation is
    /// only credited with the delta it performed.
    #[tokio::test]
    async fn a_mismatched_observation_interposes_an_external_version() {
        let db = sea_test_db().await;
        let (a, hand_edit, b) = (blob("v1"), blob("hand-edited"), blob("v2"));

        append(&db, "c:/p/a.rs", None, Some(&a), Some("conv"), 1).await;
        // The conversation's tool observed `hand_edit`, not `a`: somebody
        // touched the file in between.
        let out = append(&db, "c:/p/a.rs", Some(&hand_edit), Some(&b), Some("conv"), 2).await;
        assert!(out.external_inserted);
        assert_eq!(out.seq, 3, "external took seq 2, the real row took 3");

        let rows = chain(&db, &out.file_id).await.unwrap();
        assert_eq!(rows.len(), 3);
        let ext = &rows[1];
        assert_eq!(ext.op, VersionOp::External);
        assert_eq!(ext.source, VersionSource::External);
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
    #[tokio::test]
    async fn the_schema_refuses_an_attributed_external_row() {
        let db = sea_test_db().await;
        let a = blob("v1");
        let out = append(&db, "c:/p/a.rs", None, Some(&a), Some("conv"), 1).await;

        let bad = attributed_row(
            "bad".to_owned(),
            &out.file_id,
            99,
            VersionOp::External,
            None,
            None,
            &Attribution {
                source: VersionSource::External,
                conversation_id: Some("conv"),
                turn_id: None,
                project_id: None,
                origin: None,
                model_id: None,
                tool_name: None,
            },
            None,
            9,
        );
        assert!(db.write(async |tx| insert_version(tx, bad).await).await.is_err());
    }

    #[tokio::test]
    async fn deletion_and_recreation_stay_on_one_chain() {
        let db = sea_test_db().await;
        let (a, b) = (blob("v1"), blob("v2"));

        append(&db, "c:/p/a.rs", None, Some(&a), Some("conv"), 1).await;
        let gone = append(&db, "c:/p/a.rs", Some(&a), None, Some("conv"), 2).await;
        assert!(!gone.external_inserted);
        // Recreated: the writer observed "no file", which matches the head.
        let back = append(&db, "c:/p/a.rs", None, Some(&b), Some("conv"), 3).await;
        assert!(!back.external_inserted);
        assert_eq!(back.seq, 3);
    }

    /// Two tasks appending to one file serialise on BEGIN IMMEDIATE rather
    /// than racing the `(file_id, seq)` unique index. On a file-backed pool
    /// with more than one connection — the in-memory test database has one,
    /// and would serialise on the pool instead of on the lock. Two worker
    /// threads so the two appends really overlap rather than take turns at
    /// the await points of one thread.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_appends_do_not_collide_on_seq() {
        let dir = tempfile::tempdir().unwrap();
        let db = file_test_db(dir.path()).await;
        let a = blob("v1");
        append(&db, "c:/p/a.rs", None, Some(&a), Some("conv"), 1).await;

        let handles: Vec<_> = (0..2)
            .map(|i| {
                let db = db.clone();
                tokio::spawn(async move {
                    let a = blob("v1");
                    let next = blob(&format!("writer-{i}"));
                    append(&db, "c:/p/a.rs", Some(&a), Some(&next), Some("conv"), 10 + i).await
                })
            })
            .collect();
        for h in handles {
            h.await.unwrap();
        }

        let file = file_by_path(&db, "c:/p/a.rs").await.unwrap().unwrap();
        let rows = chain(&db, &file.id).await.unwrap();
        let seqs: Vec<i64> = rows.iter().map(|r| r.seq).collect();
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(seqs.len(), sorted.len(), "no duplicate seq: {seqs:?}");
        assert_eq!(rows.len(), 4, "both writers landed, the second behind an external row");
        for pair in rows.windows(2) {
            assert_eq!(pair[1].observed_old_sha, pair[0].new_sha);
        }
    }

    #[tokio::test]
    async fn tracked_files_reports_only_living_heads_under_the_prefix() {
        let db = sea_test_db().await;
        let a = blob("v1");

        append(&db, "c:/p/alive.rs", None, Some(&a), Some("conv"), 1).await;
        append(&db, "c:/p/dead.rs", None, Some(&a), Some("conv"), 2).await;
        append(&db, "c:/p/dead.rs", Some(&a), None, Some("conv"), 3).await;
        append(&db, "c:/elsewhere/other.rs", None, Some(&a), Some("conv"), 4).await;

        let got = tracked_files(&db, "c:/p/", 100).await.unwrap();
        let paths: Vec<&str> = got.iter().map(|(f, _)| f.norm_path.as_str()).collect();
        assert_eq!(paths, vec!["c:/p/alive.rs"]);

        let all = chains_under_prefix(&db, "c:/p/", 100).await.unwrap();
        let mut heads: Vec<(&str, bool)> = all
            .iter()
            .map(|(f, head)| (f.norm_path.as_str(), head.is_some()))
            .collect();
        heads.sort_unstable();
        assert_eq!(
            heads,
            vec![("c:/p/alive.rs", true), ("c:/p/dead.rs", false)],
            "dead heads are part of the unfiltered answer"
        );
    }

    /// SQLite's LIKE is not a path prefix test: `_` is a wildcard without an
    /// ESCAPE clause, and matching is ASCII-case-insensitive. Both would make
    /// the run_command bracket scan the wrong files.
    #[tokio::test]
    async fn tracked_files_prefix_is_literal_and_case_exact() {
        let db = sea_test_db().await;
        let a = blob("v1");

        // `p_x` as a LIKE pattern would match `pyx`; as a literal it must not.
        append(&db, "c:/p_x/one.rs", None, Some(&a), Some("conv"), 1).await;
        append(&db, "c:/pyx/two.rs", None, Some(&a), Some("conv"), 2).await;
        let got = tracked_files(&db, "c:/p_x/", 100).await.unwrap();
        let paths: Vec<&str> = got.iter().map(|(f, _)| f.norm_path.as_str()).collect();
        assert_eq!(paths, vec!["c:/p_x/one.rs"]);

        // LIKE is case-insensitive; the journal's keys are case-exact (they
        // are already case-folded per platform before they get here).
        append(&db, "c:/repo/lower.rs", None, Some(&a), Some("conv"), 3).await;
        append(&db, "c:/Repo/upper.rs", None, Some(&a), Some("conv"), 4).await;
        let got = tracked_files(&db, "c:/repo/", 100).await.unwrap();
        let paths: Vec<&str> = got.iter().map(|(f, _)| f.norm_path.as_str()).collect();
        assert_eq!(paths, vec!["c:/repo/lower.rs"]);
    }

    /// The cap counts live files: a window's worth of recently deleted chains
    /// must not evict the tracked file the scan exists to watch.
    #[tokio::test]
    async fn tracked_files_cap_is_applied_after_the_liveness_filter() {
        let db = sea_test_db().await;
        let a = blob("v1");

        // Oldest: one live file.
        append(&db, "c:/p/alive.rs", None, Some(&a), Some("conv"), 1).await;
        // Newer: three deleted chains that would fill a cap of 3 on their own.
        for i in 0..3 {
            let path = format!("c:/p/dead{i}.rs");
            append(&db, &path, None, Some(&a), Some("conv"), 10 + i).await;
            append(&db, &path, Some(&a), None, Some("conv"), 20 + i).await;
        }

        let got = tracked_files(&db, "c:/p/", 3).await.unwrap();
        let paths: Vec<&str> = got.iter().map(|(f, _)| f.norm_path.as_str()).collect();
        assert_eq!(paths, vec!["c:/p/alive.rs"]);
    }

    #[tokio::test]
    async fn unreferenced_blobs_spares_everything_a_version_names() {
        let db = sea_test_db().await;
        let (a, b) = (blob("v1"), blob("v2"));
        append(&db, "c:/p/a.rs", None, Some(&a), Some("conv"), 1).await;
        // `b` gets a row but no version referencing it.
        db.write(async |tx| ensure_blob(tx, &b, 5).await).await.unwrap();

        let orphans = unreferenced_blobs(&db).await.unwrap();
        assert_eq!(orphans, vec![b.sha256.clone()]);
        assert_eq!(
            db.write(async |tx| delete_blob_rows(tx, &orphans).await).await.unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn versions_of_turn_pairs_each_row_with_its_file_in_path_then_seq_order() {
        let db = sea_test_db().await;
        let (a, b) = (blob("v1"), blob("v2"));
        append(&db, "c:/p/b.rs", None, Some(&a), Some("conv"), 1).await;
        append(&db, "c:/p/a.rs", None, Some(&a), Some("conv"), 2).await;
        append(&db, "c:/p/a.rs", Some(&a), Some(&b), Some("conv"), 3).await;
        append(&db, "c:/p/other.rs", None, Some(&a), None, 4).await;

        let got = versions_of_turn(&db, "t1").await.unwrap();
        let keys: Vec<(&str, i64)> = got.iter().map(|(f, v)| (f.norm_path.as_str(), v.seq)).collect();
        assert_eq!(keys, vec![("c:/p/a.rs", 1), ("c:/p/a.rs", 2), ("c:/p/b.rs", 1)]);
        assert!(got.iter().all(|(f, v)| v.file_id == f.id));
    }
}

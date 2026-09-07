use diesel::prelude::*;
use diesel::sql_types::{BigInt, Text};
use diesel::sqlite::SqliteConnection;

use crate::db::models::memory::MemoryScope;
use crate::db::models::memory::{
    DeletedBy, MAX_CLIENT_GLOBAL_MEMORIES, MAX_MEMORIES_PER_PROJECT, MAX_MEMORIES_PER_SUBJECT, MAX_MEMORY_CONTENT_LEN,
    MAX_ONEBOT_GLOBAL_MEMORIES, MAX_PINNED_SUBJECTS, MAX_REMEMBERED_SUBJECTS, MAX_TRACKED_SUBJECTS, MemoryChangeset,
    MemoryInsert, MemoryProposalInsert, MemoryProposalRow, MemoryRow, MemorySubjectInsert, MemorySubjectRow, Origin,
    ProposalStatus, Visibility,
};
use crate::db::schema::{memories, memory_proposals, memory_subjects};

fn contract_error(message: String) -> diesel::result::Error {
    diesel::result::Error::QueryBuilderError(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, message)))
}

/// Trash retention. Soft-deleted rows outlive the delete so `/memory undo` and
/// the desktop trash have something to restore.
pub const TRASH_RETENTION_MS: i64 = 30 * 24 * 3600 * 1000;

/// Which memories a given caller may see, in a given place. Injection and
/// `/memory me` both go through this so the two can never disagree about what
/// the bot knows versus what it admits to knowing.
#[derive(Debug, Clone, Default)]
pub struct VisibilityCtx {
    /// `None` means no origin filter (private chats see everything about the
    /// person they are talking to). Groups pass `Origin::group_visible()`.
    pub origins: Option<Vec<Origin>>,
    /// Injection passes `true`; anything shown back to the subject passes
    /// `false`, so the operator's private notes never reach them.
    pub include_owner_only: bool,
}

impl VisibilityCtx {
    /// What the model may see about someone in a group.
    pub fn group_injection() -> Self {
        Self {
            origins: Some(Origin::group_visible().to_vec()),
            include_owner_only: true,
        }
    }

    /// What the model may see about the person it is privately talking to.
    pub fn private_injection() -> Self {
        Self {
            origins: None,
            include_owner_only: true,
        }
    }

    /// What a person may see about themselves, in whichever place they asked.
    pub fn self_view(is_group: bool) -> Self {
        Self {
            origins: is_group.then(|| Origin::group_visible().to_vec()),
            include_owner_only: false,
        }
    }
}

fn active() -> memories::BoxedQuery<'static, diesel::sqlite::Sqlite> {
    memories::table.filter(memories::deleted_at.is_null()).into_boxed()
}

/// Where an incremental read left off: the timestamp *and* id of the last row
/// that was actually sent to the model.
///
/// The id is not decoration. A batch write stamps every row it touches with the
/// same millisecond, and a cursor that cannot tell those rows apart re-reads the
/// same prefix every round — with a budget that only fits some of them, the tail
/// never gets its turn. Ordering by `(ts, id)` makes the sequence a total order,
/// which is what lets the cursor advance strictly and therefore terminate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub ts: i64,
    pub id: String,
}

/// The half-open window an incremental read covers: strictly after `after`,
/// strictly before `before_ts`.
///
/// The upper bound is the reader's own start time, and it is exclusive so that
/// the whole millisecond it names is left for the next read. Writes here are
/// concurrent — OneBot's extraction pass is detached — and without that bound a
/// row written during the read, carrying exactly that timestamp, could land on
/// either side of the cursor depending on the id it happened to be given.
#[derive(Debug, Clone)]
pub struct ReadWindow<'a> {
    pub after: Option<&'a Cursor>,
    pub before_ts: i64,
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

pub fn list_by_scope(conn: &mut SqliteConnection, scope: MemoryScope, scope_id: &str) -> QueryResult<Vec<MemoryRow>> {
    active()
        .filter(memories::scope_type.eq(scope.as_str()))
        .filter(memories::scope_id.eq(scope_id.to_string()))
        .order(memories::key.asc())
        .load::<MemoryRow>(conn)
}

/// One round trip for every participant in a turn rather than N.
///
/// `window` of `None` reads the whole layer, ordered so the block it renders is
/// stable between turns — scope_id then key, never anything that moves
/// (`last_seen_at` in particular). `Some` reads only what changed since a
/// cursor, ordered by the cursor's own key so that a budget which cannot fit
/// everything still makes progress; the caller re-groups for display.
pub fn list_by_scopes(
    conn: &mut SqliteConnection,
    scope: MemoryScope,
    scope_ids: &[String],
    ctx: &VisibilityCtx,
    window: Option<&ReadWindow<'_>>,
) -> QueryResult<Vec<MemoryRow>> {
    if scope_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut q = active()
        .filter(memories::scope_type.eq(scope.as_str()))
        .filter(memories::scope_id.eq_any(scope_ids.to_vec()));
    if let Some(ref origins) = ctx.origins {
        let allowed: Vec<&str> = origins.iter().map(|o| o.as_str()).collect();
        q = q.filter(memories::origin.eq_any(allowed));
    }
    if !ctx.include_owner_only {
        q = q.filter(memories::visibility.ne(Visibility::OwnerOnly.as_str()));
    }
    let Some(window) = window else {
        return q
            .order((memories::scope_id.asc(), memories::key.asc()))
            .load::<MemoryRow>(conn);
    };
    q = q.filter(memories::updated_at.lt(window.before_ts));
    if let Some(after) = window.after {
        q = q.filter(
            memories::updated_at
                .gt(after.ts)
                .or(memories::updated_at.eq(after.ts).and(memories::id.gt(after.id.clone()))),
        );
    }
    q.order((memories::updated_at.asc(), memories::id.asc()))
        .load::<MemoryRow>(conn)
}

/// What was soft-deleted inside `window`, so the model can be told to forget it.
///
/// A separate read with a separate cursor because a delete leaves a different
/// trace: `soft_delete_memories` sets `deleted_at` and does **not** touch
/// `updated_at`. A memory written months ago and deleted today therefore has an
/// `updated_at` older than any cursor — read the upsert side alone and its
/// removal is never reported at all.
pub fn list_deleted_by_scopes(
    conn: &mut SqliteConnection,
    scope: MemoryScope,
    scope_ids: &[String],
    ctx: &VisibilityCtx,
    window: &ReadWindow<'_>,
) -> QueryResult<Vec<MemoryRow>> {
    if scope_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut q = memories::table
        .into_boxed()
        .filter(memories::deleted_at.is_not_null())
        .filter(memories::scope_type.eq(scope.as_str()))
        .filter(memories::scope_id.eq_any(scope_ids.to_vec()))
        .filter(memories::deleted_at.lt(window.before_ts));
    if let Some(ref origins) = ctx.origins {
        let allowed: Vec<&str> = origins.iter().map(|o| o.as_str()).collect();
        q = q.filter(memories::origin.eq_any(allowed));
    }
    if !ctx.include_owner_only {
        q = q.filter(memories::visibility.ne(Visibility::OwnerOnly.as_str()));
    }
    if let Some(after) = window.after {
        q = q.filter(
            memories::deleted_at
                .gt(after.ts)
                .or(memories::deleted_at.eq(after.ts).and(memories::id.gt(after.id.clone()))),
        );
    }
    q.order((memories::deleted_at.asc(), memories::id.asc()))
        .load::<MemoryRow>(conn)
}

/// The single source of truth for "what may be shown about this person here".
pub fn visible_user_memories(
    conn: &mut SqliteConnection,
    subject_scope_id: &str,
    ctx: &VisibilityCtx,
) -> QueryResult<Vec<MemoryRow>> {
    list_by_scopes(
        conn,
        MemoryScope::OnebotUser,
        &[subject_scope_id.to_string()],
        ctx,
        None,
    )
}

pub fn get_memory(conn: &mut SqliteConnection, id: &str) -> QueryResult<MemoryRow> {
    memories::table.find(id).first::<MemoryRow>(conn)
}

pub fn get_memory_by_key(
    conn: &mut SqliteConnection,
    scope: MemoryScope,
    scope_id: &str,
    key: &str,
) -> QueryResult<Option<MemoryRow>> {
    active()
        .filter(memories::scope_type.eq(scope.as_str()))
        .filter(memories::scope_id.eq(scope_id.to_string()))
        .filter(memories::key.eq(key.to_string()))
        .first::<MemoryRow>(conn)
        .optional()
}

pub fn count_by_scope(conn: &mut SqliteConnection, scope: MemoryScope, scope_id: &str) -> QueryResult<i64> {
    active()
        .filter(memories::scope_type.eq(scope.as_str()))
        .filter(memories::scope_id.eq(scope_id.to_string()))
        .count()
        .get_result(conn)
}

/// Memories naming a person, wherever they live. Opt-out uses this to reach
/// group-scoped rows that talk about someone.
pub fn list_by_subject(conn: &mut SqliteConnection, subject_scope_id: &str) -> QueryResult<Vec<MemoryRow>> {
    active()
        .filter(memories::subject_scope_id.eq(subject_scope_id.to_string()))
        .order(memories::updated_at.desc())
        .load::<MemoryRow>(conn)
}

/// Every live memory, across all scopes. Backs the desktop browser, which needs
/// the bot-wide and per-person layers as well as project rows — fanning out one
/// query per project could only ever return the latter.
pub fn list_all(conn: &mut SqliteConnection) -> QueryResult<Vec<MemoryRow>> {
    active()
        .order((
            memories::scope_type.asc(),
            memories::scope_id.asc(),
            memories::key.asc(),
        ))
        .load::<MemoryRow>(conn)
}

pub fn list_trash(conn: &mut SqliteConnection, limit: i64) -> QueryResult<Vec<MemoryRow>> {
    memories::table
        .filter(memories::deleted_at.is_not_null())
        .order(memories::deleted_at.desc())
        .limit(limit)
        .load::<MemoryRow>(conn)
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

fn scope_quota(scope: MemoryScope) -> usize {
    match scope {
        MemoryScope::Project => MAX_MEMORIES_PER_PROJECT,
        MemoryScope::ClientGlobal => MAX_CLIENT_GLOBAL_MEMORIES,
        MemoryScope::OnebotGlobal => MAX_ONEBOT_GLOBAL_MEMORIES,
        MemoryScope::OnebotUser => MAX_MEMORIES_PER_SUBJECT,
    }
}

/// Content length and per-scope quota live here rather than in the tool layer so
/// the IPC path cannot bypass them.
pub fn validate_memory(
    conn: &mut SqliteConnection,
    scope: MemoryScope,
    scope_id: &str,
    key: &str,
    content: &str,
) -> Result<(), String> {
    if content.chars().count() > MAX_MEMORY_CONTENT_LEN {
        return Err(format!(
            "Memory content exceeds the {MAX_MEMORY_CONTENT_LEN} character limit"
        ));
    }
    if key.trim().is_empty() {
        return Err("Memory key must not be empty".to_string());
    }
    let existing = get_memory_by_key(conn, scope, scope_id, key).map_err(|e| e.to_string())?;
    if existing.is_none() {
        let count = count_by_scope(conn, scope, scope_id).map_err(|e| e.to_string())? as usize;
        if count >= scope_quota(scope) {
            return Err(format!(
                "This scope already holds its maximum of {} memories; delete some first",
                scope_quota(scope)
            ));
        }
    }
    Ok(())
}

/// Upsert against the live row only. A soft-deleted row with the same key stays
/// in the trash and a fresh row is created, so restoring never collides and the
/// delete history survives.
pub fn upsert_memory(conn: &mut SqliteConnection, new: &MemoryInsert) -> QueryResult<MemoryRow> {
    let scope = MemoryScope::parse(new.scope_type).map_err(contract_error)?;
    Visibility::parse(new.visibility).map_err(contract_error)?;
    let existing = get_memory_by_key(conn, scope, new.scope_id, new.key)?;

    if let Some(existing) = existing {
        diesel::update(memories::table.find(&existing.id))
            .set((
                memories::content.eq(new.content),
                memories::memory_type.eq(new.memory_type),
                memories::subject_scope_id.eq(new.subject_scope_id),
                memories::origin.eq(new.origin),
                memories::visibility.eq(new.visibility),
                memories::updated_at.eq(new.updated_at),
            ))
            .execute(conn)?;
        memories::table.find(&existing.id).first::<MemoryRow>(conn)
    } else {
        diesel::insert_into(memories::table).values(new).execute(conn)?;
        memories::table.find(new.id).first::<MemoryRow>(conn)
    }
}

pub fn update_memory(conn: &mut SqliteConnection, id: &str, changeset: &MemoryChangeset) -> QueryResult<MemoryRow> {
    diesel::update(memories::table.find(id)).set(changeset).execute(conn)?;
    memories::table.find(id).first::<MemoryRow>(conn)
}

pub fn soft_delete_memories(
    conn: &mut SqliteConnection,
    ids: &[String],
    by: DeletedBy,
    now: i64,
) -> QueryResult<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    diesel::update(
        memories::table
            .filter(memories::id.eq_any(ids.to_vec()))
            .filter(memories::deleted_at.is_null()),
    )
    .set((
        memories::deleted_at.eq(Some(now)),
        memories::deleted_by.eq(Some(by.as_str())),
    ))
    .execute(conn)
}

/// Bring rows back from the trash.
///
/// `updated_at` moves too, and it has to. Restoring is a change like any other,
/// but the only trace it used to leave was `deleted_at` going back to NULL —
/// which no reader can find. Anything reading the memory table incrementally
/// (`ReadWindow`) would see a row whose `updated_at` predates its cursor and
/// whose `deleted_at` is gone, and would therefore never mention it again: the
/// memory exists, the model has been told to forget it, and nothing will ever
/// tell it otherwise.
pub fn restore_memories(conn: &mut SqliteConnection, ids: &[String], now: i64) -> QueryResult<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    diesel::update(memories::table.filter(memories::id.eq_any(ids.to_vec())))
        .set((
            memories::deleted_at.eq(None::<i64>),
            memories::deleted_by.eq(None::<String>),
            memories::updated_at.eq(now),
        ))
        .execute(conn)
}

pub fn purge_memories(conn: &mut SqliteConnection, ids: &[String]) -> QueryResult<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    diesel::delete(memories::table.filter(memories::id.eq_any(ids.to_vec()))).execute(conn)
}

/// Drop trash past the retention window. Runs at startup and alongside LRU
/// enforcement rather than on a timer, whose failure mode is silent.
pub fn purge_expired_trash(conn: &mut SqliteConnection, now: i64) -> QueryResult<usize> {
    diesel::delete(
        memories::table
            .filter(memories::deleted_at.is_not_null())
            .filter(memories::deleted_at.lt(now - TRASH_RETENTION_MS)),
    )
    .execute(conn)
}

/// Hard delete, not soft: with the project gone a tombstone's scope_id points
/// nowhere, so it could be neither restored nor shown in the trash.
pub fn delete_project_memories(conn: &mut SqliteConnection, project_id: &str) -> QueryResult<usize> {
    diesel::delete(
        memories::table
            .filter(memories::scope_type.eq(MemoryScope::Project.as_str()))
            .filter(memories::scope_id.eq(project_id.to_string())),
    )
    .execute(conn)
}

/// Safety net for the case a future migration rebuilds `projects`: foreign keys
/// are off during migrations, so even a real FK would not have cascaded.
pub fn purge_orphan_project_memories(conn: &mut SqliteConnection) -> QueryResult<usize> {
    diesel::sql_query(
        "DELETE FROM memories WHERE scope_type = 'project' \
         AND scope_id NOT IN (SELECT id FROM projects)",
    )
    .execute(conn)
}

// ---------------------------------------------------------------------------
// Subjects (LRU clock)
// ---------------------------------------------------------------------------

/// Refresh someone's interaction clock, creating the row on first sight.
/// `is_protected` mirrors the admin list so eviction never has to read config;
/// it self-heals on the person's next message after the list changes.
pub fn touch_subject(
    conn: &mut SqliteConnection,
    scope_id: &str,
    display_name: Option<&str>,
    is_protected: bool,
    now: i64,
) -> QueryResult<()> {
    let existing = memory_subjects::table
        .find(scope_id)
        .first::<MemorySubjectRow>(conn)
        .optional()?;

    match existing {
        Some(_) => {
            diesel::update(memory_subjects::table.find(scope_id))
                .set((
                    memory_subjects::last_seen_at.eq(now),
                    memory_subjects::is_protected.eq(i32::from(is_protected)),
                ))
                .execute(conn)?;
            // Keep the last known nickname, but never overwrite a known one with
            // nothing: OneBot omits the card for members without one.
            if let Some(name) = display_name.filter(|n| !n.trim().is_empty()) {
                diesel::update(memory_subjects::table.find(scope_id))
                    .set(memory_subjects::display_name.eq(name))
                    .execute(conn)?;
            }
        }
        None => {
            diesel::insert_into(memory_subjects::table)
                .values(&MemorySubjectInsert {
                    scope_id,
                    display_name: display_name.filter(|n| !n.trim().is_empty()),
                    last_seen_at: now,
                    created_at: now,
                    is_protected: i32::from(is_protected),
                    is_pinned: 0,
                    opted_out: 0,
                })
                .execute(conn)?;
        }
    }
    Ok(())
}

pub fn get_subject(conn: &mut SqliteConnection, scope_id: &str) -> QueryResult<Option<MemorySubjectRow>> {
    memory_subjects::table
        .find(scope_id)
        .first::<MemorySubjectRow>(conn)
        .optional()
}

pub fn list_subjects(conn: &mut SqliteConnection) -> QueryResult<Vec<MemorySubjectRow>> {
    memory_subjects::table
        .order(memory_subjects::last_seen_at.desc())
        .load::<MemorySubjectRow>(conn)
}

pub fn set_subject_flags(
    conn: &mut SqliteConnection,
    scope_id: &str,
    is_pinned: Option<bool>,
    opted_out: Option<bool>,
) -> QueryResult<()> {
    if let Some(pinned) = is_pinned {
        if pinned {
            let count: i64 = memory_subjects::table
                .filter(memory_subjects::is_pinned.eq(1))
                .count()
                .get_result(conn)?;
            if count as usize >= MAX_PINNED_SUBJECTS {
                // Pinning is an eviction exemption, so an unbounded pin list
                // would be a way around the remembered-subject ceiling.
                return Err(diesel::result::Error::RollbackTransaction);
            }
        }
        diesel::update(memory_subjects::table.find(scope_id))
            .set(memory_subjects::is_pinned.eq(i32::from(pinned)))
            .execute(conn)?;
    }
    if let Some(out) = opted_out {
        diesel::update(memory_subjects::table.find(scope_id))
            .set(memory_subjects::opted_out.eq(i32::from(out)))
            .execute(conn)?;
    }
    Ok(())
}

/// Everything about one person goes away: their own memories plus any group
/// memory that names them. Shared by opt-out and the operator's "forget this
/// person" action. Owner-only rows survive unless `include_owner_only`.
pub fn forget_subject(
    conn: &mut SqliteConnection,
    subject_scope_id: &str,
    include_owner_only: bool,
    by: DeletedBy,
    now: i64,
) -> QueryResult<usize> {
    let mut q = memories::table
        .filter(memories::deleted_at.is_null())
        .filter(
            memories::scope_id
                .eq(subject_scope_id.to_string())
                .and(memories::scope_type.eq(MemoryScope::OnebotUser.as_str()))
                .or(memories::subject_scope_id.eq(subject_scope_id.to_string())),
        )
        .into_boxed();
    if !include_owner_only {
        q = q.filter(memories::visibility.ne(Visibility::OwnerOnly.as_str()));
    }
    let ids: Vec<String> = q.select(memories::id).load(conn)?;
    soft_delete_memories(conn, &ids, by, now)
}

/// Evict least-recently-seen people until the remembered-subject ceiling holds,
/// then trim the one subject `touched` by the write that triggered this.
/// Returns `(people forgotten, rows trimmed)`.
///
/// Owner-only rows are never touched, and correspondingly do not make someone
/// count as "remembered": otherwise a person left with nothing but the
/// operator's notes would hold a slot that evicting them could not free.
pub fn enforce_subject_lru(
    conn: &mut SqliteConnection,
    touched: Option<&str>,
    now: i64,
) -> QueryResult<(usize, usize)> {
    #[derive(QueryableByName)]
    struct ScopeIdRow {
        #[diesel(sql_type = Text)]
        scope_id: String,
    }

    // `ORDER BY last_seen_at DESC ... OFFSET n` selects exactly "everyone beyond
    // the most recent n". Exempt people are excluded from the candidate set, so
    // they neither get evicted nor consume a slot.
    let doomed: Vec<ScopeIdRow> = diesel::sql_query(
        "SELECT s.scope_id FROM memory_subjects s \
         WHERE s.is_protected = 0 AND s.is_pinned = 0 \
           AND EXISTS (SELECT 1 FROM memories m \
                       WHERE m.scope_type = 'onebot_user' AND m.scope_id = s.scope_id \
                         AND m.deleted_at IS NULL AND m.visibility = 'normal') \
         ORDER BY s.last_seen_at DESC LIMIT -1 OFFSET ?",
    )
    .bind::<BigInt, _>(MAX_REMEMBERED_SUBJECTS as i64)
    .load(conn)?;

    let mut forgotten = 0usize;
    for row in &doomed {
        let n = forget_subject(conn, &row.scope_id, false, DeletedBy::Lru, now)?;
        if n > 0 {
            forgotten += 1;
        }
    }

    // Per-person trim, oldest first, for the one person whose count just
    // changed. Sweeping every subject here issued one query per remembered
    // person on every single write — at the 200-person ceiling that is 199
    // queries that provably return nothing, since a write touches one subject.
    let mut trimmed = 0usize;
    if let Some(scope_id) = touched {
        let overflow: Vec<ScopeIdRow> = diesel::sql_query(
            "SELECT id AS scope_id FROM memories \
             WHERE scope_type = 'onebot_user' AND scope_id = ? AND deleted_at IS NULL \
               AND visibility = 'normal' \
             ORDER BY updated_at DESC LIMIT -1 OFFSET ?",
        )
        .bind::<Text, _>(scope_id)
        .bind::<BigInt, _>(MAX_MEMORIES_PER_SUBJECT as i64)
        .load(conn)?;
        let ids: Vec<String> = overflow.into_iter().map(|r| r.scope_id).collect();
        trimmed += soft_delete_memories(conn, &ids, DeletedBy::Lru, now)?;
    }

    Ok((forgotten, trimmed))
}

/// Housekeeping that does not belong on the write path: dropping tracked rows
/// for people with no memories, and emptying expired trash.
///
/// Neither depends on what was just written, and the trash sweep has no usable
/// index (both indexes are partial on `deleted_at IS NULL`), so running it per
/// write meant a full scan of `memories` every time. Startup plus an occasional
/// pass is enough — the ceilings are there to bound growth, not to be exact.
pub fn sweep_untracked_subjects(conn: &mut SqliteConnection, now: i64) -> QueryResult<usize> {
    // Opted-out rows are kept: dropping one would lose the opt-out itself and
    // the person would be remembered again the moment they spoke.
    let dropped = diesel::sql_query(
        "DELETE FROM memory_subjects WHERE scope_id IN ( \
           SELECT s.scope_id FROM memory_subjects s \
           WHERE s.is_protected = 0 AND s.is_pinned = 0 AND s.opted_out = 0 \
             AND NOT EXISTS (SELECT 1 FROM memories m \
                             WHERE m.deleted_at IS NULL \
                               AND (m.subject_scope_id = s.scope_id \
                                    OR (m.scope_type = 'onebot_user' AND m.scope_id = s.scope_id))) \
           ORDER BY s.last_seen_at DESC LIMIT -1 OFFSET ?)",
    )
    .bind::<BigInt, _>(MAX_TRACKED_SUBJECTS as i64)
    .execute(conn)?;

    purge_expired_trash(conn, now)?;
    Ok(dropped)
}

// ---------------------------------------------------------------------------
// Bot-wide proposals
// ---------------------------------------------------------------------------

pub fn create_proposal(conn: &mut SqliteConnection, new: &MemoryProposalInsert) -> QueryResult<MemoryProposalRow> {
    diesel::insert_into(memory_proposals::table).values(new).execute(conn)?;
    memory_proposals::table
        .order(memory_proposals::id.desc())
        .first::<MemoryProposalRow>(conn)
}

pub fn list_proposals(conn: &mut SqliteConnection, only_pending: bool) -> QueryResult<Vec<MemoryProposalRow>> {
    let mut q = memory_proposals::table.into_boxed();
    if only_pending {
        q = q.filter(memory_proposals::status.eq(ProposalStatus::Pending.as_str()));
    }
    q.order(memory_proposals::id.desc()).load::<MemoryProposalRow>(conn)
}

pub fn get_proposal(conn: &mut SqliteConnection, id: i32) -> QueryResult<Option<MemoryProposalRow>> {
    memory_proposals::table
        .find(id)
        .first::<MemoryProposalRow>(conn)
        .optional()
}

/// Resolve exactly once. A zero row count means it was already handled or has
/// expired — the caller must report that rather than acting twice.
pub fn resolve_proposal(
    conn: &mut SqliteConnection,
    id: i32,
    status: ProposalStatus,
    resolved_by: Option<i64>,
    now: i64,
) -> QueryResult<usize> {
    diesel::update(
        memory_proposals::table
            .find(id)
            .filter(memory_proposals::status.eq(ProposalStatus::Pending.as_str()))
            .filter(memory_proposals::expires_at.gt(now)),
    )
    .set((
        memory_proposals::status.eq(status.as_str()),
        memory_proposals::resolved_at.eq(Some(now)),
        memory_proposals::resolved_by.eq(resolved_by),
    ))
    .execute(conn)
}

pub fn expire_proposals(conn: &mut SqliteConnection, now: i64) -> QueryResult<usize> {
    diesel::update(
        memory_proposals::table
            .filter(memory_proposals::status.eq(ProposalStatus::Pending.as_str()))
            .filter(memory_proposals::expires_at.le(now)),
    )
    .set(memory_proposals::status.eq(ProposalStatus::Expired.as_str()))
    .execute(conn)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Escape a value bound for an XML attribute. QQ nicknames routinely contain
/// quotes and angle brackets, which would otherwise break the block structure.
pub fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Render one section. The leading blank line belongs to the block because every
/// call site concatenates bare strings.
pub fn format_memory_section(memories: &[MemoryRow], tag: &str, attrs: Option<&str>) -> Option<String> {
    if memories.is_empty() {
        return None;
    }
    let open = match attrs {
        Some(a) => format!("<{tag} {a}>"),
        None => format!("<{tag}>"),
    };
    let mut block = format!("\n\n{open}\n");
    for m in memories {
        block.push_str(&format!("- [{}] {}: {}\n", m.memory_type, m.key, m.content));
    }
    block.push_str(&format!("</{tag}>"));
    Some(block)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::memory::onebot_user_scope_id;
    use crate::db::test_db;

    fn mem(conn: &mut SqliteConnection, id: &str, subject: i64, origin: Origin, vis: Visibility) {
        let scope_id = onebot_user_scope_id(subject);
        upsert_memory(
            conn,
            &MemoryInsert {
                id,
                scope_type: MemoryScope::OnebotUser.as_str(),
                scope_id: &scope_id,
                key: id,
                content: "x",
                memory_type: "general",
                subject_scope_id: Some(&scope_id),
                origin: origin.as_str(),
                visibility: vis.as_str(),
                source_session_id: None,
                created_at: 1,
                updated_at: 1,
            },
        )
        .unwrap();
    }

    /// The privacy boundary: what someone told the bot in private must not
    /// resurface in a group.
    #[test]
    fn group_view_excludes_private_memories() {
        let pool = test_db();
        let conn = &mut pool.get().unwrap();
        mem(conn, "p", 1, Origin::Private, Visibility::Normal);
        mem(conn, "g", 1, Origin::Group, Visibility::Normal);

        let scope = onebot_user_scope_id(1);
        let in_group = visible_user_memories(conn, &scope, &VisibilityCtx::group_injection()).unwrap();
        assert_eq!(in_group.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["g"]);

        let in_private = visible_user_memories(conn, &scope, &VisibilityCtx::private_injection()).unwrap();
        assert_eq!(in_private.len(), 2);
    }

    /// A person may see what shapes the bot's behaviour toward them, but never
    /// the operator's private notes about them.
    #[test]
    fn self_view_hides_owner_only_rows() {
        let pool = test_db();
        let conn = &mut pool.get().unwrap();
        mem(conn, "note", 1, Origin::Admin, Visibility::OwnerOnly);
        mem(conn, "pref", 1, Origin::Group, Visibility::Normal);

        let scope = onebot_user_scope_id(1);
        let seen = visible_user_memories(conn, &scope, &VisibilityCtx::self_view(true)).unwrap();
        assert_eq!(seen.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["pref"]);

        // ...while the model still gets to act on it.
        let injected = visible_user_memories(conn, &scope, &VisibilityCtx::group_injection()).unwrap();
        assert_eq!(injected.len(), 2);
    }

    #[test]
    fn soft_deleted_rows_disappear_then_come_back() {
        let pool = test_db();
        let conn = &mut pool.get().unwrap();
        mem(conn, "a", 1, Origin::Group, Visibility::Normal);
        let scope = onebot_user_scope_id(1);

        soft_delete_memories(conn, &["a".into()], DeletedBy::SelfRemoved, 10).unwrap();
        assert!(list_by_scope(conn, MemoryScope::OnebotUser, &scope).unwrap().is_empty());
        assert_eq!(list_trash(conn, 10).unwrap().len(), 1);

        restore_memories(conn, &["a".into()], 9_000).unwrap();
        assert_eq!(list_by_scope(conn, MemoryScope::OnebotUser, &scope).unwrap().len(), 1);
    }

    /// Eviction forgets the person, but the operator's notes about them are not
    /// theirs to lose.
    #[test]
    fn eviction_spares_owner_only_rows() {
        let pool = test_db();
        let conn = &mut pool.get().unwrap();
        mem(conn, "normal", 7, Origin::Group, Visibility::Normal);
        mem(conn, "note", 7, Origin::Admin, Visibility::OwnerOnly);

        let scope = onebot_user_scope_id(7);
        forget_subject(conn, &scope, false, DeletedBy::Lru, 20).unwrap();

        let left = list_by_scope(conn, MemoryScope::OnebotUser, &scope).unwrap();
        assert_eq!(left.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["note"]);
    }

    /// Opt-out must survive the passer-by sweep, or the person would silently
    /// start being remembered again the next time they spoke.
    #[test]
    fn tracked_sweep_keeps_opted_out_rows() {
        let pool = test_db();
        let conn = &mut pool.get().unwrap();
        for i in 0..(MAX_TRACKED_SUBJECTS as i64 + 5) {
            touch_subject(conn, &onebot_user_scope_id(i), None, false, i).unwrap();
        }
        let quitter = onebot_user_scope_id(0); // oldest last_seen, first to go
        set_subject_flags(conn, &quitter, None, Some(true)).unwrap();

        sweep_untracked_subjects(conn, 999_999).unwrap();

        let row = get_subject(conn, &quitter).unwrap();
        assert!(
            row.is_some_and(|s| s.is_opted_out()),
            "opt-out row must survive the sweep"
        );
    }

    /// A write must only trim the person it wrote about. Sweeping every subject
    /// issued one query per remembered person on each write, all but one of
    /// which provably returned nothing.
    #[test]
    fn trimming_touches_only_the_written_subject() {
        let pool = test_db();
        let conn = &mut pool.get().unwrap();

        // Two people, each already at their cap.
        for uid in [1i64, 2] {
            for i in 0..MAX_MEMORIES_PER_SUBJECT {
                mem(conn, &format!("u{uid}m{i}"), uid, Origin::Group, Visibility::Normal);
            }
        }
        // One more for person 1 only, pushing them over.
        mem(conn, "u1extra", 1, Origin::Group, Visibility::Normal);

        let scope1 = onebot_user_scope_id(1);
        let (_, trimmed) = enforce_subject_lru(conn, Some(&scope1), 500).unwrap();
        assert_eq!(trimmed, 1);

        assert_eq!(
            list_by_scope(conn, MemoryScope::OnebotUser, &scope1).unwrap().len(),
            MAX_MEMORIES_PER_SUBJECT
        );
        // Person 2 was untouched by that write and must be left alone.
        assert_eq!(
            list_by_scope(conn, MemoryScope::OnebotUser, &onebot_user_scope_id(2))
                .unwrap()
                .len(),
            MAX_MEMORIES_PER_SUBJECT
        );
    }

    #[test]
    fn quota_and_length_are_enforced_for_every_writer() {
        let pool = test_db();
        let conn = &mut pool.get().unwrap();
        let scope = onebot_user_scope_id(3);

        let too_long = "x".repeat(MAX_MEMORY_CONTENT_LEN + 1);
        assert!(validate_memory(conn, MemoryScope::OnebotUser, &scope, "k", &too_long).is_err());

        for i in 0..MAX_MEMORIES_PER_SUBJECT {
            mem(conn, &format!("m{i}"), 3, Origin::Group, Visibility::Normal);
        }
        assert!(validate_memory(conn, MemoryScope::OnebotUser, &scope, "extra", "x").is_err());
        // Overwriting an existing key stays allowed at the cap.
        assert!(validate_memory(conn, MemoryScope::OnebotUser, &scope, "m0", "x").is_ok());
    }

    /// The block sits in a cached prompt prefix, so its order must not depend on
    /// anything that changes between turns.
    #[test]
    fn multi_subject_order_is_stable() {
        let pool = test_db();
        let conn = &mut pool.get().unwrap();
        mem(conn, "b", 2, Origin::Group, Visibility::Normal);
        mem(conn, "a", 1, Origin::Group, Visibility::Normal);

        let ids = vec![onebot_user_scope_id(2), onebot_user_scope_id(1)];
        let rows = list_by_scopes(
            conn,
            MemoryScope::OnebotUser,
            &ids,
            &VisibilityCtx::group_injection(),
            None,
        )
        .unwrap();
        // onebot:1 sorts before onebot:2 regardless of the order asked for.
        assert_eq!(rows.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    }

    /// A resolved proposal must never be actionable twice.
    #[test]
    fn proposal_resolves_exactly_once() {
        let pool = test_db();
        let conn = &mut pool.get().unwrap();
        let p = create_proposal(
            conn,
            &MemoryProposalInsert {
                key: "tone",
                content: "be brief",
                memory_type: "instruction",
                origin_session: None,
                proposer_id: Some(1),
                status: ProposalStatus::Pending.as_str(),
                created_at: 1,
                expires_at: 1_000,
            },
        )
        .unwrap();

        assert_eq!(
            resolve_proposal(conn, p.id, ProposalStatus::Approved, Some(1), 10).unwrap(),
            1
        );
        assert_eq!(
            resolve_proposal(conn, p.id, ProposalStatus::Approved, Some(1), 11).unwrap(),
            0
        );
    }

    #[test]
    fn expired_proposal_cannot_be_approved() {
        let pool = test_db();
        let conn = &mut pool.get().unwrap();
        let p = create_proposal(
            conn,
            &MemoryProposalInsert {
                key: "tone",
                content: "be brief",
                memory_type: "instruction",
                origin_session: None,
                proposer_id: None,
                status: ProposalStatus::Pending.as_str(),
                created_at: 1,
                expires_at: 100,
            },
        )
        .unwrap();

        assert_eq!(
            resolve_proposal(conn, p.id, ProposalStatus::Approved, None, 200).unwrap(),
            0
        );
    }

    #[test]
    fn attributes_are_escaped() {
        assert_eq!(escape_attr(r#"a"<b>&"#), "a&quot;&lt;b&gt;&amp;");
    }

    #[test]
    fn unknown_scope_and_visibility_are_rejected() {
        assert!(MemoryScope::parse("workspace").is_err());
        assert!(Visibility::parse("public").is_err());

        let pool = test_db();
        let conn = &mut pool.get().unwrap();
        let invalid_scope = MemoryInsert {
            id: "bad-scope",
            scope_type: "workspace",
            scope_id: crate::db::models::memory::GLOBAL_SCOPE_ID,
            key: "k",
            content: "v",
            memory_type: "general",
            subject_scope_id: None,
            origin: Origin::Desktop.as_str(),
            visibility: Visibility::Normal.as_str(),
            source_session_id: None,
            created_at: 1,
            updated_at: 1,
        };
        let error = upsert_memory(conn, &invalid_scope).unwrap_err();
        assert!(error.to_string().contains("unknown memory scope"));

        let invalid_visibility = MemoryInsert {
            id: "bad-visibility",
            scope_type: MemoryScope::ClientGlobal.as_str(),
            visibility: "public",
            ..invalid_scope
        };
        let error = upsert_memory(conn, &invalid_visibility).unwrap_err();
        assert!(error.to_string().contains("unknown memory visibility"));
    }
}

#[cfg(test)]
mod origin_visibility_tests {
    use super::*;
    use crate::db::models::memory::onebot_user_scope_id;
    use crate::db::test_db;

    /// Whatever origin the desktop writes for a per-person memory has to be one
    /// that group injection accepts. `Desktop` is not, so a row written with it
    /// was stored, shown as active in the UI, and silently never injected.
    #[test]
    fn operator_written_person_memories_reach_groups() {
        let pool = test_db();
        let conn = &mut pool.get().unwrap();
        let scope = onebot_user_scope_id(1);
        upsert_memory(
            conn,
            &MemoryInsert {
                id: "m1",
                scope_type: MemoryScope::OnebotUser.as_str(),
                scope_id: &scope,
                key: "note",
                content: "prefers short answers",
                memory_type: "preference",
                subject_scope_id: Some(&scope),
                origin: Origin::Admin.as_str(),
                visibility: Visibility::Normal.as_str(),
                source_session_id: None,
                created_at: 1,
                updated_at: 1,
            },
        )
        .unwrap();

        let seen = visible_user_memories(conn, &scope, &VisibilityCtx::group_injection()).unwrap();
        assert_eq!(seen.len(), 1, "operator-written memory must survive group filtering");
    }

    /// Guards the invariant the fix relies on: every origin the desktop can
    /// assign to a OneBot-scoped row must be group-visible.
    #[test]
    fn admin_origin_is_group_visible() {
        assert!(Origin::group_visible().contains(&Origin::Admin));
        assert!(!Origin::group_visible().contains(&Origin::Desktop));
        assert!(!Origin::group_visible().contains(&Origin::Private));
    }
}

#[cfg(test)]
mod legacy_length_tests {
    use super::*;
    use crate::db::test_db;

    /// Rows written under the old 10k ceiling stay readable and stay editable,
    /// as long as the edit brings them within the current limit. Only saving a
    /// still-oversized version is refused — which is the limit doing its job,
    /// not a row becoming stuck.
    #[test]
    fn oversized_legacy_rows_can_be_shortened() {
        let pool = test_db();
        let conn = &mut pool.get().unwrap();
        crate::db::ops::project::create_project(
            conn,
            &crate::db::models::project::ProjectInsert {
                id: "p1",
                name: "P",
                path: None,
                source_type: "local",
                source_id: None,
                assistant_id: None,
                description: None,
                created_at: 1,
                updated_at: 1,
            },
        )
        .unwrap();

        // Written directly, as migration 18 carries such rows through verbatim.
        let long = "x".repeat(3_000);
        diesel::insert_into(memories::table)
            .values(&MemoryInsert {
                id: "old",
                scope_type: "project",
                scope_id: "p1",
                key: "k",
                content: &long,
                memory_type: "general",
                subject_scope_id: None,
                origin: "desktop",
                visibility: "normal",
                source_session_id: None,
                created_at: 1,
                updated_at: 1,
            })
            .execute(conn)
            .unwrap();

        // Still readable and still injected.
        assert_eq!(list_by_scope(conn, MemoryScope::Project, "p1").unwrap().len(), 1);

        // An edit that is still too long is refused...
        assert!(validate_memory(conn, MemoryScope::Project, "p1", "k", &"y".repeat(600)).is_err());
        // ...but shortening to within the limit works.
        assert!(validate_memory(conn, MemoryScope::Project, "p1", "k", "short now").is_ok());
    }
}

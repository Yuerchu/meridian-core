//! Reading and writing `memories`, `memory_subjects` and `memory_proposals`.
//!
//! No function here opens a transaction of its own: a write takes the caller's
//! `WriteTx`, and the caller's `Db::write` is the `BEGIN IMMEDIATE`. Every check
//! that guards a write — length, quota, the pin ceiling, a proposal still being
//! pending — takes that same `WriteTx`, so its answer still holds when the row
//! goes in. [`remember`] is the check and the write as one call, and is what
//! every writer of a memory goes through.

use sea_orm::ActiveValue::{NotSet, Set, Unchanged};
use sea_orm::sea_query::{Expr, ExprTrait, Query};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, DbErr, EntityTrait, IntoActiveModel, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect,
};

use crate::db::entity::memory::{
    DeletedBy, MAX_CLIENT_GLOBAL_MEMORIES, MAX_MEMORIES_PER_PROJECT, MAX_MEMORIES_PER_SUBJECT, MAX_MEMORY_CONTENT_LEN,
    MAX_ONEBOT_GLOBAL_MEMORIES, MAX_PINNED_SUBJECTS, MAX_REMEMBERED_SUBJECTS, MAX_TRACKED_SUBJECTS, MemoryChangeset,
    MemoryScope, Origin, Visibility,
};
use crate::db::entity::memory_proposal::ProposalStatus;
use crate::db::entity::{memory, memory_proposal, memory_subject, project};
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::types::{EpochMs, SqlBool};

/// Trash retention. Soft-deleted rows outlive the delete so `/memory undo` and
/// the desktop trash have something to restore.
pub const TRASH_RETENTION_MS: i64 = 30 * 24 * 3600 * 1000;

/// How many ids one `IN (…)` carries. SQLite's bound-parameter ceiling is far
/// above this; the chunk keeps a sweep of a large backlog from reaching it.
const ID_CHUNK: usize = 500;

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

    fn condition(&self) -> Condition {
        let mut condition = Condition::all();
        if let Some(origins) = &self.origins {
            condition = condition.add(memory::Column::Origin.is_in(origins.iter().copied()));
        }
        if !self.include_owner_only {
            condition = condition.add(memory::Column::Visibility.ne(Visibility::OwnerOnly));
        }
        condition
    }
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

impl ReadWindow<'_> {
    /// The window on one timestamp column, with the id as the tie-break.
    fn condition(&self, ts: memory::Column) -> Condition {
        let mut condition = Condition::all().add(ts.lt(self.before_ts));
        if let Some(after) = self.after {
            condition = condition.add(
                Condition::any().add(ts.gt(after.ts)).add(
                    Condition::all()
                        .add(ts.eq(after.ts))
                        .add(memory::Column::Id.gt(after.id.as_str())),
                ),
            );
        }
        condition
    }
}

fn active() -> sea_orm::Select<memory::Entity> {
    memory::Entity::find().filter(memory::Column::DeletedAt.is_null())
}

fn in_scope(scope: MemoryScope, scope_id: &str) -> Condition {
    Condition::all()
        .add(memory::Column::ScopeType.eq(scope))
        .add(memory::Column::ScopeId.eq(scope_id))
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

pub async fn list_by_scope(db: &impl Read, scope: MemoryScope, scope_id: &str) -> Result<Vec<memory::Model>, DbErr> {
    active()
        .filter(in_scope(scope, scope_id))
        .order_by_asc(memory::Column::Key)
        .all(db.conn()?)
        .await
}

/// One round trip for every participant in a turn rather than N.
///
/// `window` of `None` reads the whole layer, ordered so the block it renders is
/// stable between turns — scope_id then key, never anything that moves
/// (`last_seen_at` in particular). `Some` reads only what changed since a
/// cursor, ordered by the cursor's own key so that a budget which cannot fit
/// everything still makes progress; the caller re-groups for display.
pub async fn list_by_scopes(
    db: &impl Read,
    scope: MemoryScope,
    scope_ids: &[String],
    ctx: &VisibilityCtx,
    window: Option<&ReadWindow<'_>>,
) -> Result<Vec<memory::Model>, DbErr> {
    if scope_ids.is_empty() {
        return Ok(Vec::new());
    }
    let query = active()
        .filter(memory::Column::ScopeType.eq(scope))
        .filter(memory::Column::ScopeId.is_in(scope_ids.iter().map(String::as_str)))
        .filter(ctx.condition());
    let query = match window {
        None => query
            .order_by_asc(memory::Column::ScopeId)
            .order_by_asc(memory::Column::Key),
        Some(window) => query
            .filter(window.condition(memory::Column::UpdatedAt))
            .order_by_asc(memory::Column::UpdatedAt)
            .order_by_asc(memory::Column::Id),
    };
    query.all(db.conn()?).await
}

/// What was soft-deleted inside `window`, so the model can be told to forget it.
///
/// A separate read with a separate cursor because a delete leaves a different
/// trace: `soft_delete_memories` sets `deleted_at` and does **not** touch
/// `updated_at`. A memory written months ago and deleted today therefore has an
/// `updated_at` older than any cursor — read the upsert side alone and its
/// removal is never reported at all.
pub async fn list_deleted_by_scopes(
    db: &impl Read,
    scope: MemoryScope,
    scope_ids: &[String],
    ctx: &VisibilityCtx,
    window: &ReadWindow<'_>,
) -> Result<Vec<memory::Model>, DbErr> {
    if scope_ids.is_empty() {
        return Ok(Vec::new());
    }
    memory::Entity::find()
        .filter(memory::Column::DeletedAt.is_not_null())
        .filter(memory::Column::ScopeType.eq(scope))
        .filter(memory::Column::ScopeId.is_in(scope_ids.iter().map(String::as_str)))
        .filter(ctx.condition())
        .filter(window.condition(memory::Column::DeletedAt))
        .order_by_asc(memory::Column::DeletedAt)
        .order_by_asc(memory::Column::Id)
        .all(db.conn()?)
        .await
}

/// The single source of truth for "what may be shown about this person here".
pub async fn visible_user_memories(
    db: &impl Read,
    subject_scope_id: &str,
    ctx: &VisibilityCtx,
) -> Result<Vec<memory::Model>, DbErr> {
    list_by_scopes(db, MemoryScope::OnebotUser, &[subject_scope_id.to_string()], ctx, None).await
}

/// Live or in the trash; `None` once purged.
pub async fn get_memory(db: &impl Read, id: &str) -> Result<Option<memory::Model>, DbErr> {
    memory::Entity::find_by_id(id).one(db.conn()?).await
}

pub async fn get_memory_by_key(
    db: &impl Read,
    scope: MemoryScope,
    scope_id: &str,
    key: &str,
) -> Result<Option<memory::Model>, DbErr> {
    active()
        .filter(in_scope(scope, scope_id))
        .filter(memory::Column::Key.eq(key))
        .one(db.conn()?)
        .await
}

pub async fn count_by_scope(db: &impl Read, scope: MemoryScope, scope_id: &str) -> Result<u64, DbErr> {
    active().filter(in_scope(scope, scope_id)).count(db.conn()?).await
}

/// Memories naming a person, wherever they live. Opt-out uses this to reach
/// group-scoped rows that talk about someone.
pub async fn list_by_subject(db: &impl Read, subject_scope_id: &str) -> Result<Vec<memory::Model>, DbErr> {
    active()
        .filter(memory::Column::SubjectScopeId.eq(subject_scope_id))
        .order_by_desc(memory::Column::UpdatedAt)
        .all(db.conn()?)
        .await
}

/// Every live memory, across all scopes. Backs the desktop browser, which needs
/// the bot-wide and per-person layers as well as project rows — fanning out one
/// query per project could only ever return the latter.
pub async fn list_all(db: &impl Read) -> Result<Vec<memory::Model>, DbErr> {
    active()
        .order_by_asc(memory::Column::ScopeType)
        .order_by_asc(memory::Column::ScopeId)
        .order_by_asc(memory::Column::Key)
        .all(db.conn()?)
        .await
}

pub async fn list_trash(db: &impl Read, limit: u64) -> Result<Vec<memory::Model>, DbErr> {
    memory::Entity::find()
        .filter(memory::Column::DeletedAt.is_not_null())
        .order_by_desc(memory::Column::DeletedAt)
        .limit(limit)
        .all(db.conn()?)
        .await
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
/// the IPC path cannot bypass them. The inner `Err` is the refusal, worded for
/// whoever tried to write.
pub async fn validate_memory(
    tx: &WriteTx,
    scope: MemoryScope,
    scope_id: &str,
    key: &str,
    content: &str,
) -> Result<Result<(), String>, DbErr> {
    if content.chars().count() > MAX_MEMORY_CONTENT_LEN {
        return Ok(Err(format!(
            "Memory content exceeds the {MAX_MEMORY_CONTENT_LEN} character limit"
        )));
    }
    if key.trim().is_empty() {
        return Ok(Err("Memory key must not be empty".to_string()));
    }
    if get_memory_by_key(tx, scope, scope_id, key).await?.is_none()
        && count_by_scope(tx, scope, scope_id).await? as usize >= scope_quota(scope)
    {
        return Ok(Err(format!(
            "This scope already holds its maximum of {} memories; delete some first",
            scope_quota(scope)
        )));
    }
    Ok(Ok(()))
}

/// Upsert against the live row only. A soft-deleted row with the same key stays
/// in the trash and a fresh row is created, so restoring never collides and the
/// delete history survives. An overwrite keeps the live row's id, `created_at`
/// and `source_session_id`, and takes everything else from `new`.
pub async fn upsert_memory(tx: &WriteTx, new: memory::Model) -> Result<memory::Model, DbErr> {
    match get_memory_by_key(tx, new.scope_type, &new.scope_id, &new.key).await? {
        Some(existing) => {
            memory::ActiveModel {
                id: Unchanged(existing.id),
                content: Set(new.content),
                memory_type: Set(new.memory_type),
                subject_scope_id: Set(new.subject_scope_id),
                origin: Set(new.origin),
                visibility: Set(new.visibility),
                updated_at: Set(new.updated_at),
                ..Default::default()
            }
            .update(tx.conn()?)
            .await
        }
        None => {
            let id = new.id.clone();
            memory::Entity::insert(new.into_active_model())
                .exec_without_returning(tx.conn()?)
                .await?;
            get_memory(tx, &id)
                .await?
                .ok_or_else(|| DbErr::RecordNotFound(format!("memory `{id}`")))
        }
    }
}

/// [`validate_memory`] and [`upsert_memory`] in the one transaction: what every
/// writer of a memory calls. Two writers racing for the last slot of a scope
/// cannot both pass the quota, because the second one's count runs after the
/// first one's insert.
pub async fn remember(tx: &WriteTx, new: memory::Model) -> Result<Result<memory::Model, String>, DbErr> {
    if let Err(refused) = validate_memory(tx, new.scope_type, &new.scope_id, &new.key, &new.content).await? {
        return Ok(Err(refused));
    }
    upsert_memory(tx, new).await.map(Ok)
}

/// `RecordNotUpdated` for an id with no row.
pub async fn update_memory(tx: &WriteTx, id: &str, changeset: MemoryChangeset) -> Result<memory::Model, DbErr> {
    let mut row = changeset.into_active_model();
    row.id = Unchanged(id.to_owned());
    row.update(tx.conn()?).await
}

pub async fn soft_delete_memories(tx: &WriteTx, ids: &[String], by: DeletedBy, now: EpochMs) -> Result<u64, DbErr> {
    let mut changed = 0;
    for chunk in ids.chunks(ID_CHUNK) {
        changed += memory::Entity::update_many()
            .col_expr(memory::Column::DeletedAt, Expr::value(now))
            .col_expr(memory::Column::DeletedBy, Expr::value(by.as_str()))
            .filter(memory::Column::Id.is_in(chunk.iter().map(String::as_str)))
            .filter(memory::Column::DeletedAt.is_null())
            .exec(tx.conn()?)
            .await?
            .rows_affected;
    }
    Ok(changed)
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
pub async fn restore_memories(tx: &WriteTx, ids: &[String], now: EpochMs) -> Result<u64, DbErr> {
    let mut changed = 0;
    for chunk in ids.chunks(ID_CHUNK) {
        changed += memory::Entity::update_many()
            .col_expr(memory::Column::DeletedAt, Expr::value(Option::<i64>::None))
            .col_expr(memory::Column::DeletedBy, Expr::value(Option::<String>::None))
            .col_expr(memory::Column::UpdatedAt, Expr::value(now))
            .filter(memory::Column::Id.is_in(chunk.iter().map(String::as_str)))
            .exec(tx.conn()?)
            .await?
            .rows_affected;
    }
    Ok(changed)
}

pub async fn purge_memories(tx: &WriteTx, ids: &[String]) -> Result<u64, DbErr> {
    let mut removed = 0;
    for chunk in ids.chunks(ID_CHUNK) {
        removed += memory::Entity::delete_many()
            .filter(memory::Column::Id.is_in(chunk.iter().map(String::as_str)))
            .exec(tx.conn()?)
            .await?
            .rows_affected;
    }
    Ok(removed)
}

/// Drop trash past the retention window. Runs at startup and alongside LRU
/// enforcement rather than on a timer, whose failure mode is silent.
pub async fn purge_expired_trash(tx: &WriteTx, now: EpochMs) -> Result<u64, DbErr> {
    Ok(memory::Entity::delete_many()
        .filter(memory::Column::DeletedAt.is_not_null())
        .filter(memory::Column::DeletedAt.lt(now - TRASH_RETENTION_MS))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

/// Every memory filed under a deleted project. Hard delete, not soft: with the
/// project gone a tombstone's scope_id points nowhere, so it could be neither
/// restored nor shown in the trash. Memories carry no foreign key to the
/// project (scope_id is polymorphic), so `project::delete_project` calls this.
pub async fn delete_project_memories(tx: &WriteTx, project_id: &str) -> Result<u64, DbErr> {
    Ok(memory::Entity::delete_many()
        .filter(memory::Column::ScopeType.eq(MemoryScope::Project))
        .filter(memory::Column::ScopeId.eq(project_id))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

/// Safety net for the case a future migration rebuilds `projects`: foreign keys
/// are off during migrations, so even a real FK would not have cascaded.
pub async fn purge_orphan_project_memories(tx: &WriteTx) -> Result<u64, DbErr> {
    Ok(memory::Entity::delete_many()
        .filter(memory::Column::ScopeType.eq(MemoryScope::Project))
        .filter(
            memory::Column::ScopeId.not_in_subquery(
                Query::select()
                    .column(project::Column::Id)
                    .from(project::Entity)
                    .to_owned(),
            ),
        )
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

// ---------------------------------------------------------------------------
// Subjects (LRU clock)
// ---------------------------------------------------------------------------

/// Refresh someone's interaction clock, creating the row on first sight.
/// `is_protected` mirrors the admin list so eviction never has to read config;
/// it self-heals on the person's next message after the list changes.
///
/// A known nickname is never overwritten with nothing: OneBot omits the card
/// for members without one.
pub async fn touch_subject(
    tx: &WriteTx,
    scope_id: &str,
    display_name: Option<&str>,
    is_protected: bool,
    now: EpochMs,
) -> Result<(), DbErr> {
    let display_name = display_name.filter(|n| !n.trim().is_empty()).map(str::to_owned);
    match get_subject(tx, scope_id).await? {
        Some(existing) => {
            memory_subject::ActiveModel {
                scope_id: Unchanged(existing.scope_id),
                last_seen_at: Set(now),
                is_protected: Set(SqlBool::from(is_protected)),
                display_name: match display_name {
                    Some(name) => Set(Some(name)),
                    None => NotSet,
                },
                ..Default::default()
            }
            .update(tx.conn()?)
            .await?;
        }
        None => {
            memory_subject::Entity::insert(memory_subject::ActiveModel {
                scope_id: Set(scope_id.to_owned()),
                display_name: Set(display_name),
                last_seen_at: Set(now),
                created_at: Set(now),
                is_protected: Set(SqlBool::from(is_protected)),
                is_pinned: Set(SqlBool::FALSE),
                opted_out: Set(SqlBool::FALSE),
            })
            .exec_without_returning(tx.conn()?)
            .await?;
        }
    }
    Ok(())
}

pub async fn get_subject(db: &impl Read, scope_id: &str) -> Result<Option<memory_subject::Model>, DbErr> {
    memory_subject::Entity::find_by_id(scope_id).one(db.conn()?).await
}

pub async fn list_subjects(db: &impl Read) -> Result<Vec<memory_subject::Model>, DbErr> {
    memory_subject::Entity::find()
        .order_by_desc(memory_subject::Column::LastSeenAt)
        .all(db.conn()?)
        .await
}

/// Whether a person has opted out of being remembered. Takes the caller's
/// write when the answer guards a write, so an opt-out cannot land between
/// the check and the memory it was meant to stop.
pub async fn is_opted_out(db: &impl Read, scope_id: &str) -> Result<bool, DbErr> {
    Ok(get_subject(db, scope_id).await?.is_some_and(|s| s.opted_out.get()))
}

/// Set either flag, leaving the other as it is. A subject with no row is left
/// without one. The inner `Err` is the pin ceiling: pinning is an eviction
/// exemption, so an unbounded pin list would be a way around the
/// remembered-subject ceiling.
pub async fn set_subject_flags(
    tx: &WriteTx,
    scope_id: &str,
    is_pinned: Option<bool>,
    opted_out: Option<bool>,
) -> Result<Result<(), String>, DbErr> {
    if is_pinned == Some(true) {
        let pinned = memory_subject::Entity::find()
            .filter(memory_subject::Column::IsPinned.eq(SqlBool::TRUE))
            .count(tx.conn()?)
            .await?;
        if pinned as usize >= MAX_PINNED_SUBJECTS {
            return Ok(Err(format!("at most {MAX_PINNED_SUBJECTS} people can be pinned")));
        }
    }
    if is_pinned.is_none() && opted_out.is_none() {
        return Ok(Ok(()));
    }
    let mut update = memory_subject::Entity::update_many().filter(memory_subject::Column::ScopeId.eq(scope_id));
    if let Some(pinned) = is_pinned {
        update = update.col_expr(memory_subject::Column::IsPinned, Expr::value(SqlBool::from(pinned)));
    }
    if let Some(out) = opted_out {
        update = update.col_expr(memory_subject::Column::OptedOut, Expr::value(SqlBool::from(out)));
    }
    update.exec(tx.conn()?).await?;
    Ok(Ok(()))
}

/// Everything about one person goes away: their own memories plus any group
/// memory that names them. Shared by opt-out and the operator's "forget this
/// person" action. Owner-only rows survive unless `include_owner_only`.
pub async fn forget_subject(
    tx: &WriteTx,
    subject_scope_id: &str,
    include_owner_only: bool,
    by: DeletedBy,
    now: EpochMs,
) -> Result<u64, DbErr> {
    let mut query = active().filter(
        Condition::any()
            .add(in_scope(MemoryScope::OnebotUser, subject_scope_id))
            .add(memory::Column::SubjectScopeId.eq(subject_scope_id)),
    );
    if !include_owner_only {
        query = query.filter(memory::Column::Visibility.ne(Visibility::OwnerOnly));
    }
    let ids: Vec<String> = query
        .select_only()
        .column(memory::Column::Id)
        .into_tuple()
        .all(tx.conn()?)
        .await?;
    soft_delete_memories(tx, &ids, by, now).await
}

/// A live `normal` memory filed under the subject in the outer query.
fn holds_a_normal_memory() -> Condition {
    Condition::all().add(Expr::exists(
        Query::select()
            .expr(Expr::val(1))
            .from(memory::Entity)
            .and_where(memory::Column::ScopeType.eq(MemoryScope::OnebotUser))
            .and_where(
                Expr::col((memory::Entity, memory::Column::ScopeId))
                    .equals((memory_subject::Entity, memory_subject::Column::ScopeId)),
            )
            .and_where(memory::Column::DeletedAt.is_null())
            .and_where(memory::Column::Visibility.eq(Visibility::Normal))
            .to_owned(),
    ))
}

/// Evict least-recently-seen people until the remembered-subject ceiling holds,
/// then trim the one subject `touched` by the write that triggered this.
/// Returns `(people forgotten, rows trimmed)`.
///
/// Owner-only rows are never touched, and correspondingly do not make someone
/// count as "remembered": otherwise a person left with nothing but the
/// operator's notes would hold a slot that evicting them could not free.
///
/// "Everyone beyond the most recent n" is taken in Rust from the ordered
/// candidates rather than with `LIMIT -1 OFFSET n`, which is SQLite's spelling
/// alone. The candidates are the remembered people, a set the ceiling bounds.
pub async fn enforce_subject_lru(tx: &WriteTx, touched: Option<&str>, now: EpochMs) -> Result<(u64, u64), DbErr> {
    // Exempt people are excluded from the candidate set, so they neither get
    // evicted nor consume a slot.
    let doomed: Vec<String> = memory_subject::Entity::find()
        .filter(memory_subject::Column::IsProtected.eq(SqlBool::FALSE))
        .filter(memory_subject::Column::IsPinned.eq(SqlBool::FALSE))
        .filter(holds_a_normal_memory())
        .order_by_desc(memory_subject::Column::LastSeenAt)
        .order_by_asc(memory_subject::Column::ScopeId)
        .select_only()
        .column(memory_subject::Column::ScopeId)
        .into_tuple()
        .all(tx.conn()?)
        .await?;

    let mut forgotten = 0;
    for scope_id in doomed.iter().skip(MAX_REMEMBERED_SUBJECTS) {
        if forget_subject(tx, scope_id, false, DeletedBy::Lru, now).await? > 0 {
            forgotten += 1;
        }
    }

    // Per-person trim, oldest first, for the one person whose count just
    // changed. Sweeping every subject here issued one query per remembered
    // person on every single write — at the 200-person ceiling that is 199
    // queries that provably return nothing, since a write touches one subject.
    let mut trimmed = 0;
    if let Some(scope_id) = touched {
        let kept: Vec<String> = active()
            .filter(in_scope(MemoryScope::OnebotUser, scope_id))
            .filter(memory::Column::Visibility.eq(Visibility::Normal))
            .order_by_desc(memory::Column::UpdatedAt)
            .order_by_asc(memory::Column::Id)
            .select_only()
            .column(memory::Column::Id)
            .into_tuple()
            .all(tx.conn()?)
            .await?;
        let overflow: Vec<String> = kept.into_iter().skip(MAX_MEMORIES_PER_SUBJECT).collect();
        trimmed = soft_delete_memories(tx, &overflow, DeletedBy::Lru, now).await?;
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
pub async fn sweep_untracked_subjects(tx: &WriteTx, now: EpochMs) -> Result<u64, DbErr> {
    let subject = (memory_subject::Entity, memory_subject::Column::ScopeId);
    let mentioned = Expr::exists(
        Query::select()
            .expr(Expr::val(1))
            .from(memory::Entity)
            .and_where(memory::Column::DeletedAt.is_null())
            .cond_where(
                Condition::any()
                    .add(Expr::col((memory::Entity, memory::Column::SubjectScopeId)).equals(subject))
                    .add(
                        Condition::all()
                            .add(memory::Column::ScopeType.eq(MemoryScope::OnebotUser))
                            .add(Expr::col((memory::Entity, memory::Column::ScopeId)).equals(subject)),
                    ),
            )
            .to_owned(),
    );
    // Opted-out rows are kept: dropping one would lose the opt-out itself and
    // the person would be remembered again the moment they spoke.
    let untracked: Vec<String> = memory_subject::Entity::find()
        .filter(memory_subject::Column::IsProtected.eq(SqlBool::FALSE))
        .filter(memory_subject::Column::IsPinned.eq(SqlBool::FALSE))
        .filter(memory_subject::Column::OptedOut.eq(SqlBool::FALSE))
        .filter(Condition::all().not().add(mentioned))
        .order_by_desc(memory_subject::Column::LastSeenAt)
        .order_by_asc(memory_subject::Column::ScopeId)
        .select_only()
        .column(memory_subject::Column::ScopeId)
        .into_tuple()
        .all(tx.conn()?)
        .await?;

    let mut dropped = 0;
    let beyond: Vec<String> = untracked.into_iter().skip(MAX_TRACKED_SUBJECTS).collect();
    for chunk in beyond.chunks(ID_CHUNK) {
        dropped += memory_subject::Entity::delete_many()
            .filter(memory_subject::Column::ScopeId.is_in(chunk.iter().map(String::as_str)))
            .exec(tx.conn()?)
            .await?
            .rows_affected;
    }

    purge_expired_trash(tx, now).await?;
    Ok(dropped)
}

// ---------------------------------------------------------------------------
// Bot-wide proposals
// ---------------------------------------------------------------------------

/// Inserts `new` with a fresh id (the one on `new` is ignored) and reads the
/// row back by the id the insert produced, not by "the newest row", which a
/// concurrent insert could be.
pub async fn create_proposal(tx: &WriteTx, new: memory_proposal::Model) -> Result<memory_proposal::Model, DbErr> {
    let mut row = new.into_active_model();
    row.id = NotSet;
    let id = memory_proposal::Entity::insert(row)
        .exec(tx.conn()?)
        .await?
        .last_insert_id;
    get_proposal(tx, id)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound(format!("memory proposal {id}")))
}

pub async fn list_proposals(db: &impl Read, only_pending: bool) -> Result<Vec<memory_proposal::Model>, DbErr> {
    let mut query = memory_proposal::Entity::find();
    if only_pending {
        query = query.filter(memory_proposal::Column::Status.eq(ProposalStatus::Pending));
    }
    query.order_by_desc(memory_proposal::Column::Id).all(db.conn()?).await
}

pub async fn get_proposal(db: &impl Read, id: i32) -> Result<Option<memory_proposal::Model>, DbErr> {
    memory_proposal::Entity::find_by_id(id).one(db.conn()?).await
}

/// Resolve exactly once. A zero row count means it was already handled or has
/// expired — the caller must report that rather than acting twice.
pub async fn resolve_proposal(
    tx: &WriteTx,
    id: i32,
    status: ProposalStatus,
    resolved_by: Option<i64>,
    now: EpochMs,
) -> Result<u64, DbErr> {
    Ok(memory_proposal::Entity::update_many()
        .col_expr(memory_proposal::Column::Status, Expr::value(status.as_str()))
        .col_expr(memory_proposal::Column::ResolvedAt, Expr::value(now))
        .col_expr(memory_proposal::Column::ResolvedBy, Expr::value(resolved_by))
        .filter(memory_proposal::Column::Id.eq(id))
        .filter(memory_proposal::Column::Status.eq(ProposalStatus::Pending))
        .filter(memory_proposal::Column::ExpiresAt.gt(now))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

pub async fn expire_proposals(tx: &WriteTx, now: EpochMs) -> Result<u64, DbErr> {
    Ok(memory_proposal::Entity::update_many()
        .col_expr(
            memory_proposal::Column::Status,
            Expr::value(ProposalStatus::Expired.as_str()),
        )
        .filter(memory_proposal::Column::Status.eq(ProposalStatus::Pending))
        .filter(memory_proposal::Column::ExpiresAt.lte(now))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
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
pub fn format_memory_section(memories: &[memory::Model], tag: &str, attrs: Option<&str>) -> Option<String> {
    if memories.is_empty() {
        return None;
    }
    let open = match attrs {
        Some(a) => format!("<{tag} {a}>"),
        None => format!("<{tag}>"),
    };
    let mut block = format!("\n\n{open}\n");
    for m in memories {
        block.push_str(&format!("- [{}] {}: {}\n", m.memory_type.as_str(), m.key, m.content));
    }
    block.push_str(&format!("</{tag}>"));
    Some(block)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::entity::memory::{GLOBAL_SCOPE_ID, MemoryType, onebot_user_scope_id};
    use crate::db::sea::cap::Db;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    fn row(id: &str, scope: MemoryScope, scope_id: &str, key: &str, origin: Origin, vis: Visibility) -> memory::Model {
        memory::Model {
            id: id.into(),
            scope_type: scope,
            scope_id: scope_id.into(),
            key: key.into(),
            content: "x".into(),
            memory_type: MemoryType::General,
            subject_scope_id: (scope == MemoryScope::OnebotUser).then(|| scope_id.to_owned()),
            origin,
            visibility: vis,
            source_session_id: None,
            deleted_at: None,
            deleted_by: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    async fn write(db: &Db, new: memory::Model) -> Result<memory::Model, String> {
        db.write(async |tx| remember(tx, new).await).await.unwrap()
    }

    async fn mem(db: &Db, id: &str, subject: i64, origin: Origin, vis: Visibility) {
        let scope_id = onebot_user_scope_id(subject);
        write(db, row(id, MemoryScope::OnebotUser, &scope_id, id, origin, vis))
            .await
            .unwrap();
    }

    fn ids(rows: &[memory::Model]) -> Vec<&str> {
        rows.iter().map(|m| m.id.as_str()).collect()
    }

    /// The privacy boundary: what someone told the bot in private must not
    /// resurface in a group.
    #[tokio::test]
    async fn group_view_excludes_private_memories() {
        let db = sea_test_db().await;
        mem(&db, "p", 1, Origin::Private, Visibility::Normal).await;
        mem(&db, "g", 1, Origin::Group, Visibility::Normal).await;

        let scope = onebot_user_scope_id(1);
        let in_group = visible_user_memories(&db, &scope, &VisibilityCtx::group_injection())
            .await
            .unwrap();
        assert_eq!(ids(&in_group), ["g"]);
        let in_private = visible_user_memories(&db, &scope, &VisibilityCtx::private_injection())
            .await
            .unwrap();
        assert_eq!(in_private.len(), 2);
    }

    /// A person may see what shapes the bot's behaviour toward them, but never
    /// the operator's private notes about them.
    #[tokio::test]
    async fn self_view_hides_owner_only_rows() {
        let db = sea_test_db().await;
        mem(&db, "note", 1, Origin::Admin, Visibility::OwnerOnly).await;
        mem(&db, "pref", 1, Origin::Group, Visibility::Normal).await;

        let scope = onebot_user_scope_id(1);
        let seen = visible_user_memories(&db, &scope, &VisibilityCtx::self_view(true))
            .await
            .unwrap();
        assert_eq!(ids(&seen), ["pref"]);
        // ...while the model still gets to act on it.
        let injected = visible_user_memories(&db, &scope, &VisibilityCtx::group_injection())
            .await
            .unwrap();
        assert_eq!(injected.len(), 2);
    }

    /// Whatever origin the desktop writes for a per-person memory has to be one
    /// that group injection accepts. `Desktop` is not, so a row written with it
    /// was stored, shown as active in the UI, and silently never injected.
    #[tokio::test]
    async fn operator_written_person_memories_reach_groups() {
        let db = sea_test_db().await;
        mem(&db, "m1", 1, Origin::Admin, Visibility::Normal).await;
        let seen = visible_user_memories(&db, &onebot_user_scope_id(1), &VisibilityCtx::group_injection())
            .await
            .unwrap();
        assert_eq!(seen.len(), 1, "operator-written memory must survive group filtering");
    }

    #[tokio::test]
    async fn soft_deleted_rows_disappear_then_come_back() {
        let db = sea_test_db().await;
        mem(&db, "a", 1, Origin::Group, Visibility::Normal).await;
        let scope = onebot_user_scope_id(1);

        db.write(async |tx| soft_delete_memories(tx, &["a".into()], DeletedBy::SelfRemoved, 10).await)
            .await
            .unwrap();
        assert!(
            list_by_scope(&db, MemoryScope::OnebotUser, &scope)
                .await
                .unwrap()
                .is_empty()
        );
        let trash = list_trash(&db, 10).await.unwrap();
        assert_eq!(
            (trash.len(), trash[0].deleted_by, trash[0].deleted_at),
            (1, Some(DeletedBy::SelfRemoved), Some(10))
        );

        db.write(async |tx| restore_memories(tx, &["a".into()], 9_000).await)
            .await
            .unwrap();
        let back = list_by_scope(&db, MemoryScope::OnebotUser, &scope).await.unwrap();
        assert_eq!((back.len(), back[0].updated_at, back[0].deleted_by), (1, 9_000, None));
    }

    /// An overwrite of a live key keeps the row and its first-written facts;
    /// a key whose row is in the trash gets a fresh row beside it.
    #[tokio::test]
    async fn an_overwrite_keeps_the_live_row_and_the_trash_keeps_its_own() {
        let db = sea_test_db().await;
        let scope = onebot_user_scope_id(1);
        let mut first = row(
            "m1",
            MemoryScope::OnebotUser,
            &scope,
            "k",
            Origin::Group,
            Visibility::Normal,
        );
        first.source_session_id = Some("s1".into());
        write(&db, first).await.unwrap();

        let mut second = row(
            "m2",
            MemoryScope::OnebotUser,
            &scope,
            "k",
            Origin::Admin,
            Visibility::Normal,
        );
        second.content = "y".into();
        second.updated_at = 5;
        let stored = write(&db, second).await.unwrap();
        assert_eq!(
            (
                stored.id.as_str(),
                stored.content.as_str(),
                stored.origin,
                stored.created_at
            ),
            ("m1", "y", Origin::Admin, 1)
        );
        assert_eq!(
            (stored.updated_at, stored.source_session_id.as_deref()),
            (5, Some("s1"))
        );

        db.write(async |tx| soft_delete_memories(tx, &["m1".into()], DeletedBy::Admin, 6).await)
            .await
            .unwrap();
        let fresh = write(
            &db,
            row(
                "m3",
                MemoryScope::OnebotUser,
                &scope,
                "k",
                Origin::Group,
                Visibility::Normal,
            ),
        )
        .await
        .unwrap();
        assert_eq!(fresh.id, "m3");
        assert_eq!(list_trash(&db, 10).await.unwrap().len(), 1);
    }

    /// Eviction forgets the person, but the operator's notes about them are not
    /// theirs to lose.
    #[tokio::test]
    async fn eviction_spares_owner_only_rows() {
        let db = sea_test_db().await;
        mem(&db, "normal", 7, Origin::Group, Visibility::Normal).await;
        mem(&db, "note", 7, Origin::Admin, Visibility::OwnerOnly).await;

        let scope = onebot_user_scope_id(7);
        db.write(async |tx| forget_subject(tx, &scope, false, DeletedBy::Lru, 20).await)
            .await
            .unwrap();
        let left = list_by_scope(&db, MemoryScope::OnebotUser, &scope).await.unwrap();
        assert_eq!(ids(&left), ["note"]);
    }

    /// Past the remembered-subject ceiling, the least recently seen person is
    /// the one forgotten; protected and pinned people take no slot.
    #[tokio::test]
    async fn eviction_forgets_the_least_recently_seen_beyond_the_ceiling() {
        let db = sea_test_db().await;
        let people = MAX_REMEMBERED_SUBJECTS as i64 + 1;
        db.write(async |tx| {
            for uid in 0..people + 2 {
                touch_subject(tx, &onebot_user_scope_id(uid), None, uid == people, 100 + uid).await?;
            }
            set_subject_flags(tx, &onebot_user_scope_id(people + 1), Some(true), None)
                .await?
                .unwrap();
            Ok::<_, DbErr>(())
        })
        .await
        .unwrap();
        for uid in 0..people + 2 {
            mem(&db, &format!("m{uid}"), uid, Origin::Group, Visibility::Normal).await;
        }

        let (forgotten, _) = db
            .write(async |tx| enforce_subject_lru(tx, None, 9_000).await)
            .await
            .unwrap();
        assert_eq!(forgotten, 1);
        let gone = list_trash(&db, 10).await.unwrap();
        assert_eq!(ids(&gone), ["m0"], "the oldest unexempt person");
    }

    /// Opt-out must survive the passer-by sweep, or the person would silently
    /// start being remembered again the next time they spoke.
    #[tokio::test]
    async fn tracked_sweep_drops_the_oldest_and_keeps_opted_out_rows() {
        let db = sea_test_db().await;
        let quitter = onebot_user_scope_id(0); // oldest last_seen, first to go
        let dropped = db
            .write(async |tx| {
                for i in 0..(MAX_TRACKED_SUBJECTS as i64 + 5) {
                    touch_subject(tx, &onebot_user_scope_id(i), None, false, i).await?;
                }
                set_subject_flags(tx, &quitter, None, Some(true)).await?.unwrap();
                sweep_untracked_subjects(tx, 999_999).await
            })
            .await
            .unwrap();

        assert_eq!(dropped, 4, "2004 eligible, 2000 kept");
        assert!(
            is_opted_out(&db, &quitter).await.unwrap(),
            "opt-out row must survive the sweep"
        );
        assert!(get_subject(&db, &onebot_user_scope_id(1)).await.unwrap().is_none());
        assert!(get_subject(&db, &onebot_user_scope_id(5)).await.unwrap().is_some());
    }

    /// A write must only trim the person it wrote about. Sweeping every subject
    /// issued one query per remembered person on each write, all but one of
    /// which provably returned nothing.
    #[tokio::test]
    async fn trimming_touches_only_the_written_subject() {
        let db = sea_test_db().await;
        // Two people, each at their cap, written straight to the table so the
        // quota does not stop the extra one.
        db.write(async |tx| {
            for uid in [1i64, 2] {
                for i in 0..MAX_MEMORIES_PER_SUBJECT {
                    let scope = onebot_user_scope_id(uid);
                    let id = format!("u{uid}m{i}");
                    let mut m = row(
                        &id,
                        MemoryScope::OnebotUser,
                        &scope,
                        &id,
                        Origin::Group,
                        Visibility::Normal,
                    );
                    m.updated_at = 10 + i as i64;
                    upsert_memory(tx, m).await?;
                }
            }
            let scope = onebot_user_scope_id(1);
            let mut extra = row(
                "u1extra",
                MemoryScope::OnebotUser,
                &scope,
                "extra",
                Origin::Group,
                Visibility::Normal,
            );
            extra.updated_at = 100;
            upsert_memory(tx, extra).await?;
            Ok::<_, DbErr>(())
        })
        .await
        .unwrap();

        let scope1 = onebot_user_scope_id(1);
        let (_, trimmed) = db
            .write(async |tx| enforce_subject_lru(tx, Some(&scope1), 500).await)
            .await
            .unwrap();
        assert_eq!(trimmed, 1);
        assert_eq!(ids(&list_trash(&db, 10).await.unwrap()), ["u1m0"], "the oldest one");
        for uid in [1, 2] {
            let left = list_by_scope(&db, MemoryScope::OnebotUser, &onebot_user_scope_id(uid))
                .await
                .unwrap();
            assert_eq!(left.len(), MAX_MEMORIES_PER_SUBJECT, "person {uid}");
        }
    }

    #[tokio::test]
    async fn quota_and_length_are_enforced_for_every_writer() {
        let db = sea_test_db().await;
        let scope = onebot_user_scope_id(3);
        let attempt = |key: &str, content: String| {
            let mut m = row(
                key,
                MemoryScope::OnebotUser,
                &scope,
                key,
                Origin::Group,
                Visibility::Normal,
            );
            m.content = content;
            m
        };

        let too_long = "x".repeat(MAX_MEMORY_CONTENT_LEN + 1);
        assert!(write(&db, attempt("k", too_long)).await.is_err());
        assert!(write(&db, attempt(" ", "x".into())).await.is_err(), "an empty key");

        for i in 0..MAX_MEMORIES_PER_SUBJECT {
            write(&db, attempt(&format!("m{i}"), "x".into())).await.unwrap();
        }
        assert!(write(&db, attempt("extra", "x".into())).await.is_err());
        // Overwriting an existing key stays allowed at the cap.
        assert!(write(&db, attempt("m0", "y".into())).await.is_ok());
    }

    /// The quota's count and the insert are one transaction: writers racing for
    /// the last slot of a scope admit exactly one.
    #[tokio::test]
    async fn racing_writers_cannot_overfill_a_scope() {
        let dir = tempfile::tempdir().unwrap();
        let (_diesel, db) = crate::db::sea::shared_test_db(dir.path()).await;
        for i in 0..MAX_ONEBOT_GLOBAL_MEMORIES - 1 {
            let key = format!("k{i}");
            write(
                &db,
                row(
                    &key,
                    MemoryScope::OnebotGlobal,
                    GLOBAL_SCOPE_ID,
                    &key,
                    Origin::Admin,
                    Visibility::Normal,
                ),
            )
            .await
            .unwrap();
        }
        let racers = (0..8).map(|i| {
            let db = db.clone();
            tokio::spawn(async move {
                let key = format!("racer{i}");
                write(
                    &db,
                    row(
                        &key,
                        MemoryScope::OnebotGlobal,
                        GLOBAL_SCOPE_ID,
                        &key,
                        Origin::Admin,
                        Visibility::Normal,
                    ),
                )
                .await
                .is_ok()
            })
        });
        let admitted = futures::future::join_all(racers)
            .await
            .into_iter()
            .filter(|joined| *joined.as_ref().unwrap())
            .count();
        assert_eq!(admitted, 1);
        assert_eq!(
            count_by_scope(&db, MemoryScope::OnebotGlobal, GLOBAL_SCOPE_ID)
                .await
                .unwrap() as usize,
            MAX_ONEBOT_GLOBAL_MEMORIES
        );
    }

    /// Rows written under the old 10k ceiling stay readable and stay editable,
    /// as long as the edit brings them within the current limit.
    #[tokio::test]
    async fn oversized_legacy_rows_can_be_shortened() {
        let db = sea_test_db().await;
        let long = "x".repeat(3_000);
        db.write(async |tx| {
            let mut old = row(
                "old",
                MemoryScope::Project,
                "p1",
                "k",
                Origin::Desktop,
                Visibility::Normal,
            );
            old.content = long;
            upsert_memory(tx, old).await
        })
        .await
        .unwrap();
        assert_eq!(list_by_scope(&db, MemoryScope::Project, "p1").await.unwrap().len(), 1);

        let check = |content: String| {
            let db = db.clone();
            async move {
                db.write(async |tx| validate_memory(tx, MemoryScope::Project, "p1", "k", &content).await)
                    .await
                    .unwrap()
            }
        };
        assert!(check("y".repeat(600)).await.is_err());
        assert!(check("short now".into()).await.is_ok());
    }

    /// The block sits in a cached prompt prefix, so its order must not depend on
    /// anything that changes between turns.
    #[tokio::test]
    async fn multi_subject_order_is_stable() {
        let db = sea_test_db().await;
        mem(&db, "b", 2, Origin::Group, Visibility::Normal).await;
        mem(&db, "a", 1, Origin::Group, Visibility::Normal).await;

        let scope_ids = vec![onebot_user_scope_id(2), onebot_user_scope_id(1)];
        let rows = list_by_scopes(
            &db,
            MemoryScope::OnebotUser,
            &scope_ids,
            &VisibilityCtx::group_injection(),
            None,
        )
        .await
        .unwrap();
        // onebot:1 sorts before onebot:2 regardless of the order asked for.
        assert_eq!(ids(&rows), ["a", "b"]);
    }

    /// The window is half-open on both cursors, with the id breaking a tie in
    /// one millisecond.
    #[tokio::test]
    async fn a_window_reads_strictly_after_its_cursor_and_before_its_bound() {
        let db = sea_test_db().await;
        db.write(async |tx| {
            for (id, at) in [("a", 10), ("b", 10), ("c", 20), ("d", 30)] {
                let mut m = row(id, MemoryScope::Project, "p1", id, Origin::Desktop, Visibility::Normal);
                m.updated_at = at;
                upsert_memory(tx, m).await?;
            }
            Ok::<_, DbErr>(())
        })
        .await
        .unwrap();
        let cursor = Cursor { ts: 10, id: "a".into() };
        let window = ReadWindow {
            after: Some(&cursor),
            before_ts: 30,
        };
        let rows = list_by_scopes(
            &db,
            MemoryScope::Project,
            &["p1".into()],
            &VisibilityCtx::default(),
            Some(&window),
        )
        .await
        .unwrap();
        assert_eq!(ids(&rows), ["b", "c"]);

        db.write(async |tx| soft_delete_memories(tx, &["a".into(), "d".into()], DeletedBy::Admin, 25).await)
            .await
            .unwrap();
        let deleted = list_deleted_by_scopes(
            &db,
            MemoryScope::Project,
            &["p1".into()],
            &VisibilityCtx::default(),
            &ReadWindow {
                after: None,
                before_ts: 26,
            },
        )
        .await
        .unwrap();
        assert_eq!(ids(&deleted), ["a", "d"]);
    }

    /// A known nickname survives a touch that carries none; a first touch makes
    /// the row.
    #[tokio::test]
    async fn a_touch_keeps_a_known_name() {
        let db = sea_test_db().await;
        let scope = onebot_user_scope_id(9);
        db.write(async |tx| {
            touch_subject(tx, &scope, Some("Nova"), false, 1).await?;
            touch_subject(tx, &scope, Some("  "), true, 2).await
        })
        .await
        .unwrap();
        let subject = get_subject(&db, &scope).await.unwrap().unwrap();
        assert_eq!(subject.display_name.as_deref(), Some("Nova"));
        assert_eq!(
            (subject.last_seen_at, subject.created_at, subject.is_protected.get()),
            (2, 1, true)
        );
        assert_eq!(subject.user_id(), Some(9));
    }

    #[tokio::test]
    async fn pinning_stops_at_its_ceiling() {
        let db = sea_test_db().await;
        let refused = db
            .write(async |tx| {
                for uid in 0..=MAX_PINNED_SUBJECTS as i64 {
                    touch_subject(tx, &onebot_user_scope_id(uid), None, false, uid).await?;
                }
                for uid in 0..MAX_PINNED_SUBJECTS as i64 {
                    set_subject_flags(tx, &onebot_user_scope_id(uid), Some(true), None)
                        .await?
                        .unwrap();
                }
                set_subject_flags(tx, &onebot_user_scope_id(MAX_PINNED_SUBJECTS as i64), Some(true), None).await
            })
            .await
            .unwrap();
        assert!(refused.is_err());
    }

    #[tokio::test]
    async fn orphaned_project_memories_are_purged() {
        let db = sea_test_db().await;
        execute_for_tests(
            &db,
            "INSERT INTO projects (id, name, source_type, created_at, updated_at) VALUES ('p1', 'P', 'local', 1, 1)",
        )
        .await
        .unwrap();
        db.write(async |tx| {
            upsert_memory(
                tx,
                row(
                    "kept",
                    MemoryScope::Project,
                    "p1",
                    "k",
                    Origin::Desktop,
                    Visibility::Normal,
                ),
            )
            .await?;
            upsert_memory(
                tx,
                row(
                    "gone",
                    MemoryScope::Project,
                    "p2",
                    "k",
                    Origin::Desktop,
                    Visibility::Normal,
                ),
            )
            .await?;
            upsert_memory(
                tx,
                row(
                    "global",
                    MemoryScope::ClientGlobal,
                    GLOBAL_SCOPE_ID,
                    "k",
                    Origin::Desktop,
                    Visibility::Normal,
                ),
            )
            .await
        })
        .await
        .unwrap();
        let purged = db
            .write(async |tx| purge_orphan_project_memories(tx).await)
            .await
            .unwrap();
        assert_eq!(purged, 1);
        let mut left: Vec<String> = list_all(&db).await.unwrap().into_iter().map(|m| m.id).collect();
        left.sort();
        assert_eq!(left, ["global", "kept"]);
    }

    fn proposal(expires_at: EpochMs) -> memory_proposal::Model {
        memory_proposal::Model {
            id: 0,
            key: "tone".into(),
            content: "be brief".into(),
            memory_type: MemoryType::Instruction,
            origin_session: None,
            proposer_id: Some(1),
            status: ProposalStatus::Pending,
            created_at: 1,
            expires_at,
            resolved_at: None,
            resolved_by: None,
        }
    }

    /// A resolved proposal must never be actionable twice, and each create
    /// answers with its own row.
    #[tokio::test]
    async fn proposal_resolves_exactly_once() {
        let db = sea_test_db().await;
        let (p, q) = db
            .write(async |tx| {
                Ok::<_, DbErr>((
                    create_proposal(tx, proposal(1_000)).await?,
                    create_proposal(tx, proposal(1_000)).await?,
                ))
            })
            .await
            .unwrap();
        assert_ne!(p.id, q.id);
        assert_eq!(p.status, ProposalStatus::Pending);

        let resolve = |at| {
            let db = db.clone();
            async move {
                db.write(async |tx| resolve_proposal(tx, p.id, ProposalStatus::Approved, Some(1), at).await)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(resolve(10).await, 1);
        assert_eq!(resolve(11).await, 0);
        let resolved = get_proposal(&db, p.id).await.unwrap().unwrap();
        assert_eq!(
            (resolved.status, resolved.resolved_at),
            (ProposalStatus::Approved, Some(10))
        );
        assert_eq!(ids_of(list_proposals(&db, true).await.unwrap()), [q.id]);
    }

    fn ids_of(rows: Vec<memory_proposal::Model>) -> Vec<i32> {
        rows.into_iter().map(|p| p.id).collect()
    }

    #[tokio::test]
    async fn expired_proposal_cannot_be_approved() {
        let db = sea_test_db().await;
        let p = db
            .write(async |tx| create_proposal(tx, proposal(100)).await)
            .await
            .unwrap();
        let approved = db
            .write(async |tx| resolve_proposal(tx, p.id, ProposalStatus::Approved, None, 200).await)
            .await
            .unwrap();
        assert_eq!(approved, 0);
        let expired = db.write(async |tx| expire_proposals(tx, 200).await).await.unwrap();
        assert_eq!(expired, 1);
        assert!(list_proposals(&db, true).await.unwrap().is_empty());
    }

    /// No `CHECK` holds these columns; the type does, at the read.
    #[tokio::test]
    async fn an_unknown_stored_value_fails_the_read() {
        let db = sea_test_db().await;
        execute_for_tests(
            &db,
            "INSERT INTO memories (id, scope_type, scope_id, key, content, created_at, updated_at) \
             VALUES ('m', 'workspace', '_', 'k', 'v', 1, 1)",
        )
        .await
        .unwrap();
        assert!(list_all(&db).await.is_err());
    }

    #[test]
    fn attributes_are_escaped() {
        assert_eq!(escape_attr(r#"a"<b>&"#), "a&quot;&lt;b&gt;&amp;");
    }
}

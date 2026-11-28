//! Persistence operations for Auto-Dream watermarks, run records, daily spend
//! checks, and 1-click prune restoration.

use sea_orm::ActiveValue::{Set, Unchanged};
use sea_orm::{ActiveModelTrait, ColumnTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

use crate::db::entity::dream_run::{DreamRunStatus, DreamTrigger};
use crate::db::entity::memory::{MemoryScope, parse_external_subject_numeric_id};
use crate::db::entity::{audit_message, conversation, dream_run, dream_watermark, message, project};
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::sea::ops::{audit, memory};
use crate::db::types::{EpochMs, Json};
use crate::decimal::Decimal;

pub async fn get_watermark(
    db: &impl Read,
    scope_type: MemoryScope,
    scope_id: &str,
) -> Result<Option<dream_watermark::Model>, DbErr> {
    dream_watermark::Entity::find_by_id((scope_type, scope_id.to_owned()))
        .one(db.conn()?)
        .await
}

pub async fn increment_turn_count(
    tx: &WriteTx,
    scope_type: MemoryScope,
    scope_id: &str,
    now: EpochMs,
) -> Result<dream_watermark::Model, DbErr> {
    match get_watermark(tx, scope_type, scope_id).await? {
        Some(existing) => {
            dream_watermark::ActiveModel {
                scope_type: Unchanged(existing.scope_type),
                scope_id: Unchanged(existing.scope_id),
                last_dream_at: Unchanged(existing.last_dream_at),
                last_message_id: Unchanged(existing.last_message_id),
                turns_since_dream: Set(existing.turns_since_dream.saturating_add(1)),
                updated_at: Set(now),
            }
            .update(tx.conn()?)
            .await
        }
        None => {
            let row = dream_watermark::Model {
                scope_type,
                scope_id: scope_id.to_owned(),
                last_dream_at: None,
                last_message_id: None,
                turns_since_dream: 1,
                updated_at: now,
            };
            dream_watermark::Entity::insert(dream_watermark::ActiveModel {
                scope_type: Set(row.scope_type),
                scope_id: Set(row.scope_id.clone()),
                last_dream_at: Set(None),
                last_message_id: Set(None),
                turns_since_dream: Set(1),
                updated_at: Set(now),
            })
            .exec_without_returning(tx.conn()?)
            .await?;
            Ok(row)
        }
    }
}

pub async fn advance_watermark(
    tx: &WriteTx,
    scope_type: MemoryScope,
    scope_id: &str,
    last_dream_at: EpochMs,
    last_message_id: Option<String>,
    now: EpochMs,
) -> Result<dream_watermark::Model, DbErr> {
    match get_watermark(tx, scope_type, scope_id).await? {
        Some(existing) => {
            let next_msg_id = last_message_id.or(existing.last_message_id);
            dream_watermark::ActiveModel {
                scope_type: Unchanged(existing.scope_type),
                scope_id: Unchanged(existing.scope_id),
                last_dream_at: Set(Some(last_dream_at)),
                last_message_id: Set(next_msg_id),
                turns_since_dream: Set(0),
                updated_at: Set(now),
            }
            .update(tx.conn()?)
            .await
        }
        None => {
            let row = dream_watermark::Model {
                scope_type,
                scope_id: scope_id.to_owned(),
                last_dream_at: Some(last_dream_at),
                last_message_id: last_message_id.clone(),
                turns_since_dream: 0,
                updated_at: now,
            };
            dream_watermark::Entity::insert(dream_watermark::ActiveModel {
                scope_type: Set(row.scope_type),
                scope_id: Set(row.scope_id.clone()),
                last_dream_at: Set(Some(last_dream_at)),
                last_message_id: Set(last_message_id),
                turns_since_dream: Set(0),
                updated_at: Set(now),
            })
            .exec_without_returning(tx.conn()?)
            .await?;
            Ok(row)
        }
    }
}

pub async fn begin_run(
    tx: &WriteTx,
    id: &str,
    scope_type: MemoryScope,
    scope_id: &str,
    trigger: DreamTrigger,
    model_id: Option<String>,
    now: EpochMs,
) -> Result<dream_run::Model, DbErr> {
    let row = dream_run::Model {
        id: id.to_owned(),
        scope_type,
        scope_id: scope_id.to_owned(),
        status: DreamRunStatus::Running,
        trigger,
        model_id: model_id.clone(),
        started_at: now,
        finished_at: None,
        turns_reviewed: 0,
        memories_created: 0,
        memories_updated: 0,
        memories_pruned: 0,
        proposals_created: 0,
        pruned_memory_ids_json: None,
        summary: None,
        error: None,
    };
    dream_run::Entity::insert(dream_run::ActiveModel {
        id: Set(row.id.clone()),
        scope_type: Set(scope_type),
        scope_id: Set(scope_id.to_owned()),
        status: Set(DreamRunStatus::Running),
        trigger: Set(trigger),
        model_id: Set(model_id),
        started_at: Set(now),
        finished_at: Set(None),
        turns_reviewed: Set(0),
        memories_created: Set(0),
        memories_updated: Set(0),
        memories_pruned: Set(0),
        proposals_created: Set(0),
        pruned_memory_ids_json: Set(None),
        summary: Set(None),
        error: Set(None),
    })
    .exec_without_returning(tx.conn()?)
    .await?;
    Ok(row)
}

pub struct DreamRunFinish {
    pub status: DreamRunStatus,
    pub model_id: Option<String>,
    pub finished_at: EpochMs,
    pub turns_reviewed: i32,
    pub memories_created: i32,
    pub memories_updated: i32,
    pub memories_pruned: i32,
    pub proposals_created: i32,
    pub pruned_memory_ids: Vec<String>,
    pub summary: Option<String>,
    pub error: Option<String>,
}

pub async fn finish_run(tx: &WriteTx, id: &str, finish: DreamRunFinish) -> Result<dream_run::Model, DbErr> {
    let existing = get_run(tx, id)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound(format!("dream run `{id}`")))?;
    let pruned_json = if finish.pruned_memory_ids.is_empty() {
        None
    } else {
        Some(Json(finish.pruned_memory_ids))
    };
    dream_run::ActiveModel {
        id: Unchanged(existing.id),
        scope_type: Unchanged(existing.scope_type),
        scope_id: Unchanged(existing.scope_id),
        status: Set(finish.status),
        trigger: Unchanged(existing.trigger),
        model_id: Set(finish.model_id.or(existing.model_id)),
        started_at: Unchanged(existing.started_at),
        finished_at: Set(Some(finish.finished_at)),
        turns_reviewed: Set(finish.turns_reviewed),
        memories_created: Set(finish.memories_created),
        memories_updated: Set(finish.memories_updated),
        memories_pruned: Set(finish.memories_pruned),
        proposals_created: Set(finish.proposals_created),
        pruned_memory_ids_json: Set(pruned_json),
        summary: Set(finish.summary),
        error: Set(finish.error),
    }
    .update(tx.conn()?)
    .await
}

pub async fn get_run(db: &impl Read, id: &str) -> Result<Option<dream_run::Model>, DbErr> {
    dream_run::Entity::find_by_id(id).one(db.conn()?).await
}

pub async fn list_runs(
    db: &impl Read,
    scope: Option<(MemoryScope, &str)>,
    limit: u64,
) -> Result<Vec<dream_run::Model>, DbErr> {
    let mut query = dream_run::Entity::find();
    if let Some((scope_type, scope_id)) = scope {
        query = query
            .filter(dream_run::Column::ScopeType.eq(scope_type))
            .filter(dream_run::Column::ScopeId.eq(scope_id));
    }
    query
        .order_by_desc(dream_run::Column::StartedAt)
        .order_by_desc(dream_run::Column::Id)
        .limit(limit)
        .all(db.conn()?)
        .await
}

/// Restore every memory pruned by `run_id` using [`memory::restore`], advancing
/// their `updated_at` so live conversations pick them up incrementally without
/// invalidating the cached prompt prefix.
pub async fn undo_pruned_memories(tx: &WriteTx, run_id: &str, now: EpochMs) -> Result<Result<u64, String>, DbErr> {
    let Some(run) = get_run(tx, run_id).await? else {
        return Ok(Err(format!("dream run `{run_id}` was not found")));
    };
    let ids = run.pruned_memory_ids_json.map(|j| j.0).unwrap_or_default();
    if ids.is_empty() {
        return Ok(Ok(0));
    }
    let restored = memory::restore(tx, &ids, now).await?;
    Ok(Ok(restored))
}

/// Exact USD spend recorded under [`audit::DREAM_ROLE`] since `since_ms`.
pub async fn daily_dream_spend(db: &impl Read, since_ms: EpochMs) -> Result<Decimal, DbErr> {
    let rows = audit_message::Entity::find()
        .filter(audit_message::Column::Role.eq(audit::DREAM_ROLE))
        .filter(audit_message::Column::CreatedAt.gte(since_ms))
        .all(db.conn()?)
        .await?;
    let per_million: Decimal = "0.000001".parse().expect("valid decimal constant");
    let mut total = Decimal::zero();
    for row in rows {
        if let (Some(tokens), Some(price)) = (row.input_tokens, row.input_price) {
            total += Decimal::from(i64::from(tokens.max(0))) * price * per_million.clone();
        }
        if let (Some(tokens), Some(price)) = (row.output_tokens, row.output_price) {
            total += Decimal::from(i64::from(tokens.max(0))) * price * per_million.clone();
        }
        if let (Some(tokens), Some(price)) = (row.cache_read_tokens, row.cache_read_price) {
            total += Decimal::from(i64::from(tokens.max(0))) * price * per_million.clone();
        }
        if let (Some(tokens), Some(price)) = (row.cache_write_tokens, row.cache_write_price) {
            total += Decimal::from(i64::from(tokens.max(0))) * price * per_million.clone();
        }
        if let (Some(tokens), Some(price)) = (row.cache_write_1h_tokens, row.cache_write_1h_price) {
            total += Decimal::from(i64::from(tokens.max(0))) * price * per_million.clone();
        }
    }
    Ok(total)
}

/// Gather chronological conversation messages for a memory scope strictly after
/// `after_message_id` (capped to the freshest `limit` rows).
pub async fn gather_scope_messages(
    db: &impl Read,
    scope_type: MemoryScope,
    scope_id: &str,
    after_message_id: Option<&str>,
    limit: usize,
) -> Result<Vec<message::Model>, DbErr> {
    let conv_ids: Vec<String> = match scope_type {
        MemoryScope::Project => {
            conversation::Entity::find()
                .filter(conversation::Column::ProjectId.eq(scope_id))
                .filter(conversation::Column::ParentConversationId.is_null())
                .select_only()
                .column(conversation::Column::Id)
                .into_tuple()
                .all(db.conn()?)
                .await?
        }
        MemoryScope::ClientGlobal => {
            conversation::Entity::find()
                .filter(conversation::Column::ProjectId.is_null())
                .filter(conversation::Column::ParentConversationId.is_null())
                .select_only()
                .column(conversation::Column::Id)
                .into_tuple()
                .all(db.conn()?)
                .await?
        }
        MemoryScope::ChannelGlobal => {
            let channel_projects: Vec<String> = project::Entity::find()
                .filter(project::Column::SourceType.ne("local"))
                .select_only()
                .column(project::Column::Id)
                .into_tuple()
                .all(db.conn()?)
                .await?;
            if channel_projects.is_empty() {
                Vec::new()
            } else {
                conversation::Entity::find()
                    .filter(conversation::Column::ProjectId.is_in(channel_projects))
                    .filter(conversation::Column::ParentConversationId.is_null())
                    .select_only()
                    .column(conversation::Column::Id)
                    .into_tuple()
                    .all(db.conn()?)
                    .await?
            }
        }
        MemoryScope::ExternalSubject => {
            let Some(uid) = parse_external_subject_numeric_id(scope_id) else {
                return Ok(Vec::new());
            };
            message::Entity::find()
                .filter(message::Column::SenderId.eq(uid))
                .select_only()
                .column(message::Column::ConversationId)
                .distinct()
                .into_tuple()
                .all(db.conn()?)
                .await?
        }
    };

    if conv_ids.is_empty() {
        return Ok(Vec::new());
    }

    let cursor: Option<(EpochMs, String)> = if let Some(after_id) = after_message_id {
        message::Entity::find_by_id(after_id)
            .one(db.conn()?)
            .await?
            .map(|m| (m.created_at, m.id))
    } else {
        None
    };

    let mut query = message::Entity::find()
        .filter(message::Column::ConversationId.is_in(conv_ids))
        .filter(message::Column::Role.is_in(["user", "assistant", "tool"]));
    if let Some((cursor_ts, ref cursor_id)) = cursor {
        query = query.filter(
            sea_orm::Condition::any()
                .add(message::Column::CreatedAt.gt(cursor_ts))
                .add(
                    sea_orm::Condition::all()
                        .add(message::Column::CreatedAt.eq(cursor_ts))
                        .add(message::Column::Id.gt(cursor_id.as_str())),
                ),
        );
    }

    let mut rows = query
        .order_by_desc(message::Column::CreatedAt)
        .order_by_desc(message::Column::Id)
        .limit(limit as u64)
        .all(db.conn()?)
        .await?;
    rows.reverse();
    Ok(rows)
}

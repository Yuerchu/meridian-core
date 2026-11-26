//! Reading and writing `media_generations`.

use sea_orm::ActiveValue::{Set, Unchanged};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DbErr, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder, QuerySelect,
};

use crate::db::entity::media_generation::{self, MediaGenerationDomain, MediaGenerationStatus};
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::types::EpochMs;

pub async fn get(db: &impl Read, id: &str) -> Result<Option<media_generation::Model>, DbErr> {
    media_generation::Entity::find_by_id(id).one(db.conn()?).await
}

pub async fn insert(tx: &WriteTx, row: media_generation::Model) -> Result<media_generation::Model, DbErr> {
    let id = row.id.clone();
    media_generation::Entity::insert(row.into_active_model())
        .exec_without_returning(tx.conn()?)
        .await?;
    get(tx, &id)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound(format!("media generation `{id}`")))
}

pub async fn list(
    db: &impl Read,
    domain: Option<MediaGenerationDomain>,
    limit: u64,
    offset: u64,
) -> Result<Vec<media_generation::Model>, DbErr> {
    let mut query = media_generation::Entity::find().order_by_desc(media_generation::Column::CreatedAt);
    if let Some(domain) = domain {
        query = query.filter(media_generation::Column::CapabilityDomain.eq(domain));
    }
    query.offset(offset).limit(limit).all(db.conn()?).await
}

#[derive(Debug, Default)]
pub struct MediaGenerationProgressChangeset {
    pub deployment_id: Option<Option<String>>,
    pub provider_id: Option<Option<String>>,
    pub remote_model_id: Option<Option<String>>,
    pub protocol_id: Option<Option<String>>,
    pub status: Option<MediaGenerationStatus>,
    pub progress_pct: Option<Option<i32>>,
    pub upstream_job_id: Option<Option<String>>,
    pub result_files_json: Option<Option<String>>,
    pub error_message: Option<Option<String>>,
    pub updated_at: EpochMs,
    pub completed_at: Option<Option<EpochMs>>,
}

pub async fn update(
    tx: &WriteTx,
    id: &str,
    changes: MediaGenerationProgressChangeset,
) -> Result<media_generation::Model, DbErr> {
    let existing = get(tx, id)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound(format!("media generation `{id}`")))?;
    let mut active = media_generation::ActiveModel {
        id: Unchanged(existing.id),
        updated_at: Set(changes.updated_at),
        ..Default::default()
    };
    if let Some(v) = changes.deployment_id {
        active.deployment_id = Set(v);
    }
    if let Some(v) = changes.provider_id {
        active.provider_id = Set(v);
    }
    if let Some(v) = changes.remote_model_id {
        active.remote_model_id = Set(v);
    }
    if let Some(v) = changes.protocol_id {
        active.protocol_id = Set(v);
    }
    if let Some(v) = changes.status {
        active.status = Set(v);
    }
    if let Some(v) = changes.progress_pct {
        active.progress_pct = Set(v);
    }
    if let Some(v) = changes.upstream_job_id {
        active.upstream_job_id = Set(v);
    }
    if let Some(v) = changes.result_files_json {
        active.result_files_json = Set(v);
    }
    if let Some(v) = changes.error_message {
        active.error_message = Set(v);
    }
    if let Some(v) = changes.completed_at {
        active.completed_at = Set(v);
    }
    active.update(tx.conn()?).await
}

pub async fn delete(tx: &WriteTx, id: &str) -> Result<bool, DbErr> {
    Ok(media_generation::Entity::delete_by_id(id)
        .exec(tx.conn()?)
        .await?
        .rows_affected
        > 0)
}

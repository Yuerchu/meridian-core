//! Reading and writing `tool_categories`.

use sea_orm::{DbErr, EntityTrait, IntoActiveModel, PaginatorTrait, QueryOrder};

use crate::db::entity::tool_category;
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};

pub async fn list_categories(db: &impl Read) -> Result<Vec<tool_category::Model>, DbErr> {
    tool_category::Entity::find()
        .order_by_asc(tool_category::Column::SortOrder)
        .order_by_asc(tool_category::Column::Id)
        .all(db.conn()?)
        .await
}

/// Inserts the row the caller built.
pub async fn create_category(tx: &WriteTx, model: tool_category::Model) -> Result<(), DbErr> {
    tool_category::Entity::insert(model.into_active_model())
        .exec_without_returning(tx.conn()?)
        .await?;
    Ok(())
}

pub async fn count_categories(db: &impl Read) -> Result<u64, DbErr> {
    tool_category::Entity::find().count(db.conn()?).await
}

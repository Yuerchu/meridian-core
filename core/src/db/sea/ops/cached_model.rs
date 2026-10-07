//! Reading and writing `cached_models`.
//!
//! No function here opens a transaction of its own: a write takes the caller's
//! `WriteTx`, and the caller's `Db::write` is the `BEGIN IMMEDIATE`.

use sea_orm::ActiveValue::{NotSet, Set};
use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter, QueryOrder};

use crate::db::entity::{cached_model, provider};
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, Snapshot, WriteTx};
use crate::db::types::EpochMs;

pub async fn list_by_provider(db: &impl Read, pid: &str) -> Result<Vec<cached_model::Model>, DbErr> {
    cached_model::Entity::find()
        .filter(cached_model::Column::ProviderId.eq(pid))
        .order_by_asc(cached_model::Column::ModelId)
        .all(db.conn()?)
        .await
}

/// The provider's list becomes exactly `models` (id, display name), all
/// stamped `fetched_at`. The delete and the insert share the caller's write,
/// so a reader sees the old list or the new one, never neither.
pub async fn replace_models(
    tx: &WriteTx,
    pid: &str,
    models: &[(String, String)],
    fetched_at: EpochMs,
) -> Result<(), DbErr> {
    cached_model::Entity::delete_many()
        .filter(cached_model::Column::ProviderId.eq(pid))
        .exec(tx.conn()?)
        .await?;
    if models.is_empty() {
        return Ok(());
    }
    cached_model::Entity::insert_many(models.iter().map(|(model_id, model_name)| cached_model::ActiveModel {
        id: NotSet,
        provider_id: Set(pid.to_owned()),
        model_id: Set(model_id.clone()),
        model_name: Set(model_name.clone()),
        fetched_at: Set(fetched_at),
    }))
    .exec_without_returning(tx.conn()?)
    .await?;
    Ok(())
}

/// How many rows went. A changed address, transport or key makes the list
/// stale; the next open fetches it again.
pub async fn delete_by_provider(tx: &WriteTx, pid: &str) -> Result<u64, DbErr> {
    Ok(cached_model::Entity::delete_many()
        .filter(cached_model::Column::ProviderId.eq(pid))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

/// What is cached for one provider, and nothing else: no request leaves the
/// machine. For the settings page, which only needs to say which configured
/// models the provider's list names, and must not make a network request (or
/// report an offline error) merely because somebody opened it.
///
/// **An empty list is an answer, not a failure** — nothing has been fetched
/// yet. `None` is the provider's absence, read in the same snapshot as the
/// list so the two cannot be confused.
pub async fn list_cached_for_provider(
    db: &impl Snapshot,
    pid: &str,
) -> Result<Option<Vec<cached_model::Model>>, DbErr> {
    if provider::Entity::find_by_id(pid).one(db.conn()?).await?.is_none() {
        return Ok(None);
    }
    list_by_provider(db, pid).await.map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::cap::Db;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    async fn provider(db: &Db, id: &str) {
        execute_for_tests(
            db,
            &format!(
                "INSERT INTO providers (id, name, base_url, created_at, updated_at) \
                 VALUES ('{id}', 'P', 'https://example.invalid', 0, 0)"
            ),
        )
        .await
        .unwrap();
    }

    async fn cached(db: &Db, pid: &str) -> Option<Vec<(String, i64)>> {
        db.read(async |tx| list_cached_for_provider(tx, pid).await)
            .await
            .unwrap()
            .map(|rows| rows.into_iter().map(|r| (r.model_id, r.fetched_at)).collect())
    }

    #[tokio::test]
    async fn nothing_fetched_yet_is_an_empty_list() {
        let db = sea_test_db().await;
        provider(&db, "p1").await;
        assert_eq!(cached(&db, "p1").await, Some(Vec::new()));
    }

    #[tokio::test]
    async fn an_unknown_provider_is_none_and_not_an_empty_cache() {
        let db = sea_test_db().await;
        assert_eq!(cached(&db, "missing").await, None);
    }

    /// Each replace is the whole list: a refetch that drops a model drops its
    /// row, and the other provider's list is untouched.
    #[tokio::test]
    async fn the_cache_is_replaced_whole_for_that_provider_only_in_model_order() {
        let db = sea_test_db().await;
        provider(&db, "p1").await;
        provider(&db, "p2").await;
        let models = |names: &[&str]| -> Vec<(String, String)> {
            names.iter().map(|n| (n.to_string(), n.to_uppercase())).collect()
        };
        db.write(async |tx| {
            replace_models(tx, "p1", &models(&["zeta", "alpha", "gone"]), 5).await?;
            replace_models(tx, "p2", &models(&["other"]), 5).await?;
            replace_models(tx, "p1", &models(&["zeta", "alpha"]), 7).await
        })
        .await
        .unwrap();

        assert_eq!(
            cached(&db, "p1").await,
            Some(vec![("alpha".to_string(), 7), ("zeta".to_string(), 7)])
        );
        let other = list_by_provider(&db, "p2").await.unwrap();
        assert_eq!((other.len(), other[0].model_name.as_str()), (1, "OTHER"));

        db.write(async |tx| replace_models(tx, "p2", &[], 9).await)
            .await
            .unwrap();
        assert!(list_by_provider(&db, "p2").await.unwrap().is_empty());
    }
}

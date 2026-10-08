//! Reading and writing `model_profiles`.
//!
//! The shell's model commands run on these; `seed_flat`, the Diesel test
//! seeder, still writes through `db::ops::model_profile`, and the pairs are
//! listed in `docs/dual-impl.md`.
//!
//! No function here opens a transaction of its own: a write takes the caller's
//! `WriteTx`, and the caller's `Db::write` is the `BEGIN IMMEDIATE`.

use std::collections::HashMap;

use sea_orm::ActiveValue::Unchanged;
use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DbErr, EntityTrait, IntoActiveModel, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect,
};

use crate::db::entity::model_config;
use crate::db::entity::model_profile;
use crate::db::entity::model_profile::ModelProfileChangeset;
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, Snapshot, WriteTx};

pub async fn get(db: &impl Read, id: &str) -> Result<Option<model_profile::Model>, DbErr> {
    model_profile::Entity::find_by_id(id).one(db.conn()?).await
}

/// Every profile, by name, with the number of provider rows pointing at it —
/// what the editor needs to say "changing this affects three providers".
/// Two statements, so it takes a snapshot.
pub async fn list_with_model_counts(db: &impl Snapshot) -> Result<Vec<(model_profile::Model, i64)>, DbErr> {
    let profiles = model_profile::Entity::find()
        .order_by_asc(model_profile::Column::Name)
        .all(db.conn()?)
        .await?;
    let counts: HashMap<String, i64> = model_config::Entity::find()
        .select_only()
        .column(model_config::Column::ProfileId)
        .column_as(Expr::col(model_config::Column::Id).count(), "count")
        .group_by(model_config::Column::ProfileId)
        .into_tuple::<(String, i64)>()
        .all(db.conn()?)
        .await?
        .into_iter()
        .collect();
    Ok(profiles
        .into_iter()
        .map(|profile| {
            let count = counts.get(&profile.id).copied().unwrap_or_default();
            (profile, count)
        })
        .collect())
}

fn not_found(id: &str) -> DbErr {
    DbErr::RecordNotFound(format!("model profile `{id}`"))
}

/// Inserts the row the caller built and reads it back.
pub async fn insert(tx: &WriteTx, model: model_profile::Model) -> Result<model_profile::Model, DbErr> {
    let id = model.id.clone();
    model_profile::Entity::insert(model.into_active_model())
        .exec_without_returning(tx.conn()?)
        .await?;
    get(tx, &id).await?.ok_or_else(|| not_found(&id))
}

/// Replaces every field but the key and `created_at`; a `None` clears.
/// `RecordNotFound` when there is no such profile, before anything is written.
pub async fn update(tx: &WriteTx, id: &str, changeset: ModelProfileChangeset) -> Result<model_profile::Model, DbErr> {
    let existing = get(tx, id).await?.ok_or_else(|| not_found(id))?;
    let mut row = changeset.into_active_model();
    row.id = Unchanged(existing.id);
    row.update(tx.conn()?).await
}

/// Drops a profile only while nothing points at it, and says whether it went:
/// when the last model moves to another profile, the one it left would
/// otherwise sit in the picker for ever.
pub async fn delete_if_unreferenced(tx: &WriteTx, id: &str) -> Result<bool, DbErr> {
    let referenced = model_config::Entity::find()
        .filter(model_config::Column::ProfileId.eq(id))
        .count(tx.conn()?)
        .await?;
    if referenced > 0 {
        return Ok(false);
    }
    Ok(model_profile::Entity::delete_by_id(id)
        .exec(tx.conn()?)
        .await?
        .rows_affected
        > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::cap::Db;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    fn profile(id: &str, name: &str) -> model_profile::Model {
        model_profile::Model {
            id: id.into(),
            name: name.into(),
            context_window: 128_000,
            compact_threshold: 100_000,
            max_output_tokens: None,
            input_price: None,
            output_price: None,
            cache_read_price: None,
            cache_write_price: None,
            pricing_tiers: None,
            capability_overrides: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    async fn with_profiles(profiles: Vec<model_profile::Model>) -> Db {
        let db = sea_test_db().await;
        execute_for_tests(
            &db,
            "INSERT INTO providers (id, name, base_url, created_at, updated_at)
                 VALUES ('p1', 'Acme', 'https://example.invalid', 0, 0),
                        ('p2', 'Relay', 'https://relay.invalid', 0, 0)",
        )
        .await
        .unwrap();
        for profile in profiles {
            db.write(async |tx| insert(tx, profile).await).await.unwrap();
        }
        db
    }

    async fn point_at(db: &Db, id: &str, provider_id: &str, profile_id: &str) {
        execute_for_tests(
            db,
            &format!(
                "INSERT INTO model_configs (id, provider_id, model_id, profile_id, created_at, updated_at)
                     VALUES ('{id}', '{provider_id}', 'claude-sonnet-5', '{profile_id}', 0, 0)"
            ),
        )
        .await
        .unwrap();
    }

    /// The count is the whole reason the list is not a plain `find`; a profile
    /// nothing points at reads as zero, in name order with the rest.
    #[tokio::test]
    async fn a_profile_carries_how_many_providers_reach_it() {
        let db = with_profiles(vec![profile("shared", "Claude Sonnet 5"), profile("lonely", "Zephyr")]).await;
        point_at(&db, "mc1", "p1", "shared").await;
        point_at(&db, "mc2", "p2", "shared").await;

        let listed = db.read(async |tx| list_with_model_counts(tx).await).await.unwrap();
        let counts: Vec<_> = listed.iter().map(|(p, n)| (p.id.as_str(), *n)).collect();
        assert_eq!(counts, [("shared", 2), ("lonely", 0)]);
    }

    /// Collecting the profile a model just left, and refusing to collect one
    /// that is still somebody's only description of a model.
    #[tokio::test]
    async fn a_profile_is_collected_only_once_nothing_points_at_it() {
        let db = with_profiles(vec![profile("held", "Held"), profile("free", "Free")]).await;
        point_at(&db, "mc1", "p1", "held").await;

        assert!(
            !db.write(async |tx| delete_if_unreferenced(tx, "held").await)
                .await
                .unwrap()
        );
        assert!(get(&db, "held").await.unwrap().is_some());
        assert!(
            db.write(async |tx| delete_if_unreferenced(tx, "free").await)
                .await
                .unwrap()
        );
        assert_eq!(get(&db, "free").await.unwrap(), None);
    }

    /// An edit that clears a price has to clear it: a changeset that read
    /// `None` as "leave this column alone" would keep billing at a rate the
    /// user just deleted. Every other field is replaced too, and the key and
    /// `created_at` stay.
    #[tokio::test]
    async fn an_edit_can_empty_a_price_again() {
        let db = with_profiles(vec![model_profile::Model {
            max_output_tokens: Some(8_000),
            input_price: Some("3".parse().unwrap()),
            output_price: Some("15".parse().unwrap()),
            cache_read_price: Some("0.3".parse().unwrap()),
            cache_write_price: Some("3.75".parse().unwrap()),
            pricing_tiers: Some(r#"[{"min_prompt_tokens":200000}]"#.into()),
            capability_overrides: Some(r#"{"supports_fast":true}"#.into()),
            created_at: 5,
            ..profile("p", "Claude")
        }])
        .await;

        let changes = ModelProfileChangeset {
            name: "Claude 2".into(),
            context_window: 200_000,
            compact_threshold: 150_000,
            max_output_tokens: None,
            input_price: None,
            output_price: None,
            cache_read_price: None,
            cache_write_price: None,
            pricing_tiers: None,
            capability_overrides: None,
            updated_at: 7,
        };
        let after = db.write(async |tx| update(tx, "p", changes).await).await.unwrap();
        assert_eq!(
            after,
            model_profile::Model {
                name: "Claude 2".into(),
                context_window: 200_000,
                compact_threshold: 150_000,
                created_at: 5,
                updated_at: 7,
                ..profile("p", "")
            }
        );
        assert_eq!(get(&db, "p").await.unwrap(), Some(after));

        let missing = db
            .write(async |tx| {
                let changes = ModelProfileChangeset {
                    name: "x".into(),
                    context_window: 1,
                    compact_threshold: 1,
                    max_output_tokens: None,
                    input_price: None,
                    output_price: None,
                    cache_read_price: None,
                    cache_write_price: None,
                    pricing_tiers: None,
                    capability_overrides: None,
                    updated_at: 9,
                };
                update(tx, "nope", changes).await
            })
            .await;
        assert!(matches!(missing, Err(DbErr::RecordNotFound(_))));
    }
}

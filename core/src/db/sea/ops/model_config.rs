//! Reading and writing `model_configs`.
//!
//! Every config is read beside its profile — there is no window, no
//! capability patch and usually no price without it — and the two are
//! separate statements, so those reads take a snapshot.
//!
//! No function here opens a transaction of its own: a write takes the caller's
//! `WriteTx`, and the caller's `Db::write` is the `BEGIN IMMEDIATE`.

use std::collections::HashMap;

use sea_orm::ActiveValue::{Set, Unchanged};
use sea_orm::{ActiveModelTrait, ColumnTrait, DbErr, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder};

use crate::db::entity::{model_config, model_profile};
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, Snapshot, WriteTx};
use crate::db::sea::ops::model_profile as profile_ops;

pub async fn get(db: &impl Read, id: &str) -> Result<Option<model_config::Model>, DbErr> {
    model_config::Entity::find_by_id(id).one(db.conn()?).await
}

/// The foreign key makes a missing profile a broken database rather than a
/// state to answer around.
fn missing_profile(config: &model_config::Model) -> DbErr {
    DbErr::RecordNotFound(format!(
        "model profile `{}` of model configuration `{}`",
        config.profile_id, config.id
    ))
}

/// A provider's models, by wire id, each with the profile it points at.
pub async fn list_by_provider_with_profiles(
    db: &impl Snapshot,
    provider_id: &str,
) -> Result<Vec<(model_config::Model, model_profile::Model)>, DbErr> {
    let configs = model_config::Entity::find()
        .filter(model_config::Column::ProviderId.eq(provider_id))
        .order_by_asc(model_config::Column::ModelId)
        .all(db.conn()?)
        .await?;
    let profile_ids: Vec<&str> = configs.iter().map(|config| config.profile_id.as_str()).collect();
    let profiles: HashMap<String, model_profile::Model> = model_profile::Entity::find()
        .filter(model_profile::Column::Id.is_in(profile_ids))
        .all(db.conn()?)
        .await?
        .into_iter()
        .map(|profile| (profile.id.clone(), profile))
        .collect();
    configs
        .into_iter()
        .map(|config| match profiles.get(&config.profile_id) {
            Some(profile) => Ok((config, profile.clone())),
            None => Err(missing_profile(&config)),
        })
        .collect()
}

/// Every provider's models, each with the profile it points at.
pub async fn list_with_profiles(db: &impl Snapshot) -> Result<Vec<(model_config::Model, model_profile::Model)>, DbErr> {
    let configs = model_config::Entity::find().all(db.conn()?).await?;
    let profiles: HashMap<String, model_profile::Model> = model_profile::Entity::find()
        .all(db.conn()?)
        .await?
        .into_iter()
        .map(|profile| (profile.id.clone(), profile))
        .collect();
    configs
        .into_iter()
        .map(|config| match profiles.get(&config.profile_id) {
            Some(profile) => Ok((config, profile.clone())),
            None => Err(missing_profile(&config)),
        })
        .collect()
}

/// One provider's door to one model, with its profile.
pub async fn get_with_profile(
    db: &impl Snapshot,
    provider_id: &str,
    model_id: &str,
) -> Result<Option<(model_config::Model, model_profile::Model)>, DbErr> {
    let Some(config) = model_config::Entity::find()
        .filter(model_config::Column::ProviderId.eq(provider_id))
        .filter(model_config::Column::ModelId.eq(model_id))
        .one(db.conn()?)
        .await?
    else {
        return Ok(None);
    };
    let profile = profile_ops::get(db, &config.profile_id)
        .await?
        .ok_or_else(|| missing_profile(&config))?;
    Ok(Some((config, profile)))
}

/// Writes `new` as this provider's config for its model: inserted when there
/// is none, otherwise every field but the key and `created_at` replaced. A
/// profile the row stops pointing at is collected if nothing else uses it.
pub async fn upsert(tx: &WriteTx, new: model_config::Model) -> Result<model_config::Model, DbErr> {
    let existing = model_config::Entity::find()
        .filter(model_config::Column::ProviderId.eq(&new.provider_id))
        .filter(model_config::Column::ModelId.eq(&new.model_id))
        .one(tx.conn()?)
        .await?;
    let Some(existing) = existing else {
        let id = new.id.clone();
        model_config::Entity::insert(new.into_active_model())
            .exec_without_returning(tx.conn()?)
            .await?;
        return get(tx, &id)
            .await?
            .ok_or_else(|| DbErr::RecordNotFound(format!("model configuration `{id}`")));
    };
    let new_profile_id = new.profile_id.clone();
    let row = model_config::ActiveModel {
        id: Unchanged(existing.id),
        provider_id: Unchanged(existing.provider_id),
        model_id: Unchanged(existing.model_id),
        profile_id: Set(new.profile_id),
        overrides_pricing: Set(new.overrides_pricing),
        input_price: Set(new.input_price),
        output_price: Set(new.output_price),
        cache_read_price: Set(new.cache_read_price),
        cache_write_price: Set(new.cache_write_price),
        cache_write_1h_price: Set(new.cache_write_1h_price),
        pricing_tiers: Set(new.pricing_tiers),
        server_tools: Set(new.server_tools),
        server_tool_price: Set(new.server_tool_price),
        created_at: Unchanged(existing.created_at),
        updated_at: Set(new.updated_at),
        protocol_override: Set(new.protocol_override),
        is_enabled: Set(new.is_enabled),
        priority: Set(new.priority),
        capability_overrides: Set(new.capability_overrides),
    }
    .update(tx.conn()?)
    .await?;
    // The profile it was pointing at a moment ago may now describe nothing.
    if existing.profile_id != new_profile_id {
        profile_ops::delete_if_unreferenced(tx, &existing.profile_id).await?;
    }
    Ok(row)
}

/// List all provider deployments serving a given logical model (`profile_id`),
/// ordered by ascending `priority`.
pub async fn list_by_profile(db: &impl Read, profile_id: &str) -> Result<Vec<model_config::Model>, DbErr> {
    model_config::Entity::find()
        .filter(model_config::Column::ProfileId.eq(profile_id))
        .order_by_asc(model_config::Column::Priority)
        .order_by_asc(model_config::Column::Id)
        .all(db.conn()?)
        .await
}

/// Deployment-level partial update for many-to-many routing & overrides.
#[derive(Debug, Clone)]
pub struct DeploymentUpdate {
    pub profile_id: Option<String>,
    pub protocol_override: Option<Option<String>>,
    pub is_enabled: Option<crate::db::types::SqlBool>,
    pub priority: Option<i32>,
    pub capability_overrides: Option<Option<String>>,
    pub updated_at: crate::db::types::EpochMs,
}

/// Update deployment routing/protocol/override fields without deleting the
/// underlying logical model.
pub async fn update_deployment(
    tx: &WriteTx,
    id: &str,
    changes: DeploymentUpdate,
) -> Result<model_config::Model, DbErr> {
    let existing = get(tx, id)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound(format!("model deployment `{id}`")))?;
    let mut active = model_config::ActiveModel {
        id: Unchanged(existing.id),
        updated_at: Set(changes.updated_at),
        ..Default::default()
    };
    if let Some(profile_id) = changes.profile_id {
        active.profile_id = Set(profile_id);
    }
    if let Some(protocol_override) = changes.protocol_override {
        active.protocol_override = Set(protocol_override);
    }
    if let Some(is_enabled) = changes.is_enabled {
        active.is_enabled = Set(is_enabled);
    }
    if let Some(priority) = changes.priority {
        active.priority = Set(priority);
    }
    if let Some(capability_overrides) = changes.capability_overrides {
        active.capability_overrides = Set(capability_overrides);
    }
    active.update(tx.conn()?).await
}

/// Unlink (delete) a single provider deployment WITHOUT auto-deleting its
/// logical model — logical models persist independently even with 0 channels.
pub async fn unlink_deployment(tx: &WriteTx, id: &str) -> Result<u64, DbErr> {
    Ok(model_config::Entity::delete_by_id(id)
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

/// Re-point an existing deployment at `target_model_id` (merging a relay's
/// custom-named model under an existing logical model).
pub async fn merge_deployment_into_model(
    tx: &WriteTx,
    deployment_id: &str,
    target_model_id: &str,
    now: crate::db::types::EpochMs,
) -> Result<model_config::Model, DbErr> {
    profile_ops::get(tx, target_model_id)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound(format!("logical model `{target_model_id}`")))?;
    update_deployment(
        tx,
        deployment_id,
        DeploymentUpdate {
            profile_id: Some(target_model_id.to_owned()),
            protocol_override: None,
            is_enabled: None,
            priority: None,
            capability_overrides: None,
            updated_at: now,
        },
    )
    .await
}

/// Split an existing deployment out onto its own brand-new logical model in one
/// transaction.
pub async fn split_deployment_to_new_model(
    tx: &WriteTx,
    deployment_id: &str,
    new_model: model_profile::Model,
    now: crate::db::types::EpochMs,
) -> Result<(model_profile::Model, model_config::Model), DbErr> {
    let created = profile_ops::insert(tx, new_model).await?;
    let updated = merge_deployment_into_model(tx, deployment_id, &created.id, now).await?;
    Ok((created, updated))
}

/// How many rows went: 0 for an id that was already gone. The profile it
/// pointed at is collected if nothing else uses it.
pub async fn delete(tx: &WriteTx, id: &str) -> Result<u64, DbErr> {
    let Some(existing) = get(tx, id).await? else {
        return Ok(0);
    };
    let deleted = model_config::Entity::delete_by_id(id)
        .exec(tx.conn()?)
        .await?
        .rows_affected;
    profile_ops::delete_if_unreferenced(tx, &existing.profile_id).await?;
    Ok(deleted)
}

#[cfg(any(test, feature = "test-support"))]
pub struct FlatModelConfig<'a> {
    pub id: &'a str,
    pub provider_id: &'a str,
    pub model_id: &'a str,
    pub display_name: Option<&'a str>,
    pub context_window: i32,
    pub compact_threshold: i32,
    pub max_output_tokens: Option<i32>,
    pub input_price: Option<crate::decimal::Decimal>,
    pub output_price: Option<crate::decimal::Decimal>,
    pub cache_read_price: Option<crate::decimal::Decimal>,
    pub cache_write_price: Option<crate::decimal::Decimal>,
    pub capability_overrides: Option<&'a str>,
    pub pricing_tiers: Option<&'a str>,
    pub server_tools: Option<&'a str>,
    pub server_tool_price: Option<crate::decimal::Decimal>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[cfg(any(test, feature = "test-support"))]
impl<'a> Default for FlatModelConfig<'a> {
    fn default() -> Self {
        Self {
            id: "mc1",
            provider_id: "p1",
            model_id: "m1",
            display_name: None,
            context_window: 128_000,
            compact_threshold: 100_000,
            max_output_tokens: None,
            input_price: None,
            output_price: None,
            cache_read_price: None,
            cache_write_price: None,
            capability_overrides: None,
            pricing_tiers: None,
            server_tools: None,
            server_tool_price: None,
            created_at: 0,
            updated_at: 0,
        }
    }
}

/// The old flat shape as a profile and a non-overriding config, for tests:
/// prices, window and capability patch on the profile, provider-side tools
/// on the row. Seeding the same model twice is a price change, which several
/// callers test, so both rows are upserted.
#[cfg(any(test, feature = "test-support"))]
pub async fn seed_flat(tx: &WriteTx, flat: &FlatModelConfig<'_>) -> Result<model_config::Model, DbErr> {
    use sea_orm::ActiveValue::Set;
    use sea_orm::sea_query::OnConflict;

    let profile_id = format!("{}-profile", flat.id);
    model_profile::Entity::insert(model_profile::ActiveModel {
        id: Set(profile_id.clone()),
        name: Set(flat.display_name.unwrap_or(flat.model_id).to_owned()),
        context_window: Set(flat.context_window),
        compact_threshold: Set(flat.compact_threshold),
        max_output_tokens: Set(flat.max_output_tokens),
        input_price: Set(flat.input_price.clone()),
        output_price: Set(flat.output_price.clone()),
        cache_read_price: Set(flat.cache_read_price.clone()),
        cache_write_price: Set(flat.cache_write_price.clone()),
        cache_write_1h_price: Set(None),
        cache_ttl: Set(None),
        pricing_tiers: Set(flat.pricing_tiers.map(str::to_owned)),
        capability_overrides: Set(flat.capability_overrides.map(str::to_owned)),
        created_at: Set(flat.created_at),
        updated_at: Set(flat.updated_at),
        canonical_id: Set(None),
        capability_domain: Set(crate::provider::CapabilityDomain::Chat),
        family: Set(None),
        icon: Set(None),
        routing_policy: Set(crate::provider::RoutingPolicy::StickyManual),
        is_archived: Set(crate::db::types::SqlBool::FALSE),
    })
    .on_conflict(
        OnConflict::column(model_profile::Column::Id)
            .update_columns([
                model_profile::Column::Name,
                model_profile::Column::ContextWindow,
                model_profile::Column::CompactThreshold,
                model_profile::Column::MaxOutputTokens,
                model_profile::Column::InputPrice,
                model_profile::Column::OutputPrice,
                model_profile::Column::CacheReadPrice,
                model_profile::Column::CacheWritePrice,
                model_profile::Column::PricingTiers,
                model_profile::Column::CapabilityOverrides,
                model_profile::Column::UpdatedAt,
            ])
            .to_owned(),
    )
    .exec_without_returning(tx.conn()?)
    .await?;
    model_config::Entity::insert(model_config::ActiveModel {
        id: Set(flat.id.to_owned()),
        provider_id: Set(flat.provider_id.to_owned()),
        model_id: Set(flat.model_id.to_owned()),
        profile_id: Set(profile_id),
        overrides_pricing: Set(crate::db::types::SqlBool::FALSE),
        input_price: Set(None),
        output_price: Set(None),
        cache_read_price: Set(None),
        cache_write_price: Set(None),
        cache_write_1h_price: Set(None),
        pricing_tiers: Set(None),
        server_tools: Set(flat.server_tools.map(str::to_owned)),
        server_tool_price: Set(flat.server_tool_price.clone()),
        created_at: Set(flat.created_at),
        updated_at: Set(flat.updated_at),
        protocol_override: Set(None),
        is_enabled: Set(crate::db::types::SqlBool::TRUE),
        priority: Set(0),
        capability_overrides: Set(None),
    })
    .on_conflict(
        OnConflict::columns([model_config::Column::ProviderId, model_config::Column::ModelId])
            .update_columns([
                model_config::Column::ProfileId,
                model_config::Column::ServerTools,
                model_config::Column::ServerToolPrice,
                model_config::Column::UpdatedAt,
            ])
            .to_owned(),
    )
    .exec_without_returning(tx.conn()?)
    .await?;
    get_with_profile(tx, flat.provider_id, flat.model_id)
        .await?
        .map(|(config, _)| config)
        .ok_or_else(|| DbErr::RecordNotFound(format!("model config {}", flat.id)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::cap::Db;
    use crate::db::sea::{execute_for_tests, sea_test_db};
    use crate::db::types::SqlBool;
    use crate::decimal::Decimal;

    const TIERS: &str = r#"[{"min_prompt_tokens":200000,"input_price":"4","output_price":"12","cache_read_price":null,"cache_write_price":null,"cache_write_1h_price":null}]"#;

    fn decimal(raw: &str) -> Decimal {
        raw.parse().unwrap()
    }

    async fn seeded(profiles: &[&str]) -> Db {
        let db = sea_test_db().await;
        execute_for_tests(
            &db,
            "INSERT INTO providers (id, name, base_url, created_at, updated_at)
                 VALUES ('p1', 'Acme', 'https://example.invalid', 0, 0)",
        )
        .await
        .unwrap();
        for id in profiles {
            let profile = model_profile::Model {
                id: (*id).into(),
                name: (*id).into(),
                context_window: 128_000,
                compact_threshold: 100_000,
                max_output_tokens: None,
                input_price: None,
                output_price: None,
                cache_read_price: None,
                cache_write_price: None,
                cache_write_1h_price: None,
                cache_ttl: None,
                pricing_tiers: None,
                capability_overrides: None,
                created_at: 0,
                updated_at: 0,
                canonical_id: None,
                capability_domain: crate::provider::CapabilityDomain::Chat,
                family: None,
                icon: None,
                routing_policy: crate::provider::RoutingPolicy::StickyManual,
                is_archived: SqlBool::FALSE,
            };
            db.write(async |tx| profile_ops::insert(tx, profile).await)
                .await
                .unwrap();
        }
        db
    }

    fn blank() -> model_config::Model {
        model_config::Model {
            id: "mc1".into(),
            provider_id: "p1".into(),
            model_id: "grok-4.6".into(),
            profile_id: "prof1".into(),
            overrides_pricing: SqlBool::FALSE,
            input_price: None,
            output_price: None,
            cache_read_price: None,
            cache_write_price: None,
            cache_write_1h_price: None,
            pricing_tiers: None,
            server_tools: None,
            server_tool_price: None,
            created_at: 100,
            updated_at: 100,
            protocol_override: None,
            is_enabled: SqlBool::TRUE,
            priority: 0,
            capability_overrides: None,
        }
    }

    async fn save(db: &Db, row: model_config::Model) -> model_config::Model {
        db.write(async |tx| upsert(tx, row).await).await.unwrap()
    }

    /// Every column an edit was given has to actually land.
    ///
    /// A whole-struct comparison on purpose. The update is a hand-written column
    /// list, and that list is exactly where a new column gets silently
    /// forgotten: `pricing_tiers` and `server_tools` were both once saved
    /// correctly the first time a model was configured, because that path
    /// inserts the whole row, and dropped by every edit after that. Comparing
    /// the row as a whole is what makes the next added column fail here instead
    /// of in somebody's settings panel.
    #[tokio::test]
    async fn an_edit_writes_every_column_it_was_given() {
        let db = seeded(&["prof1"]).await;
        save(&db, blank()).await;

        let after = save(
            &db,
            model_config::Model {
                // The id an edit carries is not the row's; the row keeps its own.
                id: "minted-for-this-save".into(),
                overrides_pricing: SqlBool::TRUE,
                input_price: Some(decimal("2")),
                output_price: Some(decimal("6")),
                cache_read_price: Some(decimal("0.5")),
                cache_write_price: Some(decimal("1.5")),
                cache_write_1h_price: Some(decimal("2.5")),
                pricing_tiers: Some(TIERS.into()),
                server_tools: Some(r#"["web_search"]"#.into()),
                server_tool_price: Some(decimal("5")),
                // The two an update must *not* move, whatever it is passed.
                created_at: 999,
                updated_at: 200,
                ..blank()
            },
        )
        .await;

        assert_eq!(
            after,
            model_config::Model {
                overrides_pricing: SqlBool::TRUE,
                input_price: Some(decimal("2")),
                output_price: Some(decimal("6")),
                cache_read_price: Some(decimal("0.5")),
                cache_write_price: Some(decimal("1.5")),
                cache_write_1h_price: Some(decimal("2.5")),
                pricing_tiers: Some(TIERS.into()),
                server_tools: Some(r#"["web_search"]"#.into()),
                server_tool_price: Some(decimal("5")),
                // When the row first appeared, not when it was last touched.
                created_at: 100,
                updated_at: 200,
                ..blank()
            },
        );
        assert_eq!(get(&db, "mc1").await.unwrap(), Some(after));
    }

    /// Clearing a setting has to clear it. An update that only ever writes
    /// `Some` leaves the old value behind, and the switch the user just turned
    /// off goes on being sent.
    #[tokio::test]
    async fn an_edit_can_empty_a_column_again() {
        let db = seeded(&["prof1"]).await;
        save(
            &db,
            model_config::Model {
                overrides_pricing: SqlBool::TRUE,
                server_tools: Some(r#"["web_search"]"#.into()),
                server_tool_price: Some(decimal("5")),
                pricing_tiers: Some(TIERS.into()),
                cache_read_price: Some(decimal("0.5")),
                ..blank()
            },
        )
        .await;

        let cleared = save(&db, blank()).await;
        assert_eq!(cleared, blank());
    }

    /// Moving the last model off a profile collects it; moving one off a
    /// shared profile leaves it standing for the others; saving without moving
    /// collects nothing.
    #[tokio::test]
    async fn moving_a_model_collects_the_profile_it_emptied() {
        let db = seeded(&["prof1", "prof2"]).await;
        save(&db, blank()).await;
        save(
            &db,
            model_config::Model {
                id: "mc2".into(),
                model_id: "grok-4.6-mini".into(),
                profile_id: "prof2".into(),
                ..blank()
            },
        )
        .await;

        let moved = save(
            &db,
            model_config::Model {
                profile_id: "prof2".into(),
                updated_at: 300,
                ..blank()
            },
        )
        .await;
        assert_eq!(moved.profile_id, "prof2");
        assert_eq!(profile_ops::get(&db, "prof1").await.unwrap(), None);
        assert!(profile_ops::get(&db, "prof2").await.unwrap().is_some());

        save(
            &db,
            model_config::Model {
                profile_id: "prof2".into(),
                updated_at: 400,
                ..blank()
            },
        )
        .await;
        assert!(profile_ops::get(&db, "prof2").await.unwrap().is_some());
    }

    /// The join every reader actually wants, in wire-id order.
    #[tokio::test]
    async fn a_model_reads_back_beside_the_profile_that_describes_it() {
        let db = seeded(&["prof1", "prof2"]).await;
        save(&db, blank()).await;
        save(
            &db,
            model_config::Model {
                id: "mc0".into(),
                model_id: "grok-4.5".into(),
                profile_id: "prof2".into(),
                ..blank()
            },
        )
        .await;

        let (config, profile) = db
            .read(async |tx| get_with_profile(tx, "p1", "grok-4.6").await)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((config.id.as_str(), profile.id.as_str()), ("mc1", "prof1"));
        let absent = db
            .read(async |tx| get_with_profile(tx, "p1", "nope").await)
            .await
            .unwrap();
        assert_eq!(absent, None);

        let listed = db
            .read(async |tx| list_by_provider_with_profiles(tx, "p1").await)
            .await
            .unwrap();
        let pairs: Vec<_> = listed
            .iter()
            .map(|(c, p)| (c.model_id.as_str(), p.id.as_str()))
            .collect();
        assert_eq!(pairs, [("grok-4.5", "prof2"), ("grok-4.6", "prof1")]);
    }

    /// Deleting the only model on a profile takes the profile with it; a
    /// second delete finds nothing.
    #[tokio::test]
    async fn deleting_the_last_model_collects_its_profile() {
        let db = seeded(&["prof1"]).await;
        save(&db, blank()).await;

        assert_eq!(db.write(async |tx| delete(tx, "mc1").await).await.unwrap(), 1);
        assert_eq!(profile_ops::get(&db, "prof1").await.unwrap(), None);
        assert_eq!(db.write(async |tx| delete(tx, "mc1").await).await.unwrap(), 0);
    }
}

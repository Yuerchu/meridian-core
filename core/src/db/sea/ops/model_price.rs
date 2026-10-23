//! Writing models.dev prices onto `model_profiles`.
//!
//! The decision and the write have to see the same row, so they share the
//! caller's transaction: a save landing between "this profile has no prices"
//! and the update would otherwise be overwritten. The rules about *which* row
//! may be written live in [`crate::provider::models_dev`], beside the document
//! they were read from.

use std::collections::HashMap;

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use tracing::warn;

use crate::db::entity::{model_config, model_profile, provider};
use crate::db::sea::cap::WriteTx;
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::ops;
use crate::provider::models_dev::{ModelFacts, Snapshot, SyncReport, catalog_of, may_write};
use crate::util::now_ms;

/// Every profile the snapshot can price.
///
/// A profile reached through two providers that disagree is skipped rather
/// than resolved: the same model on a vendor's API and on a reseller can carry
/// different rates, and choosing one would bill against a number nobody set.
pub async fn apply(tx: &WriteTx, snapshot: &Snapshot) -> Result<SyncReport, sea_orm::DbErr> {
    let providers = provider::Entity::find().all(tx.conn()?).await?;
    let by_id: HashMap<&str, &provider::Model> = providers.iter().map(|row| (row.id.as_str(), row)).collect();
    let configs = model_config::Entity::find().all(tx.conn()?).await?;
    let profiles = model_profile::Entity::find().all(tx.conn()?).await?;

    let mut per_profile: HashMap<&str, Vec<&model_config::Model>> = HashMap::new();
    for config in &configs {
        per_profile.entry(config.profile_id.as_str()).or_default().push(config);
    }

    let mut report = SyncReport::default();
    for profile in &profiles {
        let Some(configs) = per_profile.get(profile.id.as_str()) else {
            continue;
        };
        let mut agreed: Option<&ModelFacts> = None;
        let mut ambiguous = false;
        for config in configs {
            let Some(row) = by_id.get(config.provider_id.as_str()) else {
                continue;
            };
            let Some(catalog_id) = catalog_of(row) else {
                continue;
            };
            let Some(facts) = snapshot.facts(&catalog_id, &row.base_url, &config.model_id) else {
                continue;
            };
            match agreed {
                None => agreed = Some(facts),
                Some(existing) if existing == facts => {}
                Some(_) => {
                    ambiguous = true;
                    break;
                }
            }
        }

        if ambiguous {
            warn!(
                profile = %profile.name,
                "models.dev prices disagree across the providers reaching this profile; left alone"
            );
            report.ambiguous += 1;
            continue;
        }
        let Some(facts) = agreed else {
            report.unmatched += 1;
            continue;
        };
        if !may_write(profile, facts) {
            report.kept += 1;
            continue;
        }
        write_prices(tx, profile, facts).await?;
        report.filled += 1;
    }
    Ok(report)
}

/// One model, right after its configuration was saved.
///
/// `Ok(false)` when nothing was written — the provider is not one models.dev
/// describes, the model is not in the snapshot, or the profile already carries
/// prices. That is the ordinary outcome, not a failure.
pub async fn fill_one(
    tx: &WriteTx,
    snapshot: &Snapshot,
    provider_id: &str,
    model_id: &str,
) -> Result<bool, sea_orm::DbErr> {
    let Some(provider_row) = provider::Entity::find_by_id(provider_id).one(tx.conn()?).await? else {
        return Ok(false);
    };
    let Some(config) = model_config::Entity::find()
        .filter(model_config::Column::ProviderId.eq(provider_id))
        .filter(model_config::Column::ModelId.eq(model_id))
        .one(tx.conn()?)
        .await?
    else {
        return Ok(false);
    };
    let Some(catalog_id) = catalog_of(&provider_row) else {
        return Ok(false);
    };
    let Some(facts) = snapshot.facts(&catalog_id, &provider_row.base_url, model_id) else {
        return Ok(false);
    };
    let Some(profile) = model_profile::Entity::find_by_id(&config.profile_id)
        .one(tx.conn()?)
        .await?
    else {
        return Ok(false);
    };
    if !may_write(&profile, facts) {
        return Ok(false);
    }
    write_prices(tx, &profile, facts).await?;
    Ok(true)
}

/// Every column but the four rates, and the four rates.
///
/// `ModelProfileChangeset` writes the whole row, so the fields this module does
/// not decide are written back exactly as they were read.
async fn write_prices(tx: &WriteTx, profile: &model_profile::Model, facts: &ModelFacts) -> Result<(), sea_orm::DbErr> {
    let changeset = model_profile::ModelProfileChangeset {
        name: profile.name.clone(),
        context_window: profile.context_window,
        compact_threshold: profile.compact_threshold,
        max_output_tokens: profile.max_output_tokens,
        input_price: facts.input_price.clone(),
        output_price: facts.output_price.clone(),
        cache_read_price: facts.cache_read_price.clone(),
        cache_write_price: facts.cache_write_price.clone(),
        pricing_tiers: profile.pricing_tiers.clone(),
        capability_overrides: profile.capability_overrides.clone(),
        updated_at: now_ms(),
    };
    ops::model_profile::update(tx, &profile.id, changeset).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use sea_orm::{EntityTrait, IntoActiveModel};

    use crate::db::sea::cap::Db;
    use crate::db::sea::sea_test_db;
    use crate::db::types::SqlBool;
    use crate::provider::models_dev::{ModelFacts, Snapshot};
    use crate::provider::registry::{ApiFormat, CredentialKind, ProviderType, TransportProfile};

    use super::*;

    fn facts(input: &str, output: &str) -> ModelFacts {
        ModelFacts {
            input_price: Some(input.parse().unwrap()),
            output_price: Some(output.parse().unwrap()),
            cache_read_price: None,
            cache_write_price: None,
        }
    }

    fn snapshot(input: &str, output: &str) -> Snapshot {
        let mut models = HashMap::new();
        models.insert("claude-sonnet-4".to_owned(), facts(input, output));
        let mut providers = HashMap::new();
        providers.insert("anthropic".to_owned(), models);
        Snapshot::from_facts(providers)
    }

    fn provider_row(id: &str) -> provider::Model {
        provider::Model {
            id: id.to_owned(),
            name: "Anthropic".to_owned(),
            provider_type: ProviderType::Anthropic,
            base_url: "https://api.anthropic.com/v1".to_owned(),
            is_enabled: SqlBool::TRUE,
            sort_order: 0,
            created_at: 0,
            updated_at: 0,
            api_format: ApiFormat::ChatCompletions,
            catalog_id: Some("anthropic".to_owned()),
            credential_kind: CredentialKind::ApiKey,
            transport_profile: TransportProfile::Standard,
            icon: None,
            codex_request_shape: SqlBool::FALSE,
            local_backend: None,
        }
    }

    fn profile_row(id: &str, input: Option<&str>, output: Option<&str>) -> model_profile::Model {
        model_profile::Model {
            id: id.to_owned(),
            name: format!("profile {id}"),
            context_window: 200_000,
            compact_threshold: 150_000,
            max_output_tokens: None,
            input_price: input.map(|value| value.parse().unwrap()),
            output_price: output.map(|value| value.parse().unwrap()),
            cache_read_price: None,
            cache_write_price: None,
            pricing_tiers: None,
            capability_overrides: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn config_row(id: &str, provider_id: &str, profile_id: &str) -> model_config::Model {
        model_config::Model {
            id: id.to_owned(),
            provider_id: provider_id.to_owned(),
            model_id: "claude-sonnet-4".to_owned(),
            profile_id: profile_id.to_owned(),
            overrides_pricing: SqlBool::FALSE,
            input_price: None,
            output_price: None,
            cache_read_price: None,
            cache_write_price: None,
            pricing_tiers: None,
            server_tools: None,
            server_tool_price: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    async fn seed(
        db: &Db,
        providers: Vec<provider::Model>,
        profiles: Vec<model_profile::Model>,
        configs: Vec<model_config::Model>,
    ) {
        db.write(async |tx| {
            for row in providers {
                provider::Entity::insert(row.into_active_model())
                    .exec(tx.conn()?)
                    .await?;
            }
            for row in profiles {
                model_profile::Entity::insert(row.into_active_model())
                    .exec(tx.conn()?)
                    .await?;
            }
            for row in configs {
                model_config::Entity::insert(row.into_active_model())
                    .exec(tx.conn()?)
                    .await?;
            }
            Ok::<(), sea_orm::DbErr>(())
        })
        .await
        .unwrap();
    }

    async fn read_profile(db: &Db, id: &str) -> model_profile::Model {
        ops::model_profile::get(db, id).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn a_profile_nobody_priced_takes_the_published_rates() {
        let db = sea_test_db().await;
        seed(
            &db,
            vec![provider_row("p")],
            vec![profile_row("prof", None, None)],
            vec![config_row("c", "p", "prof")],
        )
        .await;

        let report = db
            .write(async |tx| apply(tx, &snapshot("3", "15")).await)
            .await
            .unwrap();
        assert_eq!(
            report,
            SyncReport {
                filled: 1,
                ..Default::default()
            }
        );

        let profile = read_profile(&db, "prof").await;
        assert_eq!(profile.input_price, Some("3".parse().unwrap()));
        assert_eq!(profile.output_price, Some("15".parse().unwrap()));
    }

    #[tokio::test]
    async fn a_profile_somebody_priced_keeps_what_it_says() {
        let db = sea_test_db().await;
        seed(
            &db,
            vec![provider_row("p")],
            vec![profile_row("prof", Some("1"), Some("2"))],
            vec![config_row("c", "p", "prof")],
        )
        .await;

        let report = db
            .write(async |tx| apply(tx, &snapshot("3", "15")).await)
            .await
            .unwrap();
        assert_eq!(
            report,
            SyncReport {
                kept: 1,
                ..Default::default()
            }
        );

        let profile = read_profile(&db, "prof").await;
        assert_eq!(profile.input_price, Some("1".parse().unwrap()));
        assert_eq!(profile.output_price, Some("2".parse().unwrap()));
    }

    #[tokio::test]
    async fn two_providers_that_disagree_leave_the_profile_alone() {
        let db = sea_test_db().await;
        // The same model reached through two vendors that price it differently:
        // choosing either would bill against a number nobody set.
        let mut second = provider_row("p2");
        second.catalog_id = Some("google".to_owned());
        seed(
            &db,
            vec![provider_row("p"), second],
            vec![profile_row("prof", None, None)],
            vec![config_row("c1", "p", "prof"), config_row("c2", "p2", "prof")],
        )
        .await;

        let mut providers = HashMap::new();
        let mut anthropic = HashMap::new();
        anthropic.insert("claude-sonnet-4".to_owned(), facts("3", "15"));
        providers.insert("anthropic".to_owned(), anthropic);
        let mut google = HashMap::new();
        google.insert("claude-sonnet-4".to_owned(), facts("4", "20"));
        providers.insert("google".to_owned(), google);
        let snapshot = Snapshot::from_facts(providers);

        let report = db.write(async |tx| apply(tx, &snapshot).await).await.unwrap();
        assert_eq!(
            report,
            SyncReport {
                ambiguous: 1,
                ..Default::default()
            }
        );
        assert_eq!(read_profile(&db, "prof").await.input_price, None);
    }

    #[tokio::test]
    async fn a_saved_model_is_filled_immediately() {
        let db = sea_test_db().await;
        seed(
            &db,
            vec![provider_row("p")],
            vec![profile_row("prof", None, None)],
            vec![config_row("c", "p", "prof")],
        )
        .await;

        let filled = db
            .write(async |tx| fill_one(tx, &snapshot("3", "15"), "p", "claude-sonnet-4").await)
            .await
            .unwrap();
        assert!(filled);
        assert_eq!(read_profile(&db, "prof").await.input_price, Some("3".parse().unwrap()));

        // A profile the person then priced is not touched again.
        db.write(async |tx| {
            ops::model_profile::update(
                tx,
                "prof",
                model_profile::ModelProfileChangeset {
                    name: "profile prof".into(),
                    context_window: 200_000,
                    compact_threshold: 150_000,
                    max_output_tokens: None,
                    input_price: Some("9".parse().unwrap()),
                    output_price: Some("9".parse().unwrap()),
                    cache_read_price: None,
                    cache_write_price: None,
                    pricing_tiers: None,
                    capability_overrides: None,
                    updated_at: 1,
                },
            )
            .await
        })
        .await
        .unwrap();
        let filled = db
            .write(async |tx| fill_one(tx, &snapshot("3", "15"), "p", "claude-sonnet-4").await)
            .await
            .unwrap();
        assert!(!filled);
        assert_eq!(read_profile(&db, "prof").await.input_price, Some("9".parse().unwrap()));
    }

    #[tokio::test]
    async fn a_relay_the_catalog_cannot_identify_is_not_priced() {
        let db = sea_test_db().await;
        let mut relay = provider_row("relay");
        relay.catalog_id = None;
        relay.base_url = "https://relay.example.com/v1".to_owned();
        seed(
            &db,
            vec![relay],
            vec![profile_row("prof", None, None)],
            vec![config_row("c", "relay", "prof")],
        )
        .await;

        let report = db
            .write(async |tx| apply(tx, &snapshot("3", "15")).await)
            .await
            .unwrap();
        assert_eq!(
            report,
            SyncReport {
                unmatched: 1,
                ..Default::default()
            }
        );
        assert_eq!(read_profile(&db, "prof").await.input_price, None);
    }
}

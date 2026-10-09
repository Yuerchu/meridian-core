//! Bringing the database to what the file says.
//!
//! Applied on every start, and idempotent: the ids in the file are stable, so
//! running twice is the same as running once. That is what makes a recreated
//! container converge instead of accumulating duplicates.
//!
//! **What the file does not name is switched off, not deleted.** Removing a
//! webhook from the file has to stop it firing — otherwise the file lies about
//! what is configured, and "I turned it off and it kept going" is the failure
//! this whole feature exists to prevent. Deleting instead would be the other
//! way to achieve that, and it is worse: a mistyped id would quietly destroy an
//! endpoint's delivery history, while a disabled row can be turned back on.

use meridian_core::db::entity::notification_webhook::{
    self, BodyTemplate, NotificationEvents, NotificationWebhookChangeset,
};
use meridian_core::db::entity::provider::{self as provider_entity, ProviderChangeset};
use meridian_core::db::sea::DbErr;
use meridian_core::db::sea::cap::Db;
use meridian_core::db::sea::ops::{
    notification as notification_ops, preference as preference_ops, provider as provider_ops,
};
use meridian_core::db::types::SqlBool;
use meridian_core::provider::registry::{ApiFormat, CredentialKind, ProviderType, TransportProfile};
use meridian_core::secrets::{SecretName, SecretScope, SecretsManager};
use meridian_core::util::now_ms;

use crate::config::DaemonConfig;

/// Says this data directory is the daemon's own.
///
/// Written on first use of an empty directory. Its absence beside existing rows
/// is what stops `--config` from being applied to a desktop install: the file
/// is a *desired state*, so applying it there would switch off every provider
/// and endpoint the desktop had configured, which is a data loss nobody asked
/// for and nothing would undo.
const OWNERSHIP_KEY: &str = "meridiand.owns_data_dir";

#[derive(Debug)]
pub struct ApplyReport {
    pub providers_written: usize,
    pub providers_disabled: usize,
    pub webhooks_written: usize,
    pub webhooks_disabled: usize,
}

/// Read a secret out of the environment, by the name the file gave.
fn from_env(kind: &str, owner: &str, var: &str) -> Result<String, String> {
    match std::env::var(var) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        // Both the unset and the empty case, deliberately together: an empty
        // variable is the usual shape of a secret that failed to be injected,
        // and treating it as "no secret" would send unsigned requests to a
        // robot that requires a signature and report them as a delivery
        // problem.
        _ => Err(format!(
            "{kind} `{owner}` needs ${var}, which is unset or empty in this process"
        )),
    }
}

/// Refuse to converge somebody else's data directory.
///
/// The check and the claim are one IMMEDIATE write: two daemons starting on
/// one empty directory cannot both find it empty and both take it.
async fn claim_data_dir(sea: &Db) -> Result<(), String> {
    sea.write(async |tx| {
        let claimed = preference_ops::get_preference(tx, OWNERSHIP_KEY).await?;
        if claimed.is_some() {
            return Ok(Ok(()));
        }
        // An unclaimed directory is only safe to take over if there is nothing
        // in it to lose. `providers` is the right thing to count: it is what
        // this daemon manages, it exists in every install, and a desktop
        // install that has ever been configured has at least one.
        let providers = provider_ops::list_providers(tx).await?.len();
        if providers > 0 {
            return Ok(Err(format!(
                "this data directory already holds {providers} provider(s) and is not marked as the daemon's. \
                 Applying a configuration here would switch off everything the file does not name. \
                 Point --data-dir somewhere of its own, or set `{OWNERSHIP_KEY}` if this really is the daemon's directory."
            )));
        }
        preference_ops::set_preference(tx, OWNERSHIP_KEY, "true", now_ms()).await?;
        Ok::<_, DbErr>(Ok(()))
    })
    .await
    .map_err(|error| format!("could not claim the data directory: {error}"))?
}

pub async fn apply(sea: &Db, secrets: &SecretsManager, config: &DaemonConfig) -> Result<ApplyReport, String> {
    claim_data_dir(sea).await?;

    // Every secret is read before anything is written. A file naming a variable
    // that is not set should leave the previous configuration running rather
    // than a half-applied one — a half-applied state here is a provider with no
    // key, which reports as an upstream failure and reads as an outage.
    let mut provider_keys = Vec::with_capacity(config.provider.len());
    for provider in &config.provider {
        provider_keys.push(from_env("provider", &provider.id, &provider.api_key_env)?);
    }
    let mut webhook_secrets = Vec::with_capacity(config.webhook.len());
    for webhook in &config.webhook {
        webhook_secrets.push(match &webhook.secret_env {
            None => None,
            Some(var) => Some(from_env("webhook", &webhook.id, var)?),
        });
    }

    let now = now_ms();
    let mut report = ApplyReport {
        providers_written: 0,
        providers_disabled: 0,
        webhooks_written: 0,
        webhooks_disabled: 0,
    };

    for (provider, api_key) in config.provider.iter().zip(&provider_keys) {
        // Two sources and no third: what the file says, then what the address
        // says. **Never what the row already holds.**
        //
        // The row looks like a reasonable fallback and is a hole. This file is
        // a desired state applied on every start — every other column is
        // restated unconditionally — so a `vendor` carried over from a previous
        // apply outranks the address the operator just changed. Point a
        // Moonshot entry at a relay and drop its `vendor`, and the stale
        // `moonshot` survives: the watcher asks a relay for
        // `/v1/users/me/balance` with that provider's key, and `--status`
        // reports `balance read as moonshot` — the one diagnostic meant to
        // catch this says the wrong thing with confidence. Changing one vendor
        // to another is the same failure with a different endpoint.
        //
        // Dropping it costs nothing: a previous apply resolved by this same
        // rule, so re-deriving reaches the same answer and the pass stays
        // idempotent. It also makes the code agree with what `ProviderEntry`
        // and the README have both said all along — omitted means derived.
        let catalog_id = provider.vendor.clone().or_else(|| {
            meridian_core::provider::catalog::identify(&provider.provider_type, &provider.base_url).map(str::to_string)
        });
        let identity = meridian_core::provider::balance::ProviderIdentity::new(
            catalog_id.as_deref(),
            &provider.provider_type,
            &provider.base_url,
        );
        if !meridian_core::provider::balance::supports_balance(identity) {
            // Not a refusal: a provider whose upstream publishes no balance is
            // a legitimate row to have. But it is worth saying, because the
            // reason nothing is ever reported about it is not otherwise visible
            // — and the commonest cause is now a missing `vendor`, since a
            // relay address identifies nobody and must not be probed.
            tracing::warn!(
                provider = %provider.id,
                provider_type = %provider.provider_type,
                vendor = catalog_id.as_deref().unwrap_or("<unidentified>"),
                "no balance can be read for this provider; the daemon can watch it for nothing"
            );
        }
        let provider_type = ProviderType::parse(&provider.provider_type)
            .map_err(|error| format!("provider `{}`: {error}", provider.id))?;
        let enabled = SqlBool::from(provider.enabled);
        // The existence check and the write are one transaction, so a second
        // apply racing this one cannot both see "absent" and both insert.
        sea.write(async |tx| match provider_ops::get_provider(tx, &provider.id).await? {
            Some(_) => provider_ops::update_provider(
                tx,
                &provider.id,
                ProviderChangeset {
                    name: Some(provider.name.clone()),
                    provider_type: Some(provider_type),
                    base_url: Some(provider.base_url.clone()),
                    // Written every time, cleared included. Adding `vendor` to
                    // an entry has to take effect on the next apply, and so
                    // does moving its address somewhere the catalog cannot
                    // name — leaving the old identity in place there is how a
                    // relay comes to be asked for a vendor's balance.
                    catalog_id: Some(catalog_id.clone()),
                    is_enabled: Some(enabled),
                    updated_at: Some(now),
                    ..Default::default()
                },
            )
            .await
            .map(|_| ()),
            None => provider_ops::create_provider(
                tx,
                provider_entity::Model {
                    id: provider.id.clone(),
                    name: provider.name.clone(),
                    provider_type,
                    base_url: provider.base_url.clone(),
                    is_enabled: enabled,
                    sort_order: 0,
                    created_at: now,
                    updated_at: now,
                    api_format: ApiFormat::ChatCompletions,
                    catalog_id: catalog_id.clone(),
                    credential_kind: CredentialKind::ApiKey,
                    transport_profile: TransportProfile::Standard,
                    // A declarative provider names no logo; the mark
                    // follows whichever vendor the catalog identified.
                    icon: None,
                    // Nor a request shape: `chat_completions` above has none
                    // to follow.
                    codex_request_shape: SqlBool::FALSE,
                },
            )
            .await
            .map(|_| ()),
        })
        .await
        .map_err(|error| format!("could not write provider `{}`: {error}", provider.id))?;

        let name = SecretName::new(&meridian_core::agent::provider_secret_name(&provider.id))
            .map_err(|error| format!("provider `{}`: {error}", provider.id))?;
        secrets
            .set(&SecretScope::Global, &name, api_key)
            .map_err(|error| format!("could not store the key for provider `{}`: {error}", provider.id))?;
        report.providers_written += 1;
    }

    // What the file no longer names, switched off in one write: the list and
    // the updates under one lock.
    let named: Vec<&str> = config.provider.iter().map(|provider| provider.id.as_str()).collect();
    let disabled = sea
        .write(async |tx| {
            let mut disabled = Vec::new();
            for row in provider_ops::list_providers(tx).await? {
                if named.contains(&row.id.as_str()) || !row.is_enabled.get() {
                    continue;
                }
                provider_ops::update_provider(
                    tx,
                    &row.id,
                    ProviderChangeset {
                        is_enabled: Some(SqlBool::FALSE),
                        updated_at: Some(now),
                        ..Default::default()
                    },
                )
                .await?;
                disabled.push(row.id);
            }
            Ok::<_, DbErr>(disabled)
        })
        .await
        .map_err(|error| format!("could not disable providers: {error}"))?;
    for id in &disabled {
        tracing::info!(provider = %id, "disabled: the configuration no longer names it");
    }
    report.providers_disabled += disabled.len();

    for (webhook, secret) in config.webhook.iter().zip(&webhook_secrets) {
        // Already validated at config parse; typed here, encoded by the row.
        let template = webhook.body_template()?.map(BodyTemplate::from);
        let events = NotificationEvents::from(webhook.events.clone());
        let enabled = SqlBool::from(webhook.enabled);
        // The existence check and the write are one transaction, so a second
        // apply racing this one cannot both see "absent" and both insert.
        sea.write(async |tx| match notification_ops::get_webhook(tx, &webhook.id).await? {
            Some(_) => notification_ops::update_webhook(
                tx,
                &webhook.id,
                NotificationWebhookChangeset {
                    name: Some(webhook.name.clone()),
                    url: Some(webhook.url.clone()),
                    format: Some(webhook.format),
                    events: Some(events),
                    is_enabled: Some(enabled),
                    body_template: Some(template),
                    updated_at: Some(now),
                },
            )
            .await
            .map(|_| ()),
            None => notification_ops::create_webhook(
                tx,
                notification_webhook::Model {
                    id: webhook.id.clone(),
                    name: webhook.name.clone(),
                    url: webhook.url.clone(),
                    format: webhook.format,
                    events,
                    is_enabled: enabled,
                    body_template: template,
                    last_attempt_at: None,
                    last_success_at: None,
                    last_error: None,
                    consecutive_failures: 0,
                    created_at: now,
                    updated_at: now,
                },
            )
            .await
            .map(|_| ()),
        })
        .await
        .map_err(|error| format!("could not write webhook `{}`: {error}", webhook.id))?;
        meridian_core::notify::write_webhook_secret(secrets, &webhook.id, secret.as_deref())?;
        report.webhooks_written += 1;
    }

    let named: Vec<&str> = config.webhook.iter().map(|webhook| webhook.id.as_str()).collect();
    // pool-read-before-write: disabling what the file no longer names. A webhook
    // added after this read is not one the file meant to disable, and each
    // update is idempotent, so the gap changes nothing a later apply would undo.
    for row in notification_ops::list_webhooks(sea)
        .await
        .map_err(|error| error.to_string())?
    {
        if named.contains(&row.id.as_str()) || !row.is_enabled.get() {
            continue;
        }
        sea.write(async |tx| {
            notification_ops::update_webhook(
                tx,
                &row.id,
                NotificationWebhookChangeset {
                    is_enabled: Some(SqlBool::FALSE),
                    body_template: None,
                    updated_at: Some(now),
                    ..Default::default()
                },
            )
            .await
        })
        .await
        .map_err(|error| format!("could not disable webhook `{}`: {error}", row.id))?;
        tracing::info!(webhook = %row.id, "disabled: the configuration no longer names it");
        report.webhooks_disabled += 1;
    }

    meridian_core::notify::save_config(sea, &config.notify_config()?).await?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_core::db::sea::sea_test_db;

    async fn provider_count(sea: &Db) -> usize {
        provider_ops::list_providers(sea).await.unwrap().len()
    }

    fn secrets(dir: &std::path::Path) -> SecretsManager {
        use meridian_core::keyring::SuppliedPassphraseStore;
        use std::sync::Arc;
        SecretsManager::new_with_keyring_store(
            dir.to_path_buf(),
            Arc::new(SuppliedPassphraseStore::new("a passphrase long enough", "test").unwrap()),
        )
    }

    const ONE_OF_EACH: &str = r#"
        [notify]
        enabled = true
        balance_threshold = "5"

        [[provider]]
        id = "ds"
        name = "DeepSeek"
        type = "deepseek"
        base_url = "https://api.deepseek.com"
        api_key_env = "TEST_DS_KEY"

        [[webhook]]
        id = "ops"
        name = "ops"
        url = "https://example.invalid/hook"
        format = "generic"
        events = ["balance_low"]
    "#;

    /// Applying twice must leave exactly what applying once left. A container
    /// that is recreated runs this on every start.
    #[tokio::test]
    async fn applying_twice_converges_rather_than_accumulating() {
        // SAFETY: single-threaded test; the variable is read by `apply` below.
        unsafe { std::env::set_var("TEST_DS_KEY", "sk-test") };
        let dir = tempfile::tempdir().unwrap();
        let sea = sea_test_db().await;
        let secrets = secrets(dir.path());
        let config = DaemonConfig::parse(ONE_OF_EACH).unwrap();

        let first = apply(&sea, &secrets, &config).await.unwrap();
        assert_eq!((first.providers_written, first.webhooks_written), (1, 1));
        let second = apply(&sea, &secrets, &config).await.unwrap();
        assert_eq!((second.providers_written, second.webhooks_written), (1, 1));

        assert_eq!(provider_count(&sea).await, 1);
        assert_eq!(notification_ops::list_webhooks(&sea).await.unwrap().len(), 1);
        assert_eq!(
            meridian_core::agent::get_provider_api_key(&secrets, "ds").as_deref(),
            Some("sk-test")
        );
    }

    /// Removing an endpoint from the file has to stop it firing. Left enabled,
    /// the file would be lying about what is configured.
    #[tokio::test]
    async fn what_the_file_stops_naming_is_switched_off_but_kept() {
        unsafe { std::env::set_var("TEST_DS_KEY", "sk-test") };
        let dir = tempfile::tempdir().unwrap();
        let sea = sea_test_db().await;
        let secrets = secrets(dir.path());

        apply(&sea, &secrets, &DaemonConfig::parse(ONE_OF_EACH).unwrap())
            .await
            .unwrap();
        let narrowed = DaemonConfig::parse(
            r#"
            [notify]
            enabled = false

            [[provider]]
            id = "ds"
            name = "DeepSeek"
            type = "deepseek"
            base_url = "https://api.deepseek.com"
            api_key_env = "TEST_DS_KEY"
        "#,
        )
        .unwrap();
        let report = apply(&sea, &secrets, &narrowed).await.unwrap();
        assert_eq!(report.webhooks_disabled, 1);

        let rows = notification_ops::list_webhooks(&sea).await.unwrap();
        assert_eq!(rows.len(), 1, "disabled, not deleted — the history is worth keeping");
        assert_eq!(rows[0].is_enabled, SqlBool::FALSE);
        // And a second pass does not count it again.
        assert_eq!(apply(&sea, &secrets, &narrowed).await.unwrap().webhooks_disabled, 0);
    }

    /// Moving an entry's address re-derives its vendor, and a vendor carried
    /// over from the previous apply must not outrank it.
    ///
    /// The file is a desired state, so a stale `catalog_id` is not a default —
    /// it is an identity the operator has just stopped asserting. Kept, it
    /// makes the watcher ask a *relay* for `/v1/users/me/balance` with that
    /// provider's key, while `--status` reports `balance read as moonshot`:
    /// the one diagnostic meant to catch this saying the wrong thing with
    /// confidence.
    ///
    /// Mutation check: putting `.or_else(|| existing…catalog_id.clone())` back
    /// between the two sources turns both halves of this red.
    #[tokio::test]
    async fn a_moved_address_re_derives_the_vendor_instead_of_keeping_the_old_one() {
        use meridian_core::provider::balance::{ProviderIdentity, balance_vendor};

        unsafe { std::env::set_var("TEST_DS_KEY", "sk-test") };
        let dir = tempfile::tempdir().unwrap();
        let sea = sea_test_db().await;
        let secrets = secrets(dir.path());

        let entry = |vendor: &str, url: &str| {
            DaemonConfig::parse(&format!(
                "[notify]\nenabled = false\n\n[[provider]]\nid = \"p1\"\nname = \"Kimi\"\n\
                 type = \"openai\"\n{vendor}base_url = \"{url}\"\napi_key_env = \"TEST_DS_KEY\"\n"
            ))
            .unwrap()
        };
        let vendor_of = async |sea: &Db| {
            let row = provider_ops::get_provider(sea, "p1").await.unwrap().unwrap();
            balance_vendor(ProviderIdentity::new(
                row.catalog_id.as_deref(),
                row.provider_type.as_str(),
                &row.base_url,
            ))
            .map(|vendor| vendor.catalog_id().to_string())
        };

        apply(
            &sea,
            &secrets,
            &entry("vendor = \"moonshot\"\n", "https://api.moonshot.cn/v1"),
        )
        .await
        .unwrap();
        assert_eq!(vendor_of(&sea).await.as_deref(), Some("moonshot"));

        // Pointed at a relay with no `vendor`: the address names nobody, so the
        // row must name nobody either.
        apply(&sea, &secrets, &entry("", "https://codex-api.example/v1"))
            .await
            .unwrap();
        assert_eq!(
            vendor_of(&sea).await,
            None,
            "a relay inherited the previous entry's identity"
        );

        // And moved to a different vendor's own address, it becomes that one
        // rather than staying unresolved or reverting to the first.
        apply(&sea, &secrets, &entry("", "https://api.siliconflow.cn/v1"))
            .await
            .unwrap();
        assert_eq!(vendor_of(&sea).await.as_deref(), Some("siliconflow"));
    }

    /// A half-applied configuration leaves a provider with no key, which
    /// reports as an upstream failure and reads as an outage.
    #[tokio::test]
    async fn a_missing_environment_variable_writes_nothing_at_all() {
        unsafe { std::env::remove_var("TEST_ABSENT_KEY") };
        let dir = tempfile::tempdir().unwrap();
        let sea = sea_test_db().await;
        let secrets = secrets(dir.path());
        let config = DaemonConfig::parse(
            r#"
            [[provider]]
            id = "ds"
            name = "DeepSeek"
            type = "deepseek"
            base_url = "https://api.deepseek.com"
            api_key_env = "TEST_ABSENT_KEY"
        "#,
        )
        .unwrap();

        let error = apply(&sea, &secrets, &config).await.unwrap_err();
        assert!(error.contains("TEST_ABSENT_KEY"), "{error}");
        assert_eq!(
            provider_count(&sea).await,
            0,
            "nothing may be written before every secret is in hand"
        );
    }

    /// An empty variable is the usual shape of a secret that failed to inject.
    #[test]
    fn an_empty_environment_variable_is_not_an_absent_secret() {
        unsafe { std::env::set_var("TEST_EMPTY_KEY", "   ") };
        assert!(from_env("provider", "ds", "TEST_EMPTY_KEY").is_err());
    }

    /// Pointing the daemon at a desktop install would switch off everything the
    /// file does not name.
    #[tokio::test]
    async fn a_populated_unclaimed_data_directory_is_refused() {
        let sea = sea_test_db().await;
        sea.write(async |tx| {
            provider_ops::create_provider(
                tx,
                provider_entity::Model {
                    id: "desktop-one".into(),
                    name: "Configured on the desktop".into(),
                    provider_type: ProviderType::Openai,
                    base_url: "https://api.openai.com/v1".into(),
                    is_enabled: SqlBool::TRUE,
                    sort_order: 0,
                    created_at: 1,
                    updated_at: 1,
                    api_format: ApiFormat::ChatCompletions,
                    catalog_id: None,
                    credential_kind: CredentialKind::ApiKey,
                    transport_profile: TransportProfile::Standard,
                    icon: None,
                    codex_request_shape: SqlBool::FALSE,
                },
            )
            .await
        })
        .await
        .unwrap();
        let error = claim_data_dir(&sea).await.unwrap_err();
        assert!(error.contains("not marked as the daemon's"), "{error}");

        // An empty one is claimed, and stays claimed once it has rows.
        let fresh = sea_test_db().await;
        claim_data_dir(&fresh).await.unwrap();
        assert_eq!(
            preference_ops::get_preference(&fresh, OWNERSHIP_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some("true")
        );
        claim_data_dir(&fresh).await.unwrap();
    }
}

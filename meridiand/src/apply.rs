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

use meridian_core::db::DbPool;
use meridian_core::db::models::notification::{NotificationWebhookChangeset, NotificationWebhookInsert, encode_events};
use meridian_core::db::models::provider::{ProviderChangeset, ProviderInsert};
use meridian_core::db::ops;
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
fn claim_data_dir(pool: &DbPool) -> Result<(), String> {
    let mut conn = pool.get().map_err(|error| format!("db connection: {error}"))?;
    let claimed = ops::preference::get_preference(&mut conn, OWNERSHIP_KEY)
        .map_err(|error| format!("could not read {OWNERSHIP_KEY}: {error}"))?;
    if claimed.is_some() {
        return Ok(());
    }

    // An unclaimed directory is only safe to take over if there is nothing in
    // it to lose. `providers` is the right thing to count: it is what this
    // daemon manages, it exists in every install, and a desktop install that
    // has ever been configured has at least one.
    let providers =
        ops::provider::count_providers(&mut conn).map_err(|error| format!("could not count providers: {error}"))?;
    if providers > 0 {
        return Err(format!(
            "this data directory already holds {providers} provider(s) and is not marked as the daemon's. \
             Applying a configuration here would switch off everything the file does not name. \
             Point --data-dir somewhere of its own, or set `{OWNERSHIP_KEY}` if this really is the daemon's directory."
        ));
    }
    ops::preference::set_preference(&mut conn, OWNERSHIP_KEY, "true", now_ms())
        .map_err(|error| format!("could not claim the data directory: {error}"))?;
    Ok(())
}

pub fn apply(pool: &DbPool, secrets: &SecretsManager, config: &DaemonConfig) -> Result<ApplyReport, String> {
    claim_data_dir(pool)?;

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

    let mut conn = pool.get().map_err(|error| format!("db connection: {error}"))?;
    let now = now_ms();
    let mut report = ApplyReport {
        providers_written: 0,
        providers_disabled: 0,
        webhooks_written: 0,
        webhooks_disabled: 0,
    };

    for (provider, api_key) in config.provider.iter().zip(&provider_keys) {
        if !meridian_core::provider::balance::supports_balance(&provider.provider_type) {
            // Not a refusal: a provider whose upstream publishes no balance is
            // a legitimate row to have. But it is worth saying, because the
            // reason nothing is ever reported about it is not otherwise visible.
            tracing::warn!(
                provider = %provider.id,
                provider_type = %provider.provider_type,
                "this upstream publishes no balance; the daemon can watch it for nothing"
            );
        }
        let existing = ops::provider::get_provider(&mut conn, &provider.id).ok();
        let enabled = i32::from(provider.enabled);
        if existing.is_some() {
            ops::provider::update_provider(
                &mut conn,
                &provider.id,
                &ProviderChangeset {
                    name: Some(provider.name.clone()),
                    provider_type: Some(provider.provider_type.clone()),
                    base_url: Some(provider.base_url.clone()),
                    is_enabled: Some(enabled),
                    updated_at: Some(now),
                    ..Default::default()
                },
            )
            .map_err(|error| format!("could not update provider `{}`: {error}", provider.id))?;
        } else {
            ops::provider::create_provider(
                &mut conn,
                &ProviderInsert {
                    id: &provider.id,
                    name: &provider.name,
                    provider_type: &provider.provider_type,
                    base_url: &provider.base_url,
                    is_enabled: enabled,
                    sort_order: 0,
                    created_at: now,
                    updated_at: now,
                    api_format: "chat_completions",
                    catalog_id: meridian_core::provider::catalog::identify(&provider.provider_type, &provider.base_url),
                    credential_kind: "api_key",
                    transport_profile: "standard",
                },
            )
            .map_err(|error| format!("could not create provider `{}`: {error}", provider.id))?;
        }

        let name = SecretName::new(&meridian_core::agent::provider_secret_name(&provider.id))
            .map_err(|error| format!("provider `{}`: {error}", provider.id))?;
        secrets
            .set(&SecretScope::Global, &name, api_key)
            .map_err(|error| format!("could not store the key for provider `{}`: {error}", provider.id))?;
        report.providers_written += 1;
    }

    let named: Vec<&str> = config.provider.iter().map(|provider| provider.id.as_str()).collect();
    for row in ops::provider::list_providers(&mut conn).map_err(|error| error.to_string())? {
        if named.contains(&row.id.as_str()) || row.is_enabled == 0 {
            continue;
        }
        ops::provider::update_provider(
            &mut conn,
            &row.id,
            &ProviderChangeset {
                is_enabled: Some(0),
                updated_at: Some(now),
                ..Default::default()
            },
        )
        .map_err(|error| format!("could not disable provider `{}`: {error}", row.id))?;
        tracing::info!(provider = %row.id, "disabled: the configuration no longer names it");
        report.providers_disabled += 1;
    }

    for (webhook, secret) in config.webhook.iter().zip(&webhook_secrets) {
        let events = encode_events(&webhook.events)?;
        let enabled = i32::from(webhook.enabled);
        let exists = ops::notification::get_webhook(&mut conn, &webhook.id).is_ok();
        if exists {
            ops::notification::update_webhook(
                &mut conn,
                &webhook.id,
                &NotificationWebhookChangeset {
                    name: Some(webhook.name.clone()),
                    url: Some(webhook.url.clone()),
                    format: Some(webhook.format.as_str().to_string()),
                    events: Some(events),
                    is_enabled: Some(enabled),
                    updated_at: Some(now),
                },
            )
            .map_err(|error| format!("could not update webhook `{}`: {error}", webhook.id))?;
        } else {
            ops::notification::create_webhook(
                &mut conn,
                &NotificationWebhookInsert {
                    id: &webhook.id,
                    name: &webhook.name,
                    url: &webhook.url,
                    format: webhook.format.as_str(),
                    events: &events,
                    is_enabled: enabled,
                    created_at: now,
                    updated_at: now,
                },
            )
            .map_err(|error| format!("could not create webhook `{}`: {error}", webhook.id))?;
        }
        meridian_core::notify::write_webhook_secret(secrets, &webhook.id, secret.as_deref())?;
        report.webhooks_written += 1;
    }

    let named: Vec<&str> = config.webhook.iter().map(|webhook| webhook.id.as_str()).collect();
    for row in ops::notification::list_webhooks(&mut conn).map_err(|error| error.to_string())? {
        if named.contains(&row.id.as_str()) || row.is_enabled == 0 {
            continue;
        }
        ops::notification::update_webhook(
            &mut conn,
            &row.id,
            &NotificationWebhookChangeset {
                is_enabled: Some(0),
                updated_at: Some(now),
                ..Default::default()
            },
        )
        .map_err(|error| format!("could not disable webhook `{}`: {error}", row.id))?;
        tracing::info!(webhook = %row.id, "disabled: the configuration no longer names it");
        report.webhooks_disabled += 1;
    }

    drop(conn);
    meridian_core::notify::save_config(pool, &config.notify_config()?)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_core::db::test_db;

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
    #[test]
    fn applying_twice_converges_rather_than_accumulating() {
        // SAFETY: single-threaded test; the variable is read by `apply` below.
        unsafe { std::env::set_var("TEST_DS_KEY", "sk-test") };
        let dir = tempfile::tempdir().unwrap();
        let pool = test_db();
        let secrets = secrets(dir.path());
        let config = DaemonConfig::parse(ONE_OF_EACH).unwrap();

        let first = apply(&pool, &secrets, &config).unwrap();
        assert_eq!((first.providers_written, first.webhooks_written), (1, 1));
        let second = apply(&pool, &secrets, &config).unwrap();
        assert_eq!((second.providers_written, second.webhooks_written), (1, 1));

        let mut conn = pool.get().unwrap();
        assert_eq!(ops::provider::count_providers(&mut conn).unwrap(), 1);
        assert_eq!(ops::notification::list_webhooks(&mut conn).unwrap().len(), 1);
        assert_eq!(
            meridian_core::agent::get_provider_api_key(&secrets, "ds").as_deref(),
            Some("sk-test")
        );
    }

    /// Removing an endpoint from the file has to stop it firing. Left enabled,
    /// the file would be lying about what is configured.
    #[test]
    fn what_the_file_stops_naming_is_switched_off_but_kept() {
        unsafe { std::env::set_var("TEST_DS_KEY", "sk-test") };
        let dir = tempfile::tempdir().unwrap();
        let pool = test_db();
        let secrets = secrets(dir.path());

        apply(&pool, &secrets, &DaemonConfig::parse(ONE_OF_EACH).unwrap()).unwrap();
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
        let report = apply(&pool, &secrets, &narrowed).unwrap();
        assert_eq!(report.webhooks_disabled, 1);

        // Scoped, because the test pool hands out one connection and `apply`
        // below needs it.
        {
            let mut conn = pool.get().unwrap();
            let rows = ops::notification::list_webhooks(&mut conn).unwrap();
            assert_eq!(rows.len(), 1, "disabled, not deleted — the history is worth keeping");
            assert_eq!(rows[0].is_enabled, 0);
        }
        // And a second pass does not count it again.
        assert_eq!(apply(&pool, &secrets, &narrowed).unwrap().webhooks_disabled, 0);
    }

    /// A half-applied configuration leaves a provider with no key, which
    /// reports as an upstream failure and reads as an outage.
    #[test]
    fn a_missing_environment_variable_writes_nothing_at_all() {
        unsafe { std::env::remove_var("TEST_ABSENT_KEY") };
        let dir = tempfile::tempdir().unwrap();
        let pool = test_db();
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

        let error = apply(&pool, &secrets, &config).unwrap_err();
        assert!(error.contains("TEST_ABSENT_KEY"), "{error}");
        let mut conn = pool.get().unwrap();
        assert_eq!(
            ops::provider::count_providers(&mut conn).unwrap(),
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
    #[test]
    fn a_populated_unclaimed_data_directory_is_refused() {
        let pool = test_db();
        {
            let mut conn = pool.get().unwrap();
            ops::provider::create_provider(
                &mut conn,
                &ProviderInsert {
                    id: "desktop-one",
                    name: "Configured on the desktop",
                    provider_type: "openai",
                    base_url: "https://api.openai.com/v1",
                    is_enabled: 1,
                    sort_order: 0,
                    created_at: 1,
                    updated_at: 1,
                    api_format: "chat_completions",
                    catalog_id: None,
                    credential_kind: "api_key",
                    transport_profile: "standard",
                },
            )
            .unwrap();
        }
        let error = claim_data_dir(&pool).unwrap_err();
        assert!(error.contains("not marked as the daemon's"), "{error}");

        // An empty one is claimed, and stays claimed once it has rows.
        let fresh = test_db();
        claim_data_dir(&fresh).unwrap();
        {
            let mut conn = fresh.get().unwrap();
            assert_eq!(
                ops::preference::get_preference(&mut conn, OWNERSHIP_KEY)
                    .unwrap()
                    .as_deref(),
                Some("true")
            );
        }
        claim_data_dir(&fresh).unwrap();
    }
}

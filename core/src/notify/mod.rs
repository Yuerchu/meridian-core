//! Watching the money, and telling somebody when it moves.
//!
//! Two sources — what an upstream says is left on the account, and what this
//! install's own ledger says it has been spending — and one way out, which is a
//! list of webhook endpoints plus whatever else has registered a sink.
//!
//! Not `#[cfg]`-gated to the desktop. Everything here is an HTTP *client* and a
//! pair of database reads; the modules that are desktop-only are the ones that
//! listen on a socket or spawn a child process.
//!
//! ## The rule this is built around
//!
//! Raising an alert and reporting one are two separate writes, in that order,
//! and the second happens only after a delivery is accepted. The watcher this
//! replaces kept its "already announced" set in memory and had to special-case
//! "nobody is connected" by hand, because marking an alert as told when nothing
//! was sent means it is never sent again until the condition clears and
//! returns. Here that is structural: `record_notified` is only reachable past a
//! successful delivery.

pub mod alert;
pub mod balance;
pub mod sink;
pub mod usage;
pub mod webhook;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::{Mutex, watch};

use crate::db::DbPool;
use crate::db::models::notification::NotificationEventKind;
use crate::decimal::Decimal;
use crate::secrets::{SecretName, SecretScope, SecretsManager};
use crate::services::Services;
use crate::util::now_ms;

pub use alert::{Alert, AlertDetail, BalanceAlert, TestAlert, UsageAlert, UsageSlice};
pub use sink::{AlertSink, AlertSinkId, AlertSinks};
pub use webhook::{DeliveryReport, webhook_secret_name};

/// Long enough that a restart loop does not become a request loop, short enough
/// that "I just set this up" gets an answer while the user is still looking at
/// the settings page.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyConfig {
    pub enabled: bool,
    /// Absent means balance alerting is off. `Some(0)` is a real and different
    /// setting: keep asking, but say something only when the upstream itself
    /// reports the account unusable.
    pub balance_threshold: Option<Decimal>,
    pub balance_interval_minutes: u32,
    pub usage_enabled: bool,
    pub usage_check_interval_minutes: u32,
    pub usage_window_hours: u32,
    pub usage_baseline_days: u32,
    pub usage_multiplier: Decimal,
    pub usage_min_cost: Decimal,
    /// `0` means every check that still sees a surge sends again.
    pub usage_cooldown_minutes: u32,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            balance_threshold: None,
            // What the OneBot watcher used, and for the reason it gave: a
            // balance moves slowly and every check spends a real request
            // against the user's key.
            balance_interval_minutes: 360,
            usage_enabled: false,
            usage_check_interval_minutes: 15,
            usage_window_hours: 1,
            usage_baseline_days: 7,
            usage_multiplier: Decimal::from(3),
            usage_min_cost: Decimal::from(1),
            usage_cooldown_minutes: 360,
        }
    }
}

impl NotifyConfig {
    fn usage_thresholds(&self) -> usage::SurgeThresholds {
        usage::SurgeThresholds {
            window_hours: self.usage_window_hours,
            baseline_days: self.usage_baseline_days,
            multiplier: self.usage_multiplier.clone(),
            min_cost: self.usage_min_cost.clone(),
        }
    }

    fn usage_cooldown_ms(&self) -> i64 {
        i64::from(self.usage_cooldown_minutes) * 60_000
    }

    /// A balance alert repeats no more often than it is checked. Tying the two
    /// together rather than adding a tenth preference: a cooldown shorter than
    /// the interval constrains nothing, and one longer is a second way of
    /// saying "check less often".
    fn balance_cooldown_ms(&self) -> i64 {
        i64::from(self.balance_interval_minutes) * 60_000
    }
}

fn parse_stored_bool(key: &str, raw: Option<String>, default: bool) -> Result<bool, String> {
    match raw.as_deref() {
        None => Ok(default),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(value) => Err(format!("preference {key} must be 'true' or 'false', got {value:?}")),
    }
}

fn parse_stored_u32(key: &str, raw: Option<String>, default: u32) -> Result<u32, String> {
    let Some(raw) = raw else {
        return Ok(default);
    };
    let value = raw
        .parse::<u32>()
        .map_err(|error| format!("preference {key} has invalid integer {raw:?}: {error}"))?;
    if value.to_string() != raw {
        return Err(format!(
            "preference {key} must use canonical decimal digits, got {raw:?}"
        ));
    }
    Ok(value)
}

fn parse_stored_decimal(key: &str, raw: Option<String>, default: Decimal) -> Result<Decimal, String> {
    match parse_optional_decimal(key, raw)? {
        Some(value) => Ok(value),
        None => Ok(default),
    }
}

fn parse_optional_decimal(key: &str, raw: Option<String>) -> Result<Option<Decimal>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Err(format!(
            "preference {key} must be absent or contain a canonical decimal"
        ));
    }
    let value = raw
        .parse::<Decimal>()
        .map_err(|error| format!("preference {key} has invalid decimal {raw:?}: {error}"))?;
    if value.to_string() != raw {
        return Err(format!("preference {key} has non-canonical decimal {raw:?}"));
    }
    value
        .require_non_negative(key)
        .map(Some)
        .map_err(|error| error.to_string())
}

pub fn load_config(pool: &DbPool) -> Result<NotifyConfig, String> {
    let mut conn = pool.get().map_err(|error| format!("db connection: {error}"))?;
    let mut get = |key: &str| -> Result<Option<String>, String> {
        crate::db::ops::preference::get_preference(&mut conn, key)
            .map_err(|error| format!("failed to read preference {key}: {error}"))
    };
    let defaults = NotifyConfig::default();

    let config = NotifyConfig {
        enabled: parse_stored_bool("notify.enabled", get("notify.enabled")?, defaults.enabled)?,
        balance_threshold: parse_optional_decimal("notify.balance.threshold", get("notify.balance.threshold")?)?,
        balance_interval_minutes: parse_stored_u32(
            "notify.balance.interval_minutes",
            get("notify.balance.interval_minutes")?,
            defaults.balance_interval_minutes,
        )?,
        usage_enabled: parse_stored_bool(
            "notify.usage.enabled",
            get("notify.usage.enabled")?,
            defaults.usage_enabled,
        )?,
        usage_check_interval_minutes: parse_stored_u32(
            "notify.usage.check_interval_minutes",
            get("notify.usage.check_interval_minutes")?,
            defaults.usage_check_interval_minutes,
        )?,
        usage_window_hours: parse_stored_u32(
            "notify.usage.window_hours",
            get("notify.usage.window_hours")?,
            defaults.usage_window_hours,
        )?,
        usage_baseline_days: parse_stored_u32(
            "notify.usage.baseline_days",
            get("notify.usage.baseline_days")?,
            defaults.usage_baseline_days,
        )?,
        usage_multiplier: parse_stored_decimal(
            "notify.usage.multiplier",
            get("notify.usage.multiplier")?,
            defaults.usage_multiplier,
        )?,
        usage_min_cost: parse_stored_decimal(
            "notify.usage.min_cost",
            get("notify.usage.min_cost")?,
            defaults.usage_min_cost,
        )?,
        usage_cooldown_minutes: parse_stored_u32(
            "notify.usage.cooldown_minutes",
            get("notify.usage.cooldown_minutes")?,
            defaults.usage_cooldown_minutes,
        )?,
    };
    // Validated on the way in as well as on the way out, so a value stored
    // before a bound existed is reported now rather than driving a watcher.
    validate(&config)?;
    Ok(config)
}

/// The bounds, in one place because `load_config` and `save_config` must agree.
pub fn validate(config: &NotifyConfig) -> Result<(), String> {
    if config.balance_interval_minutes == 0 {
        return Err("notify.balance.interval_minutes must be at least 1".into());
    }
    if config.usage_check_interval_minutes == 0 {
        return Err("notify.usage.check_interval_minutes must be at least 1".into());
    }
    if config.usage_window_hours == 0 || config.usage_window_hours > 168 {
        return Err("notify.usage.window_hours must be between 1 and 168".into());
    }
    if config.usage_baseline_days == 0 || config.usage_baseline_days > 90 {
        return Err("notify.usage.baseline_days must be between 1 and 90".into());
    }
    // Below 1 the rule reads "alert when this window cost less than the
    // average", which is not what a surge is and would fire constantly.
    if config.usage_multiplier < Decimal::from(1) {
        return Err("notify.usage.multiplier must be at least 1".into());
    }
    if usage::baseline_window_count(config.usage_window_hours, config.usage_baseline_days) == 0 {
        return Err("notify.usage.baseline_days must cover at least one whole window".into());
    }
    Ok(())
}

pub fn save_config(pool: &DbPool, config: &NotifyConfig) -> Result<(), String> {
    validate(config)?;
    let mut conn = pool.get().map_err(|error| format!("db connection: {error}"))?;
    let now = now_ms();
    fn set(conn: &mut diesel::SqliteConnection, key: &str, value: &str, now: i64) -> Result<(), String> {
        crate::db::ops::preference::set_preference(conn, key, value, now).map_err(|error| error.to_string())
    }

    set(
        &mut conn,
        "notify.enabled",
        if config.enabled { "true" } else { "false" },
        now,
    )?;
    match config.balance_threshold.as_ref() {
        Some(value) => set(&mut conn, "notify.balance.threshold", &value.to_string(), now)?,
        // Deleted rather than stored empty: absent is the "off" state, and an
        // empty string is refused by the parser on the way back in.
        None => crate::db::ops::preference::delete_preference(&mut conn, "notify.balance.threshold")
            .map_err(|error| error.to_string())?,
    }
    set(
        &mut conn,
        "notify.balance.interval_minutes",
        &config.balance_interval_minutes.to_string(),
        now,
    )?;
    set(
        &mut conn,
        "notify.usage.enabled",
        if config.usage_enabled { "true" } else { "false" },
        now,
    )?;
    set(
        &mut conn,
        "notify.usage.check_interval_minutes",
        &config.usage_check_interval_minutes.to_string(),
        now,
    )?;
    set(
        &mut conn,
        "notify.usage.window_hours",
        &config.usage_window_hours.to_string(),
        now,
    )?;
    set(
        &mut conn,
        "notify.usage.baseline_days",
        &config.usage_baseline_days.to_string(),
        now,
    )?;
    set(
        &mut conn,
        "notify.usage.multiplier",
        &config.usage_multiplier.to_string(),
        now,
    )?;
    set(
        &mut conn,
        "notify.usage.min_cost",
        &config.usage_min_cost.to_string(),
        now,
    )?;
    set(
        &mut conn,
        "notify.usage.cooldown_minutes",
        &config.usage_cooldown_minutes.to_string(),
        now,
    )?;
    Ok(())
}

/// A URL this app is willing to POST to.
///
/// Plain `http` is allowed: a group robot reached through a company proxy, or a
/// relay on the LAN, is a real arrangement and refusing it only pushes people
/// to route around this. What is refused is a scheme that is not HTTP at all —
/// `file:` and its neighbours reach the local machine, and this value arrives
/// from a settings box.
///
/// In core rather than in the command that reads it, because the URL parser
/// lives here with the client that will use it, and because a second copy of
/// this rule beside a second caller is a second answer.
pub fn validate_webhook_url(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|error| format!("`url` is not a URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("`url` must be http or https, got `{}`", parsed.scheme()));
    }
    if parsed.host().is_none() {
        return Err("`url` has no host".into());
    }
    Ok(())
}

/// Everything an endpoint has to satisfy before it is written down.
pub fn validate_endpoint(name: &str, url: &str, events: &[NotificationEventKind]) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("`name` must not be empty".into());
    }
    validate_webhook_url(url)?;
    // An endpoint subscribed to nothing is a row that looks configured and can
    // never fire. Refusing it beats letting somebody find out from the silence.
    if events.is_empty() {
        return Err("`events` must name at least one event".into());
    }
    let mut seen = std::collections::HashSet::new();
    for event in events {
        if !seen.insert(*event) {
            return Err(format!("`events` repeats `{}`", event.as_str()));
        }
    }
    Ok(())
}

pub fn secret_scope() -> SecretScope {
    SecretScope::Global
}

pub fn read_webhook_secret(secrets: &SecretsManager, id: &str) -> Option<String> {
    let name = SecretName::new(&webhook_secret_name(id)).ok()?;
    match secrets.get(&secret_scope(), &name) {
        Ok(value) => value.filter(|value| !value.is_empty()),
        Err(error) => {
            // Not silently "unsigned": an endpoint configured with a secret and
            // sent an unsigned request is rejected by the vendor, and the
            // reason has to be somewhere.
            tracing::warn!(webhook_id = %id, %error, "could not read the webhook signing secret");
            None
        }
    }
}

pub fn write_webhook_secret(secrets: &SecretsManager, id: &str, secret: Option<&str>) -> Result<(), String> {
    let name = SecretName::new(&webhook_secret_name(id)).map_err(|error| error.to_string())?;
    match secret.filter(|secret| !secret.is_empty()) {
        Some(secret) => secrets
            .set(&secret_scope(), &name, secret)
            .map_err(|error| error.to_string()),
        None => secrets
            .delete(&secret_scope(), &name)
            .map(|_| ())
            .map_err(|error| error.to_string()),
    }
}

/// Send one alert everywhere it is subscribed, and say how many accepted it.
///
/// Zero is not an error — an install with no endpoints configured has nothing
/// wrong with it — but it is what stops the alert from being recorded as
/// reported, which is the whole reason this returns a count.
pub async fn dispatch(services: &Services, alert: &Alert) -> usize {
    let mut accepted = 0usize;

    let pool = services.db.clone();
    let endpoints = tokio::task::spawn_blocking(move || {
        let mut conn = pool.get().map_err(|error| error.to_string())?;
        crate::db::ops::notification::list_enabled_webhooks(&mut conn).map_err(|error| error.to_string())
    })
    .await;
    let endpoints = match endpoints {
        Ok(Ok(endpoints)) => endpoints,
        Ok(Err(error)) => {
            tracing::warn!(%error, "could not read the notification endpoints");
            Vec::new()
        }
        Err(error) => {
            tracing::warn!(%error, "reading the notification endpoints panicked");
            Vec::new()
        }
    };

    for endpoint in endpoints {
        // A row whose subscription will not decode is skipped rather than
        // failing the whole dispatch: one corrupt endpoint must not silence the
        // healthy ones. It is recorded on its own row so it is visible.
        let wants = match endpoint.wants(alert.event) {
            Ok(wants) => wants,
            Err(error) => {
                tracing::error!(webhook_id = %endpoint.id, %error, "endpoint subscription is unreadable");
                record_attempt(services, &endpoint.id, Some(&error)).await;
                continue;
            }
        };
        if !wants {
            continue;
        }
        let secret = read_webhook_secret(&services.secrets, &endpoint.id);
        let report = webhook::deliver(&endpoint, secret.as_deref(), alert).await;
        if report.is_success() {
            accepted += 1;
        } else {
            tracing::warn!(
                webhook_id = %endpoint.id,
                status = report.status.unwrap_or(0),
                attempts = report.attempts,
                error = report.error.as_deref().unwrap_or(""),
                "a notification endpoint refused an alert"
            );
        }
        record_attempt(services, &endpoint.id, report.error.as_deref()).await;
    }

    for sink in services.alert_sinks.snapshot() {
        match sink.deliver(alert).await {
            Ok(()) => accepted += 1,
            Err(error) => tracing::warn!(sink = sink.name(), %error, "an alert sink refused an alert"),
        }
    }

    accepted
}

async fn record_attempt(services: &Services, id: &str, error: Option<&str>) {
    let pool = services.db.clone();
    let id = id.to_string();
    let error = error.map(str::to_string);
    let now = now_ms();
    let written = tokio::task::spawn_blocking(move || {
        let mut conn = pool.get().map_err(|error| error.to_string())?;
        crate::db::ops::notification::record_delivery_attempt(&mut conn, &id, now, error.as_deref())
            .map_err(|error| error.to_string())
    })
    .await;
    match written {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "could not record a webhook delivery attempt"),
        Err(error) => tracing::warn!(%error, "recording a webhook delivery attempt panicked"),
    }
}

/// Note that a condition holds, and tell somebody if that is owed.
///
/// The two writes are deliberately not one. Between them sits an actual network
/// round trip that can fail, and an alert recorded as reported that nobody
/// received is one that will not be sent again.
pub async fn raise_and_dispatch(services: &Services, alert: &Alert, cooldown_ms: i64) {
    let fingerprint = alert.fingerprint();
    let pool = services.db.clone();
    let key = alert.alert_key.clone();
    let fp = fingerprint.clone();
    let now = alert.raised_at;
    let state = tokio::task::spawn_blocking(move || {
        let mut conn = pool.get().map_err(|error| error.to_string())?;
        crate::db::ops::notification::record_raised(&mut conn, &key, &fp, now).map_err(|error| error.to_string())
    })
    .await;
    let state = match state {
        Ok(Ok(state)) => state,
        Ok(Err(error)) => {
            tracing::warn!(%error, alert_key = %alert.alert_key, "could not record a raised alert");
            return;
        }
        Err(error) => {
            tracing::warn!(%error, "recording a raised alert panicked");
            return;
        }
    };

    match alert::decide(Some(&state), &fingerprint, now, cooldown_ms) {
        alert::NotifyDecision::Suppress => {
            tracing::debug!(alert_key = %alert.alert_key, "alert suppressed; already reported");
        }
        alert::NotifyDecision::Send(reason) => {
            let accepted = dispatch(services, alert).await;
            if accepted == 0 {
                // Not recorded as told. The next tick tries again, which is the
                // behaviour a disconnected chat client or a down webhook host
                // needs and the reason for the two-write split.
                tracing::info!(
                    alert_key = %alert.alert_key,
                    ?reason,
                    "an alert reached nobody; it will be offered again"
                );
                return;
            }
            let pool = services.db.clone();
            let key = alert.alert_key.clone();
            let written = tokio::task::spawn_blocking(move || {
                let mut conn = pool.get().map_err(|error| error.to_string())?;
                crate::db::ops::notification::record_notified(&mut conn, &key, now).map_err(|error| error.to_string())
            })
            .await;
            match written {
                Ok(Ok(_)) => tracing::info!(alert_key = %alert.alert_key, accepted, ?reason, "alert sent"),
                Ok(Err(error)) => tracing::warn!(%error, "could not record that an alert was sent"),
                Err(error) => tracing::warn!(%error, "recording a sent alert panicked"),
            }
        }
    }
}

/// The condition cleared. The next occurrence is a new alert rather than a
/// repeat of this one.
pub async fn clear(services: &Services, alert_key: &str) {
    let pool = services.db.clone();
    let key = alert_key.to_string();
    let cleared = tokio::task::spawn_blocking(move || {
        let mut conn = pool.get().map_err(|error| error.to_string())?;
        crate::db::ops::notification::clear_alert(&mut conn, &key).map_err(|error| error.to_string())
    })
    .await;
    if let Ok(Err(error)) = cleared {
        tracing::warn!(%error, alert_key, "could not clear a resolved alert");
    }
}

/// Send one alert by hand, without touching the state table.
///
/// A test is not a condition: it has nothing to resolve, no cooldown to respect
/// and nothing to suppress. Routed through `raise_and_dispatch` it would leave
/// a row behind and the second press would do nothing.
pub async fn send_test(services: &Services, endpoint_id: &str) -> Result<DeliveryReport, String> {
    let pool = services.db.clone();
    let id = endpoint_id.to_string();
    let endpoint = tokio::task::spawn_blocking(move || {
        let mut conn = pool.get().map_err(|error| error.to_string())?;
        crate::db::ops::notification::get_webhook(&mut conn, &id).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())??;

    let format = endpoint.format()?;
    let alert = Alert {
        event: NotificationEventKind::Test,
        raised_at: now_ms(),
        alert_key: format!("test:{endpoint_id}"),
        title: "Meridian 通知测试".to_string(),
        summary: "如果你收到了这条消息，这个端点的地址、格式和签名都是对的。".to_string(),
        detail: AlertDetail::Test(TestAlert { format }),
    };
    let secret = read_webhook_secret(&services.secrets, endpoint_id);
    let report = webhook::deliver(&endpoint, secret.as_deref(), &alert).await;
    record_attempt(services, endpoint_id, report.error.as_deref()).await;
    Ok(report)
}

/// The polling half.
pub struct NotifyServer {
    services: Services,
    config: NotifyConfig,
    shutdown_tx: watch::Sender<bool>,
    running: Arc<AtomicBool>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyStatus {
    pub enabled: bool,
    pub running: bool,
    pub balance_watch_enabled: bool,
    pub usage_watch_enabled: bool,
}

impl NotifyServer {
    pub fn new(services: Services, config: NotifyConfig) -> Self {
        let (shutdown_tx, _) = watch::channel(false);
        Self {
            services,
            config,
            shutdown_tx,
            running: Arc::new(AtomicBool::new(false)),
            task: Mutex::new(None),
        }
    }

    pub fn config(&self) -> &NotifyConfig {
        &self.config
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn status(&self) -> NotifyStatus {
        NotifyStatus {
            enabled: self.config.enabled,
            running: self.is_running(),
            balance_watch_enabled: self.config.balance_threshold.is_some(),
            usage_watch_enabled: self.config.usage_enabled,
        }
    }

    pub async fn start(&self) -> Result<(), String> {
        if self.is_running() {
            return Err("the notification watcher is already running".into());
        }
        if !self.config.enabled {
            return Err("notifications are disabled".into());
        }
        // Nothing to watch is not a failure, but it is worth saying: the switch
        // being on with neither source configured is a setup somebody thinks is
        // working.
        if self.config.balance_threshold.is_none() && !self.config.usage_enabled {
            tracing::info!("notifications are enabled but neither a balance threshold nor usage watching is set");
        }

        let _ = self.shutdown_tx.send(false);
        let services = self.services.clone();
        let config = self.config.clone();
        let running = self.running.clone();
        let shutdown_rx = self.shutdown_tx.subscribe();
        running.store(true, Ordering::Relaxed);
        let handle = tokio::spawn(async move {
            watch_loop(services, config, shutdown_rx).await;
            running.store(false, Ordering::Relaxed);
        });
        *self.task.lock().await = Some(handle);
        Ok(())
    }

    /// Stop, and do not return until the loop has actually finished.
    ///
    /// Awaited rather than signalled and forgotten, because the caller's next
    /// act is to start the next generation. Two generations overlapping means
    /// two checks against the same upstream and two alerts for one condition.
    pub async fn stop(&self) {
        let _ = self.shutdown_tx.send(true);
        let handle = self.task.lock().await.take();
        if let Some(handle) = handle
            && let Err(error) = handle.await
        {
            tracing::warn!(%error, "the notification watcher did not stop cleanly");
        }
        self.running.store(false, Ordering::Relaxed);
    }
}

async fn watch_loop(services: Services, config: NotifyConfig, mut shutdown_rx: watch::Receiver<bool>) {
    let balance_period = Duration::from_secs(u64::from(config.balance_interval_minutes) * 60);
    let usage_period = Duration::from_secs(u64::from(config.usage_check_interval_minutes) * 60);

    let mut next_balance = FIRST_CHECK_DELAY;
    let mut next_usage = FIRST_CHECK_DELAY;

    loop {
        let sleep_for = match (config.balance_threshold.is_some(), config.usage_enabled) {
            (false, false) => {
                // Nothing to do. Park on the shutdown signal rather than
                // spinning a timer that would wake up to do nothing for ever.
                let _ = shutdown_rx.changed().await;
                return;
            }
            (true, false) => next_balance,
            (false, true) => next_usage,
            (true, true) => next_balance.min(next_usage),
        };

        tokio::select! {
            changed = shutdown_rx.changed() => {
                // Err = the sender is gone because this generation was replaced.
                if changed.is_err() || *shutdown_rx.borrow() {
                    return;
                }
            }
            _ = tokio::time::sleep(sleep_for) => {
                next_balance = next_balance.saturating_sub(sleep_for);
                next_usage = next_usage.saturating_sub(sleep_for);
                if config.balance_threshold.is_some() && next_balance.is_zero() {
                    check_balances(&services, &config).await;
                    next_balance = balance_period;
                }
                if config.usage_enabled && next_usage.is_zero() {
                    check_usage(&services, &config).await;
                    next_usage = usage_period;
                }
            }
        }
    }
}

async fn check_balances(services: &Services, config: &NotifyConfig) {
    let Some(threshold) = config.balance_threshold.as_ref() else {
        return;
    };
    for (provider_id, outcome) in balance::check_all(services, threshold).await {
        match outcome {
            balance::BalanceOutcome::Alert(alert) => {
                raise_and_dispatch(services, &alert, config.balance_cooldown_ms()).await;
            }
            balance::BalanceOutcome::Healthy => clear(services, &balance::alert_key(&provider_id)).await,
            // Neither raised nor cleared. A blip reported as an empty account is
            // an alert people learn to ignore; one that clears a real alert is
            // worse.
            balance::BalanceOutcome::Unknown => {}
        }
    }
}

async fn check_usage(services: &Services, config: &NotifyConfig) {
    let pool = services.db.clone();
    let thresholds = config.usage_thresholds();
    let now = now_ms();
    let found = tokio::task::spawn_blocking(move || {
        let mut conn = pool.get().map_err(|error| error.to_string())?;
        usage::collect(&mut conn, &thresholds, now)
    })
    .await;
    match found {
        Ok(Ok(Some(alert))) => raise_and_dispatch(services, &alert, config.usage_cooldown_ms()).await,
        Ok(Ok(None)) => clear(services, usage::USAGE_ALERT_KEY).await,
        Ok(Err(error)) => tracing::warn!(%error, "the usage check could not read the ledger"),
        Err(error) => tracing::warn!(%error, "the usage check panicked"),
    }
}

/// Start the watcher if the user has it enabled, and hand it back either way.
///
/// Returned rather than registered here, for the reason `hooks::maybe_start`
/// gives: where the IPC commands look this up is the shell's business.
pub async fn maybe_start(services: Services, config: NotifyConfig) -> AppNotify {
    let server = NotifyServer::new(services, config);
    if server.config.enabled
        && let Err(error) = server.start().await
    {
        tracing::error!(%error, "failed to auto-start the notification watcher");
    }
    AppNotify(Arc::new(Mutex::new(server)))
}

pub struct AppNotify(pub Arc<Mutex<NotifyServer>>);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    fn set(pool: &DbPool, key: &str, value: &str) {
        let mut conn = pool.get().unwrap();
        crate::db::ops::preference::set_preference(&mut conn, key, value, 1).unwrap();
    }

    #[test]
    fn absent_preferences_keep_the_documented_defaults() {
        let config = load_config(&test_db()).unwrap();
        assert_eq!(config, NotifyConfig::default());
        assert!(!config.enabled);
        assert_eq!(config.balance_threshold, None, "off until somebody sets a floor");
    }

    /// A stored value that no longer satisfies the bounds is reported at load
    /// rather than quietly driving a watcher — the same reason the other
    /// loaders validate on the way in.
    #[test]
    fn a_stored_value_outside_the_bounds_fails_the_load() {
        let pool = test_db();
        set(&pool, "notify.usage.window_hours", "0");
        assert!(load_config(&pool).is_err());

        let pool = test_db();
        set(&pool, "notify.usage.multiplier", "0.5");
        let error = load_config(&pool).unwrap_err();
        assert!(error.contains("multiplier"), "{error}");

        let pool = test_db();
        set(&pool, "notify.usage.window_hours", "48");
        set(&pool, "notify.usage.baseline_days", "1");
        let error = load_config(&pool).unwrap_err();
        assert!(error.contains("whole window"), "{error}");
    }

    #[test]
    fn a_non_canonical_number_is_refused_rather_than_rounded() {
        let pool = test_db();
        set(&pool, "notify.usage.multiplier", "3.0");
        assert!(load_config(&pool).is_err(), "3.0 is not the canonical spelling of 3");

        let pool = test_db();
        set(&pool, "notify.balance.threshold", "-1");
        assert!(load_config(&pool).is_err());

        let pool = test_db();
        set(&pool, "notify.balance.threshold", "");
        assert!(load_config(&pool).is_err(), "empty is not a spelling of absent");

        let pool = test_db();
        set(&pool, "notify.enabled", "yes");
        assert!(load_config(&pool).is_err());
    }

    /// `Some(0)` and `None` are different settings and have to survive a round
    /// trip as such: zero means "tell me when the upstream refuses the account",
    /// absent means "do not ask at all".
    #[test]
    fn a_zero_threshold_round_trips_apart_from_an_absent_one() {
        let pool = test_db();
        let mut config = NotifyConfig {
            balance_threshold: Some(Decimal::zero()),
            ..Default::default()
        };
        save_config(&pool, &config).unwrap();
        assert_eq!(load_config(&pool).unwrap().balance_threshold, Some(Decimal::zero()));

        config.balance_threshold = None;
        save_config(&pool, &config).unwrap();
        assert_eq!(load_config(&pool).unwrap().balance_threshold, None);
        let mut conn = pool.get().unwrap();
        assert_eq!(
            crate::db::ops::preference::get_preference(&mut conn, "notify.balance.threshold").unwrap(),
            None,
            "turning it off deletes the key rather than storing an empty one"
        );
    }

    #[test]
    fn a_full_config_round_trips() {
        let pool = test_db();
        let config = NotifyConfig {
            enabled: true,
            balance_threshold: Some("12.5".parse().unwrap()),
            balance_interval_minutes: 60,
            usage_enabled: true,
            usage_check_interval_minutes: 5,
            usage_window_hours: 2,
            usage_baseline_days: 14,
            usage_multiplier: "2.5".parse().unwrap(),
            usage_min_cost: "0.25".parse().unwrap(),
            usage_cooldown_minutes: 30,
        };
        save_config(&pool, &config).unwrap();
        assert_eq!(load_config(&pool).unwrap(), config);
    }

    /// This value is typed into a settings box and then handed to an HTTP
    /// client. Plain `http` stays allowed; a scheme that is not HTTP does not.
    #[test]
    fn only_http_urls_are_accepted() {
        assert!(validate_webhook_url("https://oapi.dingtalk.com/robot/send?access_token=x").is_ok());
        assert!(validate_webhook_url("http://192.168.1.9:8080/hook").is_ok());
        assert!(validate_webhook_url("file:///etc/passwd").is_err());
        assert!(validate_webhook_url("ftp://example.invalid/x").is_err());
        assert!(validate_webhook_url("not a url").is_err());
        assert!(validate_webhook_url("https://").is_err());
    }

    /// A row that looks configured and can never fire is worse than a refused
    /// save, because nothing later says why the alerts never arrived.
    #[test]
    fn an_endpoint_subscribed_to_nothing_is_refused() {
        let url = "https://a.invalid/h";
        assert!(validate_endpoint("x", url, &[]).is_err());
        assert!(
            validate_endpoint(
                "x",
                url,
                &[NotificationEventKind::BalanceLow, NotificationEventKind::BalanceLow]
            )
            .is_err()
        );
        assert!(validate_endpoint("  ", url, &[NotificationEventKind::Test]).is_err());
        assert!(validate_endpoint("x", url, &[NotificationEventKind::Test]).is_ok());
    }

    #[test]
    fn saving_an_invalid_config_is_refused_before_anything_is_written() {
        let pool = test_db();
        let config = NotifyConfig {
            usage_baseline_days: 0,
            ..Default::default()
        };
        assert!(save_config(&pool, &config).is_err());
        let mut conn = pool.get().unwrap();
        assert_eq!(
            crate::db::ops::preference::get_preference(&mut conn, "notify.usage.baseline_days").unwrap(),
            None,
            "a refused save leaves no half-written configuration"
        );
    }
}

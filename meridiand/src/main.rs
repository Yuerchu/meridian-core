//! Meridian, running where nobody is looking.
//!
//! The desktop app already runs without a window — closing it hides to the
//! tray. What it cannot do is run where there is *no display server at all*, and
//! that is the whole of what this binary is for: a container, a NAS, a VPS. It
//! is the same core, the same database, the same watcher; what it drops is the
//! window, the tray and the IPC surface, and what it gains is a configuration
//! file and a passphrase handed in from outside.
//!
//! It is deliberately **not** a server. It listens on nothing. The moment it
//! wants to serve the command surface, that is a different decision with a
//! different name, and it should be made on its own terms rather than arrived
//! at by adding routes here.
//!
//! ## Known cost
//!
//! It links `sherpa-onnx` because `meridian-core` does, unconditionally — voice
//! reaches `Services`, `state`, `bootstrap` and most of `onebot`, so gating it
//! behind a feature is a real refactor rather than a flag. The shared library
//! has to sit beside the binary (its rpath is `$ORIGIN`), exactly as the
//! AppImage arranges. That makes the image fatter than a daemon that only makes
//! HTTP requests has any right to be; see the packaging notes in the app's
//! CLAUDE.md.

mod apply;
mod config;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use meridian_core::events::EventBus;
use meridian_core::keyring::SuppliedPassphraseStore;
use meridian_core::secrets::SecretsManager;

/// Where the secrets-file passphrase comes from when there is no keychain.
const PASSPHRASE_VAR: &str = "MERIDIAN_SECRETS_PASSPHRASE";
/// The same, read from a file instead — which is how a container mounts a
/// secret without it appearing in `docker inspect` or in the process
/// environment of everything this spawns.
const PASSPHRASE_FILE_VAR: &str = "MERIDIAN_SECRETS_PASSPHRASE_FILE";

/// The default data directory, under the platform's application data location.
///
/// Deliberately **not** the desktop app's `cn.yuxiaoqiu.meridian`. A daemon
/// landing in that directory would meet the ownership guard and refuse to
/// start, which is correct and a baffling way to be greeted on a first run; and
/// if it somehow did not, it would switch off every provider the desktop had
/// configured, because the configuration file is a desired state. Adjacent, not
/// shared.
const DEFAULT_DIR_NAME: &str = "cn.yuxiaoqiu.meridiand";

#[derive(Parser, Debug)]
#[command(
    name = "meridiand",
    about = "Watch provider balances and spending, and say so over a webhook.",
    version
)]
struct Args {
    /// The configuration file. Applied on every start: it is the desired state,
    /// not a one-off command.
    #[arg(short, long, value_name = "PATH")]
    config: PathBuf,

    /// Where the database, the secrets file and the logs live.
    ///
    /// Defaults to `<platform data dir>/cn.yuxiaoqiu.meridiand`, which is
    /// beside the desktop app's directory rather than inside it. The resolved
    /// path is always logged, so it is never a mystery.
    #[arg(short, long, value_name = "PATH", env = "MERIDIAN_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Parse and validate everything, then exit without touching the database.
    ///
    /// What a deployment runs before restarting the real one.
    #[arg(long)]
    check: bool,

    /// Apply the configuration, send one test alert to this endpoint, and exit.
    ///
    /// The only way to find out whether a receiver's authentication and schema
    /// are right without waiting for a real condition to occur.
    #[arg(long, value_name = "WEBHOOK_ID")]
    test: Option<String>,

    /// Print each endpoint's delivery health and exit.
    #[arg(long)]
    status: bool,
}

fn main() -> std::process::ExitCode {
    meridian_core::logging::init_early();
    meridian_core::logging::install_panic_hook();

    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // Both, on purpose. `tracing` reaches the log file that a running
            // deployment collects; stderr reaches the operator who just typed
            // the command and is waiting for an answer.
            tracing::error!(%error, "meridiand could not start");
            eprintln!("meridiand: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args = Args::parse();
    // A TOML diagnostic can quote the source line, including a credential-bearing URL.
    let config = config::DaemonConfig::read(&args.config)?;
    // Before the database is opened, so `--check` on a machine with no data
    // directory still answers the question it was asked.
    let notify_config = config.notify_config().map_err(|_| "invalid notification settings")?;

    let data_dir = resolve_data_dir(args.data_dir)?;

    if args.check {
        println!(
            "meridiand: {} is valid — {} provider(s), {} webhook(s), watcher {}",
            args.config.display(),
            config.provider.len(),
            config.webhook.len(),
            if notify_config.enabled { "on" } else { "off" },
        );
        // Where it *would* write, which is the other half of "is this
        // configuration what I think it is" and the reason a default has to be
        // visible without starting anything.
        println!("meridiand: data directory {}", data_dir.display());
        return Ok(());
    }

    tracing::info!(data_dir = %data_dir.display(), "meridiand starting");
    let secrets = Arc::new(open_secrets(&data_dir)?);
    // No sinks, and that is correct rather than tolerated: `EventBus::emit`
    // treats an empty registry as success, precisely so a headless run is not
    // failed by having no window to miss anything.
    // Built before bootstrap, which is async; the one place this binary crosses
    // from sync to async, so nothing below ever nests a second `block_on`.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("could not start the runtime: {error}"))?;
    let services = runtime.block_on(meridian_core::bootstrap::bootstrap_with_secrets(
        data_dir,
        EventBus::new(),
        secrets.clone(),
    ))?;

    let report = runtime.block_on(apply::apply(&services.db, &services.secrets, &config))?;
    tracing::info!(
        providers = report.providers_written,
        providers_disabled = report.providers_disabled,
        webhooks = report.webhooks_written,
        webhooks_disabled = report.webhooks_disabled,
        "configuration applied"
    );

    // Both run *after* the configuration has been applied, so what they exercise
    // is the endpoint as configured rather than whatever a previous run left in
    // the database.
    if let Some(id) = args.test {
        return runtime.block_on(send_test(&services, &id));
    }
    if args.status {
        return runtime.block_on(print_status(&services));
    }

    runtime.block_on(serve(services, notify_config))
}

/// Send one test alert and report what the receiver did with it.
///
/// Exits non-zero on a refusal, so a deployment script can gate on it.
async fn send_test(services: &meridian_core::services::Services, id: &str) -> Result<(), String> {
    let report = meridian_core::notify::send_test(services, id).await?;
    let status = report
        .status
        .map(|status| status.to_string())
        .unwrap_or_else(|| "no response".into());
    match report.error.as_deref() {
        None => {
            println!("meridiand: {id} accepted — HTTP {status}");
            Ok(())
        }
        Some(error) => Err(format!(
            "{id}: {} — HTTP {status}",
            meridian_core::db::sea::ops::notification::delivery_error_class(error)
        )),
    }
}

/// Which providers are actually being watched, and what each endpoint's last
/// delivery did.
///
/// The database has recorded all of it since the feature existed; until now
/// nothing could read it without opening the file by hand, which is a diagnosis
/// nobody makes at three in the morning.
async fn print_status(services: &meridian_core::services::Services) -> Result<(), String> {
    print_provider_status(&services.db).await?;
    // A subscription that will not decode fails this read, and the error
    // names the row: it is why an endpoint is silent, and it is invisible
    // everywhere else.
    let rows = meridian_core::db::sea::ops::notification::list_webhooks(&services.db)
        .await
        .map_err(|_| "could not read the endpoints: database".to_string())?;
    if rows.is_empty() {
        println!("meridiand: no endpoints are configured");
        return Ok(());
    }
    for row in rows {
        println!("{}", webhook_status_line(&row));
    }
    Ok(())
}

fn webhook_status_line(row: &meridian_core::db::entity::notification_webhook::Model) -> String {
    let status = if !row.is_enabled.get() {
        "disabled"
    } else if row.consecutive_failures > 0 {
        meridian_core::db::sea::ops::notification::delivery_error_class(row.last_error.as_deref().unwrap_or("failed"))
    } else if row.last_success_at.is_some() {
        "accepted"
    } else {
        "never attempted"
    };
    format!("{}  {status}", row.id)
}

/// Which providers a balance can actually be read for.
///
/// This is the half of `--status` that answers "why have I never heard from
/// it", and the commonest reason is now resolution rather than delivery: an
/// upstream added after the original five is reached over the OpenAI dialect,
/// so `type` cannot name it and an entry without `vendor` at an address the
/// catalog does not recognise is watched for nothing. That failure is
/// completely silent otherwise — the daemon runs, the endpoints are healthy,
/// and no alert was ever going to be raised.
async fn print_provider_status(sea: &meridian_core::db::sea::cap::Db) -> Result<(), String> {
    use meridian_core::provider::balance::{ProviderIdentity, balance_vendor};

    let providers = meridian_core::db::sea::ops::provider::list_providers(sea)
        .await
        .map_err(|error| error.to_string())?;
    if providers.is_empty() {
        println!("meridiand: no providers are configured");
    }
    for provider in &providers {
        let vendor = balance_vendor(ProviderIdentity::new(
            provider.catalog_id.as_deref(),
            provider.provider_type.as_str(),
            &provider.base_url,
        ));
        println!(
            "{}  {}  {}",
            if provider.is_enabled.get() { "on " } else { "off" },
            provider.id,
            provider.provider_type.as_str(),
        );
        match vendor {
            Some(vendor) => println!("     balance read as `{}`", vendor.catalog_id()),
            None => println!("     no balance: this upstream publishes none, or `vendor` is unset"),
        }
    }
    if !providers.is_empty() {
        println!();
    }
    Ok(())
}

/// Where this daemon keeps its database, secrets and logs.
///
/// `--config` stays required because there is no location a configuration file
/// conventionally lives at, and picking one silently would be worse than
/// saying so. A data directory is the opposite: every daemon has a
/// conventional home, and the platform's application data directory is the one
/// people mean. Requiring it too was one rule applied to two different
/// questions.
///
/// Guessing is still refused where guessing is impossible: a platform with no
/// data directory at all — a Linux service with no `HOME` and no
/// `XDG_DATA_HOME` — gets an error naming the flag rather than a path under
/// `/`.
fn resolve_data_dir(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(explicit) = explicit {
        return Ok(explicit);
    }
    dirs::data_dir().map(|base| base.join(DEFAULT_DIR_NAME)).ok_or_else(|| {
        "no platform data directory is available (no HOME or XDG_DATA_HOME?); \
         pass --data-dir or set $MERIDIAN_DATA_DIR"
            .to_string()
    })
}

/// The keychain, or the passphrase the deployment supplied instead.
///
/// The desktop's keyring backend is the right answer wherever somebody is
/// logged in, and this binary is perfectly usable there — so it is tried first
/// and the supplied passphrase is the override, not the other way round. On a
/// machine with a session that means `meridiand` and the desktop app share one
/// secrets file, which is what somebody running both on their own laptop would
/// expect.
fn open_secrets(data_dir: &std::path::Path) -> Result<SecretsManager, String> {
    let supplied = match (std::env::var(PASSPHRASE_VAR), std::env::var(PASSPHRASE_FILE_VAR)) {
        (Ok(_), Ok(_)) => {
            // Refused rather than ranked. Two sources for one passphrase is a
            // deployment that believes something untrue about which one is in
            // force, and the way that surfaces is an unreadable secrets file.
            return Err(format!(
                "both ${PASSPHRASE_VAR} and ${PASSPHRASE_FILE_VAR} are set; use one"
            ));
        }
        (Ok(value), Err(_)) => Some((value, format!("${PASSPHRASE_VAR}"))),
        (Err(_), Ok(path)) => {
            let value = std::fs::read_to_string(&path)
                .map_err(|error| format!("could not read ${PASSPHRASE_FILE_VAR} ({path}): {error}"))?;
            Some((value, path))
        }
        (Err(_), Err(_)) => None,
    };

    match supplied {
        Some((passphrase, source)) => {
            let store = SuppliedPassphraseStore::new(passphrase, source)?;
            Ok(SecretsManager::new_with_keyring_store(
                data_dir.to_path_buf(),
                Arc::new(store),
            ))
        }
        None => {
            tracing::info!(
                "no supplied passphrase; using this machine's keychain. \
                 Set ${PASSPHRASE_VAR} or ${PASSPHRASE_FILE_VAR} where there is no login session."
            );
            Ok(SecretsManager::new(data_dir.to_path_buf()))
        }
    }
}

async fn serve(
    services: meridian_core::services::Services,
    notify_config: meridian_core::notify::NotifyConfig,
) -> Result<(), String> {
    let watcher = meridian_core::notify::NotifyServer::new(services, notify_config);
    if watcher.config().enabled {
        watcher.start().await?;
        tracing::info!("meridiand is watching");
    } else {
        // Not an error: a deployment may legitimately keep the daemon up with
        // the watcher off. But it is the state somebody will later describe as
        // "it isn't alerting", so it is said once, clearly.
        tracing::warn!("notify.enabled is false; this process will do nothing until it is turned on");
    }

    wait_for_shutdown().await;

    // Awaited, not signalled and abandoned. A tick in flight may be between
    // "the alert was delivered" and "record that somebody was told", and losing
    // that second write means the next start reports the same alert again.
    tracing::info!("stopping");
    watcher.stop().await;
    Ok(())
}

#[cfg(unix)]
async fn wait_for_shutdown() {
    use tokio::signal::unix::{SignalKind, signal};
    // SIGTERM is what an orchestrator sends, and a daemon that only handles
    // Ctrl-C gets killed rather than stopped — mid-delivery, which is the one
    // moment that costs a duplicate alert.
    let mut term = match signal(SignalKind::terminate()) {
        Ok(term) => term,
        Err(error) => {
            tracing::error!(%error, "could not listen for SIGTERM; only Ctrl-C will stop this cleanly");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => tracing::info!("SIGTERM"),
        result = tokio::signal::ctrl_c() => match result {
            Ok(()) => tracing::info!("interrupted"),
            Err(error) => tracing::error!(%error, "could not listen for Ctrl-C"),
        },
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown() {
    match tokio::signal::ctrl_c().await {
        Ok(()) => tracing::info!("interrupted"),
        Err(error) => tracing::error!(%error, "could not listen for Ctrl-C"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn webhook_test_lookup_never_echoes_a_url_shaped_id() {
        meridian_core::logging::init_early();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let store = SuppliedPassphraseStore::new("a passphrase for the test only", "test").unwrap();
        let secrets = Arc::new(SecretsManager::new_with_keyring_store(path.clone(), Arc::new(store)));
        let services = meridian_core::bootstrap::bootstrap_with_secrets(path, EventBus::new(), secrets)
            .await
            .unwrap();
        let id = "https://sentinel-user:sentinel-password@example.invalid/sentinel-path?token=sentinel-query";
        assert_eq!(
            send_test(&services, id).await.unwrap_err(),
            "notification endpoint lookup failed: not_found"
        );
    }

    #[test]
    fn webhook_status_never_prints_config_or_legacy_errors() {
        use meridian_core::db::entity::notification_webhook::{self, NotificationEvents, NotificationFormat};
        let row = notification_webhook::Model {
            id: "ops".into(),
            name: "private-name".into(),
            url: "https://user:password@example.invalid/path-token?secret=query-token".into(),
            format: NotificationFormat::Generic,
            events: NotificationEvents::from(vec![]),
            is_enabled: true.into(),
            body_template: None,
            last_attempt_at: Some(1),
            last_success_at: None,
            last_error: Some("vendor-body-token".into()),
            consecutive_failures: 1,
            created_at: 0,
            updated_at: 1,
        };
        assert_eq!(webhook_status_line(&row), "ops  failed");
    }

    /// `--help` and `--version` are the whole interface, and both have to work
    /// before anything else does.
    #[test]
    fn the_arguments_are_what_the_documentation_says() {
        use clap::CommandFactory;
        Args::command().debug_assert();

        let args = Args::try_parse_from(["meridiand", "--config", "/etc/m.toml", "--data-dir", "/var/lib/m"]).unwrap();
        assert_eq!(args.config, PathBuf::from("/etc/m.toml"));
        assert_eq!(args.data_dir, Some(PathBuf::from("/var/lib/m")));
        assert!(!args.check);

        // `--config` is required because no location is conventional for one,
        // and picking one silently would be worse than saying so.
        assert!(Args::try_parse_from(["meridiand"]).is_err());
        assert!(Args::try_parse_from(["meridiand", "--data-dir", "/var/lib/m"]).is_err());
    }

    /// A data directory is the opposite question: every daemon has a
    /// conventional home, and requiring it too was one rule applied to two
    /// different things.
    #[test]
    fn the_data_directory_falls_back_to_the_platform_location() {
        let args = Args::try_parse_from(["meridiand", "--config", "/etc/m.toml"]).unwrap();
        assert_eq!(args.data_dir, None);

        let explicit = resolve_data_dir(Some(PathBuf::from("/var/lib/m"))).unwrap();
        assert_eq!(explicit, PathBuf::from("/var/lib/m"));

        // On any machine a test runs on there is one; what matters is that it
        // is the daemon's own and not the desktop app's, which the ownership
        // guard would refuse and which would be a baffling first run.
        let fallback = resolve_data_dir(None).unwrap();
        assert!(fallback.ends_with(DEFAULT_DIR_NAME), "{}", fallback.display());
        assert_ne!(
            fallback.file_name().and_then(|name| name.to_str()),
            Some("cn.yuxiaoqiu.meridian"),
            "the daemon must not default into the desktop app's directory"
        );
    }
}

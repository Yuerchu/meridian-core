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
    #[arg(short, long, value_name = "PATH", env = "MERIDIAN_DATA_DIR")]
    data_dir: PathBuf,

    /// Parse and validate everything, then exit without touching the database.
    ///
    /// What a deployment runs before restarting the real one.
    #[arg(long)]
    check: bool,
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
    let config = config::DaemonConfig::read(&args.config)?;
    // Before the database is opened, so `--check` on a machine with no data
    // directory still answers the question it was asked.
    let notify_config = config.notify_config()?;

    if args.check {
        println!(
            "meridiand: {} is valid — {} provider(s), {} webhook(s), watcher {}",
            args.config.display(),
            config.provider.len(),
            config.webhook.len(),
            if notify_config.enabled { "on" } else { "off" },
        );
        return Ok(());
    }

    let secrets = Arc::new(open_secrets(&args.data_dir)?);
    // No sinks, and that is correct rather than tolerated: `EventBus::emit`
    // treats an empty registry as success, precisely so a headless run is not
    // failed by having no window to miss anything.
    let services =
        meridian_core::bootstrap::bootstrap_with_secrets(args.data_dir.clone(), EventBus::new(), secrets.clone())?;

    let report = apply::apply(&services.db, &services.secrets, &config)?;
    tracing::info!(
        providers = report.providers_written,
        providers_disabled = report.providers_disabled,
        webhooks = report.webhooks_written,
        webhooks_disabled = report.webhooks_disabled,
        "configuration applied"
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("could not start the runtime: {error}"))?;
    runtime.block_on(serve(services, notify_config))
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

    /// `--help` and `--version` are the whole interface, and both have to work
    /// before anything else does.
    #[test]
    fn the_arguments_are_what_the_documentation_says() {
        use clap::CommandFactory;
        Args::command().debug_assert();

        let args = Args::try_parse_from(["meridiand", "--config", "/etc/m.toml", "--data-dir", "/var/lib/m"]).unwrap();
        assert_eq!(args.config, PathBuf::from("/etc/m.toml"));
        assert!(!args.check);

        // Both are required: a daemon that guessed either would write somewhere
        // nobody meant.
        assert!(Args::try_parse_from(["meridiand", "--config", "/etc/m.toml"]).is_err());
        assert!(Args::try_parse_from(["meridiand", "--data-dir", "/var/lib/m"]).is_err());
    }
}

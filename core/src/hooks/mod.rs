//! The local endpoint another coding agent's hooks call into.
//!
//! "Hook" here means a Claude Code hook, not a git one. Claude Code fires a
//! `PermissionRequest` when it wants to leave plan mode; the plugin behind that
//! hook posts the plan here, and what this answers decides whether the plan
//! goes back for another draft or carries on to the user.
//!
//! The listener is deliberately general — one HTTP server, routes added as more
//! hook events earn one — while [`review`] is the business of this first route.
//!
//! ## The one rule
//!
//! Claude Code treats every failure of an HTTP hook as *proceed*: a refused
//! connection, a non-2xx, a timeout, a body that will not parse. That is the
//! right default and this module is built around it rather than against it.
//! Anything uncertain here becomes a non-200 or an `Inconclusive`, and the plan
//! carries on to the user, who was always the one meant to approve it. The only
//! thing that stops a plan is a review that ran, parsed, and said so.

pub(crate) mod http;
pub(crate) mod protocol;
pub(crate) mod review;
pub(crate) mod verdict;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::net::TcpListener;
use tokio::sync::{Mutex, watch};

use crate::db::DbPool;
use crate::listen_guard::validate_listen_config;
use crate::services::Services;
use crate::util::{get_conn, now_ms};

/// Which incarnation of the server wrote the handshake file.
///
/// Saving settings stops the old server and starts a new one 300ms later, but
/// the old server deletes the handshake from the tail of its own task — on the
/// runtime's schedule, not inside that window. Busy enough and it runs *after*
/// the new server has written the file, deleting a live server's only
/// advertisement: the endpoint answers fine while the plugin reports it as not
/// running, until the app is restarted.
///
/// So the file records who wrote it, and a departing server only deletes what
/// it still owns. This covers both ways the accept loop can exit — an explicit
/// `stop()` and the channel closing when the server is replaced — which a
/// synchronous delete inside `stop()` would not.
static GENERATION: AtomicU64 = AtomicU64::new(0);

const DEFAULT_PORT: u16 = 8765;
const DEFAULT_TIMEOUT_SECS: u32 = 600;
/// The longest a review is allowed to take, and not a number we are free to
/// choose: three layers have to decrease, or nobody gets to say "timed out".
///
/// ```text
/// this ceiling  1200s  ← Meridian gives up and answers 504
/// plugin budget 1260s  ← plugin gives up and reports it to the user
/// hook timeout  1320s  ← Claude Code kills the hook process; user sees nothing
/// ```
///
/// Raising this without raising `BUDGET_MS` in `plan-review-hook.mjs` and
/// `timeout` in the plugin's `hooks.json` does not buy a longer review — it
/// just moves the cutoff to the layer that cannot explain itself.
///
/// The first ceiling here was 300s, chosen on the assumption that a review is a
/// short verdict after a bit of reading. Measured against a real repository it
/// is not: the reviewer is a model of roughly the capability of the one being
/// reviewed, and it works like one — an observed run took 274s over 71 messages
/// and some sixty tool calls before it had checked what the plan claimed. That
/// is the job being done properly, not a runaway, so the budget follows the
/// work rather than the other way round.
const MAX_TIMEOUT_SECS: u32 = 1200;

fn clamp_timeout(secs: u32) -> u32 {
    secs.clamp(10, MAX_TIMEOUT_SECS)
}

/// How many times a plan may be sent back before the gate gives up and lets it
/// through. `0` means never give up on that count alone.
///
/// Not the mechanism that guarantees termination — that is the plugin's
/// stagnation check, which passes a plan the moment two rounds in a row bring
/// no material change, and which measures progress rather than counting it.
/// This covers the narrower case of an agent that keeps producing genuinely
/// different plans that keep failing review, where the question is not
/// correctness but how much the user is willing to spend. So it is theirs to
/// set, and `0` is a legitimate answer.
const DEFAULT_MAX_ROUNDS: u32 = 5;
const MAX_MAX_ROUNDS: u32 = 20;

fn clamp_rounds(rounds: u32) -> u32 {
    rounds.min(MAX_MAX_ROUNDS)
}
/// The file the plugin reads to find this server.
const HANDSHAKE: &str = "plan-gate.json";
/// The inbound request contract. Version 2 requires every key to be present,
/// including keys whose value may be JSON `null`.
const HOOK_PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Clone)]
pub struct HookConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    /// Generated on first enable. Loopback is not a boundary on a desktop —
    /// every process on the machine can reach it, and so can any page the
    /// user's browser has open.
    pub token: Option<String>,
    /// `"<provider_id>:<model_id>"`. No default: which model reviews plans is
    /// the whole point of the feature, and picking one here would quietly
    /// review with whatever happened to be first in the provider list.
    pub review_model: Option<String>,
    /// Supplies temperature, thinking and the rest. `None` uses the default
    /// assistant. The persona is always replaced by the review prompt.
    pub assistant_id: Option<String>,
    pub timeout_secs: u32,
    /// Rounds before the gate stops blocking. `0` = no limit; see
    /// [`DEFAULT_MAX_ROUNDS`].
    pub max_rounds: u32,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: "127.0.0.1".into(),
            port: DEFAULT_PORT,
            token: None,
            review_model: None,
            assistant_id: None,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            max_rounds: DEFAULT_MAX_ROUNDS,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct HookStatus {
    pub enabled: bool,
    pub running: bool,
    pub host: String,
    pub port: u16,
    /// Where the plugin will look for us. Shown in settings so a user who is
    /// debugging can check the file themselves.
    pub handshake_path: Option<String>,
}

fn parse_stored_bool(key: &str, raw: Option<String>, default: bool) -> Result<bool, String> {
    match raw.as_deref() {
        None => Ok(default),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(value) => Err(format!("preference {key} must be 'true' or 'false', got {value:?}")),
    }
}

fn parse_stored_u16(key: &str, raw: Option<String>, default: u16) -> Result<u16, String> {
    let Some(raw) = raw else {
        return Ok(default);
    };
    let value = raw
        .parse::<u16>()
        .map_err(|error| format!("preference {key} has invalid integer {raw:?}: {error}"))?;
    if value.to_string() != raw {
        return Err(format!(
            "preference {key} must use canonical decimal digits, got {raw:?}"
        ));
    }
    Ok(value)
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

pub fn load_config(pool: &DbPool) -> Result<HookConfig, String> {
    let mut conn = get_conn(pool)?;
    let mut get = |key: &str| -> Result<Option<String>, String> {
        crate::db::ops::preference::get_preference(&mut conn, key)
            .map_err(|error| format!("failed to read preference {key}: {error}"))
    };

    Ok(HookConfig {
        enabled: parse_stored_bool("hooks.enabled", get("hooks.enabled")?, false)?,
        host: get("hooks.host")?.unwrap_or_else(|| "127.0.0.1".into()),
        port: parse_stored_u16("hooks.port", get("hooks.port")?, DEFAULT_PORT)?,
        token: get("hooks.token")?.filter(|s| !s.is_empty()),
        review_model: get("hooks.plan_review.model")?.filter(|s| !s.is_empty()),
        assistant_id: get("hooks.plan_review.assistant_id")?.filter(|s| !s.is_empty()),
        // Clamped on the way in as well as on the way out, so a value stored
        // before the ceiling existed corrects itself on the next load instead
        // of waiting for someone to open the settings page and press save.
        timeout_secs: clamp_timeout(parse_stored_u32(
            "hooks.plan_review.timeout_secs",
            get("hooks.plan_review.timeout_secs")?,
            DEFAULT_TIMEOUT_SECS,
        )?),
        max_rounds: clamp_rounds(parse_stored_u32(
            "hooks.plan_review.max_rounds",
            get("hooks.plan_review.max_rounds")?,
            DEFAULT_MAX_ROUNDS,
        )?),
    })
}

pub fn save_config(pool: &DbPool, config: &HookConfig) -> Result<(), String> {
    let mut conn = get_conn(pool)?;
    let now = now_ms();
    let mut set = |key: &str, val: &str| -> Result<(), String> {
        crate::db::ops::preference::set_preference(&mut conn, key, val, now).map_err(|e| e.to_string())
    };

    set("hooks.enabled", if config.enabled { "true" } else { "false" })?;
    set("hooks.host", &config.host)?;
    set("hooks.port", &config.port.to_string())?;
    set("hooks.token", config.token.as_deref().unwrap_or(""))?;
    set("hooks.plan_review.model", config.review_model.as_deref().unwrap_or(""))?;
    set(
        "hooks.plan_review.assistant_id",
        config.assistant_id.as_deref().unwrap_or(""),
    )?;
    set(
        "hooks.plan_review.timeout_secs",
        &clamp_timeout(config.timeout_secs).to_string(),
    )?;
    set(
        "hooks.plan_review.max_rounds",
        &clamp_rounds(config.max_rounds).to_string(),
    )?;
    Ok(())
}

pub(crate) struct SharedState {
    pub config: HookConfig,
    /// Also how a review announces that its conversation exists or has moved on.
    ///
    /// The review loop deliberately streams to nobody — there is no window
    /// waiting on it. But the conversation it writes shows up in the sidebar,
    /// and the sidebar only refetches when something says so. Without that the
    /// review is invisible for the several minutes it runs, which is the same
    /// as the gate not being installed.
    pub services: Services,
}

pub struct HookServer {
    state: Arc<SharedState>,
    shutdown_tx: watch::Sender<bool>,
    running: Arc<AtomicBool>,
    handshake: Option<std::path::PathBuf>,
}

impl HookServer {
    pub fn new(services: Services, config: HookConfig) -> Self {
        let handshake = Some(services.paths.data_dir.join(HANDSHAKE));
        let (shutdown_tx, _) = watch::channel(false);
        Self {
            state: Arc::new(SharedState { config, services }),
            shutdown_tx,
            running: Arc::new(AtomicBool::new(false)),
            handshake,
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn status(&self) -> HookStatus {
        HookStatus {
            enabled: self.state.config.enabled,
            running: self.is_running(),
            host: self.state.config.host.clone(),
            port: self.state.config.port,
            handshake_path: self.handshake.as_ref().map(|p| p.display().to_string()),
        }
    }

    pub fn start(&self) -> Result<(), String> {
        if self.is_running() {
            return Err("hook server is already running".into());
        }
        validate_listen_config(
            &self.state.config.host,
            self.state.config.token.as_deref(),
            "the hook token",
        )?;

        let state = self.state.clone();
        let running = self.running.clone();
        let handshake = self.handshake.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;

        running.store(true, Ordering::Relaxed);

        tokio::spawn(async move {
            let addr = format!("{}:{}", state.config.host, state.config.port);
            let listener = match TcpListener::bind(&addr).await {
                Ok(l) => {
                    tracing::info!("hook server listening on {addr}");
                    l
                }
                Err(e) => {
                    tracing::error!(error = %e, "failed to bind hook server to {addr}");
                    running.store(false, Ordering::Relaxed);
                    return;
                }
            };

            // Written only after the bind succeeds: the file means "there is
            // something listening on this port", and writing it earlier would
            // point the plugin at a port that lost the race for it.
            if let Some(path) = &handshake {
                write_handshake(path, &state.config, generation);
            }

            loop {
                tokio::select! {
                    changed = shutdown_rx.changed() => {
                        // Err = every sender dropped, i.e. this server was
                        // replaced. Same meaning as `true`, and not treating it
                        // that way spins on a closed channel.
                        if changed.is_err() || *shutdown_rx.borrow() {
                            tracing::info!("hook server shutting down");
                            break;
                        }
                    }
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, peer)) => http::serve(stream, peer, state.clone()),
                            Err(e) => tracing::warn!(error = %e, "hook server accept failed"),
                        }
                    }
                }
            }

            if let Some(path) = &handshake {
                remove_handshake_if_ours(path, generation);
            }
            running.store(false, Ordering::Relaxed);
        });

        Ok(())
    }

    pub fn stop(&self) {
        let _ = self.shutdown_tx.send(true);
        self.running.store(false, Ordering::Relaxed);
    }
}

fn write_handshake(path: &std::path::Path, config: &HookConfig, generation: u64) {
    let body = serde_json::json!({
        "version": HOOK_PROTOCOL_VERSION,
        "host": config.host,
        "port": config.port,
        "token": config.token.as_deref().unwrap_or(""),
        "pid": std::process::id(),
        "timeoutMs": config.timeout_secs as u64 * 1000,
        // The plugin counts the rounds, so it needs the ceiling. Carried here
        // rather than configured on that side because this is where the user
        // already sets the model and the timeout.
        "maxRounds": config.max_rounds,
        "generation": generation,
    });
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Written to a sibling and renamed over the target, for the same reason the
    // plugin writes its own state that way: two generations overlapping would
    // otherwise leave a half-written file, and a reader that cannot parse it
    // concludes the service is not running.
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let staged = std::fs::write(&tmp, body.to_string()).and_then(|()| std::fs::rename(&tmp, path));
    match staged {
        // The token is in this file, so say where it is and nothing else.
        Ok(()) => tracing::info!(path = %path.display(), generation, "hook handshake written"),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            tracing::error!(error = %e, path = %path.display(), "failed to write hook handshake");
        }
    }
}

/// Delete the handshake only if this generation is still the one it names.
///
/// Anything else — a newer generation, an unreadable file, one with no
/// generation at all — is left alone. A stale file costs one failed connection
/// that the plugin reports accurately; deleting a live server's file costs a
/// silently disabled gate until the next restart.
fn remove_handshake_if_ours(path: &std::path::Path, generation: u64) {
    let owner = std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("generation").and_then(serde_json::Value::as_u64));

    match owner {
        Some(found) if found == generation => {
            let _ = std::fs::remove_file(path);
            tracing::debug!(path = %path.display(), generation, "hook handshake removed");
        }
        other => tracing::debug!(
            path = %path.display(),
            generation,
            found = ?other,
            "hook handshake left in place; it belongs to someone else"
        ),
    }
}

/// Start the server if the user has it enabled, and hand it back either way.
///
/// Returned rather than registered here: the caller is the shell, and where the
/// IPC commands look this up is its business. Nothing inside `HookServer` knows
/// a window exists, and this is the last place that could have.
pub async fn maybe_start(services: Services, config: HookConfig) -> AppHooks {
    let enabled = config.enabled;

    let server = HookServer::new(services, config);

    if enabled {
        if let Err(e) = server.start() {
            tracing::error!(error = %e, "failed to auto-start hook server");
        }
    } else {
        tracing::info!("hook server disabled, skipping auto-start");
    }

    AppHooks(Arc::new(Mutex::new(server)))
}

pub struct AppHooks(pub Arc<Mutex<HookServer>>);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;
    use diesel::RunQueryDsl;

    fn set_preference(pool: &DbPool, key: &str, value: &str) {
        let mut conn = pool.get().unwrap();
        crate::db::ops::preference::set_preference(&mut conn, key, value, 1).unwrap();
    }

    #[test]
    fn absent_hook_preferences_keep_the_documented_defaults() {
        let config = load_config(&test_db()).unwrap();
        let expected = HookConfig::default();

        assert_eq!(config.enabled, expected.enabled);
        assert_eq!(config.host, expected.host);
        assert_eq!(config.port, expected.port);
        assert_eq!(config.timeout_secs, expected.timeout_secs);
        assert_eq!(config.max_rounds, expected.max_rounds);
    }

    #[test]
    fn malformed_hook_preferences_are_not_defaulted() {
        for (key, value) in [
            ("hooks.enabled", "yes"),
            ("hooks.port", "08765"),
            ("hooks.plan_review.timeout_secs", "ten"),
            ("hooks.plan_review.max_rounds", "-1"),
        ] {
            let pool = test_db();
            set_preference(&pool, key, value);
            let error = load_config(&pool).expect_err("malformed stored preference must fail");
            assert!(error.contains(key), "{key}: {error}");
        }
    }

    #[test]
    fn hook_preference_read_errors_are_not_defaulted() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        diesel::sql_query("DROP TABLE preferences").execute(&mut conn).unwrap();
        drop(conn);

        let error = load_config(&pool).expect_err("database errors must fail config loading");
        assert!(error.contains("hooks.enabled"), "{error}");
    }

    fn handshake_with(dir: &std::path::Path, generation: u64) -> std::path::PathBuf {
        let path = dir.join(HANDSHAKE);
        write_handshake(&path, &HookConfig::default(), generation);
        path
    }

    #[test]
    fn the_handshake_advertises_the_strict_protocol_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = handshake_with(dir.path(), 1);
        let body: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();

        assert_eq!(body["version"], HOOK_PROTOCOL_VERSION);
    }

    #[test]
    fn a_departing_server_deletes_only_its_own_handshake() {
        let dir = tempfile::tempdir().unwrap();
        let path = handshake_with(dir.path(), 1);

        // The generation that has been replaced tries to clean up last.
        remove_handshake_if_ours(&path, 1);
        assert!(!path.exists(), "its own file should go");

        // Now the case that used to disable the gate: generation 2 is live and
        // generation 1 wakes up late.
        let path = handshake_with(dir.path(), 2);
        remove_handshake_if_ours(&path, 1);
        assert!(path.exists(), "a live server's file must survive a stale cleanup");
    }

    #[test]
    fn a_handshake_that_names_nobody_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(HANDSHAKE);

        for body in ["not json at all", r#"{"port":8765}"#] {
            std::fs::write(&path, body).unwrap();
            remove_handshake_if_ours(&path, 1);
            assert!(path.exists(), "should not delete on `{body}`");
        }
    }

    /// Zero survives, because "never give up on a count" is a real answer here
    /// and not the same as "unset".
    #[test]
    fn the_round_limit_allows_no_limit_at_all() {
        assert_eq!(clamp_rounds(0), 0);
        assert_eq!(clamp_rounds(5), 5);
        assert_eq!(clamp_rounds(999), MAX_MAX_ROUNDS);
    }

    /// A value stored before the ceiling existed (the author's own config held
    /// 1200) has to come back clamped, not honoured.
    #[test]
    fn the_review_timeout_cannot_outlive_the_layers_above_it() {
        assert_eq!(clamp_timeout(3600), MAX_TIMEOUT_SECS);
        assert_eq!(clamp_timeout(1200), 1200);
        assert_eq!(clamp_timeout(300), 300);
        assert_eq!(clamp_timeout(0), 10);
    }
}

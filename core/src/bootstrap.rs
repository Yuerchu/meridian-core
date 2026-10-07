//! Bringing the app up, given somewhere to put it.
//!
//! Everything here used to live in Tauri's `setup` closure, where it was mixed
//! with tray icons and window handles. The two have nothing to do with each
//! other: opening the database, running migrations and seeding built-ins are the
//! same work whether a window follows or not. What the shell still owns is the
//! one thing only it can answer — where `data_dir` is — so that arrives as an
//! argument and everything downstream is framework-free.
//!
//! A malformed first-party startup preference is returned to the shell so the
//! app can report the exact contract error instead of booting with a guessed
//! default. Lower-level failures that make the database unusable still panic.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::agent::provider_secret_name;
use crate::db::models::assistant::{AssistantChangeset, AssistantInsert};
use crate::db::models::provider::ProviderInsert;
use crate::events::EventBus;
use crate::secrets::{SecretName, SecretScope, SecretsManager};
use crate::services::{Paths, Services, ServicesInner};
use crate::sleep_inhibitor::AppSleepInhibitor;
use crate::state::{AppSubAgentInboxes, ApprovalWaiters, VoiceState};
use crate::util::now_ms;
use crate::{agent, db, mcp, tools, turn};

fn parse_tool_preset_names(preset_id: &str, json: &str) -> Result<Vec<String>, String> {
    serde_json::from_str(json).map_err(|error| format!("tool preset `{preset_id}` has invalid `tool_names`: {error}"))
}

/// Open everything the app runs on, in the order it has to happen.
///
/// The secrets manager is the OS keychain one, which is right wherever there is
/// a login session to hold it. [`bootstrap_with_secrets`] is the same startup
/// for a machine that has none.
pub async fn bootstrap(data_dir: PathBuf, events: EventBus) -> Result<Services, String> {
    let secrets = Arc::new(SecretsManager::new(data_dir.clone()));
    bootstrap_with_secrets(data_dir, events, secrets).await
}

/// The same startup, told where the secrets come from.
///
/// One parameter rather than a second copy of this function: every line below
/// is the same work whether or not a window follows, and a headless build that
/// forked it would drift — the migrations, the seeds and the skill index are
/// exactly the things that must not differ between the two.
///
/// What differs is only where the secrets-file passphrase comes from, and only
/// because a server has no keychain to keep it in. See
/// [`crate::keyring::SuppliedPassphraseStore`].
///
/// Async because the SeaORM pool opens asynchronously; the Diesel work below is
/// still blocking, which is fine for the one call that happens before anything
/// else is running. Core never blocks on a future itself: the shell and
/// meridiand each cross from sync to async once, at their own top.
pub async fn bootstrap_with_secrets(
    data_dir: PathBuf,
    events: EventBus,
    mgr: Arc<SecretsManager>,
) -> Result<Services, String> {
    std::fs::create_dir_all(&data_dir).expect("failed to create app data dir");
    crate::logging::attach_file_sink(&data_dir);

    // Skills live in app-private storage, so unlike project instructions
    // this path stays usable on Android without a SAF grant.
    let skills_root = data_dir.join("skills");
    std::fs::create_dir_all(&skills_root).expect("failed to create skills dir");

    let db_path = data_dir.join("meridian.db");
    // The schema first, on a connection of its own: a database Diesel migrated
    // is bridged to the SeaORM baseline, a new one is built from it. Nothing
    // below opens the file before this has returned.
    let ledger = db::sea::bridge::migrate_file(&db_path)
        .await
        .map_err(|error| format!("could not migrate the database: {error}"))?;
    if let db::sea::bridge::Ledger::Diesel { applied } = ledger {
        tracing::info!(
            diesel_migrations = applied,
            "bridged the database to the SeaORM baseline"
        );
    }
    let pool = db::init_db(db_path.to_str().expect("invalid db path"));
    let plan_files = Arc::new(crate::plan_files::PlanFileStore::new(&data_dir));
    startup_recovery(&pool, &plan_files);
    let sea = db::sea::open(&db_path)
        .await
        .map_err(|error| format!("could not open the database through SeaORM: {error}"))?;
    // The preference lives in the database, so the first few lines above
    // are recorded at the default level.
    crate::logging::apply_saved_level(&sea).await?;

    // Create default assistant on first run
    {
        let mut conn = pool.get().expect("db connection");
        if db::ops::assistant::get_default_assistant(&mut conn)
            .ok()
            .flatten()
            .is_none()
        {
            let id = uuid::Uuid::new_v4().to_string();
            let now = now_ms();
            let _ = db::ops::assistant::create_assistant(
                &mut conn,
                &AssistantInsert {
                    id: &id,
                    name: "Default",
                    description: None,
                    avatar: None,
                    system_prompt: "You are a helpful assistant.",
                    provider_id: None,
                    model_id: None,
                    temperature: None,
                    top_p: None,
                    max_tokens: None,
                    is_default: 1,
                    sort_order: 0,
                    created_at: now,
                    updated_at: now,
                    // No override: 0 is "the model's own window". 128000 here
                    // outranked the window of every model the default
                    // assistant was pointed at, and nobody chose it.
                    context_limit: 0,
                    compact_keep_recent: 10,
                    enabled_tools: None,
                    thinking_enabled: 0,
                    thinking_budget: None,
                    tool_preset_id: None,
                    auto_compact_enabled: 0,
                },
            );
        }
    }

    // Migrate legacy secrets-based provider to DB
    {
        let mut conn = pool.get().expect("db connection");
        let count = db::ops::provider::count_providers(&mut conn).unwrap_or(0);
        if count == 0
            && let Some(api_key) = mgr
                .get(&SecretScope::Global, &SecretName::new("API_KEY").unwrap())
                .ok()
                .flatten()
        {
            let provider_type = mgr
                .get(&SecretScope::Global, &SecretName::new("PROVIDER_TYPE").unwrap())
                .ok()
                .flatten()
                .unwrap_or_else(|| "openai".into());
            let base_url = mgr
                .get(&SecretScope::Global, &SecretName::new("API_BASE").unwrap())
                .ok()
                .flatten()
                .unwrap_or_else(|| "https://api.openai.com/v1".into());
            let model = mgr
                .get(&SecretScope::Global, &SecretName::new("MODEL").unwrap())
                .ok()
                .flatten();

            let pid = uuid::Uuid::new_v4().to_string();
            let now = now_ms();
            if let Ok(provider) = db::ops::provider::create_provider(
                &mut conn,
                &ProviderInsert {
                    id: &pid,
                    name: "Default",
                    provider_type: &provider_type,
                    base_url: &base_url,
                    is_enabled: 1,
                    sort_order: 0,
                    created_at: now,
                    updated_at: now,
                    api_format: "chat_completions",
                    // Both of these came out of environment variables, so this
                    // is as likely to be a relay as the vendor itself.
                    // `identify` answers only when it is certain.
                    catalog_id: crate::provider::catalog::identify(&provider_type, &base_url),
                    // The variables this row is migrated from only ever carried
                    // an API key against an ordinary endpoint.
                    credential_kind: "api_key",
                    transport_profile: "standard",
                    // No choice was made, so the mark follows whatever
                    // `identify` decided this row is.
                    icon: None,
                    // `chat_completions` above has no Codex shape to follow,
                    // and nothing here has said the address is a relay for one.
                    codex_request_shape: 0,
                },
            ) {
                let key_name = provider_secret_name(&provider.id);
                let _ = mgr.set(&SecretScope::Global, &SecretName::new(&key_name).unwrap(), &api_key);
                // Link default assistant to this provider
                if let Ok(Some(default_assistant)) = db::ops::assistant::get_default_assistant(&mut conn) {
                    let changeset = AssistantChangeset {
                        provider_id: Some(Some(provider.id.clone())),
                        model_id: model.map(Some),
                        updated_at: Some(now),
                        ..Default::default()
                    };
                    let _ = db::ops::assistant::update_assistant(&mut conn, &default_assistant.id, &changeset);
                }
            }
        }
    }

    seed_tool_catalog(&sea).await?;

    let redaction = Arc::new(crate::redaction::RedactionEngine::new());
    redaction.reload(&sea).await?;

    // Load custom tools from DB into tool registry
    let registry = tools::ToolRegistry::new(skills_root.clone(), data_dir.join("logs"), redaction.clone());
    match db::sea::ops::custom_tool::list_enabled_tools(&sea).await {
        Ok(custom_tools) => registry.set_custom_tools(
            custom_tools
                .iter()
                .map(|ct| Arc::new(tools::custom::CustomToolExecutor::from_db(ct)) as Arc<dyn tools::Tool>)
                .collect(),
        ),
        // Silently leaves the registry with no custom tools at all,
        // which the user reads as "my tools are gone". A row with an
        // invalid contract fails the read, so it lands here too.
        Err(e) => tracing::error!(error = %e, "custom tools could not be loaded at startup"),
    }

    // Regenerate the manual against this build's tool set, then index
    // every skill on disk. Order matters: the manual has to exist before
    // the scan or it will not be picked up until the next launch.
    {
        // Run the mode filter even though the manual is mode-agnostic:
        // a mode's exit tool lives in the registry but is only offered
        // inside that mode, so listing it here would point the model at
        // a tool that gets refused in every ordinary conversation.
        let mut tool_defs = agent::tool_defs::collect(&registry, Vec::new(), None);
        agent::tool_defs::apply_mode(
            &mut tool_defs,
            // Switchable: the manual describes what a desktop
            // conversation can do, and entering plan mode is part of it.
            agent::modes::Modes::Switchable(agent::modes::resolve(None).expect("work mode must exist")),
            &registry,
        );
        if let Err(e) = agent::manual::write_manual(&skills_root, &tool_defs) {
            tracing::error!(error = %e, "failed to write the manual skill");
        }
        if let Err(e) = agent::diagnostics::write_diagnostics(&skills_root) {
            tracing::error!(error = %e, "failed to write the diagnostics skill");
        }
        let mut conn = pool.get().expect("db connection");
        if let Err(e) = agent::skills::sync_index(&mut conn, &skills_root) {
            tracing::error!(error = %e, "failed to index skills");
        }
        // After the index, which creates the rows the bindings point at.
        agent::skills::seed_builtin_bindings(&mut conn);
    }

    Ok(Services::new(ServicesInner {
        db: pool,
        sea,
        secrets: mgr,
        tools: Arc::new(registry),
        mcp: mcp::McpRegistry::new(),
        turns: Arc::new(turn::TurnCoordinator::new()),
        approvals: ApprovalWaiters::new(),
        sub_agent_inboxes: AppSubAgentInboxes::default(),
        compact_breakers: Mutex::new(HashMap::new()),
        voice: VoiceState::new(),
        // 语料目录的锁在这里被拿到，或者拿不到。拿不到不是启动失败——它只是
        // 让采集整个停用，而应用的其余部分与语料无关。
        corpus: Arc::new(crate::voice_corpus::CorpusCoordinator::new(&data_dir)),
        voice_limiter: Arc::new(crate::tts::limiter::VoiceLimiter::default()),
        sleep: AppSleepInhibitor::new(),
        events,
        alert_sinks: crate::notify::AlertSinks::new(),
        paths: Paths { data_dir, skills_root },
        plan_files,
        #[cfg(not(target_os = "android"))]
        acp: crate::acp::AcpRegistry::new(),
        #[cfg(not(target_os = "android"))]
        containers: crate::container::DockerConnector::new(Default::default()),
        // Filled by the shell, which is the only half that knows how to run a
        // turn. Left empty a follow-up is never delivered, which is the right
        // way for this to be missing.
        turn_starter: std::sync::OnceLock::new(),
        journal_shared: crate::journal::capture::JournalShared::new(),
        redaction,
        redaction_mappings: crate::redaction::RedactionMappings::new(),
    }))
}

/// The built-in tool categories and presets, and the repairs earlier seeds
/// need.
///
/// Categories are seeded only into an empty table; presets per id, so an
/// existing install picks up a preset added since. A single row that will not
/// insert is skipped, as it always was. A preset that will not *decode* is not:
/// it fails startup, because the repair below would otherwise have to guess
/// what the list meant.
async fn seed_tool_catalog(sea: &db::sea::cap::Db) -> Result<(), String> {
    use crate::db::entity::{tool_category, tool_preset};
    use crate::db::sea::ops;
    use crate::db::types::{Json, SqlBool};

    let unreadable = |error: sea_orm::DbErr| format!("could not read the tool presets: {error}");

    if ops::tool_category::count_categories(sea).await.unwrap_or(0) == 0 {
        let now = now_ms();
        let cats = [
            ("cat_interaction", "Interaction", "User interaction tools", 0),
            ("cat_filesystem", "Filesystem", "File and directory operations", 1),
            ("cat_system", "System", "System and shell commands", 2),
            ("cat_coding", "Coding", "Code analysis and editing", 3),
        ];
        for (id, name, desc, order) in cats {
            let row = tool_category::Model {
                id: id.into(),
                name: name.into(),
                description: Some(desc.into()),
                icon: None,
                sort_order: order,
                created_at: now,
            };
            let _ = sea
                .write(async |tx| ops::tool_category::create_category(tx, row).await)
                .await;
        }
    }

    let now = now_ms();
    let presets = [
        (
            "preset_coding",
            "Coding Agent",
            "All tools for coding tasks",
            r#"["ask_user","update_todos","read_file","write_file","edit_file","apply_patch","run_command","list_directory","search_files","glob","read_app_logs"]"#,
            0,
        ),
        (
            "preset_research",
            "Research",
            "Minimal tools for research and reading",
            r#"["ask_user","read_file","list_directory","search_files","glob","web_search","read_app_logs"]"#,
            1,
        ),
        (
            "preset_writing",
            "Writing",
            "Tools for writing and editing files",
            r#"["ask_user","read_file","write_file","edit_file"]"#,
            2,
        ),
    ];
    // Seed per id (not only on an empty table) so existing installs
    // pick up newly added built-in presets.
    for (id, name, desc, tools_json, order) in presets {
        if ops::tool_preset::get_preset(sea, id)
            .await
            .map_err(unreadable)?
            .is_some()
        {
            continue;
        }
        let row = tool_preset::Model {
            id: id.into(),
            name: name.into(),
            description: Some(desc.into()),
            icon: None,
            tool_names: Json(parse_tool_preset_names(id, tools_json)?),
            is_builtin: SqlBool::TRUE,
            sort_order: order,
            created_at: now,
            updated_at: now,
        };
        let _ = sea
            .write(async |tx| ops::tool_preset::create_preset(tx, row).await)
            .await;
    }
    // Repair presets from earlier seeds: "glob_files" never existed
    // (real tool name is "glob"), the built-in Research preset
    // gained web_search, and Coding gained update_todos.
    for p in ops::tool_preset::list_presets(sea).await.map_err(unreadable)? {
        let builtin = p.is_builtin.get();
        let mut names = p.tool_names.into_inner();
        let mut changed = false;
        for n in names.iter_mut() {
            if n == "glob_files" {
                *n = "glob".into();
                changed = true;
            }
        }
        if p.id == "preset_research" && builtin && !names.iter().any(|n| n == "web_search") {
            names.push("web_search".into());
            changed = true;
        }
        if p.id == "preset_coding" && builtin && !names.iter().any(|n| n == "update_todos") {
            names.push("update_todos".into());
            changed = true;
        }
        // Diagnosing a failure is useful in both, and this
        // backfill is what reaches installs that already ran
        // the seed above.
        if matches!(p.id.as_str(), "preset_coding" | "preset_research")
            && builtin
            && !names.iter().any(|n| n == "read_app_logs")
        {
            names.push("read_app_logs".into());
            changed = true;
        }
        if changed {
            let changeset = tool_preset::ToolPresetChangeset {
                tool_names: Some(Json(names)),
                updated_at: Some(now),
                ..Default::default()
            };
            let _ = sea
                .write(async |tx| ops::tool_preset::update_preset(tx, &p.id, changeset).await)
                .await;
        }
    }
    Ok(())
}

/// Repairs and housekeeping that need the migrated schema and have to happen
/// before anything can enqueue or start a turn, in the order they ran when they
/// lived in `init_db`, followed by the plan files. Each one logs and carries on:
/// none of them is a reason to keep the app from starting.
///
/// A list, not a loop over anything, so a change of order or a dropped item
/// shows up in review; `a_turn_killed_by_a_crash_holds_its_queue_at_the_next_start`
/// fails if the interrupted-turn reconciliation stops running at startup.
pub(crate) fn startup_recovery(pool: &db::DbPool, plan_files: &crate::plan_files::PlanFileStore) {
    let mut conn = pool.get().expect("db connection");
    let now = now_ms();

    // Memories no longer hang off projects by foreign key, and migrations run
    // with foreign keys off anyway, so a table rebuild can leave orphans behind.
    match db::ops::plan_review::backfill_legacy_artifacts(&mut conn, now) {
        Ok(0) => {}
        Ok(n) => tracing::info!(documents = n, "backfilled legacy plan artifacts"),
        // The old rows remain readable through their existing path, so this is
        // diagnosable degradation rather than a reason to make the database
        // unavailable. The next startup retries the idempotent backfill.
        Err(error) => tracing::error!(error = %error, "could not backfill legacy plan artifacts"),
    }
    match db::ops::plan_review::reconcile_dispatched_deliveries(&mut conn, now) {
        Ok(0) => {}
        Ok(n) => tracing::warn!(deliveries = n, "reconciled plan review deliveries after restart"),
        Err(error) => tracing::error!(error = %error, "could not reconcile plan review deliveries"),
    }
    let orphans = db::ops::memory::purge_orphan_project_memories(&mut conn).unwrap_or(0);
    let proposals = db::ops::memory::expire_proposals(&mut conn, now).unwrap_or(0);
    // Bounded-growth housekeeping. Kept off the write path: neither sweep
    // depends on what was just written, and the trash purge has no usable index
    // (both are partial on `deleted_at IS NULL`), so doing it per write meant a
    // full table scan each time.
    let swept = db::ops::memory::sweep_untracked_subjects(&mut conn, now).unwrap_or(0);
    // Startup housekeeping deletes rows the user may later go looking for. When
    // it removed nothing there is nothing to say, but when it did, this is the
    // only record that it happened.
    if orphans > 0 || proposals > 0 || swept > 0 {
        tracing::info!(
            orphan_memories_deleted = orphans,
            proposals_expired = proposals,
            subjects_swept = swept,
            "startup housekeeping removed rows"
        );
    }

    // Turns only ever run inside the process that recorded them, so anything
    // still marked running was killed rather than finished. This is the only
    // moment that fact is knowable — after this the row would just look like a
    // turn that has been going for a very long time.
    match db::ops::turn::reconcile_interrupted(&mut conn, now) {
        Ok(0) => {}
        Ok(n) => tracing::info!(turns = n, "turns left running by the previous session"),
        // Not fatal: it costs the diagnosis, not the conversation.
        Err(e) => tracing::error!(error = %e, "could not reconcile interrupted turns"),
    }

    match plan_files.reconcile_all(&mut conn, now_ms()) {
        Ok(reports) => {
            let conflicts = reports.iter().filter(|(_, report)| report.conflict.is_some()).count();
            if conflicts > 0 {
                tracing::warn!(documents = conflicts, "plan files require explicit conflict recovery");
            }
        }
        Err(error) => tracing::error!(error = %error, "could not reconcile durable plan files"),
    }
}

/// Reconnect whatever the user marked for auto-connect.
///
/// Detached from startup, so a server that takes ten seconds does not hold up
/// the window, and concurrent, so the slowest one does not decide when the rest
/// come up. Going through the same entry point as the settings page matters: its
/// idempotence is what stops this and a hand-clicked Connect from starting two
/// processes for one server.
pub async fn reconnect_mcp(services: Services) {
    let servers = match db::sea::ops::mcp_server::list_enabled_mcp_servers(&services.sea).await {
        Ok(servers) => servers,
        // A row that does not decode fails the whole list, and with it every
        // auto-connect; said here, since nobody is looking at the settings
        // page yet.
        Err(error) => {
            tracing::error!(error = %error, "could not read the MCP servers to reconnect");
            return;
        }
    };
    if servers.is_empty() {
        return;
    }
    let registry = services.mcp.clone();
    let attempts = servers.into_iter().map(|server| {
        let registry = registry.clone();
        async move {
            // Logged rather than surfaced: nobody is looking at
            // the settings page yet, and one broken server must
            // not stop the others from coming up.
            if let Err(e) = registry.connect(&server).await {
                tracing::warn!(
                    server_id = %server.id,
                    server_name = %server.name,
                    error = %e,
                    "MCP server failed to auto-connect at startup"
                );
            }
        }
    });
    futures::future::join_all(attempts).await;
}

/// Resume queues whose native plan continuation durably finished before the
/// previous process could acknowledge it. Ordinary queues still never pump at
/// startup; these rows carry an explicit, persisted user decision and a Done
/// continuation turn, so leaving them behind would let a later direct prompt
/// overtake the already queued follow-up.
pub async fn resume_completed_plan_review_queues(services: Services) {
    let pool = services.db.clone();
    let resumes = tokio::task::spawn_blocking(move || {
        let mut conn = pool.get().map_err(|error| error.to_string())?;
        crate::db::ops::plan_review::list_startup_queue_resumes(&mut conn).map_err(|error| error.to_string())
    })
    .await;
    let resumes = match resumes {
        Ok(Ok(resumes)) => resumes,
        Ok(Err(error)) => {
            tracing::warn!(%error, "could not read plan-review queue resumes at startup");
            return;
        }
        Err(error) => {
            tracing::warn!(%error, "reading plan-review queue resumes panicked");
            return;
        }
    };

    for (delivery_id, conversation_id) in resumes {
        let before = next_pending_queue_id(&services, &conversation_id).await;
        crate::agent::queue::pump(&services, &conversation_id).await;
        let after = next_pending_queue_id(&services, &conversation_id).await;
        let progressed = match (before, after) {
            (Ok(before), Ok(after)) => queue_resume_progressed(before.as_deref(), after.as_deref()),
            (Err(error), _) | (_, Err(error)) => {
                tracing::warn!(%error, conversation_id, "could not verify plan-review queue resume progress");
                false
            }
        };
        if !progressed {
            // Keep the durable marker. A missing starter, a busy lease or a
            // failed turn start all leave the same queue head untouched; the
            // next startup must be allowed to try this explicit user-authored
            // continuation again.
            tracing::warn!(conversation_id, "plan-review queue resume made no durable progress");
            continue;
        }
        let pool = services.db.clone();
        let cleared = tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|error| error.to_string())?;
            crate::db::ops::plan_review::finish_startup_queue_resume(&mut conn, &delivery_id, now_ms())
                .map_err(|error| error.to_string())
        })
        .await;
        if !matches!(cleared, Ok(Ok(true))) {
            tracing::warn!(
                conversation_id,
                "could not clear a completed plan-review queue resume marker"
            );
        }
    }
}

async fn next_pending_queue_id(services: &Services, conversation_id: &str) -> Result<Option<String>, String> {
    let pool = services.db.clone();
    let conversation_id = conversation_id.to_string();
    tokio::task::spawn_blocking(move || {
        let mut conn = pool.get().map_err(|error| error.to_string())?;
        crate::db::ops::queue::next_pending(&mut conn, &conversation_id)
            .map(|row| row.map(|row| row.id))
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

fn queue_resume_progressed(before: Option<&str>, after: Option<&str>) -> bool {
    before.is_none() || before != after
}

#[cfg(test)]
mod tests {
    use super::{parse_tool_preset_names, queue_resume_progressed};

    #[test]
    fn tool_preset_names_require_an_array_of_strings() {
        assert_eq!(
            parse_tool_preset_names("preset", r#"["read_file","glob"]"#).unwrap(),
            ["read_file", "glob"]
        );
        assert!(parse_tool_preset_names("preset", "{").is_err());
        assert!(parse_tool_preset_names("preset", r#"{"tool":"read_file"}"#).is_err());
        assert!(parse_tool_preset_names("preset", r#"["read_file",1]"#).is_err());
    }

    /// A crash leaves a turn marked running and a follow-up queued behind it.
    /// The next start must hold that queue before anything can deliver it: the
    /// follow-up was written against a turn that never finished. This goes
    /// through the real startup, so it fails if the reconciliation stops being
    /// part of it, wherever it lives.
    #[tokio::test]
    async fn a_turn_killed_by_a_crash_holds_its_queue_at_the_next_start() {
        use std::sync::Arc;

        use crate::db::models::queue::{Delivery, QueueState};
        use crate::db::ops::{conversation, queue, turn};
        use crate::keyring::SuppliedPassphraseStore;
        use crate::secrets::SecretsManager;

        // As the shell and meridiand do before bootstrapping: the saved log level
        // is applied during startup and needs the subscriber in place.
        crate::logging::init_early();
        let dir = tempfile::tempdir().unwrap();
        {
            // The previous session, on the previous release: migrated by
            // Diesel, a turn running, a follow-up queued behind it, and then
            // the process was killed. Today's start has to bridge the file
            // before it can reconcile it.
            let path = dir.path().join("meridian.db");
            crate::db::sea::bridge::previous_release_file(&path, crate::db::sea::legacy::LEGACY.len())
                .await
                .unwrap();
            let pool = crate::db::init_db(path.to_str().unwrap());
            let mut conn = pool.get().unwrap();
            conversation::create_conversation(&mut conn, "c1", Some("t"), None, None, 1000).unwrap();
            turn::begin(&mut conn, "t1", "c1", crate::turn::TurnOrigin::Desktop, None, 1000).unwrap();
            queue::enqueue(&mut conn, "q1", "c1", "now rename it", Delivery::FollowUp, 1).unwrap();
        }

        let store = SuppliedPassphraseStore::new("a passphrase for the test only", "test").unwrap();
        let secrets = Arc::new(SecretsManager::new_with_keyring_store(
            dir.path().to_path_buf(),
            Arc::new(store),
        ));
        let services = super::bootstrap_with_secrets(dir.path().to_path_buf(), crate::events::EventBus::new(), secrets)
            .await
            .unwrap();

        let mut conn = services.db.get().unwrap();
        assert_eq!(queue::list(&mut conn, "c1").unwrap()[0].state(), QueueState::Held);
        assert!(
            queue::next_pending(&mut conn, "c1").unwrap().is_none(),
            "nothing is delivered on the premise of a turn that never finished",
        );
    }

    #[test]
    fn startup_queue_resume_marker_is_kept_until_the_head_advances() {
        assert!(!queue_resume_progressed(Some("queue-1"), Some("queue-1")));
        assert!(queue_resume_progressed(Some("queue-1"), Some("queue-2")));
        assert!(queue_resume_progressed(Some("queue-1"), None));
        assert!(queue_resume_progressed(None, None));
    }
}

//! Hosting another coding agent, over the Agent Client Protocol.
//!
//! Meridian is the ACP *client*: it starts `claude-code-acp` as a child process
//! and speaks JSON-RPC to it over stdio. The turn runs in the adapter; what this
//! app owns is the transcript, the approval cards and the window.
//!
//! This is not the shape the roadmap's "correct a common wrong turn" paragraph
//! rejected. That one is `~/.claude/ide/*.lock`, where the *editor* is the
//! server and Claude Code connects to it for selections and diffs — which
//! indeed carries no session lifecycle. ACP is the other way round, and the
//! lifecycle is the protocol.
//!
//! Desktop only. Every session is a child process, which Android does not have.

pub mod approvals;
pub mod bridge;
pub mod elicitation;
pub mod import;
pub mod mapping;
pub mod mounts;
pub mod peer;
mod plan_review;
pub mod process;
pub mod protocol;
pub mod session;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::db::DbPool;

pub use plan_review::{AcpPlanReviewDelivery, AcpPlanReviewDeliveryOutcome};
pub use session::AcpSession;

/// The sessions running right now.
///
/// In memory, and deliberately not persisted. A session is a child process: when
/// this app exits, the adapter goes with it, and the `sessionId` it handed out
/// means nothing to the next one. Reopening a conversation after a restart would
/// need `session/load` — which the adapter does support — and that is the step
/// that earns a table to keep the id in. Until then the honest model is that a
/// hosted session lasts as long as the app does, while its transcript is an
/// ordinary conversation that outlives it.
#[derive(Default)]
pub struct AcpRegistry {
    /// Keyed by conversation, which is what every caller has: the id in the
    /// sidebar, in the approval card, in the IPC command.
    sessions: Mutex<HashMap<String, Arc<AcpSession>>>,
}

impl AcpRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn get(&self, conversation_id: &str) -> Option<Arc<AcpSession>> {
        self.lock().get(conversation_id).cloned()
    }

    /// Register a freshly opened session, and say which one won.
    ///
    /// Two callers can reach [`reopen_session`] for the same dormant
    /// conversation at once — two clients, or a window and a phone — and both
    /// will pass the "is there a live one?" check before either has registered
    /// anything. A plain insert lets the second overwrite the first, and the
    /// displaced session is then a running adapter that nothing holds a handle
    /// to: `close` cannot find it, `close_all` cannot find it, and it outlives
    /// the app.
    ///
    /// So the decision is made here, under the lock, and the loser is handed
    /// back to be closed rather than dropped. `Arc` going out of scope is not
    /// enough — the child belongs to a spawned task, which keeps running.
    ///
    /// Returns `(the registered session, the one to close)`.
    #[must_use = "the losing session owns a live adapter and has to be closed"]
    pub fn adopt(&self, session: Arc<AcpSession>) -> (Arc<AcpSession>, Option<Arc<AcpSession>>) {
        let mut map = self.lock();
        match map.get(&session.conversation_id) {
            // Somebody got there first and theirs still works. Keep it, so that
            // every later `cancel` and `close` names the process that is
            // actually serving this conversation.
            Some(live) if live.is_alive() => {
                let winner = live.clone();
                (winner, Some(session))
            }
            _ => {
                let displaced = map.insert(session.conversation_id.clone(), session.clone());
                // A dead one, if there was anything. Still worth closing: dead
                // to us means the pipe ended, which does not by itself mean the
                // process has been reaped.
                (session, displaced)
            }
        }
    }

    /// Everything the registry is holding, dead or alive.
    ///
    /// For shutting things down, where a dead entry is still worth closing —
    /// the pipe ending does not mean the process was reaped.
    pub fn conversations(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }

    /// The ones that could still answer.
    ///
    /// What a caller asking "is this session running" means. A session stays in
    /// the map after its adapter dies — nothing removes it until somebody calls
    /// `close` — so the unfiltered list reports an adapter that exited half an
    /// hour ago as live, and the composer offers knobs for a session that
    /// cannot take them.
    pub fn live_conversations(&self) -> Vec<String> {
        self.lock()
            .iter()
            .filter(|(_, session)| session.is_alive())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Take a session out and shut it down. Absent is success: closing twice is
    /// what a window and an app exit racing looks like.
    pub async fn close(&self, conversation_id: &str) {
        let session = self.lock().remove(conversation_id);
        if let Some(session) = session {
            session.close().await;
        }
    }

    /// Close whichever of these conversations has a session, for a caller
    /// holding a whole subtree.
    ///
    /// Deleting a conversation is the case: the rows go, and without this the
    /// adapter that was serving them keeps running with nothing left to serve
    /// and no way for anyone to reach it again.
    pub async fn close_each(&self, conversation_ids: &[String]) {
        for id in conversation_ids {
            self.close(id).await;
        }
    }

    /// Shut everything down, for app exit.
    ///
    /// Worth calling even though every child is `kill_on_drop`: that only fires
    /// if the value is actually dropped, and a process leaving through
    /// `std::process::exit` runs no destructors. Without this a few node
    /// processes outlive the app that started them.
    pub async fn close_all(&self) {
        let sessions: Vec<_> = self.lock().drain().map(|(_, s)| s).collect();
        for session in sessions {
            session.close().await;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<AcpSession>>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// How to start the adapter.
///
/// Not bundled. `claude-code-acp` is a node package and the machines that want
/// this feature already have node and `claude` on them; shipping a copy would
/// mean shipping a second Claude Code that ages separately from the one the
/// user actually logs into.
#[derive(Debug, Clone)]
pub struct AcpConfig {
    pub command: String,
    pub args: Vec<String>,
}

/// The adapter package.
///
/// Renamed from `@zed-industries/claude-code-acp`, which is deprecated on npm
/// and stopped at 0.16.2 — a version that announces a tool call twice from its
/// two sources instead of revising the first. The successor deduplicates
/// (`emittedToolCalls`), and is where updates now go.
const ADAPTER_PACKAGE: &str = "@agentclientprotocol/claude-agent-acp";

/// **Deliberately unpinned, and that is a standing hazard rather than an
/// oversight.** The package is a fast-moving `0.x` (64 releases by 0.70) that
/// wraps a `claude` updating on its own schedule, so pinning trades one kind of
/// drift for another: a pinned adapter falls behind the CLI it is a shim for.
///
/// What it costs is that everything this client knows about the adapter's
/// behaviour — that message chunks carry `messageId`, what `session/list`
/// returns, that a load recites the history at all — was measured against one
/// build and can change with no commit here. Two mitigations, both cheap:
/// `handshake` logs the version that answered, and anyone who wants a pin puts
/// one in `acp.args`, which is a user setting.
impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            // `-y` because the first launch on a machine would otherwise stop
            // on npx's install prompt, on a stdin that is a JSON-RPC pipe with
            // nobody to type into it.
            command: "npx".into(),
            args: vec!["-y".into(), ADAPTER_PACKAGE.into()],
        }
    }
}

impl AcpConfig {
    pub fn load(pool: &DbPool) -> Result<Self, String> {
        let mut conn = crate::util::get_conn(pool)?;
        let mut get = |key: &str| -> Result<Option<String>, String> {
            crate::db::ops::preference::get_preference(&mut conn, key)
                .map_err(|error| format!("failed to read preference {key}: {error}"))
        };

        let default = Self::default();
        Ok(Self {
            command: get("acp.command")?.unwrap_or(default.command),
            // Stored as JSON rather than a space-separated string: an argument
            // containing a space is ordinary on Windows, and splitting one back
            // apart would break a path under `Program Files`.
            args: match get("acp.args")? {
                Some(raw) => serde_json::from_str::<Vec<String>>(&raw)
                    .map_err(|error| format!("preference acp.args has invalid JSON: {error}"))?,
                None => default.args,
            },
        })
    }

    pub fn save(&self, pool: &DbPool) -> Result<(), String> {
        let mut conn = crate::util::get_conn(pool)?;
        let now = crate::util::now_ms();
        let mut set = |key: &str, value: &str| -> Result<(), String> {
            crate::db::ops::preference::set_preference(&mut conn, key, value, now).map_err(|e| e.to_string())
        };
        set("acp.command", &self.command)?;
        let args = serde_json::to_string(&self.args).map_err(|e| e.to_string())?;
        set("acp.args", &args)?;
        Ok(())
    }
}

/// Write down what a session opened as, so the next run can pick it up.
///
/// Called after every successful open, resumed or not. The id is the *agent's*
/// answer rather than what was asked for: a resume goes through the SDK and it
/// says which session it actually recovered.
///
/// A failure here is logged and not returned. The session is running and the
/// user is waiting on their message; what a lost write costs is one
/// conversation that starts fresh next time, which is what it did before this
/// table existed.
async fn remember_session(services: &crate::services::Services, session: &AcpSession) {
    let pool = services.db.clone();
    let conversation_id = session.conversation_id.clone();
    let acp_session_id = session.acp_session_id.clone();
    let cwd = session.cwd.clone();
    let written = tokio::task::spawn_blocking(move || {
        let mut conn = crate::util::get_conn(&pool)?;
        crate::db::ops::acp_session::upsert(
            &mut conn,
            &conversation_id,
            Some(&acp_session_id),
            &cwd,
            crate::util::now_ms(),
        )
        .map_err(|e| e.to_string())
    })
    .await;
    match written {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "could not record which agent session this conversation is"),
        Err(e) => tracing::warn!(error = %e, "recording the agent session panicked"),
    }
}

/// Open a new hosted session, and give it a conversation.
///
/// The adapter starts *first*. A conversation whose adapter never came up is a
/// row in the sidebar that can never be opened, and the usual reason for
/// failure — the command is not installed — is one every attempt would repeat.
pub async fn open_session(services: &crate::services::Services, cwd: &str) -> Result<String, String> {
    let config = AcpConfig::load(&services.db)?;
    let conversation_id = uuid::Uuid::new_v4().to_string();

    let session = AcpSession::open(services.clone(), &config, conversation_id.clone(), cwd.to_string()).await?;

    // The session is running but not yet in the registry, so a `?` here would
    // strand it: nothing holds a handle, nothing can close it, and the adapter
    // outlives the app's interest in it. Close it by hand — this is the one
    // window where the registry cannot do it for us.
    if let Err(e) = write_conversation_row(services, &conversation_id, cwd).await {
        session.close().await;
        return Err(e);
    }
    // The id was minted a few lines up and belongs to nobody else, so there is
    // no contest here — but going through the same door as `reopen_session`
    // keeps `insert`-that-silently-overwrites from existing at all.
    let (session, loser) = services.acp.adopt(session);
    if let Some(loser) = loser {
        loser.close().await;
    }
    remember_session(services, &session).await;

    let _ = services.events.emit_conversation_updated(&conversation_id);
    Ok(conversation_id)
}

/// Bring a conversation from an earlier run back to life.
///
/// The agent's own session is resumed when there is one on record and it still
/// exists — which is the whole point of `acp_sessions`. When it cannot be, the
/// adapter starts a fresh one in the same directory and the agent is told, on
/// its first prompt, that the transcript above is invisible to it. That is not
/// a nicety: a hosted prompt carries only the newest message, so a blind agent
/// stays blind and would otherwise answer as though it had been following along.
pub async fn reopen_session(
    services: &crate::services::Services,
    conversation_id: &str,
) -> Result<std::sync::Arc<AcpSession>, String> {
    if let Some(existing) = services.acp.get(conversation_id)
        && existing.is_alive()
    {
        return Ok(existing);
    }

    let pool = services.db.clone();
    let id = conversation_id.to_string();
    // Both facts in one read, because they are one row. `head_message_id`
    // answers the third question — whether there is anything above for the
    // agent to be blind to — and a conversation opened and never used has
    // nothing, so telling it would be noise.
    let (cwd, resume, transcript_above) = tokio::task::spawn_blocking(move || {
        let mut conn = crate::util::get_conn(&pool)?;
        let row = crate::db::ops::acp_session::get(&mut conn, &id)
            .map_err(|e| e.to_string())?
            .filter(|row| !row.cwd.trim().is_empty())
            .ok_or("this conversation has no recorded working directory; start a new session")?;
        let has_messages = crate::db::ops::conversation::get_conversation(&mut conn, &id)
            .ok()
            .and_then(|c| c.head_message_id)
            .is_some();
        Ok::<_, String>((row.cwd, row.acp_session_id, has_messages))
    })
    .await
    .map_err(|e| e.to_string())??;

    let config = AcpConfig::load(&services.db)?;
    let session = AcpSession::reopen(
        services.clone(),
        &config,
        conversation_id.to_string(),
        cwd,
        resume,
        transcript_above,
    )
    .await?;

    // The check at the top of this function is not a claim on the conversation,
    // and starting an adapter takes seconds — long enough for a second caller
    // to have passed the same check and be doing the same thing. Whoever
    // reaches the registry first wins; the other closes what it started and
    // uses the winner, so exactly one adapter is left running and it is the one
    // the registry names.
    let (session, loser) = services.acp.adopt(session);
    if let Some(loser) = loser {
        loser.close().await;
    }
    // After `adopt`, so the id written down is the session that won. The loser's
    // would name a process that is being shut down two lines up.
    remember_session(services, &session).await;
    Ok(session)
}

/// The sidebar row for a hosted session.
async fn write_conversation_row(
    services: &crate::services::Services,
    conversation_id: &str,
    cwd: &str,
) -> Result<(), String> {
    use crate::db::models::conversation::ConversationInsert;
    use diesel::Connection;

    let pool = services.db.clone();
    let conversation_id = conversation_id.to_string();
    let cwd = cwd.to_string();
    let title = title_for(&cwd);

    tokio::task::spawn_blocking(move || {
        let mut conn = crate::util::get_conn(&pool)?;
        let now = crate::util::now_ms();
        conn.transaction::<_, diesel::result::Error, _>(|conn| {
            // Same reasoning as a review conversation: file it under the project
            // that owns this directory when there is one, and leave it ungrouped
            // rather than inventing a project the user did not ask for.
            let project_id = crate::db::ops::project::find_project_by_path(conn, &cwd)?.map(|p| p.id);
            let assistant_id = crate::db::ops::assistant::get_default_assistant(conn)
                .ok()
                .flatten()
                .map(|a| a.id);

            crate::db::ops::conversation::insert(
                conn,
                ConversationInsert {
                    id: &conversation_id,
                    title: Some(&title),
                    assistant_id: assistant_id.as_deref(),
                    is_pinned: 0,
                    is_archived: 0,
                    created_at: now,
                    updated_at: now,
                    project_id: project_id.as_deref(),
                    parent_conversation_id: None,
                    spawned_by_message_id: None,
                    spawned_by_call_id: None,
                    spawned_turn_id: None,
                    agent_kind: Some(AGENT_KIND),
                    agent_provider_id: None,
                    agent_model_id: None,
                },
            )?;
            // The directory, in the same transaction as the row it belongs to.
            // No session id yet — the adapter has one by now, but this write
            // happens before `adopt` decides which session won, and a row
            // naming the loser is a row naming a process being killed. It
            // arrives a moment later through `remember_session`.
            crate::db::ops::acp_session::upsert(conn, &conversation_id, None, &cwd, now)?;
            Ok(())
        })
        .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// What `conversations.agent_kind` says for a hosted session. The sidebar reads
/// it to mark the row, and it is what tells a reopened conversation apart from
/// an ordinary one.
pub const AGENT_KIND: &str = "claude_code";

/// How long a configuration check may take before it is called a failure.
///
/// Generous, because the default command is `npx -y`, and the very first run on
/// a machine downloads the package before the adapter says anything at all.
/// Bounded, because the failure this catches is a command that starts and then
/// waits for something — a login prompt on a pipe with nobody at the other end —
/// which without a deadline would hang the settings page rather than answer it.
const CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// What a working adapter said about itself.
pub struct AdapterReport {
    pub agent: Option<String>,
    pub protocol_version: u32,
    pub load_session: bool,
}

/// Start an adapter, greet it, shut it down, and report what it said.
///
/// No session is opened: `initialize` is the whole handshake and the part that
/// fails when the command is wrong. Opening one would also need a directory,
/// which is not something a settings page should have to invent.
pub async fn check_adapter(config: &AcpConfig) -> Result<AdapterReport, String> {
    /// A session that never happens has no updates to handle and no questions
    /// to answer.
    struct Deaf;

    #[async_trait::async_trait]
    impl peer::Handler for Deaf {
        async fn notification(&self, _method: String, _params: serde_json::Value) {}

        async fn request(&self, method: String, _params: serde_json::Value) -> Result<serde_json::Value, String> {
            Err(format!("`{method}` arrived during a configuration check"))
        }
    }

    let process = process::AdapterProcess::spawn(&config.command, &config.args).await?;
    let peer = peer::Peer::start(process, Arc::new(Deaf) as Arc<dyn peer::Handler>);

    let greeting = tokio::time::timeout(
        CHECK_TIMEOUT,
        peer.request(
            "initialize",
            serde_json::to_value(protocol::InitializeParams {
                protocol_version: protocol::PROTOCOL_VERSION,
                client_capabilities: protocol::ClientCapabilities::default(),
                client_info: protocol::Implementation {
                    name: "meridian".into(),
                    title: Some("Meridian".into()),
                    version: env!("CARGO_PKG_VERSION").into(),
                },
            })
            .map_err(|e| e.to_string())?,
        ),
    )
    .await;

    // Shut down on every path, including the timeout: the point of a check is
    // that it leaves nothing behind.
    let outcome = match greeting {
        Ok(Ok(value)) => serde_json::from_value::<protocol::InitializeResult>(value)
            .map_err(|e| format!("the adapter's greeting could not be read: {e}"))
            .map(|init| AdapterReport {
                agent: init.agent_info.map(|a| format!("{} {}", a.name, a.version)),
                protocol_version: init.protocol_version,
                load_session: init.agent_capabilities.load_session,
            }),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!(
            "`{}` did not answer within {}s",
            config.command,
            CHECK_TIMEOUT.as_secs()
        )),
    };
    peer.stop().await;
    outcome
}

fn title_for(cwd: &str) -> String {
    let leaf = cwd.rsplit(['/', '\\']).find(|s| !s.is_empty()).unwrap_or("(unknown)");
    format!("Claude Code · {leaf}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    /// The directory a session is about is the useful half of its name, and it
    /// has to survive both separators — a Windows path reaches this with
    /// backslashes and a trailing one is ordinary.
    #[test]
    fn a_title_names_the_directory() {
        assert_eq!(title_for(r"C:\Users\me\Code\meridian"), "Claude Code · meridian");
        assert_eq!(title_for("/home/me/code/meridian/"), "Claude Code · meridian");
        assert_eq!(title_for(""), "Claude Code · (unknown)");
    }

    /// An argument with a space in it has to survive a round trip. Stored
    /// space-separated it would not, and the first Windows user with the
    /// adapter under `Program Files` would find out.
    #[test]
    fn arguments_survive_a_space() {
        let config = AcpConfig {
            command: "node".into(),
            args: vec![r"C:\Program Files\acp\index.js".into(), "--verbose".into()],
        };
        let encoded = serde_json::to_string(&config.args).unwrap();
        let decoded: Vec<String> = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, config.args);
    }

    #[test]
    fn the_default_does_not_stop_on_an_install_prompt() {
        let default = AcpConfig::default();
        assert_eq!(default.command, "npx");
        assert!(default.args.contains(&"-y".to_string()));
    }

    #[test]
    fn absent_acp_preferences_use_fresh_install_defaults() {
        let loaded = AcpConfig::load(&test_db()).unwrap();
        let expected = AcpConfig::default();
        assert_eq!(loaded.command, expected.command);
        assert_eq!(loaded.args, expected.args);
    }

    #[test]
    fn malformed_stored_acp_arguments_are_not_defaulted() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        crate::db::ops::preference::set_preference(&mut conn, "acp.args", "npx -y adapter", 1).unwrap();
        drop(conn);

        let error = AcpConfig::load(&pool).unwrap_err();
        assert!(error.contains("acp.args has invalid JSON"), "{error}");
    }
}

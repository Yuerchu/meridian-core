//! Everything long-lived, in one value.
//!
//! These used to be a dozen newtypes registered with Tauri one at a time and
//! fetched back with `app.state::<AppDb>()`, which made `AppHandle` the way to
//! reach the database — and so made the database unreachable without a window.
//! The handle is a service locator here and nothing more, so replacing it with
//! the services themselves costs nothing and is what lets a turn run under a
//! socket, a chat bot, or a test.
//!
//! Cloning is cheap and expected: the struct is one `Arc`, so a spawned task
//! takes a clone rather than borrowing, and nothing needs a lifetime.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::agent::CompactCircuitBreaker;
use crate::db::DbPool;
use crate::events::EventBus;
use crate::mcp;
use crate::secrets::SecretsManager;
use crate::sleep_inhibitor::AppSleepInhibitor;
use crate::state::{AppSubAgentInboxes, ApprovalWaiters, VoiceState};
use crate::tools;
use crate::turn::TurnCoordinator;

/// The directories the app owns, resolved once.
///
/// Only the shell can answer where these are — on Android it is a path no
/// constant could name — so they are resolved at startup and carried, rather
/// than each caller asking the framework again.
pub struct Paths {
    pub data_dir: PathBuf,
    /// App-private, unlike project instructions, so it stays usable on Android
    /// without a SAF grant.
    pub skills_root: PathBuf,
}

#[derive(Clone)]
pub struct Services(Arc<ServicesInner>);

pub struct ServicesInner {
    pub db: DbPool,
    pub secrets: Arc<SecretsManager>,
    pub tools: Arc<tools::ToolRegistry>,
    /// No outer mutex: the registry locks internally and never across I/O.
    /// Holding one here is what let a single slow MCP call stop every
    /// conversation in the app from assembling its tool set.
    pub mcp: Arc<mcp::McpRegistry>,
    /// Who currently holds each conversation. One table for every writer,
    /// desktop and OneBot alike: `start_onebot` rebuilds the OneBot server's
    /// whole shared state, and an occupancy table that reset when QQ restarted
    /// would hand out a conversation a desktop turn was still writing.
    pub turns: Arc<TurnCoordinator>,
    pub approvals: ApprovalWaiters,
    pub sub_agent_inboxes: AppSubAgentInboxes,
    pub compact_breakers: Mutex<HashMap<String, Arc<CompactCircuitBreaker>>>,
    pub voice: VoiceState,
    /// 谁现在可以往语音语料里写。
    ///
    /// 在这里而不是 OneBot 的 `SharedState` 里，因为 `start_onebot` 会整个重建
    /// 那份 state：跟着重建的协调器会把正在进行的采集和刚刚做出的撤权一起忘掉。
    /// 语料目录的独占锁也挂在它上面，所以它的生命周期必须是进程，不是某一代
    /// OneBot 服务。
    pub corpus: Arc<crate::voice_corpus::CorpusCoordinator>,
    /// 谁可以现在发一次语音。
    ///
    /// 在这里而不是 `QqToolExecutor` 上：那个每一轮重建，而"每轮最多一次"放在
    /// 一个每轮重建的东西里根本约束不住任何事。
    pub voice_limiter: Arc<crate::tts::limiter::VoiceLimiter>,
    pub sleep: AppSleepInhibitor,
    pub events: EventBus,
    pub paths: Paths,
    /// Crash-safe app-private `plan.md` materialisation shared by every runner.
    /// The shared value owns the per-document locks; constructing one per turn
    /// would only serialize each turn with itself.
    pub plan_files: Arc<crate::plan_files::PlanFileStore>,
    /// Hosted coding-agent sessions. Absent on Android, where a session — a
    /// child process — cannot exist.
    #[cfg(not(target_os = "android"))]
    pub acp: Arc<crate::acp::AcpRegistry>,
    /// Where a conversation's commands run, when that is not this machine.
    ///
    /// Held here because a container outlives every command that enters it and
    /// has to be findable again — `sandbox::execute` is a function taking
    /// parameters and has nowhere to keep one. Built at startup whether or not
    /// anybody has turned it on: constructing it costs nothing and reaches no
    /// daemon, and having it absent until first use would make "is Docker
    /// available" a question asked in the middle of a turn.
    #[cfg(not(target_os = "android"))]
    pub containers: Arc<crate::container::DockerConnector>,
    /// Per-path locks for the shadow file journal.
    ///
    /// Lives here rather than on a `JournalCtx` because two independent
    /// desktop turns writing the same file must serialise. A table per turn
    /// made that guarantee only hold between a parent and its sub-agent.
    /// Gitignore matchers stay on the `JournalCtx`: a process-wide cache
    /// cannot see an external `.gitignore` rewrite and would keep
    /// snapshotting secrets until restart.
    pub journal_shared: Arc<crate::journal::capture::JournalShared>,
    pub redaction: Arc<crate::redaction::RedactionEngine>,
    pub redaction_mappings: crate::redaction::RedactionMappings,
    /// How to start an ordinary turn, once the shell has said.
    ///
    /// The one direction that has to cross the line the other way. Running a
    /// turn is `commands::chat`, which is a Tauri command and lives above it;
    /// the prompt queue is below and has to be able to start one. Moving
    /// `commands/` down would be the other answer and is a much larger change
    /// for a single call — see the architecture note on why that stays where it
    /// is until something without a window needs to serve commands.
    ///
    /// A `OnceLock` because it is registered once at startup and read for the
    /// life of the process, and because leaving it unset has to be safe: a
    /// build with no desktop runner simply never delivers a follow-up.
    pub turn_starter: std::sync::OnceLock<Arc<dyn StartTurn>>,
}

/// Starting an ordinary turn, from something that is not a window.
///
/// Deliberately narrow. Everything a composer would decide — which model, which
/// mode, how much thinking — is left out, because a queued message was typed
/// without any of that in front of it and the conversation's own configuration
/// is the answer it was written under.
#[async_trait::async_trait]
pub trait StartTurn: Send + Sync {
    /// Run one turn to completion, with `queued` as the message.
    ///
    /// The queue item's id travels with it so the runner can spend it in the
    /// same transaction that writes its row — the rule the whole ledger rests
    /// on, and one only the runner is in a position to keep.
    async fn start(
        &self,
        conversation_id: &str,
        queued: &crate::db::models::queue::QueuedPromptRow,
    ) -> Result<(), String>;
}

impl Services {
    pub fn new(inner: ServicesInner) -> Self {
        Services(Arc::new(inner))
    }
}

/// So a caller writes `services.db` rather than `services.inner().db`. The
/// fields are the interface; the `Arc` is an implementation detail of how they
/// are shared.
impl std::ops::Deref for Services {
    type Target = ServicesInner;

    fn deref(&self) -> &ServicesInner {
        &self.0
    }
}

/// A `Services` with nothing running behind it.
///
/// Every part of it is lazy — the secrets manager does not reach the keyring
/// until asked, the MCP registry has no servers, the sleep inhibitor nothing to
/// inhibit — so this costs an in-memory database and whatever `dir` is.
///
/// It lived in `acp::session`'s tests while that was the only module driving
/// these directly, with a note saying the second caller should move it here.
/// `acp::bridge` is the second caller.
#[cfg(test)]
pub fn bare_services(dir: &std::path::Path) -> Services {
    Services::new(ServicesInner {
        db: crate::db::test_db(),
        secrets: Arc::new(crate::secrets::SecretsManager::new(dir.to_path_buf())),
        tools: Arc::new(tools::ToolRegistry::new(
            dir.join("skills"),
            dir.join("logs"),
            Arc::new(crate::redaction::RedactionEngine::disabled()),
        )),
        mcp: mcp::McpRegistry::new(),
        turns: Arc::new(TurnCoordinator::new()),
        approvals: ApprovalWaiters::new(),
        sub_agent_inboxes: AppSubAgentInboxes::default(),
        compact_breakers: Mutex::new(HashMap::new()),
        voice: VoiceState::new(),
        corpus: Arc::new(crate::voice_corpus::CorpusCoordinator::new(dir)),
        voice_limiter: Arc::new(crate::tts::limiter::VoiceLimiter::default()),
        sleep: AppSleepInhibitor::new(),
        events: EventBus::new(),
        paths: Paths {
            data_dir: dir.to_path_buf(),
            skills_root: dir.join("skills"),
        },
        plan_files: Arc::new(crate::plan_files::PlanFileStore::new(dir)),
        #[cfg(not(target_os = "android"))]
        acp: crate::acp::AcpRegistry::new(),
        #[cfg(not(target_os = "android"))]
        containers: crate::container::DockerConnector::new(Default::default()),
        turn_starter: std::sync::OnceLock::new(),
        journal_shared: crate::journal::capture::JournalShared::new(),
        redaction: Arc::new(crate::redaction::RedactionEngine::disabled()),
        redaction_mappings: crate::redaction::RedactionMappings::new(),
    })
}

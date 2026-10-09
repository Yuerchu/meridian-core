//! One plan, one reviewing turn, one verdict.
//!
//! Assembled the same way [`crate::commands::sub_agent`] assembles an `Explore`
//! run, minus the four things that only exist because a window is attached: no
//! emitter, no approval cards, no inbox, no child-turn guard. What is left is
//! the loop itself, which has never known about Tauri.
//!
//! Two settings carry most of the weight and are easy to get wrong:
//!
//! * `working_directory` must be the repository the plan is about. Without it
//!   `reach::locate` calls every path `Outside`, every `read_file` wants
//!   approval, and nobody is there to give it — the reviewer would end up
//!   guessing at a repository it never managed to read.
//! * `ApprovalRule::ByReach` rather than `ByPermission`. `read_file` declares
//!   `Ask`; only reach knows that reading inside the project is not worth
//!   interrupting anyone for.

use std::sync::Arc;

use hyper::StatusCode;
use tokio_util::sync::CancellationToken;

use crate::agent::engine::{self, ApprovalDecision, Approvals};
use crate::agent::turn_record;
use crate::db::entity::{assistant, conversation};
use crate::db::models::turn::TurnStatus;
use crate::db::sea::DbErr;
use crate::db::sea::cap::Db;
use crate::db::sea::ops as sea_ops;
use crate::events::ChatStreamEvent;
use crate::provider::ToolCall;
use crate::tools::{FileAccess, ShellType, ToolContext};
use crate::turn::TurnOrigin;
use crate::util::now_ms;

use super::SharedState;
use super::protocol::{Kind, ReviewJob, ReviewResponse};
use super::verdict::{self, Outcome};

/// Progress goes to a window that may not be open, and that is fine.
///
/// The twin of `onebot::agent::BestEffortEmit`, for the same reason: a headless
/// runner's events are a courtesy to whoever happens to be looking, not the
/// answer itself, so a failed send is not the turn failing. Not shared with it
/// because `engine` is deliberately free of Tauri — this is the layer that is
/// allowed to know about windows.
struct BestEffortEmit(crate::events::EventBus);

impl engine::Emit for BestEffortEmit {
    fn emit(&self, channel: &str, payload: serde_json::Value) -> Result<(), String> {
        let _ = self.0.emit(channel, payload);
        Ok(())
    }
}

/// Tell the window the turn is over.
///
/// The engine does not send this one — each caller does, because each decides
/// what the ending was (`chat.rs:1011` for the desktop,
/// `onebot::agent::turn_stop_payload` for QQ). Leaving it out is what left a
/// finished review with its cursor still blinking: the front end sets a
/// `streaming` flag from the first chunk and clears it only here, so with no
/// stop it sits there until the window is reloaded.
///
/// Goes out even when there is no `message_id` — a turn that died before
/// writing an assistant row still has to clear that flag.
fn stopped(state: &SharedState, conversation_id: &str, turn_id: &str, outcome: &engine::TurnOutcome) {
    let _ = state.services.events.emit_chat(ChatStreamEvent::Stop {
        reason: outcome.chat_stop_reason(),
        message_id: outcome.progress.message_id.clone(),
        turn_id: turn_id.to_string(),
        conversation_id: conversation_id.to_string(),
        input_tokens: outcome.progress.input_tokens.value(),
        output_tokens: outcome.progress.output_tokens.value(),
    });
}

/// Tell the sidebar a review conversation appeared or moved on.
///
/// `conversation-updated` is the same event OneBot's headless path sends
/// (`onebot/handler.rs:561`); the frontend answers it by refetching the list
/// (`use-global-event-listener.ts:176`). Without it a review is invisible for
/// the several minutes it runs, which reads exactly like the gate not being
/// installed.
fn announce(state: &SharedState, conversation_id: &str) {
    let _ = state.services.events.emit_conversation_updated(conversation_id);
}

/// Why no review happened. Every one of these is a non-200, and every non-200
/// lets the plan through — so these are explanations, not refusals of service.
pub(crate) struct Refused {
    pub status: StatusCode,
    pub message: String,
}

fn refuse(status: StatusCode, message: impl Into<String>) -> Refused {
    Refused {
        status,
        message: message.into(),
    }
}

/// Nobody to ask.
///
/// `Ok(None)` is "nobody answered", which the loop words as withheld rather
/// than denied — the honest description of a question with no audience.
/// Reaching this at all is a bug signal: the reviewer holds four read-only
/// tools and `ByReach` never asks about reading inside the project, so a
/// question here means the path was judged `Outside`, which almost always
/// means `working_directory` is wrong.
struct NoApprovals;

#[async_trait::async_trait]
impl Approvals for NoApprovals {
    async fn ask(
        &self,
        _assistant_message_id: &str,
        call: &ToolCall,
        _retry: Option<crate::agent::engine::Escalation<'_>>,
    ) -> Result<Option<ApprovalDecision>, String> {
        tracing::warn!(tool = %call.name, "plan review asked for approval; nobody can answer");
        Ok(None)
    }
}

/// Run one review to completion.
///
/// Takes an owned `Arc` because the caller spawns this rather than awaiting it
/// inline: a review costs minutes and real money, and tying its lifetime to an
/// HTTP connection means an impatient Ctrl-C throws all of it away *and* leaves
/// the `turns` row at `running` forever — the code that would record the ending
/// is inside the future that just got dropped. Observed exactly that: a turn
/// abandoned seven seconds in, still `running` eight minutes later, its own
/// timeout unable to fire because nothing was polling it.
pub(crate) async fn run(state: Arc<SharedState>, job: ReviewJob) -> Result<ReviewResponse, Refused> {
    let state = state.as_ref();
    let cwd = job.cwd.clone();
    if !tokio::fs::metadata(&cwd).await.map(|m| m.is_dir()).unwrap_or(false) {
        return Err(refuse(
            StatusCode::BAD_REQUEST,
            format!("cwd is not a directory: {cwd}"),
        ));
    }

    let model = state.config.review_model.clone().ok_or_else(|| {
        refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "no review model configured; pick one in Settings → Hooks",
        )
    })?;

    let assistant = effective_assistant(state, &model, &cwd, &job).await?;
    let params = resolve_params(state, &assistant).await?;

    let (conversation_id, is_new) = open_or_reuse(&state.services.db, &job).await;
    let turn_id = uuid::Uuid::new_v4().to_string();
    let cancel = CancellationToken::new();

    let origin = match job.kind {
        Kind::Plan => TurnOrigin::PlanReview,
        Kind::Implementation => TurnOrigin::ImplReview,
    };
    let _lease = Arc::clone(&state.services.turns)
        .try_acquire_turn_with(&conversation_id, origin, turn_id.clone(), cancel.clone())
        .map_err(|busy| refuse(StatusCode::CONFLICT, busy.to_string()))?;

    // Nothing to undo on failure: the id was never handed out, so the client
    // still holds whatever it held before and the next round validates it the
    // same way. A half-written conversation is caught by the transaction.
    let user_message_id = write_round(&state.services.db, &conversation_id, &turn_id, &job, &assistant, is_new)
        .await
        .map_err(|e| refuse(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    // Before the model is called, not after: the subject is already written, and
    // a review that runs for minutes should be openable from the moment it
    // starts rather than appearing once it is over.
    announce(state, &conversation_id);

    let outcome = run_turn(
        state,
        &assistant,
        &params,
        &conversation_id,
        &turn_id,
        &user_message_id,
        &cwd,
        &cancel,
    )
    .await;

    let timed_out = cancel.is_cancelled();
    let (status, error) = match (&outcome.reply, timed_out) {
        (Err(e), _) => (TurnStatus::Failed, Some(e.clone())),
        (Ok(_), true) => (TurnStatus::Cancelled, None),
        (Ok(_), false) => (TurnStatus::Done, None),
    };
    turn_record::finish(&state.services.db, &turn_id, status, error.as_deref()).await;
    stopped(state, &conversation_id, &turn_id, &outcome);
    announce(state, &conversation_id);

    let reply = match outcome.reply {
        Ok(reply) => reply,
        Err(e) => {
            tracing::warn!(error = %e, kind = job.kind.agent_kind(), "review turn failed");
            return Err(refuse(StatusCode::BAD_GATEWAY, e));
        }
    };
    if timed_out {
        return Err(refuse(
            StatusCode::GATEWAY_TIMEOUT,
            format!("the review did not finish within {}s", state.config.timeout_secs),
        ));
    }

    // Deliberately logs a length, never the text: this reply quotes the subject
    // and the repository, and exported logs leave the machine.
    tracing::info!(
        session = %job.session_id,
        kind = job.kind.agent_kind(),
        round = job.round,
        chars = reply.chars().count(),
        "review finished"
    );

    // Every arm carries the conversation back, including the inconclusive one:
    // a round that produced no verdict still wrote a transcript, and the next
    // round should continue in it rather than start the reviewer over.
    Ok(match verdict::parse(&reply) {
        Outcome::Approve { summary } => ReviewResponse::approve(summary, turn_id, conversation_id),
        Outcome::Revise { summary, message } => ReviewResponse::revise(summary, message, turn_id, conversation_id),
        Outcome::Inconclusive { reason } => {
            tracing::warn!(reason, chars = reply.chars().count(), "review gave no usable verdict");
            ReviewResponse::inconclusive(format!("{reason}，本次不阻断"), conversation_id)
        }
    })
}

/// The reviewing assistant: someone else's baseline, our persona and tools.
///
/// `context_limit` and `max_tokens` are cleared for the same reason the
/// sub-agent runner clears them — they were set beside a different model — and
/// `tool_preset_id` because a preset wins over an explicit list, which would
/// hand write tools to an agent whose whole definition is that it cannot
/// change anything.
async fn effective_assistant(
    state: &SharedState,
    model: &str,
    cwd: &str,
    job: &ReviewJob,
) -> Result<assistant::Model, Refused> {
    let (provider_id, model_id) = model.split_once(':').ok_or_else(|| {
        refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("`{model}` is not a provider:model pair; set one in Settings → Hooks"),
        )
    })?;

    let sea = &state.services.db;
    let base = match state.config.assistant_id.as_deref() {
        Some(id) => sea_ops::assistant::get_assistant(sea, id)
            .await
            .map_err(|e| e.to_string())
            .and_then(|found| found.ok_or_else(|| format!("assistant `{id}` not found"))),
        None => sea_ops::assistant::get_default_assistant(sea)
            .await
            .map_err(|e| e.to_string())
            .and_then(|found| found.ok_or_else(|| "there is no assistant to base the review on".to_string())),
    }
    .map_err(|e| refuse(StatusCode::SERVICE_UNAVAILABLE, e))?;

    // What the client says it will allow, not what we would allow: it is the
    // side doing the counting. Falling back to our own setting keeps the
    // reviewer honest when an older plugin sends nothing.
    let max_rounds = job.max_rounds.unwrap_or(state.config.max_rounds);
    let round = job.round.max(1);
    let tools = crate::tools::reviewer_tools(&state.services.tools);
    let system_prompt = match job.kind {
        Kind::Plan => verdict::prompt(cwd, round, max_rounds, job.stagnant, &tools),
        Kind::Implementation => verdict::implementation_prompt(cwd, round, max_rounds, job.stagnant, &tools),
    };
    Ok(assistant::Model {
        provider_id: Some(provider_id.to_string()),
        model_id: Some(model_id.to_string()),
        context_limit: 0,
        max_tokens: None,
        tool_preset_id: None,
        enabled_tools: Some(crate::db::types::Json(
            tools.iter().map(|name| name.to_string()).collect(),
        )),
        system_prompt,
        ..base
    })
}

async fn resolve_params(
    state: &SharedState,
    assistant: &assistant::Model,
) -> Result<crate::agent::TurnParams, Refused> {
    let resolved =
        crate::agent::resolve_with_overrides(&state.services.secrets, &state.services.db, Some(assistant), None, None)
            .await
            .map_err(|e| refuse(StatusCode::SERVICE_UNAVAILABLE, e))?;

    // A missing `model_configs` row is an error rather than a fallback: this
    // project's rule is that turn parameters are configured, never invented.
    let mut params = crate::agent::resolve_turn_params(
        &state.services.db,
        crate::agent::TurnParamsResolveRequest {
            assistant: Some(assistant),
            provider_id: assistant.provider_id.as_deref(),
            provider_type: &resolved.provider_type,
            api_format: &resolved.api_format,

            transport_profile: &resolved.transport_profile,
            codex_request_shape: resolved.codex_request_shape,
            codex_request_kind: crate::provider::codex_metadata::CodexRequestKind::Review,
            codex_thread_source: crate::provider::codex_metadata::CodexThreadSource::Hook,
            model: &resolved.model,
            thinking_level: None,
            fast: false,
        },
    )
    .await
    .map_err(|e| refuse(StatusCode::SERVICE_UNAVAILABLE, e))?;

    // `max_tokens` here is the model's own maximum, not the baseline assistant's
    // ceiling: `effective_assistant` cleared that override, so resolution fell to
    // the model (`agent::resolve_max_tokens`). A review is a short verdict after
    // a lot of reading and must not be clamped to some assistant's small limit —
    // nor left absent, which Anthropic refuses.
    //
    // The reviewer's four read-only tools are the whole of what it may do, and
    // `turn_config` is only half of enforcing that: the tools the *provider*
    // runs never pass through a tool set at all, they ride on the request
    // parameters. Left in, a reviewer pointed at a model with server-side search
    // switched on could search the open web about an uncommitted diff — with no
    // approval, and no sign of it in the transcript.
    params.params.server_tools = Vec::new();
    Ok(params)
}

/// The conversation this session's rounds accumulate in.
///
/// Returns whether it had to be created, because a first round writes the
/// conversation row and later ones must not.
///
/// The client says where its earlier rounds went; nothing here remembers. That
/// is what makes a Meridian that died between two rounds indistinguishable from
/// one that did not — there is no session table to lose.
///
/// The id is checked, never trusted. Any process on this machine can reach the
/// endpoint, and an id is just a string in a request body: without a check, a
/// caller could name one of the user's own conversations and have the reviewer
/// append to it. So the row has to exist *and* be one this feature created.
/// Anything else is treated as if no id had been sent at all.
/// Takes the database rather than the whole `SharedState` because that is all it
/// uses — and because the wider signature was the entire reason this went
/// untested: standing up `secrets`/`tools`/`mcp`/`coordinator` to check one
/// boolean is enough friction that nobody does it. Never returns an error:
/// an id it cannot vouch for means "open a new one", not "refuse the request".
async fn open_or_reuse(db: &Db, job: &ReviewJob) -> (String, bool) {
    if let Some(claimed) = job.conversation_id.clone().filter(|id| !id.trim().is_empty()) {
        // Matched against *this* kind, so the two gates cannot be handed each
        // other's transcripts: an implementation review continuing in a plan
        // review's conversation would inherit an intention it was meant to
        // judge without.
        let wanted = job.kind.agent_kind();
        let ours = sea_ops::conversation::get_conversation(db, &claimed)
            .await
            .ok()
            .flatten()
            .is_some_and(|conversation| conversation.agent_kind.as_deref() == Some(wanted));

        if ours {
            return (claimed, false);
        }
        // Deleted, or never ours. Either way the next rounds start somewhere
        // new rather than being refused: a review that runs without the earlier
        // context still beats no review.
        tracing::info!(
            session = %job.session_id,
            kind = job.kind.agent_kind(),
            "the conversation the client named is gone or not ours; opening a new one"
        );
    }

    (uuid::Uuid::new_v4().to_string(), true)
}

/// This round's prompt, and the turn row that says it started.
///
/// On a first round the conversation is written here too, in the same
/// transaction: all three or none. A conversation without its turn row is a run
/// startup reconciliation cannot see; a turn row without its conversation is a
/// report about somewhere the user cannot go.
async fn write_round(
    db: &Db,
    conversation_id: &str,
    turn_id: &str,
    job: &ReviewJob,
    assistant: &assistant::Model,
    is_new: bool,
) -> Result<String, String> {
    let message_id = uuid::Uuid::new_v4().to_string();
    let returned = message_id.clone();
    let prompt = round_prompt(job);
    let title = title_for(job.kind, &job.cwd);
    let agent_kind = job.kind.agent_kind();
    // Must match the lease taken in `run`, or the row and the register disagree
    // about what is occupying this conversation.
    let origin = match job.kind {
        Kind::Plan => TurnOrigin::PlanReview,
        Kind::Implementation => TurnOrigin::ImplReview,
    };
    let cwd = job.cwd.as_str();

    let now = now_ms();
    db.write(async |tx| {
        // File the review under the project that owns this directory, when
        // there is one. Two reasons, and the second is the load-bearing one:
        // otherwise every repository's reviews pile into the ungrouped list,
        // and — because a desktop turn takes its working directory from
        // `conversation.project_id -> project.path` — a conversation with no
        // project has no working directory the moment anyone types into it.
        // The review sets its own root out of band, so without this the
        // transcript is only readable, not continuable.
        //
        // No project for this path just means null, as before. Inventing one
        // would put a row in a list the user curates.
        let head = if is_new {
            let project_id = sea_ops::project::find_project_by_path(tx, cwd).await?.map(|p| p.id);
            sea_ops::conversation::insert(
                tx,
                conversation::Model {
                    title: Some(title),
                    assistant_id: Some(assistant.id.clone()),
                    project_id,
                    // Left null on purpose: a review is meant to be visible
                    // in the sidebar. `parent_conversation_id` is what hides
                    // a sub-agent's transcript, and hiding this one would
                    // put the reviewer's reasoning somewhere with no way in.
                    parent_conversation_id: None,
                    agent_kind: Some(agent_kind.to_string()),
                    agent_provider_id: assistant.provider_id.clone(),
                    agent_model_id: assistant.model_id.clone(),
                    ..sea_ops::conversation::new_row(conversation_id, now)
                },
            )
            .await?;
            None
        } else {
            sea_ops::conversation::get_conversation(tx, conversation_id)
                .await?
                .and_then(|c| c.head_message_id)
        };

        let row = crate::db::entity::message::Model {
            turn_id: Some(turn_id.to_string()),
            ..sea_ops::message::new_row(&message_id, conversation_id, "user", &prompt, now)
        };
        sea_ops::message::append_message(tx, row, head.as_deref()).await?;
        sea_ops::turn::begin(tx, turn_id, conversation_id, origin, None, now).await?;
        Ok::<_, DbErr>(())
    })
    .await
    .map_err(|e| e.to_string())?;

    Ok(returned)
}

fn title_for(kind: Kind, cwd: &str) -> String {
    let leaf = cwd.rsplit(['/', '\\']).find(|s| !s.is_empty()).unwrap_or("(unknown)");
    format!("{} · {leaf}", kind.title_prefix())
}

/// What the reviewer is actually asked, this round.
fn round_prompt(job: &ReviewJob) -> String {
    let mut out = String::new();
    if job.round > 1 && !job.history.is_empty() {
        out.push_str("前几轮的结论：\n");
        for entry in &job.history {
            out.push_str(&format!(
                "- 第 {} 轮：{} — {}\n",
                entry.round, entry.verdict, entry.summary
            ));
        }
        out.push('\n');
    }

    match job.kind {
        Kind::Plan => out.push_str("待审查的计划：\n\n"),
        Kind::Implementation => {
            // The claim first and the evidence second, labelled as such: the
            // summary is what the author says it did, and half the value of
            // this review is noticing where the two disagree.
            if let Some(note) = &job.note {
                out.push_str("作者对这次改动的自述（**这是主张，不是证据**）：\n\n");
                out.push_str(note);
                out.push_str("\n\n");
            }
            out.push_str("未提交的改动（`git diff`，这是证据）：\n\n");
        }
    }
    out.push_str(&job.subject);
    out
}

#[allow(clippy::too_many_arguments)]
async fn run_turn(
    state: &SharedState,
    assistant: &assistant::Model,
    params: &crate::agent::TurnParams,
    conversation_id: &str,
    turn_id: &str,
    user_message_id: &str,
    cwd: &str,
    cancel: &CancellationToken,
) -> engine::TurnOutcome {
    let config = match build_config(state, assistant, conversation_id, params).await {
        Ok(c) => c,
        Err(e) => return engine::TurnOutcome::failed(e),
    };
    let provider = match build_provider(state, assistant).await {
        Ok(p) => p,
        Err(e) => return engine::TurnOutcome::failed(e),
    };

    let history = load_history(state, conversation_id).await;
    let chat_messages = match crate::agent::build_messages_with_senders(
        config.system_prompt.trim(),
        &history,
        Vec::new(),
        &Default::default(),
    ) {
        Ok(messages) => messages,
        Err(error) => return engine::TurnOutcome::failed(error),
    };

    let mut budget = crate::agent::TokenBudget::new(
        &provider.1.provider_type,
        &params.params.model,
        params.context_limit,
        params.max_output,
        params.compact_threshold,
    );
    budget.update_estimate(&chat_messages);

    let tool_secrets = crate::agent::build_tool_secrets(&state.services.secrets, &state.services.db).await;

    let tool_context = ToolContext {
        // The one field the whole review depends on. See the module header.
        working_directory: Some(cwd.to_string()),
        shell: ShellType::default_for_platform(),
        file_access: FileAccess::Unrestricted,
        project_id: None,
        conversation_id: Some(conversation_id.to_string()),
        turn_id: Some(turn_id.to_string()),
        assistant_id: Some(assistant.id.clone()),
        db: Some(state.services.db.clone()),
        #[cfg(not(target_os = "android"))]
        sandbox_policy: crate::sandbox::CommandSandbox::UNCONFINED,
        #[cfg(not(target_os = "android"))]
        background: None,
        tool_secrets,
        cancel: cancel.clone(),
        journal: None,
    };

    let approvals = NoApprovals;
    let setup = engine::TurnSetup {
        trigger: crate::turn::TurnTrigger::User,
        provider: &*provider.0,
        params: params.params.clone(),
        chat_messages,
        tool_defs: config.tool_defs,
        offered: config.offered,
        mode: crate::agent::modes::Modes::Fixed.spec(),
        tool_context,
        budget,
        turn_id: turn_id.to_string(),
        conversation_id: conversation_id.to_string(),
        provider_id: Some(provider.1.provider_id.clone()),
        provider_name: Some(provider.1.provider_name.clone()),
        parent_cursor: Some(user_message_id.to_string()),
        cancel: cancel.clone(),
        keep_recent: assistant.compact_keep_recent.max(0) as usize,
        context_limit: params.context_limit,
        approval_rule: engine::ApprovalRule::ByReach { accept_edits: false },
        withheld: engine::WithheldWording::Explained,
        files_root: None,
        stickers: None,
        interrupted: None,
        compaction: engine::CompactionPolicy::Desktop {
            enabled: true,
            breaker: Arc::new(crate::agent::CompactCircuitBreaker::new()),
        },
        // The gate reports tokens, not money — see the response it builds. What
        // the review cost is still recorded, on its audit rows.
        pricing: None,
    };
    // Streams into the conversation view exactly like a desktop turn, so the
    // review can be watched while `ExitPlanMode` blocks on it. Nobody is
    // required to be watching — see `BestEffortEmit`.
    let emitter = BestEffortEmit(state.services.events.clone());
    let ports = engine::TurnPorts {
        emit: Some(&emitter as &dyn engine::Emit),
        approvals: &approvals,
        interim: None,
        surface_tools: None,
        steering: None,
        transitions: None,
        sub_agents: None,
    };

    let services = engine::TurnServices {
        db: &state.services.db,
        tools: &state.services.tools,
        mcp: &state.services.mcp,
        redaction: &state.services.redaction,
        redaction_mappings: &state.services.redaction_mappings,
    };
    let deadline = std::time::Duration::from_secs(state.config.timeout_secs.max(1) as u64);

    let running = engine::run_turn(&services, setup, ports);
    tokio::pin!(running);
    tokio::select! {
        outcome = &mut running => outcome,
        _ = tokio::time::sleep(deadline) => {
            // Cancel and then wait: the loop owns the rows it is partway
            // through writing, and abandoning the future would leave them.
            cancel.cancel();
            running.await
        }
    }
}

async fn load_history(state: &SharedState, conversation_id: &str) -> sea_ops::message::ActiveContext {
    state
        .services
        .db
        .read(async |tx| {
            let Some(conversation) = sea_ops::conversation::get_conversation(tx, conversation_id).await? else {
                return Ok(None);
            };
            let messages = sea_ops::message::list_messages(tx, conversation_id).await?;
            Ok::<_, DbErr>(Some(sea_ops::message::active_context(
                &messages,
                conversation.head_message_id.as_deref(),
            )))
        })
        .await
        .ok()
        .flatten()
        .unwrap_or(sea_ops::message::ActiveContext {
            path: Vec::new(),
            summary: None,
            anchor_index: None,
            head_id: None,
        })
}

async fn build_config(
    state: &SharedState,
    assistant: &assistant::Model,
    conversation_id: &str,
    params: &crate::agent::TurnParams,
) -> Result<crate::agent::turn_config::TurnConfig, String> {
    let input = crate::agent::turn_config::TurnConfigResolveRequest {
        // The reviewer gets four read-only tools and no shell; searching
        // the web is not among them, provider-side or otherwise.
        server_tools: Vec::new(),
        assistant: Some(assistant.clone()),
        conversation_id: conversation_id.to_string(),
        project_id: None,
        mode: crate::agent::modes::Modes::Fixed,
        sub_agents: None,
        // An explicit whitelist and nothing outside it. MCP tools ask
        // unconditionally, and nobody is watching this run.
        mcp_defs: Vec::new(),
        exposure: crate::agent::turn_config::ToolExposure::when(params.caps.supports_tools),
        persona: assistant.system_prompt.clone(),
        context_blocks: Vec::new(),
        session_tools: None,
        // No shell either — see `hook-gates.md`.
        command_shell: None,
    };
    crate::agent::turn_config::resolve_on(&state.services.db, &state.services.tools, input).await
}

async fn build_provider(
    state: &SharedState,
    assistant: &assistant::Model,
) -> Result<(Box<dyn crate::provider::ChatProvider>, crate::agent::ResolvedProvider), String> {
    let resolved =
        crate::agent::resolve_with_overrides(&state.services.secrets, &state.services.db, Some(assistant), None, None)
            .await?;
    let provider = crate::provider::registry::create_provider(resolved.wire())?;
    Ok((provider, resolved))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_title_names_the_repository_and_which_gate() {
        assert_eq!(title_for(Kind::Plan, "C:/Users/x/Code/meridian"), "计划审查 · meridian");
        assert_eq!(
            title_for(Kind::Plan, "C:\\Users\\x\\Code\\meridian\\"),
            "计划审查 · meridian"
        );
        assert_eq!(title_for(Kind::Plan, ""), "计划审查 · (unknown)");
        // Two gates land in the same sidebar; the title is what tells them apart.
        assert_eq!(
            title_for(Kind::Implementation, "C:/Code/meridian"),
            "改动审查 · meridian"
        );
    }

    /// A conversation row with a chosen `agent_kind`, which the convenience
    /// helper `create_conversation` cannot set.
    async fn conversation(db: &Db, id: &str, agent_kind: Option<&str>) {
        let row = conversation::Model {
            title: Some("t".into()),
            agent_kind: agent_kind.map(str::to_owned),
            ..sea_ops::conversation::new_row(id, 1)
        };
        db.write(async |tx| sea_ops::conversation::insert(tx, row).await)
            .await
            .unwrap();
    }

    /// A later round is written under the earlier one, so the reviewer's
    /// transcript is one path. The row used to name the head in its own
    /// `parent_id` while `append_message` was handed `None`, and the
    /// parameter won: every round after the first started a new root, and
    /// the active path held only the latest round.
    #[tokio::test]
    async fn a_later_round_continues_the_earlier_one() {
        let db = crate::db::sea::sea_test_db().await;
        let reviewer = sea_ops::assistant::tests::assistant_row("a1", 0);
        db.write(async |tx| sea_ops::assistant::create_assistant(tx, reviewer.clone()).await)
            .await
            .unwrap();
        let first = job(Kind::Plan, 1, vec![]);
        let one = write_round(&db, "rev", "t1", &first, &reviewer, true).await.unwrap();
        db.write(async |tx| sea_ops::turn::finish(tx, "t1", TurnStatus::Done, None, 2).await)
            .await
            .unwrap();
        let two = write_round(&db, "rev", "t2", &job(Kind::Plan, 2, vec![]), &reviewer, false)
            .await
            .unwrap();

        let second = sea_ops::message::get_message(&db, &two).await.unwrap().unwrap();
        assert_eq!(second.parent_id.as_deref(), Some(one.as_str()));
        let conversation = sea_ops::conversation::get_conversation(&db, "rev")
            .await
            .unwrap()
            .unwrap();
        let history = sea_ops::message::list_messages(&db, "rev").await.unwrap();
        let path = sea_ops::message::active_context(&history, conversation.head_message_id.as_deref()).path;
        assert_eq!(
            path.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            [one.as_str(), two.as_str()]
        );
    }

    fn claiming(id: Option<&str>) -> ReviewJob {
        ReviewJob {
            conversation_id: id.map(str::to_string),
            ..job(Kind::Plan, 1, vec![])
        }
    }

    /// The property this whole check exists for. The endpoint is reachable by
    /// any process on the machine and the id is just a string in a request
    /// body; without the `agent_kind` test, naming one of the user's own
    /// conversations would get the reviewer to append to it.
    #[tokio::test]
    async fn a_users_own_conversation_is_never_written_to() {
        let db = crate::db::sea::sea_test_db().await;
        conversation(&db, "private", None).await;

        let (id, is_new) = open_or_reuse(&db, &claiming(Some("private"))).await;
        assert_ne!(id, "private", "must not write into a conversation that is not ours");
        assert!(is_new);
    }

    #[tokio::test]
    async fn a_sub_agent_conversation_is_not_ours_either() {
        let db = crate::db::sea::sea_test_db().await;
        conversation(&db, "delegated", Some("sub_agent")).await;

        let (id, is_new) = open_or_reuse(&db, &claiming(Some("delegated"))).await;
        assert_ne!(id, "delegated");
        assert!(is_new);
    }

    #[tokio::test]
    async fn a_plan_review_conversation_is_reused() {
        let db = crate::db::sea::sea_test_db().await;
        conversation(&db, "ours", Some(Kind::Plan.agent_kind())).await;

        let (id, is_new) = open_or_reuse(&db, &claiming(Some("ours"))).await;
        assert_eq!(id, "ours");
        assert!(!is_new, "an existing conversation must not be written again");
    }

    /// The two gates must not inherit each other's transcripts. An
    /// implementation review continuing where a plan review left off would be
    /// judging code against an intention it had already agreed to — which is
    /// exactly the independence the split was for.
    #[tokio::test]
    async fn the_two_gates_do_not_share_a_conversation() {
        let db = crate::db::sea::sea_test_db().await;
        conversation(&db, "planning", Some(Kind::Plan.agent_kind())).await;

        let asking = ReviewJob {
            conversation_id: Some("planning".into()),
            ..job(Kind::Implementation, 1, vec![])
        };
        let (id, is_new) = open_or_reuse(&db, &asking).await;
        assert_ne!(id, "planning");
        assert!(is_new);
    }

    /// The client outlived the conversation — the user deleted it. Open a new
    /// one rather than refusing: a review without the earlier context still
    /// beats no review.
    #[tokio::test]
    async fn an_unknown_id_opens_a_new_conversation() {
        let db = crate::db::sea::sea_test_db().await;
        let (id, is_new) = open_or_reuse(&db, &claiming(Some("gone"))).await;
        assert_ne!(id, "gone");
        assert!(is_new);
    }

    #[tokio::test]
    async fn nothing_claimed_opens_a_new_conversation() {
        let db = crate::db::sea::sea_test_db().await;
        for claimed in [None, Some(""), Some("   ")] {
            let (id, is_new) = open_or_reuse(&db, &claiming(claimed)).await;
            assert!(!id.is_empty());
            assert!(is_new, "claimed = {claimed:?}");
        }
    }

    fn job(kind: Kind, round: u32, history: Vec<(u32, &str, &str)>) -> ReviewJob {
        ReviewJob {
            kind,
            session_id: "s".into(),
            cwd: "C:/repo".into(),
            conversation_id: None,
            subject: match kind {
                Kind::Plan => "步骤一".into(),
                Kind::Implementation => "--- a/foo.rs\n+++ b/foo.rs\n+fn bar() {}".into(),
            },
            note: None,
            round,
            max_rounds: Some(3),
            stagnant: false,
            history: history
                .into_iter()
                .map(|(round, verdict, summary)| super::super::protocol::HistoryEntry {
                    round,
                    verdict: match verdict {
                        "approve" => super::super::protocol::HistoryVerdict::Approve,
                        "revise" => super::super::protocol::HistoryVerdict::Revise,
                        "inconclusive" => super::super::protocol::HistoryVerdict::Inconclusive,
                        other => panic!("unknown test verdict {other:?}"),
                    },
                    summary: summary.into(),
                })
                .collect(),
        }
    }

    fn request(round: u32, history: Vec<(u32, &str, &str)>) -> ReviewJob {
        job(Kind::Plan, round, history)
    }

    #[test]
    fn the_first_round_is_just_the_plan() {
        let prompt = round_prompt(&request(1, vec![]));
        assert!(prompt.starts_with("待审查的计划："), "{prompt}");
        assert!(prompt.contains("步骤一"));
    }

    /// The author's summary is a claim and the diff is evidence; a reviewer
    /// that cannot tell them apart cannot notice where they disagree, which is
    /// half of what this gate is for.
    #[test]
    fn an_implementation_review_labels_the_claim_and_the_evidence() {
        let asking = ReviewJob {
            note: Some("加了 bar()".into()),
            ..job(Kind::Implementation, 1, vec![])
        };
        let prompt = round_prompt(&asking);

        let claim = prompt.find("加了 bar()").expect("the summary should be there");
        let evidence = prompt.find("+fn bar() {}").expect("the diff should be there");
        assert!(claim < evidence, "the claim comes first, then what actually happened");
        assert!(prompt.contains("主张"), "{prompt}");
        assert!(prompt.contains("证据"), "{prompt}");
    }

    #[test]
    fn an_implementation_review_without_a_summary_is_just_the_diff() {
        let prompt = round_prompt(&job(Kind::Implementation, 1, vec![]));
        assert!(!prompt.contains("自述"), "{prompt}");
        assert!(prompt.contains("+fn bar() {}"), "{prompt}");
    }

    /// Later rounds carry what was already said, so the reviewer does not
    /// contradict itself between drafts.
    #[test]
    fn later_rounds_carry_the_earlier_conclusions() {
        let prompt = round_prompt(&request(2, vec![(1, "revise", "缺回滚")]));
        assert!(prompt.contains("第 1 轮：revise — 缺回滚"), "{prompt}");
        assert!(prompt.contains("步骤一"), "{prompt}");
    }
}

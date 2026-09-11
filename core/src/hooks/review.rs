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

use diesel::Connection;
use hyper::StatusCode;
use tokio_util::sync::CancellationToken;

use crate::agent::engine::{self, ApprovalDecision, Approvals};
use crate::agent::turn_record;
use crate::db;
use crate::db::DbPool;
use crate::db::models::assistant::AssistantRow;
use crate::db::models::conversation::ConversationInsert;
use crate::db::models::message::MessageInsert;
use crate::db::models::turn::TurnStatus;
use crate::events::ChatStreamEvent;
use crate::provider::ToolCall;
use crate::tools::{FileAccess, ShellType, ToolContext};
use crate::turn::TurnOrigin;
use crate::util::{get_conn, now_ms};

use super::SharedState;
use super::protocol::{Kind, ReviewJob, ReviewResponse};
use super::verdict::{self, Outcome, REVIEW_TOOLS};

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
        input_tokens: Some(outcome.progress.input_tokens),
        output_tokens: Some(outcome.progress.output_tokens),
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
        _retry_reason: Option<&str>,
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
    let user_message_id = write_round(state, &conversation_id, &turn_id, &job, &assistant, is_new)
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
) -> Result<AssistantRow, Refused> {
    let (provider_id, model_id) = model.split_once(':').ok_or_else(|| {
        refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("`{model}` is not a provider:model pair; set one in Settings → Hooks"),
        )
    })?;

    let pool = state.services.db.clone();
    let wanted = state.config.assistant_id.clone();
    let base = tokio::task::spawn_blocking(move || -> Result<AssistantRow, String> {
        let mut conn = get_conn(&pool)?;
        match wanted {
            Some(id) => db::ops::assistant::get_assistant(&mut conn, &id).map_err(|e| e.to_string()),
            None => db::ops::assistant::get_default_assistant(&mut conn)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "there is no assistant to base the review on".to_string()),
        }
    })
    .await
    .map_err(|e| refuse(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| refuse(StatusCode::SERVICE_UNAVAILABLE, e))?;

    // What the client says it will allow, not what we would allow: it is the
    // side doing the counting. Falling back to our own setting keeps the
    // reviewer honest when an older plugin sends nothing.
    let max_rounds = job.max_rounds.unwrap_or(state.config.max_rounds);
    let round = job.round.max(1);
    let system_prompt = match job.kind {
        Kind::Plan => verdict::prompt(cwd, round, max_rounds, job.stagnant),
        Kind::Implementation => verdict::implementation_prompt(cwd, round, max_rounds, job.stagnant),
    };
    Ok(AssistantRow {
        provider_id: Some(provider_id.to_string()),
        model_id: Some(model_id.to_string()),
        context_limit: 0,
        max_tokens: None,
        tool_preset_id: None,
        enabled_tools: serde_json::to_string(REVIEW_TOOLS).ok(),
        system_prompt,
        ..base
    })
}

async fn resolve_params(state: &SharedState, assistant: &AssistantRow) -> Result<crate::agent::TurnParams, Refused> {
    let pool = state.services.db.clone();
    let secrets = state.services.secrets.clone();
    let a = assistant.clone();
    let resolved = {
        let pool = pool.clone();
        tokio::task::spawn_blocking(move || crate::agent::resolve_with_overrides(&secrets, &pool, Some(&a), None, None))
            .await
            .map_err(|e| refuse(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map_err(|e| refuse(StatusCode::SERVICE_UNAVAILABLE, e))?
    };

    let assistant = assistant.clone();
    let provider_id = assistant.provider_id.clone();
    // A missing `model_configs` row is an error rather than a fallback: this
    // project's rule is that turn parameters are configured, never invented.
    let mut params = tokio::task::spawn_blocking(move || {
        crate::agent::resolve_turn_params(
            &pool,
            crate::agent::TurnParamsResolveRequest {
                assistant: Some(&assistant),
                provider_id: provider_id.as_deref(),
                provider_type: &resolved.provider_type,
                api_format: &resolved.api_format,

                transport_profile: &resolved.transport_profile,
                model: &resolved.model,
                thinking_level: None,
                fast: false,
            },
        )
    })
    .await
    .map_err(|e| refuse(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| refuse(StatusCode::SERVICE_UNAVAILABLE, e))?;

    // Left absent rather than clamped to the baseline assistant's ceiling. That
    // number was chosen for whatever that assistant answers, and a review is a
    // short verdict after a lot of reading — letting the provider fit the reply
    // to the room it has is closer to right than any number carried over here.
    params.params.max_tokens = None;
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
/// Takes the pool rather than the whole `SharedState` because that is all it
/// uses — and because the wider signature was the entire reason this went
/// untested: standing up `secrets`/`tools`/`mcp`/`coordinator` to check one
/// boolean is enough friction that nobody does it. Never returns an error:
/// an id it cannot vouch for means "open a new one", not "refuse the request".
async fn open_or_reuse(pool: &DbPool, job: &ReviewJob) -> (String, bool) {
    if let Some(claimed) = job.conversation_id.clone().filter(|id| !id.trim().is_empty()) {
        let pool = pool.clone();
        let id = claimed.clone();
        // Matched against *this* kind, so the two gates cannot be handed each
        // other's transcripts: an implementation review continuing in a plan
        // review's conversation would inherit an intention it was meant to
        // judge without.
        let wanted = job.kind.agent_kind();
        let ours = tokio::task::spawn_blocking(move || {
            let mut conn = get_conn(&pool).ok()?;
            let conversation = db::ops::conversation::get_conversation(&mut conn, &id).ok()?;
            Some(conversation.agent_kind.as_deref() == Some(wanted))
        })
        .await
        .ok()
        .flatten()
        .unwrap_or(false);

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
    state: &SharedState,
    conversation_id: &str,
    turn_id: &str,
    job: &ReviewJob,
    assistant: &AssistantRow,
    is_new: bool,
) -> Result<String, String> {
    let pool = state.services.db.clone();
    let conversation_id = conversation_id.to_string();
    let turn_id = turn_id.to_string();
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
    let cwd = job.cwd.clone();
    let (assistant_id, provider_id, model_id) = (
        assistant.id.clone(),
        assistant.provider_id.clone(),
        assistant.model_id.clone(),
    );

    tokio::task::spawn_blocking(move || {
        let mut conn = get_conn(&pool)?;
        let now = now_ms();
        conn.transaction::<_, diesel::result::Error, _>(|conn| {
            // File the review under the project that owns this directory, when
            // there is one. Two reasons, and the second is the load-bearing one:
            // otherwise every repository's reviews pile into the ungrouped list,
            // and — because a desktop turn takes its working directory from
            // `conversation.project_id -> project.path` (`chat.rs:380`) — a
            // conversation with no project has no working directory the moment
            // anyone types into it. The review sets its own root out of band, so
            // without this the transcript is only readable, not continuable.
            //
            // No project for this path just means null, as before. Inventing one
            // would put a row in a list the user curates.
            let project_id = if is_new {
                db::ops::project::find_project_by_path(conn, &cwd)?.map(|p| p.id)
            } else {
                None
            };

            if is_new {
                db::ops::conversation::insert(
                    conn,
                    ConversationInsert {
                        id: &conversation_id,
                        title: Some(&title),
                        assistant_id: Some(&assistant_id),
                        is_pinned: 0,
                        is_archived: 0,
                        created_at: now,
                        updated_at: now,
                        project_id: project_id.as_deref(),
                        // Left null on purpose: a review is meant to be visible
                        // in the sidebar. `parent_conversation_id` is what hides
                        // a sub-agent's transcript, and hiding this one would
                        // put the reviewer's reasoning somewhere with no way in.
                        parent_conversation_id: None,
                        spawned_by_message_id: None,
                        spawned_by_call_id: None,
                        spawned_turn_id: None,
                        agent_kind: Some(agent_kind),
                        agent_provider_id: provider_id.as_deref(),
                        agent_model_id: model_id.as_deref(),
                    },
                )?;
            }

            let head = if is_new {
                None
            } else {
                db::ops::conversation::get_conversation(conn, &conversation_id)
                    .ok()
                    .and_then(|c| c.head_message_id)
            };

            db::ops::message::append_message(
                conn,
                &MessageInsert {
                    id: &message_id,
                    conversation_id: &conversation_id,
                    role: "user",
                    content: &prompt,
                    provider_id: None,
                    model_id: None,
                    input_tokens: None,
                    output_tokens: None,
                    tool_calls: None,
                    tool_call_id: None,
                    sort_order: 0,
                    created_at: now,
                    reasoning_content: None,
                    rating: None,
                    schema_version: 2,
                    is_compact_summary: 0,
                    sender_id: None,
                    parent_id: head.as_deref(),
                    compact_anchor_id: None,
                    source: None,
                    turn_id: Some(&turn_id),
                    tool_outcome: None,
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                    server_tool_calls: None,
                    provider_name: None,
                },
                None,
            )?;

            // The synchronous op rather than `turn_record::begin`: that one
            // opens its own blocking task and so its own connection, which
            // would put this row outside the transaction the other two are in.
            db::ops::turn::begin(conn, &turn_id, &conversation_id, origin, None, now)?;
            Ok(())
        })
        .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;

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
    assistant: &AssistantRow,
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

    let tool_secrets = {
        let pool = state.services.db.clone();
        let secrets = state.services.secrets.clone();
        tokio::task::spawn_blocking(move || crate::agent::build_tool_secrets(&secrets, &pool))
            .await
            .unwrap_or_default()
    };

    let tool_context = ToolContext {
        // The one field the whole review depends on. See the module header.
        working_directory: Some(cwd.to_string()),
        shell: ShellType::default_for_platform(),
        file_access: FileAccess::Unrestricted,
        project_id: None,
        conversation_id: Some(conversation_id.to_string()),
        turn_id: Some(turn_id.to_string()),
        assistant_id: Some(assistant.id.clone()),
        db_pool: Some(state.services.db.clone()),
        #[cfg(not(target_os = "android"))]
        sandbox_policy: None,
        tool_secrets,
        cancel: cancel.clone(),
        journal: None,
    };

    let approvals = NoApprovals;
    let setup = engine::TurnSetup {
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
        pool: &state.services.db,
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

async fn load_history(state: &SharedState, conversation_id: &str) -> db::ops::message::ActiveContext {
    let pool = state.services.db.clone();
    let id = conversation_id.to_string();
    tokio::task::spawn_blocking(move || {
        let mut conn = get_conn(&pool).ok()?;
        let conversation = db::ops::conversation::get_conversation(&mut conn, &id).ok()?;
        let messages = db::ops::message::list_messages(&mut conn, &id).ok()?;
        Some(db::ops::message::active_context(
            &messages,
            conversation.head_message_id.as_deref(),
        ))
    })
    .await
    .ok()
    .flatten()
    .unwrap_or(db::ops::message::ActiveContext {
        path: Vec::new(),
        summary: None,
        anchor_index: None,
        head_id: None,
    })
}

async fn build_config(
    state: &SharedState,
    assistant: &AssistantRow,
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
    };
    let pool = state.services.db.clone();
    let tools = state.services.tools.clone();
    tokio::task::spawn_blocking(move || {
        let mut conn = get_conn(&pool)?;
        crate::agent::turn_config::resolve(&mut conn, &tools, input)
    })
    .await
    .map_err(|e| e.to_string())?
}

async fn build_provider(
    state: &SharedState,
    assistant: &AssistantRow,
) -> Result<(Box<dyn crate::provider::ChatProvider>, crate::agent::ResolvedProvider), String> {
    let pool = state.services.db.clone();
    let secrets = state.services.secrets.clone();
    let a = assistant.clone();
    let resolved = tokio::task::spawn_blocking(move || {
        crate::agent::resolve_with_overrides(&secrets, &pool, Some(&a), None, None)
    })
    .await
    .map_err(|e| e.to_string())??;
    let provider = crate::provider::registry::create_provider(
        &resolved.provider_type,
        &resolved.base_url,
        &resolved.credential,
        &resolved.api_format,
        &resolved.transport_profile,
    )?;
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
    fn conversation(pool: &DbPool, id: &str, agent_kind: Option<&str>) {
        let mut conn = pool.get().unwrap();
        db::ops::conversation::insert(
            &mut conn,
            ConversationInsert {
                id,
                title: Some("t"),
                assistant_id: None,
                is_pinned: 0,
                is_archived: 0,
                created_at: 1,
                updated_at: 1,
                project_id: None,
                parent_conversation_id: None,
                spawned_by_message_id: None,
                spawned_by_call_id: None,
                spawned_turn_id: None,
                agent_kind,
                agent_provider_id: None,
                agent_model_id: None,
            },
        )
        .unwrap();
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
        let pool = crate::db::test_db();
        conversation(&pool, "private", None);

        let (id, is_new) = open_or_reuse(&pool, &claiming(Some("private"))).await;
        assert_ne!(id, "private", "must not write into a conversation that is not ours");
        assert!(is_new);
    }

    #[tokio::test]
    async fn a_sub_agent_conversation_is_not_ours_either() {
        let pool = crate::db::test_db();
        conversation(&pool, "delegated", Some("sub_agent"));

        let (id, is_new) = open_or_reuse(&pool, &claiming(Some("delegated"))).await;
        assert_ne!(id, "delegated");
        assert!(is_new);
    }

    #[tokio::test]
    async fn a_plan_review_conversation_is_reused() {
        let pool = crate::db::test_db();
        conversation(&pool, "ours", Some(Kind::Plan.agent_kind()));

        let (id, is_new) = open_or_reuse(&pool, &claiming(Some("ours"))).await;
        assert_eq!(id, "ours");
        assert!(!is_new, "an existing conversation must not be written again");
    }

    /// The two gates must not inherit each other's transcripts. An
    /// implementation review continuing where a plan review left off would be
    /// judging code against an intention it had already agreed to — which is
    /// exactly the independence the split was for.
    #[tokio::test]
    async fn the_two_gates_do_not_share_a_conversation() {
        let pool = crate::db::test_db();
        conversation(&pool, "planning", Some(Kind::Plan.agent_kind()));

        let asking = ReviewJob {
            conversation_id: Some("planning".into()),
            ..job(Kind::Implementation, 1, vec![])
        };
        let (id, is_new) = open_or_reuse(&pool, &asking).await;
        assert_ne!(id, "planning");
        assert!(is_new);
    }

    /// The client outlived the conversation — the user deleted it. Open a new
    /// one rather than refusing: a review without the earlier context still
    /// beats no review.
    #[tokio::test]
    async fn an_unknown_id_opens_a_new_conversation() {
        let pool = crate::db::test_db();
        let (id, is_new) = open_or_reuse(&pool, &claiming(Some("gone"))).await;
        assert_ne!(id, "gone");
        assert!(is_new);
    }

    #[tokio::test]
    async fn nothing_claimed_opens_a_new_conversation() {
        let pool = crate::db::test_db();
        for claimed in [None, Some(""), Some("   ")] {
            let (id, is_new) = open_or_reuse(&pool, &claiming(claimed)).await;
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

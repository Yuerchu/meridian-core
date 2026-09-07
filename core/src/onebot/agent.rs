use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::agent::engine::{self};
use crate::agent::{
    TokenBudget, build_messages_with_senders, microcompact, resolve_provider_config, trim_to_context_limit,
};
use crate::db::DbPool;
use crate::db::models::message::MessageInsert;
use crate::db::models::turn::TurnPhase;
use crate::mcp::McpRegistry;
use crate::provider::{self, ChatMessage, ToolCall};
use crate::secrets::SecretsManager;
use crate::tools::{self, ToolRegistry};
use crate::util::{get_conn, now_ms};

/// `(tool_call, sandbox_block_reason)` → what the user typed, or `None` if
/// nobody answered. The reason is `Some` only for the retry-without-sandbox
/// escalation ask, so the prompt can say why a second approval for the same
/// call is being requested.
///
/// Words rather than a verdict: what a reply means depends on what was asked,
/// and only the adapter that knows the tool can say. A transport that decided
/// would be able to hand a permission prompt somebody's sentence as if it were
/// an answer, which is the one mapping the ports forbid.
pub type ApprovalFn = Box<
    dyn Fn(ToolCall, Option<String>) -> Pin<Box<dyn Future<Output = Result<Option<String>, String>> + Send>>
        + Send
        + Sync,
>;

/// What the user is being asked for. Decides how the prompt is worded, what the
/// acknowledgement says, and what their words become.
#[derive(Clone, Copy, PartialEq)]
pub enum AskKind {
    /// A tool wants to run. `Y` authorises it; anything else refuses it, and
    /// what they typed travels back as the reason.
    Permission,
    /// `ask_user` asked a question. There is nothing to authorise: whatever
    /// they type is the answer, verbatim.
    Question,
}

impl AskKind {
    pub fn of(tool: &str) -> Self {
        if tool == "ask_user" {
            Self::Question
        } else {
            Self::Permission
        }
    }

    /// The per-tool mapping the ports describe, in the one place it happens.
    fn decide(self, said: &str) -> crate::agent::engine::ApprovalDecision {
        use crate::agent::engine::ApprovalDecision;
        let said = said.trim();
        match (self, said) {
            // A reply with no words in it — an image, a sticker — answered
            // nothing and authorised nothing.
            (_, "") => ApprovalDecision::Denied(None),
            (Self::Question, _) => ApprovalDecision::Response(said.to_string()),
            (Self::Permission, s) if s.eq_ignore_ascii_case("y") || s.eq_ignore_ascii_case("yes") => {
                ApprovalDecision::Approved
            }
            (Self::Permission, s) => ApprovalDecision::Denied(Some(s.to_string())),
        }
    }
}

/// Called with each tool-calling iteration's assistant text before the tools
/// execute, so headless frontends can deliver mid-turn commentary in order
/// (the final iteration's text is the return value instead).
pub type TextNotifyFn = Box<dyn Fn(String) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// What one round of a headless turn left behind, for the caller to report.
///
/// The engine's, under this side's names. There were two of these — identical
/// field for field, because the engine's was transcribed from this one — and one
/// copy is exactly as much as a value that only travels between the loop and its
/// caller needs.
///
/// The terminal `stop` event is deliberately *not* emitted from inside the loop.
/// A QQ conversation can be open in the desktop UI, where a stop is read as
/// permission to send again — so it has to go out after the conversation has
/// actually been handed back, and only once the turn is really over. Whether it
/// is over is the caller's question: a `TurnEnd::Continue` round is the same
/// turn going round again, and announcing a stop between rounds would invite a
/// desktop message the coordinator would then refuse.
pub type TurnProgress = engine::TurnProgress;

/// A headless round's reply, plus what the caller needs to close it out.
pub type HeadlessOutcome = engine::TurnOutcome;

/// The terminal `chat-stream` event for a turn that has ended.
///
/// Built even when the round never got as far as writing an assistant row — a
/// provider that refuses the very first request produces exactly that. The
/// front end is sitting on the `streaming` flag its optimistic send set, and
/// with no stop to clear it, it sits there until the window is reloaded. So
/// `message_id` may be null; the event still goes out.
pub fn turn_stop_event(
    conversation_id: &str,
    turn_id: &str,
    message_id: Option<&str>,
    reason: crate::events::ChatStopReason,
    input_tokens: i32,
    output_tokens: i32,
) -> crate::events::ChatStreamEvent {
    crate::events::ChatStreamEvent::Stop {
        reason,
        message_id: message_id.map(str::to_string),
        turn_id: turn_id.to_string(),
        conversation_id: conversation_id.to_string(),
        input_tokens: Some(input_tokens),
        output_tokens: Some(output_tokens),
    }
}

/// A QQ turn answer travels over the chat transport; these events are a
/// courtesy to a desktop window that may not even be open. So a send that
/// fails is not the turn failing -- see the Emit trait for the desktop
/// opposite reading.
struct BestEffortEmit(crate::events::EventBus);

impl crate::agent::engine::Emit for BestEffortEmit {
    fn emit(&self, channel: &str, payload: serde_json::Value) -> Result<(), String> {
        let _ = self.0.emit(channel, payload);
        Ok(())
    }
}

/// Asking in a chat, where the answer arrives as whatever the person typed.
///
/// The mapping to a decision is per tool and cannot be otherwise. `ask_user`
/// asks a *question*, and the loop reads its answer as the thing to hand back to
/// the model. Everything else is a permission, where a yes is `Approved` and
/// nothing else will do: reading somebody's sentence as a `Response` there would
/// turn typing into authorisation to run a command.
struct ChatApprovals<'a> {
    approval_fn: &'a ApprovalFn,
    pool: DbPool,
    turn_id: String,
}

#[async_trait::async_trait]
impl crate::agent::engine::Approvals for ChatApprovals<'_> {
    async fn ask(
        &self,
        _assistant_message_id: &str,
        call: &ToolCall,
        retry_reason: Option<&str>,
    ) -> Result<Option<crate::agent::engine::ApprovalDecision>, String> {
        // A QQ approval is a message in a chat and can sit there for the full
        // minute, so this is a window the process can easily be killed in — and
        // dying here means nothing ran, which is worth being able to say.
        let said = engine::in_phase(
            &self.pool,
            &self.turn_id,
            TurnPhase::AwaitingApproval,
            Some(&call.name),
            (self.approval_fn)(call.clone(), retry_reason.map(str::to_string)),
        )
        .await?;
        //
        // `None` is nobody answering — the minute ran out, or the turn was swept
        // out from under the question. Distinct from a refusal for the first
        // time, and the loop already words the two differently.
        Ok(said.map(|text| AskKind::of(&call.name).decide(&text)))
    }
}

/// Mid-turn commentary, sent as its own chat message.
///
/// Only the final iteration's text is returned to the caller, so without this
/// everything the model says on the way to a tool call would exist solely in
/// the database. The desktop needs no equivalent: it has already streamed every
/// one of those characters to the window.
struct ChatCommentary<'a>(&'a TextNotifyFn);

#[async_trait::async_trait]
impl crate::agent::engine::Commentary for ChatCommentary<'_> {
    async fn say(&self, text: String) {
        (self.0)(text).await;
    }
}

/// The tools that belong to the chat rather than to the machine.
struct QqSurface<'a>(&'a super::qq_tools::QqToolExecutor);

#[async_trait::async_trait]
impl crate::agent::engine::SurfaceTools for QqSurface<'_> {
    fn owns(&self, name: &str) -> bool {
        self.0.owns(name)
    }
    fn requires_approval(&self, name: &str) -> bool {
        self.0.requires_approval(name)
    }
    async fn execute(&self, name: &str, arguments: &str) -> Result<String, String> {
        self.0.execute(name, arguments).await
    }
}

/// What arrived while the turn was running.
///
/// Also where a turn loses authority. The permission a turn opened with belongs
/// to whoever opened it, and a group hands the floor to anyone: an ordinary
/// member talking into a turn an admin started used to inherit its tools, and
/// `qq_get_friend_list` / `qq_get_group_list` need no approval, so inheriting
/// them was a leak that needed nobody's consent. Once someone without admin
/// standing has spoken, the rest of the turn runs on the ordinary set — it does
/// not climb back, because the admin cannot vouch for a request they have not
/// seen.
struct InboxSteering<'a> {
    inbox: &'a super::InboxHandle,
    /// What an ordinary member of this session may run. Empty when the turn has
    /// no QQ tools to fall back to, which is the safe reading of "no idea".
    ordinary: std::collections::HashSet<String>,
    /// Set by `drain`, read by `narrowed`. Atomic rather than `Cell` because
    /// the port is held across an await and must stay `Sync`.
    demoted: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl crate::agent::engine::Steering for InboxSteering<'_> {
    fn narrowed(&self) -> Option<std::collections::HashSet<String>> {
        self.demoted
            .load(std::sync::atomic::Ordering::Relaxed)
            .then(|| self.ordinary.clone())
    }

    async fn drain(&self) -> Vec<crate::agent::engine::Steered> {
        let items = self.inbox.drain();
        // A notice carries no sender — nobody said it, so it cannot lower
        // anything. Only a person who is not an admin does.
        if items.iter().any(|i| i.sender.as_ref().is_some_and(|s| !s.is_admin)) {
            self.demoted.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        items
            .into_iter()
            .map(|item| {
                crate::agent::engine::Steered::typed(
                    item.text,
                    // A notice is something the system generated rather than
                    // something a person said, and it travels as context instead
                    // of as a user message.
                    match item.sender.as_ref() {
                        Some(s) => crate::agent::engine::SteeredOrigin::User(Some(s.into())),
                        None => crate::agent::engine::SteeredOrigin::System,
                    },
                )
            })
            .collect()
    }
}

/// One model call with no tools, no history and no persistence — used by the
/// post-turn extraction pass.
///
/// Kept separate from `headless_chat` on purpose: this must not be able to call
/// tools, write messages, or otherwise act on the conversation. It only reads
/// what happened and answers a question about it.
pub(super) async fn oneshot_completion(
    state: &Arc<super::SharedState>,
    conversation_id: &str,
    system_prompt: &str,
    user_prompt: &str,
) -> Result<String, String> {
    let assistant = {
        let pool = state.services.db.clone();
        let conv_id = conversation_id.to_string();
        tokio::task::spawn_blocking(move || {
            let mut conn = get_conn(&pool)?;
            let conv =
                crate::db::ops::conversation::get_conversation(&mut conn, &conv_id).map_err(|e| e.to_string())?;
            Ok::<_, String>(
                conv.assistant_id
                    .and_then(|id| crate::db::ops::assistant::get_assistant(&mut conn, &id).ok()),
            )
        })
        .await
        .map_err(|e| e.to_string())??
    };

    // Both resolutions take a pooled connection, and the first also reads the OS
    // credential store, so they run off the async thread.
    //
    // The turn parameters are resolved like any other turn: an extraction
    // request that invents its own temperature is rejected by models the chat
    // path already talks to.
    let (provider_type, base_url, credential, api_format, transport_profile, turn, provider_id, provider_name, model) = {
        let pool2 = state.services.db.clone();
        let secrets2 = state.services.secrets.clone();
        let assistant2 = assistant.clone();
        tokio::task::spawn_blocking(move || {
            let crate::agent::ResolvedProvider {
                provider_id,
                provider_name,
                provider_type,
                base_url,
                credential,
                model,
                api_format,
                transport_profile,
            } = resolve_provider_config(&secrets2, &pool2, assistant2.as_ref())?;
            let effective_model = assistant2.as_ref().and_then(|a| a.model_id.clone()).unwrap_or(model);
            let turn = crate::agent::resolve_turn_params(
                &pool2,
                crate::agent::TurnParamsResolveRequest {
                    assistant: assistant2.as_ref(),
                    provider_id: assistant2.as_ref().and_then(|a| a.provider_id.as_deref()),
                    provider_type: &provider_type,
                    api_format: &api_format,

                    transport_profile: &transport_profile,
                    model: &effective_model,
                    thinking_level: None,
                    fast: false,
                },
            )?;
            Ok::<_, String>((
                provider_type,
                base_url,
                credential,
                api_format,
                transport_profile,
                turn,
                provider_id,
                provider_name,
                effective_model,
            ))
        })
        .await
        .map_err(|e| e.to_string())??
    };
    let provider =
        provider::registry::create_provider(&provider_type, &base_url, &credential, &api_format, &transport_profile)?;

    let messages = vec![
        ChatMessage {
            role: "system".into(),
            content: system_prompt.into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            tool_error: false,
            provider_state: None,
            origin: provider::MessageOrigin::Assistant,
        },
        // The transcript is data being analysed, not an instruction being
        // followed, and it carries no speaker of its own.
        ChatMessage::system_context(user_prompt),
    ];

    // `chat_with_tools` with an empty list, for the usage `chat` throws away.
    // Extraction runs after every QQ turn and used to appear on no bill at all.
    let answer = provider
        .chat_with_tools(messages, Vec::new(), crate::agent::without_thinking(turn.params))
        .await
        .map_err(|e| e.to_string())?;

    if let Some(usage) = answer.usage {
        let pool = state.services.db.clone();
        let conv_id = conversation_id.to_string();
        let _ = tokio::task::spawn_blocking(move || {
            let mut conn = get_conn(&pool)?;
            let Some(message_id) = crate::db::ops::conversation::get_conversation(&mut conn, &conv_id)
                .ok()
                .and_then(|c| c.head_message_id)
            else {
                tracing::warn!("could not record what the extraction cost: no message to file it against");
                return Ok(());
            };
            let cost = crate::db::ops::audit::SideRequestCost {
                role: crate::db::ops::audit::EXTRACTION_ROLE,
                message_id: &message_id,
                conversation_id: &conv_id,
                turn_id: None,
                provider_id: Some(&provider_id),
                provider_name: Some(&provider_name),
                model_id: Some(&model),
                usage: crate::db::models::message::MessageUsage {
                    input_tokens: usage.prompt_tokens,
                    output_tokens: usage.completion_tokens,
                    cache_read_tokens: usage.cache_read_tokens,
                    cache_write_tokens: usage.cache_write_tokens,
                    server_tool_calls: usage.billable_tool_calls,
                },
                peak_prompt_tokens: usage.prompt_tokens,
                summary: "extraction",
            };
            if let Err(e) = crate::db::ops::audit::record_side_request(&mut conn, cost) {
                tracing::warn!(error = %e, "could not record what the extraction cost");
            }
            Ok::<_, String>(())
        })
        .await;
    }

    Ok(answer.text)
}

/// Run a headless chat session with optional Tauri event streaming.
///
/// - `is_admin`: controls whether tools are available at all
/// - `approval_fn`: called for Ask-permission tools (admin only); returns true to approve
/// - `services`: when `Some`, emits `chat-stream` events for real-time UI updates
///
/// Every event except the terminal `stop` goes out from in here. That one is
/// handed back in `TurnProgress` instead — see it for why.
#[allow(clippy::too_many_arguments)]
pub async fn headless_chat(
    pool: &DbPool,
    secrets: &Arc<SecretsManager>,
    tool_registry: &Arc<ToolRegistry>,
    mcp_registry: &Arc<McpRegistry>,
    conversation_id: &str,
    turn_id: &str,
    project_id: Option<&str>,
    incoming: &[super::IncomingMessage],
    assistant_id: Option<&str>,
    model_override: Option<&str>,
    is_admin: bool,
    approval_fn: &ApprovalFn,
    interim_text_fn: Option<&TextNotifyFn>,
    cancel: &CancellationToken,
    services: Option<&crate::services::Services>,
    qq_tools: Option<&super::qq_tools::QqToolExecutor>,
    session_inbox: Option<&super::InboxHandle>,
    // Needed to tell a turn that really is running from one whose row still
    // says so because it was killed. `None` in tests that do not care.
    coordinator: Option<&Arc<crate::turn::TurnCoordinator>>,
) -> HeadlessOutcome {
    // Written into as the round goes, so the `?`-heavy body below can bail out
    // anywhere and still leave the caller enough to close the turn out.
    let mut progress = TurnProgress::default();
    let reply = headless_chat_inner(
        pool,
        secrets,
        tool_registry,
        mcp_registry,
        conversation_id,
        turn_id,
        project_id,
        incoming,
        assistant_id,
        model_override,
        is_admin,
        approval_fn,
        interim_text_fn,
        cancel,
        services,
        qq_tools,
        session_inbox,
        coordinator,
        &mut progress,
    )
    .await;
    HeadlessOutcome { reply, progress }
}

#[allow(clippy::too_many_arguments)]
async fn headless_chat_inner(
    pool: &DbPool,
    // The `Arc` rather than a plain reference: provider resolution is handed to
    // `spawn_blocking`, which needs an owned handle.
    secrets: &Arc<SecretsManager>,
    tool_registry: &Arc<ToolRegistry>,
    mcp_registry: &Arc<McpRegistry>,
    conversation_id: &str,
    // Which run of the turn this is. A QQ conversation can be opened in the
    // desktop UI, and these events go down the same `chat-stream` channel, so
    // without the id the front end cannot tell this turn's stop from anyone
    // else's — and a QQ session left in the UI would stream forever.
    // Unchanged across `TurnEnd::Continue` rounds: they are one turn.
    turn_id: &str,
    project_id: Option<&str>,
    // This turn's inbound messages, each keeping its own speaker. Several
    // arrive at once when messages queued up while a previous turn was running.
    incoming: &[super::IncomingMessage],
    assistant_id: Option<&str>,
    model_override: Option<&str>,
    is_admin: bool,
    approval_fn: &ApprovalFn,
    interim_text_fn: Option<&TextNotifyFn>,
    cancel: &CancellationToken,
    services: Option<&crate::services::Services>,
    qq_tools: Option<&super::qq_tools::QqToolExecutor>,
    session_inbox: Option<&super::InboxHandle>,
    coordinator: Option<&Arc<crate::turn::TurnCoordinator>>,
    progress: &mut TurnProgress,
) -> Result<String, String> {
    // Every stream event this round sends goes through here. `None` when no
    // window is attached, which for a QQ turn is the ordinary case.
    let emitter = services.map(|s| BestEffortEmit(s.events.clone()));
    let emit = emitter.as_ref().map(|e| e as &dyn crate::agent::engine::Emit);

    // Keep the machine awake for the rest of the turn (RAII; missing pref = enabled).
    let _sleep_guard = {
        let sleep_enabled = {
            let pool = pool.clone();
            tokio::task::spawn_blocking(move || -> Result<bool, String> {
                let mut conn = get_conn(&pool)?;
                let stored = crate::db::ops::preference::get_preference(&mut conn, "sleep_inhibitor.enabled")
                    .map_err(|error| error.to_string())?;
                crate::db::ops::preference::parse_bool_preference("sleep_inhibitor.enabled", stored.as_deref(), true)
            })
            .await
            .map_err(|error| error.to_string())??
        };
        services.filter(|_| sleep_enabled).map(|s| s.sleep.begin_turn())
    };

    // Load assistant + the conversation's active path
    let (assistant, ctx) = {
        let pool = pool.clone();
        let conv_id = conversation_id.to_string();
        let aid = assistant_id.map(String::from);
        tokio::task::spawn_blocking(move || {
            let mut conn = get_conn(&pool)?;
            let conv =
                crate::db::ops::conversation::get_conversation(&mut conn, &conv_id).map_err(|e| e.to_string())?;
            let effective_aid = aid.as_deref().or(conv.assistant_id.as_deref());
            let assistant = effective_aid.and_then(|aid| crate::db::ops::assistant::get_assistant(&mut conn, aid).ok());
            let history = crate::db::ops::message::list_messages(&mut conn, &conv_id).map_err(|e| e.to_string())?;
            // Resolved once and carried for the turn; see the desktop loop for
            // why the head is not re-read per row.
            let ctx = crate::db::ops::message::active_context(&history, conv.head_message_id.as_deref());
            Ok::<_, String>((assistant, ctx))
        })
        .await
        .map_err(|e| e.to_string())??
    };

    // Resolve provider off the async thread: it takes a pooled connection and
    // reads the OS credential store, either of which can block for as long as
    // the pool's acquire timeout.
    let crate::agent::ResolvedProvider {
        provider_type,
        base_url,
        credential,
        model,
        api_format,
        transport_profile,
        provider_id,
        provider_name,
    } = {
        let pool2 = pool.clone();
        let secrets2 = secrets.clone();
        let assistant2 = assistant.clone();
        tokio::task::spawn_blocking(move || resolve_provider_config(&secrets2, &pool2, assistant2.as_ref()))
            .await
            .map_err(|e| e.to_string())??
    };
    let provider =
        provider::registry::create_provider(&provider_type, &base_url, &credential, &api_format, &transport_profile)?;

    // The same resolver the desktop loop uses. Sharing it is what keeps a QQ
    // assistant's tool set honest: this path used to read `enabled_tools` only,
    // so an assistant configured with a tool preset quietly got a different set
    // here than in the app.
    //
    // Collaboration modes stay off: a headless turn has no way to switch them,
    // and a QQ session already cannot touch the filesystem (its file access is
    // an empty root set), so plan mode would guard nothing.
    // What the model is *shown* has to belong to the session, while what this
    // turn's speaker may *run* is `offered`. The two used to be the same set,
    // and because `is_admin` is decided per message, a group where an admin and
    // an ordinary member both speak alternated between two tool arrays — and so
    // between two system prompts, `base_prompt` being derived from the tool set.
    // Every alternation was a full cache miss on a prefix that had not
    // otherwise changed.
    //
    // Resolving that by showing everyone everything would have been a different
    // mistake: a registry or MCP definition carries the user's own server names
    // and argument schemas, and a group cannot show them to its admin without
    // showing them to everyone in it. So these stay off in a group entirely —
    // `qq_tools` covers what a group session can actually reach anyway, its
    // file access being an empty root set — and a private chat keeps them,
    // where the one counterpart makes `is_admin` constant for the session and
    // the prefix stays put either way.
    let full_toolset = qq_tools
        .map(|q| super::qq_tools::exposes_full_toolset(q.session_kind(), is_admin))
        .unwrap_or(false);
    // Read off the published snapshot rather than through the connection lock,
    // so a server that is mid-call cannot hold up this turn from starting.
    let mcp_defs = if full_toolset {
        mcp_registry.tool_definitions().as_ref().clone()
    } else {
        Vec::new()
    };
    let effective_model = model_override
        .or(assistant.as_ref().and_then(|a| a.model_id.as_deref()))
        .unwrap_or(&model)
        .to_string();
    // Same resolution as the desktop chat command, so a per-model config the
    // user wrote applies here too. No per-request tier: OneBot turns run off
    // the assistant's stored defaults. Off the async thread because it takes a
    // pooled connection.
    //
    // Ahead of the tool set because what the model can be sent at all — whether
    // it takes a tools field — decides what that set may contain.
    let mut turn_params = {
        let pool2 = pool.clone();
        let assistant2 = assistant.clone();
        let pt = provider_type.clone();
        let af = api_format.clone();
        let tp = transport_profile.clone();
        let em = effective_model.clone();
        // The provider this turn actually resolved to, not the assistant's
        // stored field. They differ whenever the assistant names none and the
        // fallback picked the first enabled one — and with the assistant's
        // empty field there is no `model_configs` row to find, so that turn
        // silently loses its context window, its prices, its capability
        // overrides and its provider-side tools. The desktop has always passed
        // the resolved id.
        let pid = provider_id.clone();
        tokio::task::spawn_blocking(move || {
            crate::agent::resolve_turn_params(
                &pool2,
                crate::agent::TurnParamsResolveRequest {
                    assistant: assistant2.as_ref(),
                    provider_id: Some(pid.as_str()),
                    provider_type: &pt,
                    api_format: &af,

                    transport_profile: &tp,
                    model: &em,
                    thinking_level: None,
                    fast: false,
                },
            )
        })
        .await
        .map_err(|e| e.to_string())??
    };
    // The same reasoning as the desktop path: one room, one stable prefix, one
    // server holding it. Worth more here than there, since a group's prefix is
    // long and every message in it is another turn against the same one.
    turn_params.params.cache_key = Some(conversation_id.to_string());
    let context_limit = turn_params.context_limit;
    // Off `turn_params` rather than a second `get_capabilities` call. That one
    // goes through `capabilities::resolve` without `apply_overrides`, so a
    // per-model capability the user wrote was ignored here while every other
    // parameter of the same request honoured it.
    let supports_tools = turn_params.caps.supports_tools;
    let supports_images = turn_params.caps.supports_images;
    if !supports_tools {
        tracing::info!(model = %effective_model, "the model cannot take tools; none are offered this turn");
    }

    let turn = {
        let pool2 = pool.clone();
        let registry = tool_registry.clone();
        let input = crate::agent::turn_config::TurnConfigResolveRequest {
            server_tools: turn_params.params.server_tools.clone(),
            assistant: assistant.clone(),
            conversation_id: conversation_id.to_string(),
            project_id: project_id.map(|s| s.to_string()),
            // No transitions port on this side, so no transition tool. Left
            // switchable, an admin session is always offered `enter_plan` --
            // it only needs one write tool in the set -- and calling it reaches
            // the registry, whose refusal to be called outside the loop lands
            // in the transcript as this turn's tool result.
            mode: crate::agent::modes::Modes::Fixed,
            // And no sub-agents port either, for the same reason stated the
            // same way: a QQ session's file access is an empty root set, so a
            // delegated run could reach nothing, and a group chat has nowhere
            // to put the approvals it would raise.
            sub_agents: None,
            mcp_defs,
            // Session-scoped, not speaker-scoped — see `full_toolset` above.
            // The QQ tools are appended further down, on the same footing.
            //
            // A session that cannot have the registry still gets the few tools
            // whose definitions say nothing about this machine; `web_search` was
            // only ever excluded by being filed with the rest.
            exposure: if !supports_tools {
                crate::agent::turn_config::ToolExposure::None
            } else if full_toolset {
                crate::agent::turn_config::ToolExposure::All
            } else {
                crate::agent::turn_config::ToolExposure::Only(super::qq_tools::OPEN_REGISTRY_TOOLS)
            },
            persona: assistant.as_ref().map(|a| a.system_prompt.clone()).unwrap_or_default(),
            // Memory is absent on purpose — it ships as a user-role message.
            context_blocks: Vec::new(),
        };
        tokio::task::spawn_blocking(move || {
            let mut conn = pool2.get().map_err(|e| e.to_string())?;
            crate::agent::turn_config::resolve(&mut conn, &registry, input)
        })
        .await
        .map_err(|e| e.to_string())??
    };
    let mut tool_defs = turn.tool_defs;
    let system_prompt = turn.system_prompt;

    // Who this turn may recall. A private chat is about the one person on the
    // other end; a group is about whoever actually spoke, filtered so nothing
    // learned one-to-one can surface in front of everyone.
    let is_group = incoming.iter().any(|m| m.sender.as_ref().is_some_and(|s| s.is_group));
    let subjects: Vec<crate::agent::MemorySubjectRef> = incoming
        .iter()
        .filter_map(|m| m.sender.as_ref())
        .map(|s| {
            crate::agent::MemorySubjectRef::from_user(s.user_id, s.nickname.clone())
                .with_standing(s.role.clone(), s.title.clone())
        })
        .collect();
    let budget_tokens = crate::agent::memory_budget(context_limit);
    let memory_request = if is_group {
        crate::agent::MemoryRequest::onebot_group(project_id.map(|s| s.to_string()), subjects, budget_tokens)
    } else {
        match subjects.into_iter().next() {
            Some(subject) => crate::agent::MemoryRequest::onebot_private(subject, budget_tokens),
            None => crate::agent::MemoryRequest::desktop(project_id.map(|s| s.to_string()), budget_tokens),
        }
    };
    // No auto-compaction on this side, so there is no later point at which the
    // path could change under us — see the desktop loop, where this has to wait.
    let t0 = now_ms();
    let roster = crate::agent::roster_block(&memory_request);
    let injection = crate::agent::plan_injection_async(pool, memory_request, ctx.live().to_vec(), t0).await?;
    let keep_recent = assistant.as_ref().map(|a| a.compact_keep_recent as usize).unwrap_or(10);

    let budget = TokenBudget::new(
        &provider_type,
        &effective_model,
        context_limit,
        turn_params.max_output,
        turn_params.compact_threshold,
    );

    // Nicknames are not on the message row (they change), so history is
    // re-attributed from the subject table.
    let sender_names = {
        let pool2 = pool.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = get_conn(&pool2)?;
            let subjects = crate::db::ops::memory::list_subjects(&mut conn).map_err(|e| e.to_string())?;
            Ok::<_, String>(
                subjects
                    .into_iter()
                    .filter_map(|s| {
                        let uid = s.user_id()?;
                        Some((uid, s.display_name?))
                    })
                    .collect::<crate::agent::SenderNames>(),
            )
        })
        .await
        .map_err(|e| e.to_string())?
        .unwrap_or_default()
    };

    // How the previous turns stopped, for any that did not stop cleanly. Same
    // block the desktop gets: a QQ turn is just as capable of dying with a tool
    // half run, and the model is the one that has to decide what to do about it.
    // Emptied by the first reply read to the end, not by reading the record and
    // not by getting a request away.
    let interrupted = match coordinator {
        Some(c) => crate::agent::interrupted::load_block(pool, c, conversation_id, turn_id).await?,
        None => None,
    };

    // Background first, then what was just said, then who is in the room. The
    // order matters and is explained on `trailing_with_memory`, whose shape this
    // reproduces — it cannot be called directly, because a QQ turn can open with
    // several messages from several people and that function takes one.
    let mut trailing: Vec<provider::ChatMessage> = Vec::new();
    for block in [
        injection.as_ref().and_then(|i| i.text.as_deref()),
        interrupted.as_ref().map(|r| r.text()),
    ]
    .into_iter()
    .flatten()
    {
        if !block.trim().is_empty() {
            trailing.push(provider::ChatMessage::system_context(block.trim_start()));
        }
    }
    trailing.extend(incoming.iter().map(|m| match m.sender.as_ref() {
        Some(s) => provider::ChatMessage::user_from(&m.text, s.into()),
        None => provider::ChatMessage::user(&m.text),
    }));
    if let Some(roster) = roster.as_deref().filter(|r| !r.trim().is_empty()) {
        trailing.push(provider::ChatMessage::system_context(roster.trim_start()));
    }

    let mut chat_messages = build_messages_with_senders(&system_prompt, &ctx, trailing, &sender_names)?;
    let data_dir = services.map(|s| s.paths.data_dir.as_path());
    crate::agent::resolve_sticker_parts_in_messages(&mut chat_messages, pool, data_dir, supports_images)?;
    let files_root = data_dir.map(crate::files::files_dir);
    crate::agent::resolve_file_uris_in_messages(&mut chat_messages, files_root.as_deref())?;
    microcompact(&mut chat_messages, &budget, keep_recent);
    trim_to_context_limit(&mut chat_messages, context_limit, keep_recent);

    let params = turn_params.params;

    // Session-scoped QQ tools are available to everyone (read-only,
    // scope-locked) -- but not to a model that cannot take a tools field at
    // all. This used to be covered by the clear() that followed; with the check
    // moved into the resolver, these are the one set it does not reach.
    if let Some(qq) = qq_tools.filter(|_| supports_tools) {
        // The desktop registry also owns the two generic sticker tools. On QQ
        // the session-scoped implementation must replace them: duplicate tool
        // names are rejected by providers, and only this one actually sends.
        tool_defs.retain(|definition| !qq.owns(&definition.name));
        tool_defs.extend(qq.definitions());
    }
    // What may actually execute, which is where this turn's speaker is
    // weighed. An ordinary member in a group is held to the scope-locked
    // read-only QQ tools exactly as before; what changed is that the refusal
    // happens at dispatch rather than by withholding the definition, so the
    // prefix does not change shape depending on who spoke. `run_turn` checks
    // this ahead of every path — surface tools included — and `execute`
    // re-checks it.
    //
    // `ordinary_names` rather than `permitted_names`: the executor was built
    // with the authority this *turn* opened with, and a round that a plain
    // member started or joined must not read its permissions off that.
    //
    // The open registry tools are read back off `tool_defs` rather than from the
    // constant, so a tool the assistant has switched off is not authorised by a
    // list that only says which ones *may* be shown. Shown to everyone, so
    // runnable by everyone: withholding at dispatch what the array advertises to
    // the whole group is how you get a model repeatedly calling a tool it is
    // told it has, in front of an audience.
    let offered: std::collections::HashSet<String> = if is_admin {
        tool_defs.iter().map(|t| t.name.clone()).collect()
    } else {
        qq_tools
            .filter(|_| supports_tools)
            .map(|q| super::qq_tools::ordinary_offered(q.ordinary_names(), &tool_defs))
            .unwrap_or_default()
    };

    // Persist this turn's inbound messages, one row each so every speaker keeps
    // their own attribution. They share one timestamp because they were drained
    // as a single batch; every row written later in the turn stamps its own
    // now_ms() so relative times differ and the turn's elapsed time is derivable.
    let now = now_ms();
    let user_msg_id = uuid::Uuid::new_v4().to_string();
    // Walks down the branch as the turn writes. A group turn can open with
    // several user rows, and steering can add more mid-flight, so this has to be
    // a cursor rather than one precomputed parent.
    let mut parent_cursor: Option<String> = ctx.head_id.clone();
    // Ahead of the messages, because that is where it was sent and where the next
    // turn has to find it.
    if let Some(ref injection) = injection {
        parent_cursor =
            crate::agent::persist_injection(pool, injection, conversation_id, turn_id, parent_cursor, now).await;
    }
    {
        let pool = pool.clone();
        let conv_id = conversation_id.to_string();
        let first_id = user_msg_id.clone();
        let rows: Vec<(String, String, Option<i64>)> = incoming
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let id = if i == 0 {
                    first_id.clone()
                } else {
                    uuid::Uuid::new_v4().to_string()
                };
                (id, m.text.clone(), m.sender.as_ref().map(|s| s.user_id))
            })
            .collect();
        let mut parent = parent_cursor.clone();
        parent_cursor = rows.last().map(|(id, _, _)| id.clone()).or(parent_cursor);
        let turn = turn_id.to_string();
        tokio::task::spawn_blocking(move || {
            let mut conn = get_conn(&pool)?;
            for (msg_id, msg, sender_id) in &rows {
                crate::db::ops::message::append_message(
                    &mut conn,
                    &MessageInsert {
                        id: msg_id,
                        conversation_id: &conv_id,
                        role: "user",
                        content: msg,
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
                        sender_id: *sender_id,
                        parent_id: None,
                        compact_anchor_id: None,
                        source: None,
                        turn_id: Some(&turn),
                        tool_outcome: None,
                        // What someone said cost no tokens and came from no upstream.
                        cache_read_tokens: None,
                        cache_write_tokens: None,
                        server_tool_calls: None,
                        provider_name: None,
                    },
                    parent.as_deref(),
                )
                .map_err(|e| e.to_string())?;
                crate::db::ops::emoji::link_stickers_in_content(&mut conn, msg_id, msg).map_err(|e| e.to_string())?;
                // Queued messages chain to each other, not all to the same parent.
                parent = Some(msg_id.clone());
            }
            Ok::<_, String>(())
        })
        .await
        .map_err(|e| e.to_string())??;
    }

    // Build tool context
    let (shell_type, sandbox_pref) = {
        let pool2 = pool.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool2.get().ok()?;
            let shell = crate::db::ops::preference::get_preference(&mut conn, "shell")
                .ok()
                .flatten();
            let sandbox = crate::db::ops::preference::get_preference(&mut conn, "sandbox.enabled")
                .ok()
                .flatten();
            Some((shell, sandbox))
        })
        .await
        .ok()
        .flatten()
        .unwrap_or((None, None))
    };
    let execution_mode = crate::sandbox::ExecutionMode::parse(sandbox_pref.as_deref())?;
    // Headless sessions have no project directory. A requested container is
    // therefore refused explicitly instead of being downgraded to the platform
    // default; there is no honest answer to what the container should mount.
    #[cfg(not(target_os = "android"))]
    let sandbox_policy = crate::sandbox::resolve_sandbox_policy(
        execution_mode,
        None,
        conversation_id,
        services.map(|services| services.containers.clone() as std::sync::Arc<dyn crate::container::CommandConnector>),
    )
    .map_err(|error| error.to_string())?;
    #[cfg(target_os = "android")]
    let _ = execution_mode;
    let tool_context = tools::ToolContext {
        working_directory: None,
        shell: shell_type
            .map(|value| tools::ShellType::parse(&value))
            .transpose()?
            .unwrap_or_else(tools::ShellType::default_for_platform),
        // Headless (QQ) sessions have no project dir, and Unrestricted access
        // with no directory to be restricted to is the whole host filesystem.
        // An empty root set denies every path at the validation layer instead.
        file_access: tools::FileAccess::Roots(vec![]),
        project_id: project_id.map(|s| s.to_string()),
        conversation_id: Some(conversation_id.to_string()),
        turn_id: Some(turn_id.to_string()),
        assistant_id: assistant_id.map(|s| s.to_string()),
        db_pool: Some(pool.clone()),
        // No journal: the empty root set above refuses every file write at the
        // validation layer, so there is nothing a journal here could ever
        // record — wiring one would be dead code asserting otherwise.
        journal: None,
        #[cfg(not(target_os = "android"))]
        sandbox_policy,
        tool_secrets: {
            let pool2 = pool.clone();
            let secrets2 = secrets.clone();
            tokio::task::spawn_blocking(move || crate::agent::build_tool_secrets(&secrets2, &pool2))
                .await
                .map_err(|e| e.to_string())?
        },
        cancel: cancel.clone(),
    };

    // The loop is the desktop's too now. What stays here is what the two do not
    // share: this session's own setup above, and the round's accounting below —
    // the lease, the turn record and the terminal event all belong to the
    // caller, because `TurnEnd::Continue` makes a round and a turn two different
    // things and only the caller knows which one is ending.
    let asker = ChatApprovals {
        approval_fn,
        pool: pool.clone(),
        turn_id: turn_id.to_string(),
    };
    // `unattended`: a QQ approval is a message in a chat that nobody may be
    // reading, which is why most of them end in a timeout today. So a review
    // that cannot reach a verdict refuses rather than falling back to asking —
    // there is nobody to ask. Needs `services`, which the tests do not build;
    // without it the asker is left exactly as it was.
    let approvals = match services {
        Some(services) => crate::agent::auto_review::AutoReviewed::wrap(
            &asker,
            crate::agent::auto_review::Context {
                services: services.clone(),
                conversation_id: conversation_id.to_string(),
                turn_id: turn_id.to_string(),
                // A QQ session's file access is an empty root set, so there is
                // no project for a path to be inside of. Saying so is what
                // stops the reviewer reading "outside the project" as the
                // finding it would be on the desktop.
                working_directory: None,
                // The same empty root set the turn itself runs under. The
                // escalating pass gets no more of the disk than the turn had,
                // which here is none of it.
                file_access: tools::FileAccess::Roots(vec![]),
                // Only a group is. A private chat has one counterpart and they
                // are why the turn is running — treating them as a bystander
                // because they are not an admin would have the reviewer see a
                // task nobody asked for and refuse everything.
                multi_party: qq_tools
                    .is_some_and(|q| matches!(q.session_kind(), crate::onebot::session::SessionKind::Group)),
                unattended: true,
            },
        )?,
        None => crate::agent::auto_review::AutoReviewed::inert(&asker),
    };
    // Outermost, so it sees the reviewer's own refusals as well as the ones a
    // person gave. Underneath it, the denials cheapest to repeat — the ones
    // nothing stopped to ask about — would be exactly the ones it missed.
    let approvals = crate::agent::denied::DeniedMemory::wrap(&approvals);
    let commentary = interim_text_fn.map(ChatCommentary);
    let surface = qq_tools.map(QqSurface);
    let steering = session_inbox.map(|inbox| InboxSteering {
        inbox,
        ordinary: qq_tools
            .filter(|_| supports_tools)
            .map(|q| super::qq_tools::ordinary_offered(q.ordinary_names(), &tool_defs))
            .unwrap_or_default(),
        demoted: std::sync::atomic::AtomicBool::new(false),
    });

    let outcome = engine::run_turn(
        &engine::TurnServices {
            pool,
            tools: tool_registry,
            mcp: mcp_registry,
        },
        engine::TurnSetup {
            provider: &*provider,
            params,
            chat_messages,
            tool_defs,
            offered,
            // Modes stay off. A headless turn has no way to switch them, and a
            // QQ session already cannot touch the filesystem — its file access
            // is an empty root set — so plan mode would guard nothing. The same
            // `Modes::Fixed` above is what keeps the transition tools out of the
            // set; this is where the mode itself lands.
            mode: crate::agent::modes::Modes::Fixed.spec(),
            tool_context,
            budget,
            turn_id: turn_id.to_string(),
            conversation_id: conversation_id.to_string(),
            // Whatever the resolver landed on — there is no per-request picker
            // on this side, so this is the assistant's provider or the first
            // enabled one, and either way it is the endpoint that was called.
            provider_id: Some(provider_id),
            provider_name: Some(provider_name),
            parent_cursor,
            cancel: cancel.clone(),
            keep_recent,
            context_limit,
            // The declared permission and nothing else: no reach check, and no
            // standing yes to project edits — there is no project. The desktop
            // consults both. On the drift list.
            approval_rule: engine::ApprovalRule::ByPermission,
            // Was `Terse` — "Unknown tool" — which was true while a tool the
            // speaker could not run was also absent from the definitions. It is
            // no longer: the model can now see a tool it is refused, and being
            // told it does not exist invites both a retry and a workaround
            // through whatever tool does.
            withheld: engine::WithheldWording::Explained,
            // Steering resolves image URIs against this, which is why it is here
            // rather than only in the setup above.
            files_root,
            interrupted,
            compaction: engine::CompactionPolicy::OneBot,
            // A QQ turn accumulates tokens for the session summary and has
            // nowhere to show a price. The bill for it is built from the audit
            // rows like everyone else's, and those are tier-priced at write time.
            pricing: None,
        },
        engine::TurnPorts {
            emit,
            approvals: &approvals,
            interim: commentary.as_ref().map(|c| c as &dyn engine::Commentary),
            surface_tools: surface.as_ref().map(|s| s as &dyn engine::SurfaceTools),
            steering: steering.as_ref().map(|s| s as &dyn engine::Steering),
            // No modes, so no way between them. Which is also why `enter_plan`
            // still reaches the registry and answers with a sentence — see the
            // drift list; this batch preserves it rather than deciding it.
            transitions: None,
            sub_agents: None,
        },
    )
    .await;

    // Handed over whole, and before the reply is unwrapped: this is what the
    // caller closes the turn out with whatever became of the round, and a round
    // that failed still has to name the row it was writing. Adding up across
    // rounds is the caller's job — one of these is made per round, not per turn.
    *progress = outcome.progress;

    // No stop event here. The caller emits it, after handing the conversation
    // back and only once the turn is genuinely over.
    outcome.reply
}

#[cfg(test)]
mod tests {
    use super::*;

    // The stream-reading tests moved with the reader itself, to
    // `agent::engine::stream`. They were never about OneBot.

    fn outcome(reply: Result<String, String>, aborted: bool) -> HeadlessOutcome {
        HeadlessOutcome {
            reply,
            progress: TurnProgress {
                aborted,
                ..Default::default()
            },
        }
    }

    #[test]
    fn a_round_that_failed_ends_as_an_error() {
        assert_eq!(outcome(Err("no api key".into()), false).stop_reason(), "error");
    }

    #[test]
    fn a_round_the_loop_guard_cut_short_says_so() {
        assert_eq!(outcome(Ok(String::new()), true).stop_reason(), "loop_detected");
        assert_eq!(outcome(Ok("hi".into()), false).stop_reason(), "end_turn");
    }

    /// The path that strands the front end if it is missed: a provider that
    /// refuses the very first request means no assistant row was ever created,
    /// so there is no message to hang the event off — but the composer is
    /// already disabled by the optimistic send, and only a stop re-enables it.
    #[test]
    fn a_turn_that_never_wrote_a_message_still_gets_a_terminal_event() {
        let failed = outcome(Err("no api key".into()), false);
        assert!(failed.progress.message_id.is_none());

        let event = turn_stop_event(
            "conv-1",
            "turn-1",
            failed.progress.message_id.as_deref(),
            failed.chat_stop_reason(),
            0,
            0,
        );
        let payload = serde_json::to_value(event).unwrap();

        assert_eq!(payload["type"], "stop");
        assert_eq!(payload["reason"], "error");
        assert_eq!(payload["turn_id"], "turn-1");
        assert_eq!(payload["conversation_id"], "conv-1");
        assert!(payload["message_id"].is_null());
    }

    /// A QQ conversation open in the desktop has to be able to tell this turn's
    /// end from anyone else's, or it streams for good.
    #[test]
    fn a_terminal_event_names_its_turn_and_its_message() {
        let payload = serde_json::to_value(turn_stop_event(
            "conv-1",
            "turn-1",
            Some("msg-9"),
            crate::events::ChatStopReason::EndTurn,
            12,
            34,
        ))
        .unwrap();
        assert_eq!(payload["message_id"], "msg-9");
        assert_eq!(payload["turn_id"], "turn-1");
        assert_eq!(payload["input_tokens"], 12);
        assert_eq!(payload["output_tokens"], 34);
    }

    /// Turning a chat's yes or no into a decision, which is the one place this
    /// side has to make a choice the desktop never faces.
    mod approvals {
        use super::*;
        use crate::agent::engine::{ApprovalDecision, Approvals};
        use crate::db::test_db;
        use crate::turn::TurnOrigin;

        /// `said` of `None` is nobody answering: the minute ran out, or the turn
        /// was swept out from under the question.
        fn asked(said: Option<&str>, tool: &str) -> Option<ApprovalDecision> {
            let said = said.map(str::to_string);
            let pool = test_db();
            {
                let mut conn = pool.get().unwrap();
                crate::db::ops::conversation::create_conversation(&mut conn, "c1", Some("t"), None, None, 1).unwrap();
                crate::db::ops::turn::begin(&mut conn, "t1", "c1", TurnOrigin::OneBot, None, 1000).unwrap();
            }
            let approval_fn: ApprovalFn = Box::new(move |_, _| {
                let said = said.clone();
                Box::pin(async move { Ok(said) })
            });
            let adapter = ChatApprovals {
                approval_fn: &approval_fn,
                pool: pool.clone(),
                turn_id: "t1".into(),
            };
            let call = ToolCall {
                id: "call-1".into(),
                name: tool.into(),
                arguments: "{}".into(),
            };
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(adapter.ask("m1", &call, None))
                .unwrap()
        }

        /// The distinction the whole mapping exists for. `ask_user` asked a
        /// question, so what they typed *is* the answer and the loop hands it to
        /// the model verbatim. It used to arrive as the words "User approved."
        /// however they had replied.
        #[test]
        fn what_the_user_typed_is_what_the_question_gets() {
            assert!(matches!(
                asked(Some("用第二个方案，但先备份"), "ask_user"),
                Some(ApprovalDecision::Response(ref s)) if s == "用第二个方案，但先备份",
            ));
        }

        /// And a yes to anything else is permission, which is the only thing
        /// that authorises a call. Reading a sentence as a `Response` here would
        /// turn somebody typing into authorisation to run a command.
        #[test]
        fn a_yes_to_a_tool_is_permission() {
            for tool in ["run_command", "mcp__server__do", "qq_recall"] {
                for said in ["y", "Y", "yes", "  Y  "] {
                    assert!(
                        matches!(asked(Some(said), tool), Some(ApprovalDecision::Approved)),
                        "{tool} / {said:?}",
                    );
                }
            }
        }

        /// Anything else refuses it, and why travels with the refusal. The model
        /// is about to decide what to do instead, and "no" on its own is most of
        /// a reason short of one.
        #[test]
        fn a_refusal_carries_its_reason_back() {
            assert!(matches!(
                asked(Some("别动生产库"), "run_command"),
                Some(ApprovalDecision::Denied(Some(ref s))) if s == "别动生产库",
            ));
        }

        /// Nobody answered: the minute ran out, or the turn was swept out from
        /// under the question. Not the same as a refusal, and reachable from this
        /// side for the first time — the old transport could only say no.
        #[test]
        fn nobody_answering_is_not_a_refusal() {
            for tool in ["ask_user", "run_command"] {
                assert!(asked(None, tool).is_none(), "{tool}");
            }
        }

        /// A reply with no words in it — an image, a sticker — answered nothing
        /// and authorised nothing.
        #[test]
        fn a_reply_with_no_words_in_it_answers_nothing() {
            for tool in ["ask_user", "run_command"] {
                assert!(
                    matches!(asked(Some("   "), tool), Some(ApprovalDecision::Denied(None))),
                    "{tool}",
                );
            }
        }

        /// The same claim against a real message rather than a hand-made string.
        ///
        /// This is where it was untrue: a picture survives parsing as a
        /// private-use codepoint or as the characters `[图片]`, both of which
        /// outlive a trim. Read as an answer, a sticker sent while a tool waited
        /// used to become the refusal's stated reason — or, for a question, the
        /// answer handed to the model, sentinel and all.
        #[test]
        fn media_sent_while_something_waits_is_not_an_answer() {
            let media = serde_json::json!([
                {"type": "image", "data": {"file": "a.jpg"}},
                {"type": "face", "data": {"id": "1"}},
            ]);
            let typed = crate::onebot::format::parse_segments(&media, None).typed;

            for kind in [AskKind::Permission, AskKind::Question] {
                assert!(
                    matches!(kind.decide(&typed), ApprovalDecision::Denied(None)),
                    "media authorised or answered something",
                );
            }
        }

        /// And nothing of ours reaches the model when there are words to carry.
        #[test]
        fn an_answer_sent_with_a_picture_carries_only_the_words() {
            let with_words = serde_json::json!([
                {"type": "image", "data": {"file": "a.jpg"}},
                {"type": "text", "data": {"text": " 用第二个方案"}},
            ]);
            let typed = crate::onebot::format::parse_segments(&with_words, None).typed;

            let ApprovalDecision::Response(answer) = AskKind::Question.decide(&typed) else {
                panic!("a question's answer is what they wrote");
            };
            assert_eq!(answer, "用第二个方案");
            assert!(!answer.contains(crate::onebot::format::IMAGE_SENTINEL));

            let ApprovalDecision::Denied(Some(reason)) = AskKind::Permission.decide(&typed) else {
                panic!("anything but a yes refuses, with a reason");
            };
            assert!(!reason.contains(crate::onebot::format::IMAGE_SENTINEL));
        }
    }
}

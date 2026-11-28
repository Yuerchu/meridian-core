//! Platform-neutral delegated sub-agent runner inside `meridian-core`.
//!
//! Runs a delegated `Explore` or `Agent` turn in its own child conversation
//! without depending on Tauri or any specific transport. Desktop shells,
//! headless services, and external servers (such as `Meridian Server`) plug in:
//!
//! - [`ApprovalsFactory`] to supply how approval prompts are presented or
//!   reviewed for a child turn (e.g. desktop bubbles, headless auto-review, or
//!   fail-closed denial), and
//! - [`SandboxFactory`] to derive or isolate the child turn's [`tools::ToolContext`]
//!   (leaving an explicit zero-coupling seam for future container/worktree
//!   agent sandboxing).
//!
//! Tool sets are derived dynamically from the parent turn's [`CharacterAssembly`]
//! rather than maintained as a hardcoded whitelist: `Explore` inherits read-only
//! counterparts of parent mixins — automatically including `Always`-approved
//! MCP tools (such as `fetch`) and read-only plugin mixins — while `Agent`
//! inherits working capabilities and structurally drops `SubAgent`, `PlanMode`,
//! `BackgroundJobs`, and `InteractiveUi`.

use std::path::PathBuf;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::agent::character::CharacterAssembly;
use crate::agent::engine::{self, ApprovalDecision, Stranded, SubAgentReport, SubAgentSpec, SubAgentStatus};
use crate::agent::sub_agents::SubAgentKind;
use crate::agent::turn_config::{TurnConfig, TurnConfigResolveRequest};
use crate::agent::{TurnParams, TurnParamsResolveRequest, turn_record};
use crate::db;
use crate::db::entity::assistant;
use crate::db::models::turn::{ERROR_LOOP_DETECTED, TurnStatus};
use crate::events::{ChatStopReason, ChatStreamEvent};
use crate::mcp::McpRegistry;
use crate::provider::{ChatProvider, ToolCall};
use crate::secrets::SecretsManager;
use crate::services::Services;
use crate::state::SubAgentInbox;
use crate::tools::{self, ToolRegistry};
use crate::turn::{TurnCoordinator, TurnLease, TurnOrigin};
use crate::util::now_ms;

/// Which preference key selects the default model for a sub-agent kind.
pub fn default_model_preference(kind: SubAgentKind) -> String {
    format!("sub_agent.{}.model", kind.as_str())
}

/// Metadata passed to [`ApprovalsFactory::build`] when creating an approvals
/// port for a delegated child turn.
pub struct SubAgentApprovalContext<'a> {
    pub parent_conversation_id: &'a str,
    pub parent_message_id: &'a str,
    pub parent_call_id: &'a str,
    pub sub_conversation_id: &'a str,
    pub turn_id: &'a str,
    pub kind: SubAgentKind,
    pub cancel: CancellationToken,
}

/// Pluggable factory producing the [`engine::Approvals`] port for a child turn.
///
/// Desktop shells bubble approvals onto the parent's sub-agent card; headless
/// servers can supply channel approvals, auto-review wrappers, or fail-closed
/// denial.
pub trait ApprovalsFactory: Send + Sync {
    fn build(&self, ctx: SubAgentApprovalContext<'_>) -> Box<dyn engine::Approvals>;
}

/// Fail-closed approvals factory that declines every interactive approval
/// prompt. Useful for unattended headless runners and unit tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyApprovalsFactory;

struct DenyApprovals;

#[async_trait::async_trait]
impl engine::Approvals for DenyApprovals {
    async fn ask(
        &self,
        _assistant_message_id: &str,
        _call: &ToolCall,
        _retry: Option<engine::Escalation<'_>>,
    ) -> Result<Option<ApprovalDecision>, String> {
        Ok(Some(ApprovalDecision::Denied(Some(
            "interactive approval is unavailable in this sub-agent environment".to_string(),
        ))))
    }
}

impl ApprovalsFactory for DenyApprovalsFactory {
    fn build(&self, _ctx: SubAgentApprovalContext<'_>) -> Box<dyn engine::Approvals> {
        Box::new(DenyApprovals)
    }
}

/// Metadata passed to [`SandboxFactory::resolve`] when preparing a child
/// sub-agent's execution environment.
pub struct SubAgentSandboxContext<'a> {
    pub sub_conversation_id: &'a str,
    pub turn_id: &'a str,
    pub kind: SubAgentKind,
    pub model_id: &'a str,
}

/// Extension hook that derives or wraps a child sub-agent's [`tools::ToolContext`].
///
/// `base_ctx` already carries the child's `conversation_id`, `turn_id`, derived
/// `cancel` token, and re-identified `journal.for_turn(...)`, so custom sandbox
/// implementations only need to adjust filesystem roots, workspace isolation, or
/// container/sandbox policies.
pub trait SandboxFactory: Send + Sync {
    fn resolve(
        &self,
        base_ctx: tools::ToolContext,
        ctx: SubAgentSandboxContext<'_>,
    ) -> Result<tools::ToolContext, String>;
}

/// Default [`SandboxFactory`] that inherits the parent's workspace and sandbox
/// policy unchanged.
#[derive(Debug, Default, Clone, Copy)]
pub struct InheritToolContextSandboxFactory;

impl SandboxFactory for InheritToolContextSandboxFactory {
    fn resolve(
        &self,
        base_ctx: tools::ToolContext,
        _ctx: SubAgentSandboxContext<'_>,
    ) -> Result<tools::ToolContext, String> {
        Ok(base_ctx)
    }
}

/// Platform-neutral [`engine::SubAgents`] implementation owned by `meridian-core`.
pub struct CoreSubAgentRunner {
    pub services: Services,
    pub secrets: Arc<SecretsManager>,
    pub registry: Arc<ToolRegistry>,
    pub coordinator: Arc<TurnCoordinator>,
    pub mcp: Arc<McpRegistry>,
    pub approvals_factory: Arc<dyn ApprovalsFactory>,
    pub sandbox_factory: Arc<dyn SandboxFactory>,
    /// Parent turn's resolved capability assembly. Child `Explore` / `Agent`
    /// characters derive strictly from this assembly, guaranteeing a sub-agent
    /// never widens tool scope beyond its parent.
    pub parent_character: CharacterAssembly,
    pub parent_conversation_id: String,
    pub parent_cancel: CancellationToken,
    pub assistant: Option<assistant::Model>,
    pub project_id: Option<String>,
    pub tool_context: tools::ToolContext,
    pub files_root: Option<PathBuf>,
    /// The user's standing yes, on this conversation. Inherited by `Agent` and
    /// never by `Explore`.
    pub accept_edits: bool,
    pub keep_recent: usize,
}

#[async_trait::async_trait]
impl engine::SubAgents for CoreSubAgentRunner {
    async fn run(&self, spec: SubAgentSpec) -> Result<SubAgentReport, String> {
        self.delegate(spec).await
    }
}

impl CoreSubAgentRunner {
    async fn delegate(&self, spec: SubAgentSpec) -> Result<SubAgentReport, String> {
        // Resolve model first so invalid names return a clean tool error without
        // leaving an orphan child conversation row behind.
        let (assistant, turn_params) = self.resolve_model(&spec).await?;

        let sub_conversation_id = uuid::Uuid::new_v4().to_string();
        let turn_id = uuid::Uuid::new_v4().to_string();

        let cancel = self.parent_cancel.child_token();
        let lease = self
            .coordinator
            .try_acquire_turn_with(
                &sub_conversation_id,
                TurnOrigin::SubAgent,
                turn_id.clone(),
                cancel.clone(),
            )
            .map_err(|busy| busy.to_string())?;

        let (user_message_id, sent_at) = self
            .open_conversation(&sub_conversation_id, &turn_id, &spec, &assistant)
            .await?;

        let inbox_handle = self.services.sub_agent_inboxes.open(&sub_conversation_id);
        let mut guard = ChildTurnGuard {
            services: self.services.clone(),
            conversation_id: sub_conversation_id.clone(),
            turn_id: turn_id.clone(),
            message_id: None,
            armed: true,
            lease: Some(lease),
            inbox: Some(Arc::clone(&inbox_handle)),
        };

        let _ = self.services.events.emit_chat(ChatStreamEvent::SubAgentStarted {
            conversation_id: self.parent_conversation_id.clone(),
            message_id: spec.parent_message_id.clone(),
            call_id: spec.parent_call_id.clone(),
            sub_conversation_id: sub_conversation_id.clone(),
            spawned_turn_id: turn_id.clone(),
            kind: spec.kind,
            description: spec.description.clone(),
        });

        let outcome = self
            .run_loop(
                &spec,
                &assistant,
                &turn_params,
                &sub_conversation_id,
                &turn_id,
                &user_message_id,
                sent_at,
                &cancel,
                &inbox_handle,
            )
            .await;

        guard.message_id = outcome.progress.message_id.clone();
        let status = classify(&outcome, &cancel);
        let (stored, error) = match status {
            SubAgentStatus::Done => (TurnStatus::Done, None),
            SubAgentStatus::Cancelled => (TurnStatus::Cancelled, None),
            SubAgentStatus::Aborted => (TurnStatus::Failed, Some(ERROR_LOOP_DETECTED.to_string())),
            SubAgentStatus::Failed => (TurnStatus::Failed, outcome.reply.as_ref().err().cloned()),
        };
        turn_record::finish(&self.services.db, &turn_id, stored, error.as_deref()).await;

        let stranded = self
            .persist_stranded(&sub_conversation_id, &turn_id, outcome.progress.final_cursor.as_deref())
            .await;

        let _ = self.services.events.emit_chat(ChatStreamEvent::Stop {
            reason: outcome.chat_stop_reason(),
            message_id: outcome.progress.message_id.clone(),
            turn_id: turn_id.clone(),
            conversation_id: sub_conversation_id.clone(),
            input_tokens: outcome.progress.input_tokens.value(),
            output_tokens: outcome.progress.output_tokens.value(),
        });
        guard.disarm();
        guard.release();

        Ok(SubAgentReport {
            status,
            reply: outcome.reply.unwrap_or_else(|e| e),
            steps: outcome.progress.steps,
            stranded,
        })
    }

    async fn persist_stranded(&self, sub_conversation_id: &str, turn_id: &str, final_cursor: Option<&str>) -> Stranded {
        let leftover = self.services.sub_agent_inboxes.close(sub_conversation_id);
        let mut stranded = Stranded {
            accepted: leftover.len(),
            unrecorded: 0,
        };
        let mut cursor = final_cursor.map(str::to_string);
        for item in leftover {
            match engine::write_steering(
                &self.services.db,
                sub_conversation_id,
                turn_id,
                &item.text,
                None,
                cursor.as_deref(),
                item.received_at,
            )
            .await
            {
                Ok(id) => cursor = Some(id),
                Err(e) => {
                    stranded.unrecorded += 1;
                    tracing::warn!(
                        conversation_id = %sub_conversation_id,
                        error = %e,
                        "a message accepted for a sub-agent could not be written down",
                    );
                }
            }
        }
        stranded
    }

    async fn resolve_model(&self, spec: &SubAgentSpec) -> Result<(assistant::Model, TurnParams), String> {
        let base = self
            .assistant
            .clone()
            .ok_or("This conversation has no assistant, so there is nothing to run a sub-agent on.")?;

        let key = default_model_preference(spec.kind);
        let configured = db::sea::ops::preference::get(&self.services.db, &key)
            .await
            .ok()
            .flatten();

        let chosen = spec.model.clone().or(configured).filter(|s| !s.trim().is_empty());
        let (provider_id, model_id) = match chosen.as_deref() {
            Some(q) => {
                let (p, m) = q
                    .split_once(':')
                    .ok_or_else(|| format!("`{q}` is not a model name. Use one of the values listed on `model`."))?;
                (Some(p.to_string()), Some(m.to_string()))
            }
            None => (base.provider_id.clone(), base.model_id.clone()),
        };

        let assistant = effective_assistant(base, spec.kind, provider_id, model_id);
        let params = self.resolve_params(&assistant).await?;
        Ok((assistant, params))
    }

    async fn resolve_params(&self, assistant: &assistant::Model) -> Result<TurnParams, String> {
        let configured_max = self.assistant.as_ref().and_then(|a| a.max_tokens);
        let sea = &self.services.db;
        let resolved = crate::agent::resolve_with_overrides(&self.secrets, sea, Some(assistant), None, None).await?;
        let mut params = crate::agent::resolve_turn_params(
            sea,
            TurnParamsResolveRequest {
                assistant: Some(assistant),
                provider_id: assistant.provider_id.as_deref(),
                provider_type: &resolved.provider_type,
                api_format: &resolved.api_format,
                transport_profile: &resolved.transport_profile,
                codex_request_shape: resolved.codex_request_shape,
                codex_request_kind: crate::provider::codex_metadata::CodexRequestKind::Turn,
                codex_thread_source: crate::provider::codex_metadata::CodexThreadSource::SubAgent,
                model: &resolved.model,
                thinking_level: None,
                fast: false,
            },
        )
        .await?;

        params.params.max_tokens = Some(clamp_max_tokens(configured_max, params.max_output));
        Ok(params)
    }

    async fn open_conversation(
        &self,
        sub_conversation_id: &str,
        turn_id: &str,
        spec: &SubAgentSpec,
        assistant: &assistant::Model,
    ) -> Result<(String, i64), String> {
        let message_id = uuid::Uuid::new_v4().to_string();
        let now = now_ms();
        let conversation = db::entity::conversation::Model {
            title: Some(spec.description.clone()),
            assistant_id: Some(assistant.id.clone()),
            project_id: self.project_id.clone(),
            parent_conversation_id: Some(self.parent_conversation_id.clone()),
            spawned_by_message_id: Some(spec.parent_message_id.clone()),
            spawned_by_call_id: Some(spec.parent_call_id.clone()),
            spawned_turn_id: Some(turn_id.to_string()),
            agent_kind: Some(spec.kind.as_str().to_string()),
            agent_provider_id: assistant.provider_id.clone(),
            agent_model_id: assistant.model_id.clone(),
            ..db::sea::ops::conversation::new_row(sub_conversation_id, now)
        };
        let prompt = db::entity::message::Model {
            turn_id: Some(turn_id.to_string()),
            ..db::sea::ops::message::new_row(&message_id, sub_conversation_id, "user", &spec.prompt, now)
        };
        self.services
            .db
            .write(async |tx| {
                db::sea::ops::conversation::insert(tx, conversation).await?;
                db::sea::ops::message::append(tx, prompt, None).await?;
                db::sea::ops::turn::begin(tx, turn_id, sub_conversation_id, TurnOrigin::SubAgent, None, now).await
            })
            .await
            .map_err(|e| e.to_string())?;

        Ok((message_id, now))
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_loop(
        &self,
        spec: &SubAgentSpec,
        assistant: &assistant::Model,
        turn_params: &TurnParams,
        sub_conversation_id: &str,
        turn_id: &str,
        user_message_id: &str,
        sent_at: i64,
        cancel: &CancellationToken,
        inbox: &SubAgentInbox,
    ) -> engine::TurnOutcome {
        let base_tool_context = tools::ToolContext {
            conversation_id: Some(sub_conversation_id.to_string()),
            turn_id: Some(turn_id.to_string()),
            cancel: cancel.clone(),
            journal: self.tool_context.journal.as_ref().map(|j| {
                j.for_turn(
                    sub_conversation_id.to_string(),
                    turn_id.to_string(),
                    TurnOrigin::SubAgent.as_str().to_string(),
                    Some(turn_params.params.model.clone()),
                )
            }),
            ..self.tool_context.clone()
        };
        let tool_context = match self.sandbox_factory.resolve(
            base_tool_context,
            SubAgentSandboxContext {
                sub_conversation_id,
                turn_id,
                kind: spec.kind,
                model_id: &turn_params.params.model,
            },
        ) {
            Ok(ctx) => ctx,
            Err(e) => return engine::TurnOutcome::failed(e),
        };

        let child_character = derive_child_character(
            &self.parent_character,
            spec.kind,
            assistant.system_prompt.clone(),
            turn_params.caps.supports_tools,
            turn_params.params.server_tools.clone(),
            &tool_context,
        );
        let child_plugin_mixins = child_character.plugin_mixins.clone();
        let config = match self
            .build_config(assistant, sub_conversation_id, child_character)
            .await
        {
            Ok(c) => c,
            Err(e) => return engine::TurnOutcome::failed(e),
        };

        let provider = match self.build_provider(assistant).await {
            Ok(p) => p,
            Err(e) => return engine::TurnOutcome::failed(e),
        };

        let chat_messages = match crate::agent::build_messages_with_senders(
            config.system_prompt.trim(),
            &db::sea::ops::message::ActiveContext {
                path: Vec::new(),
                summary: None,
                anchor_index: None,
                head_id: None,
            },
            crate::agent::trailing_with_memory(
                None,
                None,
                None,
                Some(crate::agent::persisted_user_message(&spec.prompt, None, None, sent_at)),
                None,
            ),
            &Default::default(),
        ) {
            Ok(messages) => messages,
            Err(error) => return engine::TurnOutcome::failed(error),
        };

        let mut budget = crate::agent::TokenBudget::new(
            &provider.1.provider_type,
            &turn_params.params.model,
            turn_params.context_limit,
            turn_params.max_output,
            turn_params.compact_threshold,
        );
        budget.update_estimate(&chat_messages);

        let approvals = self.approvals_factory.build(SubAgentApprovalContext {
            parent_conversation_id: &self.parent_conversation_id,
            parent_message_id: &spec.parent_message_id,
            parent_call_id: &spec.parent_call_id,
            sub_conversation_id,
            turn_id,
            kind: spec.kind,
            cancel: cancel.clone(),
        });
        let emitter = SubAgentEmit(self.services.events.clone());
        let turn_hooks = self.services.snapshot_turn_hooks();

        let mut params = turn_params.params.clone();
        params.cache_key = Some(sub_conversation_id.to_string());
        let user_media = match crate::provider::registry::WireMediaPolicy::from_resolved(
            &provider.1.provider_type,
            &provider.1.api_format,
            &turn_params.caps,
        ) {
            Ok(policy) => policy,
            Err(error) => return engine::TurnOutcome::failed(error),
        };

        Box::pin(engine::run_turn(
            &engine::TurnServices {
                db: &self.services.db,
                tools: &self.registry,
                mcp: &self.mcp,
                redaction: &self.services.redaction,
                redaction_mappings: &self.services.redaction_mappings,
            },
            engine::TurnSetup {
                trigger: crate::turn::TurnTrigger::User,
                provider: &*provider.0,
                params,
                chat_messages,
                tool_defs: config.tool_defs,
                offered: config.offered,
                deferred_tools: config.deferred_tools,
                approval_policies: config.approval_policies,
                mode: crate::agent::modes::Modes::Fixed.spec(),
                tool_context,
                budget,
                turn_id: turn_id.to_string(),
                conversation_id: sub_conversation_id.to_string(),
                provider_id: Some(provider.1.provider_id.clone()),
                provider_name: Some(provider.1.provider_name.clone()),
                parent_cursor: Some(user_message_id.to_string()),
                cancel: cancel.clone(),
                keep_recent: self.keep_recent,
                context_limit: turn_params.context_limit,
                warm: None,
                approval_rule: engine::ApprovalRule::ByReach {
                    accept_edits: self.accept_edits && spec.kind == SubAgentKind::Agent,
                },
                withheld: engine::WithheldWording::Explained,
                files_root: self.files_root.clone(),
                user_media,
                interrupted: None,
                compaction: engine::CompactionPolicy::Attended {
                    enabled: true,
                    breaker: Arc::new(crate::agent::CompactCircuitBreaker::new()),
                },
                pricing: None,
            },
            engine::TurnPorts {
                emit: Some(&emitter),
                steering: Some(inbox),
                mixins: &child_plugin_mixins,
                hooks: &turn_hooks,
                ..engine::TurnPorts::with_approvals(approvals.as_ref())
            },
        ))
        .await
    }

    async fn build_config(
        &self,
        assistant: &assistant::Model,
        sub_conversation_id: &str,
        character: CharacterAssembly,
    ) -> Result<TurnConfig, String> {
        let input = TurnConfigResolveRequest {
            assistant: Some(assistant.clone()),
            conversation_id: sub_conversation_id.to_string(),
            project_id: self.project_id.clone(),
            character,
        };
        crate::agent::turn_config::resolve_on(&self.services.db, &self.registry, input).await
    }

    async fn build_provider(
        &self,
        assistant: &assistant::Model,
    ) -> Result<(Box<dyn ChatProvider>, crate::agent::ResolvedProvider), String> {
        let resolved =
            crate::agent::resolve_with_overrides(&self.secrets, &self.services.db, Some(assistant), None, None).await?;
        let provider = crate::provider::registry::create_provider(resolved.wire())?;
        Ok((provider, resolved))
    }
}

/// Derive the child [`CharacterAssembly`] dynamically from the parent turn's
/// capability profile.
///
/// - `Explore` derives read-only counterparts (including `Always`-approved MCP
///   tools and read-only plugin mixins) with zero write/exec/UI/delegation tools.
/// - `Agent` inherits working capabilities from the parent while structurally
///   dropping `SubAgent`, `PlanMode`, `BackgroundJobs`, and `InteractiveUi`.
pub fn derive_child_character(
    parent_character: &CharacterAssembly,
    kind: SubAgentKind,
    persona: String,
    supports_tools: bool,
    server_tools: Vec<crate::provider::ServerToolKind>,
    child_tool_context: &tools::ToolContext,
) -> CharacterAssembly {
    match kind {
        SubAgentKind::Explore => {
            parent_character.derive_explore_readonly(persona, supports_tools, server_tools, Vec::new())
        }
        SubAgentKind::Agent => {
            #[cfg(not(target_os = "android"))]
            let command_shell = Some(crate::tools::command_shell::CommandShell::select(
                child_tool_context.shell,
                child_tool_context.sandbox_policy.is_container(),
            ));
            #[cfg(target_os = "android")]
            let command_shell = {
                let _ = child_tool_context;
                None
            };
            parent_character.derive_working_sub_agent(persona, supports_tools, server_tools, command_shell, Vec::new())
        }
    }
}

/// Adjust the parent assistant row for a delegated run.
///
/// Clears parent-model-specific `context_limit` and `max_tokens`. For `Explore`,
/// clears `tool_preset_id` and `enabled_tools` so [`CharacterAssembly::derive_explore_readonly`]
/// is the sole structural authority over read-only capabilities (including any
/// `Always`-approved MCP tools inherited from the parent).
pub fn effective_assistant(
    base: assistant::Model,
    kind: SubAgentKind,
    provider_id: Option<String>,
    model_id: Option<String>,
) -> assistant::Model {
    let mut a = assistant::Model {
        provider_id,
        model_id,
        context_limit: 0,
        max_tokens: None,
        ..base
    };
    if kind == SubAgentKind::Explore {
        a.tool_preset_id = None;
        a.enabled_tools = None;
    }
    a
}

/// The smaller of what the user asked for and what this model can write.
pub fn clamp_max_tokens(configured: Option<i32>, model_ceiling: usize) -> i32 {
    crate::agent::resolve_max_tokens(
        configured.map(|m| m.min(model_ceiling.min(i32::MAX as usize) as i32)),
        model_ceiling,
    )
}

/// Decide once how a delegated run ended so the stored turn row and tool result agree.
pub fn classify(outcome: &engine::TurnOutcome, cancel: &CancellationToken) -> SubAgentStatus {
    if outcome.reply.is_err() {
        SubAgentStatus::Failed
    } else if outcome.progress.aborted {
        SubAgentStatus::Aborted
    } else if cancel.is_cancelled() {
        SubAgentStatus::Cancelled
    } else {
        SubAgentStatus::Done
    }
}

struct SubAgentEmit(crate::events::EventBus);

impl engine::Emit for SubAgentEmit {
    fn emit(&self, channel: &str, payload: serde_json::Value) -> Result<(), String> {
        let _ = self.0.emit(channel, payload);
        Ok(())
    }
}

struct ChildTurnGuard {
    services: Services,
    conversation_id: String,
    turn_id: String,
    message_id: Option<String>,
    armed: bool,
    lease: Option<TurnLease>,
    inbox: Option<Arc<SubAgentInbox>>,
}

impl ChildTurnGuard {
    fn release(&mut self) {
        self.lease.take();
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ChildTurnGuard {
    fn drop(&mut self) {
        self.services
            .approvals
            .lock()
            .retain(|_, pending| pending.turn_id != self.turn_id);
        if self.inbox.take().is_some() {
            let stranded = self.services.sub_agent_inboxes.close(&self.conversation_id);
            if !stranded.is_empty() {
                tracing::warn!(
                    conversation_id = %self.conversation_id,
                    count = stranded.len(),
                    "a sub-agent ended with messages still in its inbox",
                );
            }
        }
        self.lease.take();
        if self.armed {
            let _ = self.services.events.emit_chat(ChatStreamEvent::Stop {
                reason: ChatStopReason::Error,
                message_id: self.message_id.clone(),
                turn_id: self.turn_id.clone(),
                conversation_id: self.conversation_id.clone(),
                input_tokens: None,
                output_tokens: None,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::character::CharacterBase;
    use crate::agent::hooks::{DeclaredTool, MixinResolveContext, ToolApprovalPolicy, ToolMixin};
    use crate::agent::mixins::BuiltinMixin;
    use crate::agent::modes::Modes;
    use crate::agent::sub_agents::SubAgentCatalog;
    use crate::provider::ToolDefinition;
    use crate::tools::ToolOutput;

    struct DummyPluginMixin {
        id: &'static str,
        tool_name: &'static str,
        explore: bool,
    }

    #[async_trait::async_trait]
    impl ToolMixin for DummyPluginMixin {
        fn id(&self) -> &'static str {
            self.id
        }

        fn owned_names(&self) -> Vec<String> {
            vec![self.tool_name.to_string()]
        }

        fn declarations(&self, _ctx: &MixinResolveContext<'_>) -> Vec<DeclaredTool> {
            vec![DeclaredTool::eager(ToolDefinition {
                name: self.tool_name.to_string(),
                description: "plugin test tool".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            })]
        }

        fn allow_in_explore(&self) -> bool {
            self.explore
        }

        async fn execute_rich(
            &self,
            _name: &str,
            _args: serde_json::Value,
            _ctx: &tools::ToolContext,
        ) -> Result<ToolOutput, String> {
            Ok(ToolOutput::text("ok".to_string()))
        }
    }

    fn test_tool_context(conversation_id: Option<&str>, turn_id: Option<&str>) -> tools::ToolContext {
        tools::ToolContext {
            working_directory: None,
            shell: tools::ShellType::Bash,
            file_access: tools::FileAccess::Unrestricted,
            project_id: None,
            conversation_id: conversation_id.map(str::to_string),
            turn_id: turn_id.map(str::to_string),
            assistant_id: None,
            db: None,
            #[cfg(not(target_os = "android"))]
            sandbox_policy: crate::sandbox::CommandSandbox::UNCONFINED,
            #[cfg(not(target_os = "android"))]
            background: None,
            tool_secrets: std::collections::HashMap::new(),
            cancel: CancellationToken::new(),
            journal: None,
        }
    }

    fn parent(preset: Option<&str>, enabled: Option<&str>) -> assistant::Model {
        assistant::Model {
            id: "a1".into(),
            name: "A".into(),
            description: None,
            avatar: None,
            system_prompt: "You are helpful.".into(),
            provider_id: Some("p1".into()),
            model_id: Some("big-model".into()),
            temperature: Some(0.7),
            top_p: None,
            max_tokens: Some(64_000),
            is_default: crate::db::types::SqlBool::FALSE,
            sort_order: 0,
            created_at: 0,
            updated_at: 0,
            context_limit: 200_000,
            compact_keep_recent_tokens: 20_000,
            enabled_tools: enabled.map(|json| crate::db::types::Json::decode(json).unwrap()),
            thinking_enabled: crate::db::types::SqlBool::FALSE,
            thinking_budget: None,
            tool_preset_id: preset.map(str::to_string),
            auto_compact_enabled: crate::db::types::SqlBool::FALSE,
            cache_ttl: None,
            cache_expiry_policy: crate::agent::cache_warmer::CacheExpiryPolicy::Compact,
            cache_max_warm_count: crate::agent::cache_warmer::DEFAULT_CACHE_MAX_WARM_COUNT,
            cache_compact_min_tokens: crate::agent::cache_warmer::DEFAULT_CACHE_COMPACT_MIN_TOKENS,
            logical_model_id: None,
            deployment_id: None,
        }
    }

    fn registry() -> ToolRegistry {
        ToolRegistry::new(
            PathBuf::from("/nonexistent"),
            PathBuf::from("/nonexistent"),
            Arc::new(crate::redaction::RedactionEngine::disabled()),
        )
    }

    fn parent_character_with_mcps() -> CharacterAssembly {
        let fetch_tool = DeclaredTool::deferred(ToolDefinition {
            name: "mcp__fetch__get".into(),
            description: "Fetch a URL".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        })
        .with_approval(ToolApprovalPolicy::Always);
        let order_tool = DeclaredTool::deferred(ToolDefinition {
            name: "mcp__luckin__order_coffee".into(),
            description: "Place a coffee order".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        })
        .with_approval(ToolApprovalPolicy::RequestApprove);

        CharacterBase::new("Parent persona")
            .with_mode(Modes::Switchable(crate::agent::modes::ChatMode::Work.spec()))
            .with_builtin(BuiltinMixin::workspace_fs())
            .with_builtin(BuiltinMixin::shell_exec(None))
            .with_builtin(BuiltinMixin::BackgroundJobs)
            .with_builtin(BuiltinMixin::InteractiveUi)
            .with_builtin(BuiltinMixin::PlanMode)
            .with_builtin(BuiltinMixin::WebSearch)
            .with_builtin(BuiltinMixin::diagnostics())
            .with_builtin(BuiltinMixin::memory())
            .with_builtin(BuiltinMixin::SubAgent {
                catalog: SubAgentCatalog { models: Vec::new() },
            })
            .with_builtin(BuiltinMixin::Mcp {
                tools: vec![fetch_tool, order_tool],
            })
            .with_plugin_mixin(Arc::new(DummyPluginMixin {
                id: "plugin_readonly",
                tool_name: "plugin_inspect",
                explore: true,
            }))
            .with_plugin_mixin(Arc::new(DummyPluginMixin {
                id: "plugin_writer",
                tool_name: "plugin_mutate",
                explore: false,
            }))
            .build()
    }

    async fn resolve_child(parent_character: &CharacterAssembly, child: assistant::Model, kind: SubAgentKind) -> TurnConfig {
        let db = crate::db::sea::sea_test_db().await;
        db.write(async |tx| crate::db::sea::ops::conversation::create(tx, "sub-1", None, None, None, 1).await)
            .await
            .unwrap();
        let derived = derive_child_character(
            parent_character,
            kind,
            child.system_prompt.clone(),
            true,
            Vec::new(),
            &test_tool_context(Some("sub-1"), None),
        );
        crate::agent::turn_config::resolve_on(
            &db,
            &registry(),
            TurnConfigResolveRequest {
                assistant: Some(child),
                conversation_id: "sub-1".into(),
                project_id: None,
                character: derived,
            },
        )
        .await
        .unwrap()
    }

    #[test]
    fn the_parents_window_and_allowance_do_not_follow_the_child() {
        let child = effective_assistant(
            parent(None, None),
            SubAgentKind::Agent,
            Some("p2".into()),
            Some("small-model".into()),
        );
        assert_eq!(child.context_limit, 0);
        assert_eq!(child.max_tokens, None);
        assert_eq!(child.model_id.as_deref(), Some("small-model"));
        assert_eq!(child.provider_id.as_deref(), Some("p2"));
        assert_eq!(child.temperature, Some(0.7));
        assert_eq!(child.compact_keep_recent_tokens, 20_000);
    }

    #[test]
    fn an_output_allowance_is_the_smaller_of_the_two_or_the_models_own() {
        assert_eq!(clamp_max_tokens(Some(64_000), 8_192), 8_192);
        assert_eq!(clamp_max_tokens(Some(4_096), 8_192), 4_096);
        assert_eq!(clamp_max_tokens(None, 8_192), 8_192);
    }

    #[tokio::test]
    async fn explore_derives_readonly_and_always_mcp_tools_dynamically_from_parent() {
        let parent_char = parent_character_with_mcps();
        let child = effective_assistant(
            parent(Some("a-preset-with-writes"), None),
            SubAgentKind::Explore,
            None,
            None,
        );
        assert_eq!(child.tool_preset_id, None);
        assert_eq!(child.enabled_tools, None);

        let cfg = resolve_child(&parent_char, child, SubAgentKind::Explore).await;
        assert!(cfg.offered.contains("read_file"));
        assert!(cfg.offered.contains("read_app_logs"));
        assert!(cfg.offered.contains("recall_memory"));
        assert!(
            cfg.offered.contains("mcp__fetch__get"),
            "Always-approved MCP tools like fetch must be dynamically available to Explore"
        );
        assert!(cfg.deferred_tools.contains("mcp__fetch__get"));
        assert!(
            cfg.offered.contains("plugin_inspect"),
            "read-only plugin mixins opting into explore must survive derivation"
        );

        for forbidden in [
            "write_file",
            "edit_file",
            "apply_patch",
            "delete_file",
            "run_command",
            "save_memory",
            "mcp__luckin__order_coffee",
            "plugin_mutate",
            "ask_user",
            "enter_plan",
            crate::agent::sub_agents::RUN_AGENT_TOOL,
        ] {
            assert!(
                !cfg.offered.contains(forbidden),
                "{forbidden} must never reach a read-only Explore sub-agent"
            );
        }
    }

    #[tokio::test]
    async fn working_agent_inherits_mcps_and_writes_while_stripping_recursion_and_ui() {
        let parent_char = parent_character_with_mcps();
        let child = effective_assistant(parent(None, None), SubAgentKind::Agent, None, None);
        assert!(child.enabled_tools.is_none());

        let cfg = resolve_child(&parent_char, child, SubAgentKind::Agent).await;
        assert!(cfg.offered.contains("write_file"));
        assert!(cfg.offered.contains("mcp__fetch__get"));
        assert!(cfg.offered.contains("mcp__luckin__order_coffee"));
        assert!(cfg.offered.contains("plugin_inspect"));
        assert!(cfg.offered.contains("plugin_mutate"));

        for stripped in [
            crate::agent::sub_agents::RUN_AGENT_TOOL,
            "enter_plan",
            "ask_user",
            "list_background_tasks",
        ] {
            assert!(
                !cfg.offered.contains(stripped),
                "{stripped} must be stripped from working sub-agents"
            );
        }
    }

    #[test]
    fn sandbox_factory_hook_can_customize_child_tool_context() {
        struct IsolatedDirFactory(String);
        impl SandboxFactory for IsolatedDirFactory {
            fn resolve(
                &self,
                mut base_ctx: tools::ToolContext,
                ctx: SubAgentSandboxContext<'_>,
            ) -> Result<tools::ToolContext, String> {
                assert_eq!(ctx.kind, SubAgentKind::Agent);
                base_ctx.working_directory = Some(format!("{}/{}", self.0, ctx.sub_conversation_id));
                Ok(base_ctx)
            }
        }

        let factory = IsolatedDirFactory("/sandboxes".into());
        let base = test_tool_context(Some("sub-42"), Some("turn-1"));
        let resolved = factory
            .resolve(
                base,
                SubAgentSandboxContext {
                    sub_conversation_id: "sub-42",
                    turn_id: "turn-1",
                    kind: SubAgentKind::Agent,
                    model_id: "claude-haiku-4-5",
                },
            )
            .unwrap();
        assert_eq!(resolved.working_directory.as_deref(), Some("/sandboxes/sub-42"));
        assert_eq!(resolved.conversation_id.as_deref(), Some("sub-42"));
    }

    #[test]
    fn how_a_run_ended_is_decided_once() {
        let ended = |reply: Result<String, String>, aborted: bool, stopped: bool| {
            let cancel = CancellationToken::new();
            if stopped {
                cancel.cancel();
            }
            classify(
                &engine::TurnOutcome {
                    reply,
                    progress: engine::TurnProgress {
                        aborted,
                        ..Default::default()
                    },
                },
                &cancel,
            )
        };

        assert_eq!(ended(Ok("done".into()), false, false), SubAgentStatus::Done);
        assert_eq!(ended(Ok("half".into()), false, true), SubAgentStatus::Cancelled);
        assert_eq!(
            ended(Ok("round and round".into()), true, false),
            SubAgentStatus::Aborted
        );
        assert_eq!(ended(Err("no key".into()), false, false), SubAgentStatus::Failed);
        assert_eq!(ended(Err("no key".into()), false, true), SubAgentStatus::Failed);

        assert_eq!(SubAgentStatus::Done.outcome(), "success");
        for bad in [
            SubAgentStatus::Cancelled,
            SubAgentStatus::Aborted,
            SubAgentStatus::Failed,
        ] {
            assert_eq!(bad.outcome(), "error", "{bad:?}");
        }
    }

    #[test]
    fn each_kind_reads_its_own_configured_default() {
        assert_eq!(
            default_model_preference(SubAgentKind::Explore),
            "sub_agent.explore.model"
        );
        assert_eq!(default_model_preference(SubAgentKind::Agent), "sub_agent.agent.model");
    }
}

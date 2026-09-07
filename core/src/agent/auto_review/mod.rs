//! Deciding an approval without waking anybody up.
//!
//! `tools::reach` already answers most of the question: reading inside the
//! project never asks, writing inside it asks until the user opens
//! accept-edits, and the paths that arrange for code to run later always ask.
//! What is left over is narrow and genuinely uncertain — a path outside the
//! project, a shell command, an MCP tool nobody can introspect, a sandbox
//! escalation — and today all of it costs a card and a wait. On the desktop
//! that is an interruption every few turns. In a QQ group there is nobody
//! holding the phone, so those approvals mostly end in a timeout, which is the
//! same as the feature not existing.
//!
//! So: a second model looks at the action and answers for the user. This is a
//! **decorator over [`Approvals`]**, not a change to the loop — `turn.rs` asks
//! the same port in the same six places and never learns that some of the
//! answers were not typed by a person.
//!
//! Three rules hold the design together, and each of them is a way this can be
//! got wrong rather than a preference:
//!
//! 1. **An unreadable answer is not a denial.** Timeouts, refused connections,
//!    a model that returns prose — none of those are evidence about the action.
//!    Where somebody is watching, they fall back to the card that would have
//!    been drawn anyway; where nobody is (`unattended`), they deny, because a
//!    permission prompt that proceeds when nobody answered is not a permission
//!    prompt. This is the exact inverse of `hooks/`, which fails *open* — a
//!    missed code review costs one missed code review, while a wrongly-granted
//!    `run_command` costs whatever the command does.
//! 2. **`ask_user` and the mode transitions are never answered here.** A
//!    `Response` is the answer to a question, and only `ask_user` asked one;
//!    entering or leaving plan mode is the user being shown something, not a
//!    permission being checked. Both go straight through to whoever is behind
//!    this.
//! 3. **A reviewer that keeps refusing stops being consulted.** Denials in a
//!    row mean the model is working around the refusal rather than accepting
//!    it, and every attempt costs another review. The circuit breaker exists so
//!    that spends a bounded amount of the user's money before it stops.

mod assessment;
mod investigate;
mod projection;

use std::sync::Mutex;

pub use assessment::{Assessment, AuthLevel, Outcome, Read, RiskLevel};

use crate::agent::engine::{ApprovalDecision, Approvals};
use crate::db::models::message::MessageUsage;
use crate::events::{
    AutoReviewAuthorization, AutoReviewEvidence, AutoReviewOutcome as EventOutcome, AutoReviewRisk, AutoReviewStage,
    AutoReviewVerdict, ChatStreamEvent,
};
use crate::provider::{ChatMessage, TokenUsage, ToolCall};
use crate::services::Services;
use crate::util::get_conn;

/// The policy the reviewer is briefed with, before the user's own additions.
const POLICY: &str = include_str!("policy.md");

/// Long enough for a slow local model, short enough that a wedged endpoint does
/// not read as the app hanging. Codex allows 90s for a reviewer that may run a
/// dozen tool calls; the first pass here makes exactly one request.
const FIRST_PASS_TIMEOUT_SECS: u64 = 45;

/// Denials in a row before the reviewer stops being consulted at all. Codex's
/// number, and for its reason: three refusals is a model working around the
/// refusal rather than accepting it.
const MAX_CONSECUTIVE_DENIALS: u32 = 3;
/// Denials in one turn, however they are spaced. A turn that has been refused
/// this many times is not going to finish.
const MAX_DENIALS_PER_TURN: u32 = 10;

/// What the user configured. Read once when the turn starts: a setting that
/// changed mid-turn would mean two halves of one turn judged by different rules.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub enabled: bool,
    /// `provider:model`, the same shape `hooks.review_model` uses.
    pub model: Option<String>,
    /// Whether a first pass that is unsure may spend a second, tool-using one.
    pub escalate: bool,
    pub allow_rules: String,
    pub deny_rules: String,
    pub environment: String,
}

impl Settings {
    /// `autoreview.*`, read straight from preferences.
    pub fn load(pool: &crate::db::DbPool) -> Result<Self, String> {
        let mut conn = get_conn(pool)?;
        fn read(conn: &mut diesel::SqliteConnection, key: &str) -> Result<Option<String>, String> {
            crate::db::ops::preference::get_preference(conn, key)
                .map_err(|error| format!("failed to read preference {key}: {error}"))
        }
        Ok(Settings {
            enabled: parse_stored_bool("autoreview.enabled", read(&mut conn, "autoreview.enabled")?, false)?,
            model: read(&mut conn, "autoreview.model")?.filter(|m| !m.trim().is_empty()),
            // Defaults on: the escalating pass is what keeps false positives
            // from making the whole mode unusable, and it only runs when the
            // cheap pass was not sure.
            escalate: parse_stored_bool("autoreview.escalate", read(&mut conn, "autoreview.escalate")?, true)?,
            allow_rules: read(&mut conn, "autoreview.allow_rules")?.unwrap_or_default(),
            deny_rules: read(&mut conn, "autoreview.deny_rules")?.unwrap_or_default(),
            environment: read(&mut conn, "autoreview.environment")?.unwrap_or_default(),
        })
    }
}

fn parse_stored_bool(key: &str, raw: Option<String>, default: bool) -> Result<bool, String> {
    match raw.as_deref() {
        None => Ok(default),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(value) => Err(format!("preference {key} must be 'true' or 'false', got {value:?}")),
    }
}

/// Where the turn being reviewed is running.
pub struct Context {
    pub services: Services,
    pub conversation_id: String,
    pub turn_id: String,
    /// The project root, when the conversation has one. The policy leans on it
    /// heavily — "inside the project" is most of what separates routine from
    /// not — and its absence is itself a fact worth telling the reviewer.
    pub working_directory: Option<String>,
    /// The reviewed turn's own file boundary. The escalating pass runs inside
    /// it and never outside — see `investigate::context`.
    pub file_access: crate::tools::FileAccess,
    /// Whether more than one person can type into this conversation.
    ///
    /// A flag rather than the roster itself, because the roster is
    /// `onebot.admin_users` and reading it here keeps it out of a signature
    /// that already carries nineteen arguments. It also makes the desktop case
    /// unmistakable: `false` means there is nobody to distinguish, not that
    /// somebody forgot to pass the list.
    ///
    /// **A QQ private chat is not multi-party.** There is one counterpart and
    /// they are the reason the turn is running; marking them a bystander
    /// because they are not in `onebot.admin_users` would mean the reviewer
    /// sees a task nobody asked for and refuses everything. `qq_tools.rs`
    /// narrows a private chat by `is_admin` for the same reason it can — one
    /// counterpart cannot change under the session.
    pub multi_party: bool,
    /// True when nothing would be drawn if this fell back to asking. QQ and
    /// any headless runner; false for the desktop and for a phone attached to
    /// it, both of which can put a card in front of somebody.
    pub unattended: bool,
}

#[derive(Debug, Default)]
struct Circuit {
    consecutive: u32,
    total: u32,
    tripped: bool,
}

impl Circuit {
    /// Returns whether this denial is the one that trips the breaker.
    fn denied(&mut self) -> bool {
        self.consecutive += 1;
        self.total += 1;
        if !self.tripped && (self.consecutive >= MAX_CONSECUTIVE_DENIALS || self.total >= MAX_DENIALS_PER_TURN) {
            self.tripped = true;
            return true;
        }
        false
    }

    fn allowed(&mut self) {
        self.consecutive = 0;
    }
}

/// An asker with automatic review in front of it — or, when the feature is off,
/// the same asker with nothing in front of it.
///
/// Borrows rather than owning, because the thing it wraps borrows too:
/// `ChatApprovals` holds the callback OneBot answers through, so no
/// implementation of this port is `'static` and `Arc<dyn Approvals>` cannot
/// hold one. It is a stack value living exactly as long as the turn, which is
/// what `TurnPorts` wants anyway.
pub struct AutoReviewed<'a> {
    inner: &'a dyn Approvals,
    /// `None` when the feature is off or unconfigured. Every call then goes
    /// straight through, which is why the call sites do not branch.
    active: Option<Active>,
}

struct Active {
    settings: Settings,
    context: Context,
    /// Who may authorise things here, resolved once. Empty unless the
    /// conversation is a multi-party one.
    admins: Vec<i64>,
    circuit: Mutex<Circuit>,
}

/// The QQ admin roster, read the same way `onebot::load_config` reads it.
///
/// An absent list is an empty one, which makes every speaker a bystander — the
/// cautious end. An unreadable list is a damaged first-party contract and must
/// stop the turn rather than silently changing who may authorise it.
fn admin_roster(pool: &crate::db::DbPool) -> Result<Vec<i64>, String> {
    let mut conn = get_conn(pool)?;
    let stored = crate::db::ops::preference::get_preference(&mut conn, "onebot.admin_users")
        .map_err(|error| format!("failed to read preference onebot.admin_users: {error}"))?;
    match stored {
        None => Ok(Vec::new()),
        Some(raw) => serde_json::from_str::<Vec<i64>>(&raw)
            .map_err(|error| format!("preference onebot.admin_users has invalid JSON: {error}")),
    }
}

impl<'a> AutoReviewed<'a> {
    /// Put automatic review in front of an asker, or don't.
    ///
    /// Always returns a wrapper, and the wrapper is inert unless the feature is
    /// on and a model is named — so a call site is one line with no branch. A
    /// configured model is part of being on: without one there is nothing to
    /// ask, and silently falling back to the conversation's own model would
    /// bill a review at whatever the user picked for answering.
    /// A wrapper that reviews nothing, for a runner that has no [`Services`] to
    /// review with — the OneBot tests, and nothing else today.
    pub fn inert(inner: &'a dyn Approvals) -> Self {
        AutoReviewed { inner, active: None }
    }

    pub fn wrap(inner: &'a dyn Approvals, context: Context) -> Result<Self, String> {
        let settings = Settings::load(&context.services.db)?;
        if !settings.enabled || settings.model.is_none() {
            return Ok(AutoReviewed { inner, active: None });
        }
        let admins = if context.multi_party {
            admin_roster(&context.services.db)?
        } else {
            Vec::new()
        };
        Ok(AutoReviewed {
            inner,
            active: Some(Active {
                settings,
                context,
                admins,
                circuit: Mutex::new(Circuit::default()),
            }),
        })
    }

    /// Tool calls this never answers on the user's behalf.
    ///
    /// `ask_user` because its answer is a `Response` — the words themselves are
    /// what the model gets back — and nothing here has words to give. The mode
    /// transitions because entering or leaving plan mode is the user being
    /// shown a plan, not a permission being checked; a reviewer that approved
    /// `exit_plan` would be agreeing to a plan on their behalf.
    fn passthrough(name: &str) -> bool {
        name == "ask_user" || crate::agent::modes::transition_tools().any(|t| t == name)
    }
}

impl Active {
    /// Tell whoever is watching that the reviewer refused, or gave up.
    ///
    /// Best-effort on purpose: this is commentary beside a decision that has
    /// already been made, so a closed window must not turn it into a failure.
    /// The verdict itself is on the message row, which survives a reload.
    fn announce(&self, message_id: &str, call: &ToolCall, verdict: &AutoReviewVerdict) {
        let _ = self.context.services.events.emit_chat(ChatStreamEvent::AutoReview {
            conversation_id: self.context.conversation_id.clone(),
            turn_id: self.context.turn_id.clone(),
            // The pair, not the call id alone: provider call ids repeat, so it
            // is the message plus the call that identifies a card.
            message_id: message_id.to_string(),
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            verdict: verdict.clone(),
        });
    }

    /// The reviewer's own brief: the shared policy plus whatever the user added.
    fn system_prompt(&self) -> String {
        let section = |tag: &str, body: &str, prompt: String| {
            let body = body.trim();
            if body.is_empty() {
                return prompt;
            }
            prompt.replace(&format!("<{tag}>\n</{tag}>"), &format!("<{tag}>\n{body}\n</{tag}>"))
        };
        let prompt = POLICY.to_string();
        let prompt = section("user_environment", &self.settings.environment, prompt);
        let prompt = section("user_allow_rules", &self.settings.allow_rules, prompt);
        section("user_deny_rules", &self.settings.deny_rules, prompt)
    }

    /// Everything the reviewer is shown, assembled from the database.
    async fn scene_text(&self, call: &ToolCall, retry_reason: Option<&str>) -> Result<String, String> {
        let pool = self.context.services.db.clone();
        let id = self.context.conversation_id.clone();
        let history = tokio::task::spawn_blocking(move || {
            let mut conn = get_conn(&pool)?;
            let conversation = crate::db::ops::conversation::get_conversation(&mut conn, &id)
                .map_err(|error| format!("failed to load conversation {id} for auto review: {error}"))?;
            let messages = crate::db::ops::message::list_messages(&mut conn, &id)
                .map_err(|error| format!("failed to load conversation {id} messages for auto review: {error}"))?;
            Ok::<_, String>(crate::db::ops::message::active_context(
                &messages,
                conversation.head_message_id.as_deref(),
            ))
        })
        .await
        .map_err(|error| format!("auto-review transcript task failed: {error}"))??;

        let live = history.live();
        // The roster only means anything where there is more than one person to
        // apply it to. A private chat has an empty one *and* a single
        // counterpart, and reading the first as the second is what made every
        // one of their messages a bystander's.
        let party = if self.context.multi_party {
            projection::Party::Multi { admins: &self.admins }
        } else {
            projection::Party::Single
        };
        projection::render(
            &projection::Scene {
                history: live,
                party,
                working_directory: self.context.working_directory.as_deref(),
                retry_reason,
            },
            call,
        )
    }

    /// One review, start to finish. `Err` is anything that stopped it from
    /// producing a verdict — never a verdict of its own.
    async fn review(&self, message_id: &str, call: &ToolCall, retry_reason: Option<&str>) -> Result<Verdict, String> {
        let model = self.settings.model.as_deref().ok_or("no review model configured")?;
        let (provider_id, model_id) = model
            .split_once(':')
            .ok_or_else(|| format!("`{model}` is not a provider:model pair"))?;

        let secrets = self.context.services.secrets.clone();
        let pool = self.context.services.db.clone();
        let (pid, mid) = (provider_id.to_string(), model_id.to_string());
        let resolved = tokio::task::spawn_blocking(move || {
            crate::agent::resolve_with_overrides(&secrets, &pool, None, Some(mid), Some(&pid))
        })
        .await
        .map_err(|e| e.to_string())??;

        let pool = self.context.services.db.clone();
        // `ResolvedProvider` is not `Clone`, and these strings are all the
        // resolver wants from it.
        let r = (
            resolved.provider_id.clone(),
            resolved.provider_type.clone(),
            resolved.api_format.clone(),
            resolved.model.clone(),
            resolved.transport_profile.clone(),
        );
        let params = tokio::task::spawn_blocking(move || {
            crate::agent::resolve_turn_params(
                &pool,
                crate::agent::TurnParamsResolveRequest {
                    assistant: None,
                    provider_id: Some(&r.0),
                    provider_type: &r.1,
                    api_format: &r.2,

                    transport_profile: &r.4,
                    model: &r.3,
                    thinking_level: None,
                    fast: false,
                },
            )
        })
        .await
        .map_err(|e| e.to_string())??;

        // Zero temperature and no reasoning budget: this is a classification,
        // and a review paid for in thinking tokens on every tool call is one
        // the user turns off. The escalating pass is where depth is bought.
        let mut chat_params = crate::agent::without_thinking(params.params.clone());
        chat_params.temperature = Some(0.0);

        let provider = crate::provider::registry::create_provider(
            &resolved.provider_type,
            &resolved.base_url,
            &resolved.credential,
            &resolved.api_format,
            &resolved.transport_profile,
        )?;

        let scene = self.scene_text(call, retry_reason).await?;
        let messages = vec![system(&self.system_prompt()), ChatMessage::user(&scene)];

        let asked = provider.chat_with_tools(messages.clone(), vec![], chat_params.clone());
        let first = tokio::time::timeout(std::time::Duration::from_secs(FIRST_PASS_TIMEOUT_SECS), asked)
            .await
            .map_err(|_| format!("审查在 {FIRST_PASS_TIMEOUT_SECS}s 内没有返回"))?
            .map_err(|e| e.to_string())?;

        let mut usage = usage_of(first.usage.as_ref());
        let mut peak_prompt = usage.input_tokens;
        let mut stage = AutoReviewStage::Quick;
        let mut evidence: Vec<AutoReviewEvidence> = Vec::new();

        let read = assessment::parse(&first.text);
        // Escalate on anything the quick pass did not settle: a denial (the
        // false positives are what make this mode unusable), a dangerous
        // allow, and an answer nobody could read. Each of those is a question
        // the reviewer could answer for itself by looking at the repository.
        let settled = matches!(&read, Read::Verdict(a) if a.settled());
        let read = if settled || !self.settings.escalate {
            read
        } else {
            stage = AutoReviewStage::Investigate;
            let deeper = investigate::run(investigate::Job {
                services: &self.context.services,
                provider: provider.as_ref(),
                params: chat_params,
                system_prompt: self.system_prompt(),
                scene: &scene,
                working_directory: self.context.working_directory.as_deref(),
                file_access: &self.context.file_access,
                turn_id: &self.context.turn_id,
                conversation_id: &self.context.conversation_id,
            })
            .await;
            usage = add(usage, deeper.usage);
            peak_prompt = peak(peak_prompt, deeper.peak_prompt);
            evidence = deeper.evidence;
            match deeper.read {
                // The deep pass could not answer either. Keep whatever the
                // quick one said rather than throwing away a readable verdict.
                Read::Unreadable(_) => read,
                answered => answered,
            }
        };

        Ok(Verdict {
            read,
            stage,
            usage,
            peak_prompt,
            evidence,
            model: model.to_string(),
            provider_id: resolved.provider_id,
            provider_name: resolved.provider_name,
            model_id: resolved.model,
            message_id: message_id.to_string(),
        })
    }

    /// File the verdict against the message, and what it cost against the log.
    ///
    /// Neither failure stops anything: the decision has already been made, and
    /// refusing to act on it because a bookkeeping write failed would turn a
    /// database hiccup into a stuck turn.
    async fn record(&self, call: &ToolCall, verdict: &Verdict, stored_verdict: AutoReviewVerdict) {
        let pool = self.context.services.db.clone();
        let (message_id, call_id) = (verdict.message_id.clone(), call.id.clone());
        let cost = verdict.clone();
        let conversation_id = self.context.conversation_id.clone();
        let turn_id = self.context.turn_id.clone();
        let summary = summary_of(&verdict.read);

        let _ = tokio::task::spawn_blocking(move || {
            let Ok(mut conn) = get_conn(&pool) else {
                return;
            };
            if let Err(e) =
                crate::db::ops::message::record_auto_review(&mut conn, &message_id, &call_id, &stored_verdict)
            {
                tracing::warn!(error = %e, "could not file the auto-review verdict");
            }
            if let Err(e) = crate::db::ops::audit::record_side_request(
                &mut conn,
                crate::db::ops::audit::SideRequestCost {
                    role: crate::db::ops::audit::AUTO_REVIEW_ROLE,
                    message_id: &message_id,
                    conversation_id: &conversation_id,
                    turn_id: Some(&turn_id),
                    provider_id: Some(&cost.provider_id),
                    provider_name: Some(&cost.provider_name),
                    model_id: Some(&cost.model_id),
                    usage: cost.usage,
                    peak_prompt_tokens: cost.peak_prompt,
                    summary: &summary,
                },
            ) {
                tracing::warn!(error = %e, "could not record what the auto review cost");
            }
        })
        .await;
    }
}

/// A finished review, with everything needed to file it.
#[derive(Debug, Clone)]
struct Verdict {
    read: Read,
    stage: AutoReviewStage,
    usage: MessageUsage,
    /// The largest single round's prompt, for choosing a price tier. Never
    /// the sum in `usage` — see `ReviewCost::peak_prompt_tokens`.
    peak_prompt: Option<i32>,
    evidence: Vec<AutoReviewEvidence>,
    model: String,
    provider_id: String,
    provider_name: String,
    model_id: String,
    message_id: String,
}

impl Verdict {
    fn event_verdict(&self) -> AutoReviewVerdict {
        let (outcome, risk, authorization, rationale) = match &self.read {
            Read::Verdict(a) => (
                match a.outcome {
                    Outcome::Allow => EventOutcome::Allow,
                    Outcome::Deny => EventOutcome::Deny,
                },
                Some(match a.risk {
                    RiskLevel::Low => AutoReviewRisk::Low,
                    RiskLevel::Medium => AutoReviewRisk::Medium,
                    RiskLevel::High => AutoReviewRisk::High,
                    RiskLevel::Critical => AutoReviewRisk::Critical,
                }),
                Some(match a.authorization {
                    AuthLevel::Unknown => AutoReviewAuthorization::Unknown,
                    AuthLevel::Low => AutoReviewAuthorization::Low,
                    AuthLevel::Medium => AutoReviewAuthorization::Medium,
                    AuthLevel::High => AutoReviewAuthorization::High,
                }),
                Some(a.rationale.clone()),
            ),
            Read::Unreadable(why) => (EventOutcome::Unreadable, None, None, Some((*why).to_string())),
        };
        AutoReviewVerdict {
            outcome,
            risk,
            authorization,
            rationale,
            stage: Some(self.stage),
            model: Some(self.model.clone()),
            evidence: self.evidence.clone(),
        }
    }
}

/// One line for the audit log. Never the transcript that was sent — that
/// carries the user's own messages and this table is exportable.
fn summary_of(read: &Read) -> String {
    match read {
        Read::Verdict(a) => format!("{:?}/{:?}: {}", a.outcome, a.risk, a.rationale),
        Read::Unreadable(why) => format!("unreadable: {why}"),
    }
}

/// A system-role message. `ChatMessage` has no constructor for one — the only
/// other place that needs it is `build_messages_with_senders`, which builds a
/// whole payload rather than a single message.
pub(crate) fn system(content: &str) -> ChatMessage {
    ChatMessage {
        role: "system".into(),
        content: content.into(),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
        tool_error: false,
        provider_state: None,
        origin: crate::provider::MessageOrigin::Assistant,
    }
}

fn usage_of(usage: Option<&TokenUsage>) -> MessageUsage {
    match usage {
        None => MessageUsage::default(),
        Some(u) => MessageUsage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
            cache_read_tokens: u.cache_read_tokens,
            cache_write_tokens: u.cache_write_tokens,
            server_tool_calls: None,
        },
    }
}

/// `None` stays `None`, because it means the upstream said nothing rather than
/// that it said zero — the distinction migration 28 exists to keep.
/// The larger of two rounds' prompts, keeping "nobody reported" distinct from
/// zero.
///
/// A review's rounds are summed for what they cost, but a *tier* is a fact about
/// one request. `add` answers the first question and this one answers the
/// second; using the sum for both bills a review of seven small rounds as though
/// it had made one enormous one.
fn peak(current: Option<i32>, round: Option<i32>) -> Option<i32> {
    match (current, round) {
        (None, other) | (other, None) => other,
        (Some(a), Some(b)) => Some(a.max(b)),
    }
}

fn add(a: MessageUsage, b: MessageUsage) -> MessageUsage {
    let sum = |x: Option<i32>, y: Option<i32>| match (x, y) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    };
    MessageUsage {
        input_tokens: sum(a.input_tokens, b.input_tokens),
        output_tokens: sum(a.output_tokens, b.output_tokens),
        cache_read_tokens: sum(a.cache_read_tokens, b.cache_read_tokens),
        cache_write_tokens: sum(a.cache_write_tokens, b.cache_write_tokens),
        server_tool_calls: None,
    }
}

#[async_trait::async_trait]
impl Approvals for AutoReviewed<'_> {
    async fn ask(
        &self,
        assistant_message_id: &str,
        call: &ToolCall,
        retry_reason: Option<&str>,
    ) -> Result<Option<ApprovalDecision>, String> {
        let Some(active) = self.active.as_ref().filter(|_| !Self::passthrough(&call.name)) else {
            return self.inner.ask(assistant_message_id, call, retry_reason).await;
        };

        // Already given up on this turn. Refusing without asking is the point:
        // the alternative is paying for a review of every further attempt to
        // get around the last refusal.
        if active.circuit.lock().map(|c| c.tripped).unwrap_or(false) {
            return Ok(Some(ApprovalDecision::Denied(Some(
                "本轮已被自动审查连续拒绝多次，后续操作一律拒绝。请让用户直接介入。".into(),
            ))));
        }

        let verdict = match active.review(assistant_message_id, call, retry_reason).await {
            Ok(v) => v,
            Err(e) => {
                // Not evidence about the action — see the module header.
                tracing::warn!(tool = %call.name, error = %e, "auto review did not produce a verdict");
                // Said out loud even though there is nothing to file: a review
                // that never got as far as a model has no usage to record, but
                // a reviewer that has stopped working is exactly the thing the
                // user has to be able to notice. Without this the card falls
                // back to asking with no indication of why.
                active.announce(
                    assistant_message_id,
                    call,
                    &AutoReviewVerdict {
                        outcome: EventOutcome::Unreadable,
                        risk: None,
                        authorization: None,
                        rationale: Some(e.clone()),
                        stage: None,
                        model: None,
                        evidence: Vec::new(),
                    },
                );
                return self
                    .unresolved(active.context.unattended, assistant_message_id, call, retry_reason, &e)
                    .await;
            }
        };

        let event_verdict = verdict.event_verdict();
        active.record(call, &verdict, event_verdict.clone()).await;

        match &verdict.read {
            Read::Verdict(a) if a.outcome == Outcome::Allow => {
                if let Ok(mut c) = active.circuit.lock() {
                    c.allowed();
                }
                active.announce(assistant_message_id, call, &event_verdict);
                tracing::info!(
                    tool = %call.name,
                    risk = ?a.risk,
                    authorization = ?a.authorization,
                    stage = ?verdict.stage,
                    "auto review allowed a tool call"
                );
                Ok(Some(ApprovalDecision::Approved))
            }
            Read::Verdict(a) => {
                let tripped = active.circuit.lock().map(|mut c| c.denied()).unwrap_or(false);
                active.announce(assistant_message_id, call, &event_verdict);
                tracing::info!(
                    tool = %call.name,
                    risk = ?a.risk,
                    authorization = ?a.authorization,
                    stage = ?verdict.stage,
                    "auto review denied a tool call"
                );
                let mut reason = a.rationale.clone();
                if tripped {
                    reason.push_str("\n（本轮拒绝次数已达上限，后续操作将一律拒绝。）");
                }
                Ok(Some(ApprovalDecision::Denied(Some(reason))))
            }
            Read::Unreadable(why) => {
                // The same reason as the `Err` arm above, and the same event:
                // this one *did* cost money, so the card has something to show
                // for it — including what the escalating pass looked at before
                // giving up.
                active.announce(assistant_message_id, call, &event_verdict);
                self.unresolved(active.context.unattended, assistant_message_id, call, retry_reason, why)
                    .await
            }
        }
    }
}

impl AutoReviewed<'_> {
    /// No verdict. Fall back to a person if there is one, refuse if there is not.
    ///
    /// Takes the flag rather than the whole `Active`, because this is the one
    /// decision in the module worth testing on its own and standing up a
    /// `Services` to reach it would mean nobody did.
    async fn unresolved(
        &self,
        unattended: bool,
        assistant_message_id: &str,
        call: &ToolCall,
        retry_reason: Option<&str>,
        why: &str,
    ) -> Result<Option<ApprovalDecision>, String> {
        if unattended {
            return Ok(Some(ApprovalDecision::Denied(Some(format!(
                "自动审查没有给出结论（{why}），且当前没有人可以确认，因此拒绝。"
            )))));
        }
        self.inner.ask(assistant_message_id, call, retry_reason).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::modes::{ENTER_PLAN_TOOL, EXIT_PLAN_TOOL};
    use crate::db::test_db;
    use diesel::RunQueryDsl;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn set_preference(pool: &crate::db::DbPool, key: &str, value: &str) {
        let mut conn = pool.get().unwrap();
        crate::db::ops::preference::set_preference(&mut conn, key, value, 1).unwrap();
    }

    #[test]
    fn absent_auto_review_preferences_keep_the_existing_defaults() {
        let settings = Settings::load(&test_db()).unwrap();

        assert!(!settings.enabled);
        assert!(settings.escalate);
        assert!(settings.model.is_none());
        assert!(settings.allow_rules.is_empty());
        assert!(settings.deny_rules.is_empty());
        assert!(settings.environment.is_empty());
    }

    #[test]
    fn malformed_auto_review_boole_are_not_defaulted() {
        for (key, value) in [("autoreview.enabled", "1"), ("autoreview.escalate", "FALSE")] {
            let pool = test_db();
            set_preference(&pool, key, value);
            let error = Settings::load(&pool).expect_err("malformed stored boolean must fail settings loading");
            assert!(error.contains(key), "{key}: {error}");
        }
    }

    #[test]
    fn malformed_admin_roster_is_not_an_empty_roster() {
        for value in ["not json", r#"{"admin": 1}"#, r#"[1,"2"]"#] {
            let pool = test_db();
            set_preference(&pool, "onebot.admin_users", value);
            let error = admin_roster(&pool).expect_err("malformed admin roster must fail");
            assert!(error.contains("onebot.admin_users"), "{value}: {error}");
        }

        let pool = test_db();
        assert!(admin_roster(&pool).unwrap().is_empty());
        set_preference(&pool, "onebot.admin_users", "[1,2]");
        assert_eq!(admin_roster(&pool).unwrap(), vec![1, 2]);
    }

    #[test]
    fn auto_review_preference_read_errors_are_not_defaulted() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        diesel::sql_query("DROP TABLE preferences").execute(&mut conn).unwrap();
        drop(conn);

        let error = Settings::load(&pool).expect_err("database errors must fail settings loading");
        assert!(error.contains("autoreview.enabled"), "{error}");
    }

    /// Whoever is behind the reviewer. Counts, because half of what these tests
    /// check is that it was *not* reached.
    #[derive(Default)]
    struct Asker(AtomicUsize);

    #[async_trait::async_trait]
    impl Approvals for Asker {
        async fn ask(
            &self,
            _assistant_message_id: &str,
            _call: &ToolCall,
            _retry_reason: Option<&str>,
        ) -> Result<Option<ApprovalDecision>, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Some(ApprovalDecision::Approved))
        }
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: name.into(),
            arguments: "{}".into(),
        }
    }

    /// Off, or unconfigured. Every call goes through untouched, which is what
    /// lets the two call sites wrap unconditionally.
    #[tokio::test]
    async fn an_inert_wrapper_is_the_asker_it_wraps() {
        let asker = Asker::default();
        let wrapped = AutoReviewed::inert(&asker);

        let decision = wrapped.ask("m1", &call("run_command"), None).await.unwrap();
        assert!(matches!(decision, Some(ApprovalDecision::Approved)));
        assert_eq!(asker.0.load(Ordering::SeqCst), 1);
    }

    /// A permission prompt that proceeds because nobody answered is not a
    /// permission prompt. This is the inverse of `hooks/`, deliberately.
    #[tokio::test]
    async fn no_verdict_and_nobody_watching_is_a_refusal() {
        let asker = Asker::default();
        let wrapped = AutoReviewed::inert(&asker);

        let decision = wrapped
            .unresolved(true, "m1", &call("run_command"), None, "审查超时")
            .await
            .unwrap();
        match decision {
            Some(ApprovalDecision::Denied(Some(reason))) => {
                assert!(reason.contains("审查超时"), "the reason travels: {reason}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(asker.0.load(Ordering::SeqCst), 0, "nobody was asked");
    }

    /// And where somebody *is* watching, an unreadable answer costs a card
    /// rather than a refusal — the review not working must not become a policy.
    #[tokio::test]
    async fn no_verdict_with_a_window_open_falls_back_to_asking() {
        let asker = Asker::default();
        let wrapped = AutoReviewed::inert(&asker);

        let decision = wrapped
            .unresolved(false, "m1", &call("run_command"), None, "审查超时")
            .await
            .unwrap();
        assert!(matches!(decision, Some(ApprovalDecision::Approved)));
        assert_eq!(asker.0.load(Ordering::SeqCst), 1);
    }

    /// A `Response` is the answer to a question, and only `ask_user` asked one.
    /// A reviewer approving `exit_plan` would be agreeing to a plan for the user.
    #[test]
    fn questions_and_mode_switches_are_never_answered_here() {
        for name in ["ask_user", ENTER_PLAN_TOOL, EXIT_PLAN_TOOL] {
            assert!(AutoReviewed::passthrough(name), "{name}");
        }
        for name in ["run_command", "delete_file", "mcp__x__y", "write_file"] {
            assert!(!AutoReviewed::passthrough(name), "{name}");
        }
    }

    #[test]
    fn three_denials_in_a_row_trip_the_breaker() {
        let mut c = Circuit::default();
        assert!(!c.denied());
        assert!(!c.denied());
        assert!(c.denied(), "the third consecutive denial trips it");
        assert!(!c.denied(), "it only trips once");
        assert!(c.tripped);
    }

    /// An allow between denials means the reviewer is still discriminating,
    /// which is the opposite of the state the breaker is for.
    #[test]
    fn an_allow_resets_the_consecutive_count() {
        let mut c = Circuit::default();
        c.denied();
        c.denied();
        c.allowed();
        assert!(!c.denied(), "the run was broken, so this is the first again");
        assert!(!c.tripped);
    }

    /// Spaced-out denials still add up: a turn refused ten times is not going
    /// to finish, however patiently it was refused.
    #[test]
    fn enough_denials_trip_it_however_they_are_spaced() {
        let mut c = Circuit::default();
        let mut tripped = false;
        for _ in 0..MAX_DENIALS_PER_TURN {
            tripped |= c.denied();
            c.allowed();
        }
        assert!(tripped);
    }

    /// A review is up to seven requests, and the two questions its usage answers
    /// need different arithmetic: what it *cost* is the sum, which tier it
    /// *reached* is the largest single round. Summing for both is what would bill
    /// a review of seven small rounds at the long-context rate.
    #[test]
    fn a_peak_is_the_largest_round_not_the_running_total() {
        assert_eq!(peak(None, None), None, "silence is not zero");
        assert_eq!(peak(None, Some(40_000)), Some(40_000));
        assert_eq!(peak(Some(40_000), None), Some(40_000));
        assert_eq!(peak(Some(40_000), Some(90_000)), Some(90_000));
        assert_eq!(peak(Some(90_000), Some(40_000)), Some(90_000), "never shrinks");

        // Seven rounds of 50k: the sum crosses grok-4.6's 200k threshold and no
        // single request came close to it.
        let rounds = [50_000; 7];
        let summed: i32 = rounds.iter().sum();
        let highest = rounds.iter().fold(None, |acc, r| peak(acc, Some(*r)));
        assert_eq!(summed, 350_000);
        assert_eq!(highest, Some(50_000));
        assert!(summed > 200_000 && highest.unwrap() < 200_000, "the whole point");
    }

    /// `None` means the upstream said nothing about caching and `Some(0)` means
    /// it said nothing was cached. Summing two silences must not invent a zero.
    #[test]
    fn adding_two_silences_stays_silent() {
        let quiet = MessageUsage::default();
        assert_eq!(add(quiet, quiet).cache_read_tokens, None);

        let said = MessageUsage {
            cache_read_tokens: Some(0),
            ..Default::default()
        };
        assert_eq!(add(quiet, said).cache_read_tokens, Some(0));
    }

    #[test]
    fn usage_adds_up_across_the_two_passes() {
        let first = MessageUsage {
            input_tokens: Some(100),
            output_tokens: Some(10),
            ..Default::default()
        };
        let second = MessageUsage {
            input_tokens: Some(400),
            output_tokens: Some(30),
            ..Default::default()
        };
        let total = add(first, second);
        assert_eq!(total.input_tokens, Some(500));
        assert_eq!(total.output_tokens, Some(40));
    }
}

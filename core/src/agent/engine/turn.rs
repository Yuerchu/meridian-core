//! One turn: ask the model, run what it asked for, ask again.
//!
//! This is the loop both runners had a copy of. Everything the two genuinely
//! disagreed about is a port or a policy now; everything else was the same code
//! typed twice, which is how the approval rule ended up fixed on one side only.
//!
//! What it is not responsible for, and must not become responsible for:
//!
//! - **The lease.** Acquiring and returning the conversation is the caller's,
//!   and both callers cover about thirty exits with a guard whose drop order was
//!   argued line by line. A loop that took the lease would have to reproduce that
//!   argument, badly.
//! - **The turn record.** `begin` and `finish` bracket the guard, not the loop.
//!   Opening the record before the guard exists leaves a window where a dropped
//!   task releases the conversation silently, and only the caller knows whether
//!   the end was `Done`, `Cancelled` or a loop it had to stop.
//! - **The terminal event.** A `TurnEnd::Continue` round is the same turn going
//!   round again; announcing a stop between rounds would invite a desktop
//!   message the coordinator then refuses.
//! - **The first user row.** Written before any of this, and the two runners
//!   write different numbers of them. It is setup, not loop.
//!
//! So the loop reports and the caller decides: [`TurnOutcome`] carries the reply
//! *and* what got done, including when the reply is an error, because a turn
//! that died halfway still owes the front end an event naming the row it was
//! writing.

use std::collections::HashSet;

use futures::FutureExt;
use tokio_util::sync::CancellationToken;

use crate::agent::modes::ModeSpec;
use crate::agent::tokenizer::MIN_REPLY_TOKENS;
use crate::agent::{MAX_STREAM_RETRIES, STREAM_RETRY_BASE, is_context_window_error, is_retryable_stream_error};
use crate::agent::{TokenBudget, serialize_tool_calls_openai};
use crate::db::DbPool;
use crate::db::models::message::MessageUsage;
use crate::db::models::turn::TurnPhase;
use crate::events::{ChatStopReason, ChatStreamEvent, ToolOutcome};
use crate::mcp::McpRegistry;
use crate::provider::{ChatMessage, ChatParams, ChatProvider, ToolDefinition};
use crate::tools::{self, ToolContext, ToolRegistry};

use super::compaction::{Compacting, CompactionPolicy};
use super::ports::{Steered, SteeredOrigin, Steering, SubAgentReport, SubAgentSpec, SubAgentStatus, TurnPorts};

/// How many times a finished answer may be reopened by something that arrived
/// while it was being written.
///
/// Small on purpose. Each continuation is a whole extra round trip started by
/// somebody typing rather than by the model needing one, and the point is to
/// catch the message sent a second before the answer landed — not to turn a
/// turn into a chat session. Past it the messages stay in the inbox and the
/// caller accounts for them.
const MAX_TAIL_CONTINUATIONS: usize = 3;

/// How many times one turn may be resumed after a `pause_turn`.
///
/// The Messages API stops a turn while a server tool is running long and asks
/// for the same request back with the round's blocks appended. Each resume is
/// a request with nothing new in it, so a server that paused on every round
/// would otherwise be a loop the loop guard cannot see — it counts tool calls,
/// and these rounds make none.
const MAX_PAUSE_CONTINUATIONS: usize = 8;

/// What the model is told when an approval question ended with no answer.
///
/// Not "denied by user", because nobody did that: `Ok(None)` from the
/// `Approvals` port is a card that expired, a turn that outlived its question,
/// or a reviewer that fell back to drawing a card nobody saw. `crate::approval`
/// promises that timing out is never a denial — and until this constant, the
/// promise held everywhere except in the one sentence the model actually reads.
const UNANSWERED_APPROVAL: &str = "No one answered the approval request before it expired. The tool was not run — this was not \
     a refusal, so you may ask again later or continue without it.";
use super::{
    ApprovalDecision, append_steering, append_tool_result, begin_assistant, complete_assistant, consume_stream,
    in_phase, transitions,
};

/// Why no request was made. Counts only -- this reaches a window, and the
/// numbers are the whole diagnosis: what the conversation costs against what it
/// is allowed to.
fn no_room(budget: &TokenBudget) -> String {
    format!(
        "the conversation fills the context window ({used} of {limit} tokens) and compacting it \
         freed nothing, so there is no room left for a reply",
        used = budget.current_estimate,
        limit = budget.context_limit,
    )
}

/// Read a `run_agent` call into something the port can act on.
///
/// Every failure here is the model's to fix, so each one says what to write
/// instead rather than just what was wrong. They come back as a tool result, not
/// as an error that ends the turn: naming a model that has not been configured
/// is an ordinary mistake, and the answer to it is to pick another one.
fn parse_sub_agent(arguments: &str, parent_message_id: &str, parent_call_id: &str) -> Result<SubAgentSpec, String> {
    let args: serde_json::Value =
        serde_json::from_str(arguments).map_err(|e| format!("arguments were not valid JSON: {e}"))?;
    let text = |key: &str| -> Result<String, String> {
        args.get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("`{key}` is required and must be a non-empty string"))
    };

    Ok(SubAgentSpec {
        kind: crate::agent::sub_agents::SubAgentKind::parse(&text("agent")?)?,
        description: text("description")?,
        prompt: text("prompt")?,
        model: args.get("model").and_then(|v| v.as_str()).map(str::to_string),
        parent_message_id: parent_message_id.to_string(),
        parent_call_id: parent_call_id.to_string(),
    })
}

fn parse_tool_arguments(arguments: &str) -> Result<serde_json::Value, String> {
    let value: serde_json::Value =
        serde_json::from_str(arguments).map_err(|error| format!("invalid tool arguments JSON: {error}"))?;
    if !value.is_object() {
        return Err("tool arguments must be a JSON object".into());
    }
    Ok(value)
}

/// A provider may emit `exit_plan` before the `update_plan` beside it. Tool
/// calls are ordinarily sequential and retain their wire order; the one
/// exception is this batch barrier, which commits every plan patch before any
/// exit snapshots the head. Relative order within both groups is stable.
fn plan_batch_order(calls: &[crate::provider::ToolCall]) -> Vec<&crate::provider::ToolCall> {
    if !calls
        .iter()
        .any(|call| call.name == crate::agent::modes::EXIT_PLAN_TOOL)
        || !calls
            .iter()
            .any(|call| call.name == crate::agent::modes::UPDATE_PLAN_TOOL)
    {
        return calls.iter().collect();
    }
    calls
        .iter()
        .filter(|call| call.name != crate::agent::modes::EXIT_PLAN_TOOL)
        .chain(
            calls
                .iter()
                .filter(|call| call.name == crate::agent::modes::EXIT_PLAN_TOOL),
        )
        .collect()
}

/// What the parent's model is told about a run that has ended.
///
/// The verdict comes first and in words, because the reply on its own does not
/// carry one: a cancelled run and a finished one both return whatever text had
/// been written. Reading half an answer as the answer is the failure this
/// sentence exists to prevent.
fn sub_agent_result(report: &SubAgentReport) -> String {
    let steps = report.steps;
    let head = match report.status {
        SubAgentStatus::Done => format!("Sub-agent finished after {steps} steps."),
        SubAgentStatus::Cancelled => format!(
            "Sub-agent was stopped after {steps} steps. Anything below is partial and does not \
             answer the task; do not treat it as a conclusion."
        ),
        SubAgentStatus::Aborted => format!(
            "Sub-agent was stopped after {steps} steps because it kept repeating itself. \
             Anything below is partial."
        ),
        SubAgentStatus::Failed => {
            format!("Sub-agent failed after {steps} steps.")
        }
    };
    // Said plainly, because the likeliest content of a message that missed the
    // run is the correction the user is about to ask why it ignored.
    let missed = if report.stranded.is_empty() {
        String::new()
    } else {
        let n = report.stranded.accepted;
        let lost = report.stranded.unrecorded;
        let where_to_read = if lost == 0 {
            "They are in its transcript.".to_string()
        } else if lost == n {
            "None of them could be written down, so they are nowhere but here.".to_string()
        } else {
            format!("{} of them could not be written down.", lost)
        };
        format!(
            "\n\nThe user sent {n} message(s) to the sub-agent after it had stopped reading, so it \
             never saw them. {where_to_read} Read them before acting on the answer above."
        )
    };
    let body = report.reply.trim();
    if body.is_empty() {
        format!("{head} It returned no text.{missed}")
    } else {
        format!("{head}\n\n{body}{missed}")
    }
}

/// The long-lived things a turn borrows. No `AppHandle`, and no state locator:
/// each of these is already an explicit parameter on the OneBot side, and making
/// them explicit on the desktop side is most of what stops the loop from needing
/// a window.
pub struct TurnServices<'a> {
    pub pool: &'a DbPool,
    pub tools: &'a ToolRegistry,
    pub mcp: &'a McpRegistry,
}

/// How a registry tool's declared permission becomes a question.
///
/// Two rules because the two runners have two. `ByReach` also consults where the
/// call would land — `read_file` inside the project and `read_file` pointed at
/// `~/.ssh` are the same tool — and lets a standing yes answer ordinary project
/// edits. `ByPermission` does neither, so it asks more often and cannot be told
/// to stop. Converging them is on the drift list; it is a change to when a
/// person is interrupted, which is not something to do in passing.
pub enum ApprovalRule {
    ByReach { accept_edits: bool },
    ByPermission,
}

/// What the model is told when it names a tool this turn did not offer.
///
/// `Explained` says the tool is unavailable *and* not to route around it,
/// because a model told a writing tool does not exist will reach for one that
/// does — `run_command` writes files perfectly well — and defeat the very
/// pruning the mode exists to do.
///
/// There was a `Terse` alternative reading "Unknown tool", which the OneBot
/// runner used while a tool its speaker could not run was also absent from the
/// definitions it was sent. Once that runner started showing the whole
/// session's tools and refusing at dispatch instead — so that an admin's turn
/// and an ordinary member's turn share one cached prefix — telling the model a
/// tool it can see does not exist would have invited exactly the retry and the
/// workaround this wording exists to prevent.
pub enum WithheldWording {
    Explained,
}

impl WithheldWording {
    fn say(&self, tool: &str) -> String {
        match self {
            WithheldWording::Explained => format!(
                "The tool '{tool}' is not available in this conversation right now. Do not try \
                 to achieve the same effect through another tool."
            ),
        }
    }
}

/// Everything decided before the first request goes out.
pub struct TurnSetup<'a> {
    pub provider: &'a dyn ChatProvider,
    pub params: ChatParams,
    pub chat_messages: Vec<ChatMessage>,
    pub tool_defs: Vec<ToolDefinition>,
    /// The only thing that authorises a call. Checked instead of the assistant's
    /// configuration, because a tool a mode removed is still in the registry.
    pub offered: HashSet<String>,
    pub mode: &'static ModeSpec,
    pub tool_context: ToolContext,
    pub budget: TokenBudget,
    pub turn_id: String,
    pub conversation_id: String,
    /// Which configured upstream this turn is talking to, and what it was called,
    /// for the rows it writes.
    ///
    /// Carried rather than derived, because nothing the loop already holds knows
    /// it: `ChatParams` has only the model name, and `provider` is a trait object
    /// with no identity. Each of the three callers has the answer in scope and
    /// each arrives at it differently — a per-request override on the desktop,
    /// the assistant's own on OneBot, a resolved `provider:model` on a sub-agent.
    ///
    /// `None` is legitimate rather than a bug: the test harness runs turns with
    /// no `providers` row to point at, and the foreign key on the column would
    /// refuse a made-up id.
    pub provider_id: Option<String>,
    pub provider_name: Option<String>,
    /// Where the next row hangs. A cursor rather than one precomputed parent:
    /// the turn writes as it goes, and steering can add rows mid-flight.
    pub parent_cursor: Option<String>,
    pub cancel: CancellationToken,
    pub keep_recent: usize,
    pub context_limit: usize,
    pub approval_rule: ApprovalRule,
    pub withheld: WithheldWording,
    /// Where a steered message's `file://` parts resolve against.
    pub files_root: Option<std::path::PathBuf>,
    /// How earlier turns ended, for any that did not end cleanly. Retired by the
    /// first reply read all the way to the end — not by getting a request away,
    /// and not by reading the record.
    pub interrupted: Option<crate::agent::interrupted::Report>,
    pub compaction: CompactionPolicy,
    /// What this model costs, so each round can be priced at its own prompt
    /// size. `None` leaves `progress.cost` unset — for a model nobody has
    /// priced, and for the runners that report tokens rather than money.
    pub pricing: Option<crate::agent::pricing::TurnPricing>,
}

/// What one reply reported, in the shape a row stores.
///
/// Written here rather than as a `From` impl on `MessageUsage`, so that
/// `db::models` goes on knowing nothing about providers. The database layer
/// stores four numbers under a stated contract; which wire field each came from,
/// and what had to be folded together to satisfy that contract, is the provider
/// layer's business.
///
/// A response with no usage block leaves every field `None` rather than zero. A
/// provider that did not say is not a provider that said the reply was free.
fn row_usage(usage: Option<&crate::provider::TokenUsage>) -> MessageUsage {
    match usage {
        Some(u) => MessageUsage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
            cache_read_tokens: u.cache_read_tokens,
            cache_write_tokens: u.cache_write_tokens,
            server_tool_calls: u.billable_tool_calls,
        },
        None => MessageUsage::default(),
    }
}

/// What a turn left behind, whatever became of it.
#[derive(Default)]
pub struct TurnProgress {
    /// The assistant row being written, once there was one. `None` means it
    /// failed before creating one — the stop still has to go out, it just has no
    /// message to hang off.
    pub message_id: Option<String>,
    pub input_tokens: i32,
    pub output_tokens: i32,
    /// Prompt tokens this turn's rounds got out of the upstream's cache, and
    /// wrote into it, summed the same way as the two above.
    ///
    /// Plain `i32` rather than `Option<i32>`, unlike the row columns. A turn
    /// total has nowhere to put "nobody said": it is a sum over rounds that may
    /// disagree about whether they reported at all, and `Some(0) + None` has no
    /// honest answer. That distinction is kept per row, where each number has
    /// exactly one reporter.
    pub cache_read_tokens: i32,
    pub cache_write_tokens: i32,
    /// What this turn cost, summed per round rather than derived from the totals
    /// above.
    ///
    /// The distinction only matters where a model prices by prompt size, and
    /// there it matters a lot: five 50k requests and one 250k request leave
    /// identical totals behind, and on `grok-4.6` the second is billed at twice
    /// the rate. Only this loop ever sees the individual sizes, so pricing after
    /// the fact from `input_tokens` would put the first turn in the second one's
    /// bracket.
    ///
    /// `None` means no cost could be worked out — either nobody priced this
    /// model, or no round reported any usage to price. Both are distinct from
    /// zero, which would be a claim that the turn was free.
    pub cost: Option<crate::agent::pricing::RequestCost>,
    /// The loop guard cut it short.
    pub aborted: bool,
    /// The model declined: the Messages API's `refusal` stop, or
    /// chat-completions' `content_filter`. An ordinary ending as far as the
    /// loop is concerned, and a different thing to tell the reader — an
    /// answer that is empty because it was withheld is not an empty answer.
    pub refused: bool,
    /// The last reply hit the output limit and stopped mid-sentence.
    pub truncated: bool,
    /// How many times the model was asked — one per assistant row. Not tool
    /// calls: a round that made three is still one step, and a round that made
    /// none still cost a request.
    pub steps: usize,
    /// The last row this turn wrote that is still on the active path.
    ///
    /// Not `message_id`. That is the last *assistant* row, and a turn's last
    /// reachable row is routinely something else — a tool result when the work
    /// ended on a call, or a steering row that was drained. Anything the caller
    /// appends afterwards has to hang off this one; hanging it off the assistant
    /// row opens a branch and pushes whatever came after it off the path, which
    /// the reader sees as an answer disappearing.
    pub final_cursor: Option<String>,
    /// A durable plan review was committed. The exit tool intentionally has no
    /// result row yet; a later explicit decision continues that pending call.
    pub waiting_review: Option<String>,
}

/// A turn's reply, plus what the caller needs to close it out.
pub struct TurnOutcome {
    pub reply: Result<String, String>,
    pub progress: TurnProgress,
}

impl TurnOutcome {
    /// A turn that never got as far as asking anything. Its progress really is
    /// nothing, which is a different thing from a turn that failed partway —
    /// and why a caller is handed progress either way.
    pub fn failed(error: String) -> Self {
        Self {
            reply: Err(error),
            progress: TurnProgress::default(),
        }
    }

    /// What to tell first-party clients this turn ended as.
    pub fn chat_stop_reason(&self) -> ChatStopReason {
        if self.reply.is_err() {
            ChatStopReason::Error
        } else if self.progress.aborted {
            ChatStopReason::LoopDetected
        } else if self.progress.refused {
            ChatStopReason::Refusal
        } else if self.progress.truncated {
            ChatStopReason::MaxTokens
        } else {
            ChatStopReason::EndTurn
        }
    }

    /// The same reason for transports whose protocol still carries strings.
    pub fn stop_reason(&self) -> &'static str {
        self.chat_stop_reason().as_str()
    }
}

/// Run one turn to its end.
///
/// Never propagates with `?`. The body does, and this wraps it, because the
/// progress a failed turn made is exactly what the caller needs to report it —
/// and a caller that had to write `let outcome = run_turn(..).await?` would
/// throw that away at the only moment it matters.
pub async fn run_turn(services: &TurnServices<'_>, setup: TurnSetup<'_>, ports: TurnPorts<'_>) -> TurnOutcome {
    let mut progress = TurnProgress::default();
    let reply = run(services, setup, ports, &mut progress).await;
    TurnOutcome { reply, progress }
}

async fn run(
    services: &TurnServices<'_>,
    setup: TurnSetup<'_>,
    ports: TurnPorts<'_>,
    progress: &mut TurnProgress,
) -> Result<String, String> {
    let TurnSetup {
        provider,
        params,
        mut chat_messages,
        mut tool_defs,
        mut offered,
        mut mode,
        tool_context,
        mut budget,
        turn_id,
        conversation_id,
        provider_id,
        provider_name,
        mut parent_cursor,
        cancel,
        keep_recent,
        context_limit,
        approval_rule,
        withheld,
        files_root,
        mut interrupted,
        compaction,
        pricing,
    } = setup;
    let pool = services.pool;
    let emit = ports.emit;

    // Two ways to send, and the difference is the whole reason `Emit` returns a
    // `Result`. A window that missed a chunk is showing a transcript that never
    // catches up, so the desktop's adapter reports the failure and the turn ends
    // on it; OneBot's answer travels by another road entirely and its adapter
    // never fails. `reset` is best-effort on both sides and always was.
    let announce = |event: ChatStreamEvent| -> Result<(), String> {
        match emit {
            Some(e) => e.emit_chat(event),
            None => Ok(()),
        }
    };
    let whisper_chat = |event: ChatStreamEvent| {
        if let Some(e) = emit {
            let _ = e.emit_chat(event);
        }
    };
    let whisper_conversation_updated = || {
        if let Some(e) = emit {
            let _ = e.emit_conversation_updated(&conversation_id);
        }
    };

    let mut last_assistant_text = String::new();
    let mut loop_guard = crate::agent::ToolLoopGuard::default();
    let mut turn_aborted = false;
    // How many times a finished answer has been reopened by something that
    // arrived while it was being written. Bounded because otherwise anyone
    // holding down Enter keeps a turn alive indefinitely, and the loop guard
    // does not cover this: it counts tool calls, and these rounds have none.
    let mut continuations: usize = 0;
    let mut pauses: usize = 0;

    // What the reply may cost, before it is trimmed to what each request has
    // left. Held apart from `params` because the trimming is per request and
    // must never compound: taking it from the previous request's already
    // reduced value would ratchet the ceiling down round after round.
    let configured_reply = params.max_tokens.filter(|m| *m > 0).map(|m| m as usize);
    loop {
        if cancel.is_cancelled() {
            break;
        }

        // Trim old, large tool results before estimating tokens. Cheaper than
        // compaction and runs every round, so the budget sees a tighter history
        // and compaction fires less often.
        super::pruning::prune_tool_results(&mut chat_messages, &super::pruning::PruningConfig::default());

        // A caller may already have trimmed the initial history, but request
        // safety cannot depend on that. Re-apply the user-context aggregate cap
        // at the provider boundary on every round, including low-pressure
        // histories that never enter compaction, then estimate exactly what is
        // about to be sent.
        crate::agent::context::cap_user_provided_context(&mut chat_messages, context_limit);
        budget.update_estimate(&chat_messages);

        // A prompt that already fills the window has nowhere to put an answer,
        // and no output ceiling makes it servable. Sending it anyway buys one
        // refusal and lands in the recovery below, so take the recovery
        // directly -- it is the same pass, minus a request that could only
        // fail. Unlike the threshold pass this is not a preference: the
        // alternative is a turn that cannot continue at all.
        if budget.room_for_reply() < MIN_REPLY_TOKENS {
            compaction
                .on_overflow(Compacting {
                    messages: &mut chat_messages,
                    budget: &mut budget,
                    provider,
                    params: &params,
                    keep_recent,
                    context_limit,
                    emit,
                    conversation_id: &conversation_id,
                })
                .await;
            if budget.room_for_reply() < MIN_REPLY_TOKENS {
                return Err(no_room(&budget));
            }
        }

        let assistant_msg_id = begin_assistant(
            pool,
            &conversation_id,
            &turn_id,
            (provider_id.as_deref(), provider_name.as_deref()),
            &params.model,
            parent_cursor.as_deref(),
        )
        .await?;
        parent_cursor = Some(assistant_msg_id.clone());
        // Recorded before the event, and whether or not anyone is watching: the
        // caller closes the turn out with it even when nothing was attached.
        progress.message_id = Some(assistant_msg_id.clone());
        // One per row, which is the same thing a reader counting assistant rows
        // afterwards arrives at. Two ways of asking "how long did this take"
        // that disagreed would be worse than either.
        progress.steps += 1;
        announce(ChatStreamEvent::MessageStart {
            message_id: assistant_msg_id.clone(),
            turn_id: turn_id.clone(),
            conversation_id: conversation_id.clone(),
        })?;

        let result = {
            let mut attempt = 0u32;
            let mut retry_delay: Option<std::time::Duration> = None;
            loop {
                if attempt > 0 {
                    let delay = retry_delay
                        .take()
                        .unwrap_or_else(|| crate::client::backoff(STREAM_RETRY_BASE, attempt as u64));
                    // Before the wait, not after. The backoff is the part anyone
                    // watching actually sits through, and a turn that says
                    // nothing for it is indistinguishable from one that has hung.
                    //
                    // How many and how long, but not what went wrong: a
                    // provider's error body can echo the request back, and this
                    // goes to a window.
                    whisper_chat(ChatStreamEvent::Retry {
                        attempt,
                        max_attempts: MAX_STREAM_RETRIES,
                        delay_ms: delay.as_millis() as u64,
                        message_id: assistant_msg_id.clone(),
                        conversation_id: conversation_id.clone(),
                    });
                    tokio::time::sleep(delay).await;
                    // A retry replays the whole stream under the same message
                    // id; tell any window to drop what it already appended.
                    whisper_chat(ChatStreamEvent::Reset {
                        message_id: assistant_msg_id.clone(),
                        conversation_id: conversation_id.clone(),
                    });
                }
                // Asked for against what this prompt has left rather than
                // against the model's advertised maximum. Most providers count
                // the prompt and `max_tokens` against one window, so a long
                // conversation plus a full-size output allowance is a request
                // they have to refuse — while the conversation itself would
                // have fitted perfectly well.
                let opened = provider
                    .stream_chat_with_tools(
                        chat_messages.clone(),
                        tool_defs.clone(),
                        ChatParams {
                            max_tokens: budget.reply_ceiling(configured_reply).map(|c| c as i32),
                            ..params.clone()
                        },
                    )
                    .await;
                let read = match opened {
                    Ok(stream) => consume_stream(stream, &cancel, emit, &assistant_msg_id, &conversation_id).await,
                    Err(e) => Err(e.to_string()),
                };
                match read {
                    Ok(r) => {
                        // A reply the model finished producing is the only proof
                        // it received what the request carried, and `Ok` alone
                        // does not say that: a provider will answer 200 and then
                        // refuse over SSE, and `Ok` also covers a stream the user
                        // cancelled two hundred milliseconds in. Either would
                        // retire the warning that a tool may be half-run in
                        // favour of a request nothing read. Repeating it costs a
                        // paragraph; losing it costs the safety of whatever the
                        // model does next.
                        //
                        // Taken rather than read, so it is settled once — the
                        // block itself is already baked into `chat_messages` and
                        // rides along with every later iteration.
                        if r.ran_to_completion
                            && let Some(report) = interrupted.take()
                        {
                            crate::agent::interrupted::confirm_delivered(pool, report).await;
                        }
                        break r;
                    }
                    Err(e) if is_context_window_error(&e) => {
                        compaction
                            .on_overflow(Compacting {
                                messages: &mut chat_messages,
                                budget: &mut budget,
                                provider,
                                params: &params,
                                keep_recent,
                                context_limit,
                                emit,
                                conversation_id: &conversation_id,
                            })
                            .await;
                        crate::agent::context::cap_user_provided_context(&mut chat_messages, context_limit);
                        budget.update_estimate(&chat_messages);
                        whisper_chat(ChatStreamEvent::Reset {
                            message_id: assistant_msg_id.clone(),
                            conversation_id: conversation_id.clone(),
                        });
                        // There is one recovery attempt, so a second request that
                        // cannot be served spends it on nothing. Said plainly
                        // here rather than passed on as whatever the provider
                        // calls an empty output allowance.
                        if budget.room_for_reply() < MIN_REPLY_TOKENS {
                            return Err(format!("Context overflow recovery failed: {}", no_room(&budget)));
                        }
                        // Recomputed, not reused: the recovery above is what just
                        // made room, and asking with the pre-compaction ceiling
                        // would waste it.
                        let stream = provider
                            .stream_chat_with_tools(
                                chat_messages.clone(),
                                tool_defs.clone(),
                                ChatParams {
                                    max_tokens: budget.reply_ceiling(configured_reply).map(|c| c as i32),
                                    ..params.clone()
                                },
                            )
                            .await
                            .map_err(|e| format!("Context overflow recovery failed: {e}"))?;
                        let recovered = consume_stream(stream, &cancel, emit, &assistant_msg_id, &conversation_id)
                            .await
                            .map_err(|e| format!("Context overflow recovery failed: {e}"))?;
                        // The same rule, and reachable without the request above
                        // ever having succeeded: a provider that refuses an
                        // oversized request outright makes this the first stream
                        // anyone reads to the end.
                        if recovered.ran_to_completion
                            && let Some(report) = interrupted.take()
                        {
                            crate::agent::interrupted::confirm_delivered(pool, report).await;
                        }
                        break recovered;
                    }
                    Err(e) if is_retryable_stream_error(&e) && attempt < MAX_STREAM_RETRIES => {
                        tracing::warn!(error = %e, attempt, "request failed, retrying");
                        retry_delay = crate::agent::parse_retry_after(&e);
                        attempt += 1;
                        continue;
                    }
                    // Logged once by the caller, together with every other way a
                    // turn can end early.
                    Err(e) => return Err(e),
                }
            }
        };

        if let Some(ref u) = result.usage {
            progress.input_tokens += u.prompt_tokens.unwrap_or(0);
            progress.output_tokens += u.completion_tokens.unwrap_or(0);
            // Added, not replaced, for the same reason as the two above: a turn
            // is however many requests it took, and the round that reused a
            // cached prefix and the round that had to rebuild it are both part
            // of what it cost. Subsets of `input_tokens`, so a caller wanting
            // "tokens paid for at full price" subtracts them rather than adds.
            progress.cache_read_tokens += u.cache_read_tokens.unwrap_or(0);
            progress.cache_write_tokens += u.cache_write_tokens.unwrap_or(0);
            // Priced here, per round, because this is the last place the size of
            // *this* request is known. Everything downstream has only the sums,
            // and on a model with tiered rates the sum sits in a bracket no
            // single request necessarily reached.
            if let Some(ref pricing) = pricing {
                let prices = pricing.for_prompt(u.prompt_tokens.unwrap_or(0) as i64);
                *progress.cost.get_or_insert_default() += crate::agent::pricing::compute_cost(u, &prices);
            }
            budget.calibrate_from_usage(u);
        }

        // A reply cut off at the length limit may hold half a call. Treating it
        // as a request would send arguments the model never finished writing.
        let has_tool_calls = !result.tool_calls.is_empty()
            && !matches!(result.finish_reason.as_deref(), Some("length") | Some("max_tokens"));
        // Per round, so the last one decides: a turn that was cut short in an
        // early round and then answered in full is not a truncated turn.
        progress.truncated = matches!(result.finish_reason.as_deref(), Some("length") | Some("max_tokens"));
        progress.refused = matches!(
            result.finish_reason.as_deref(),
            Some("refusal") | Some("content_filter")
        );

        // The stored copy obeys the strict read contract the context rebuild
        // parses it under. A stream cut short mid-argument — which neither
        // finish_reason above catches, because the upstream stopped for another
        // reason or stopped talking entirely — would otherwise be persisted as
        // a fragment and fail every later rebuild of this conversation. The
        // dispatch below still sees the original arguments, so the model gets
        // the ordinary invalid-arguments answer it already recovers from by
        // retrying; only what is written down is normalised.
        let storable_calls: Vec<crate::provider::ToolCall> = result
            .tool_calls
            .iter()
            .map(|call| {
                let parseable = serde_json::from_str::<serde_json::Value>(&call.arguments)
                    .map(|v| v.is_object())
                    .unwrap_or(false);
                if parseable {
                    call.clone()
                } else {
                    tracing::warn!(
                        tool = %call.name,
                        call_id = %call.id,
                        "a tool call's arguments never arrived intact; the stored copy is empty"
                    );
                    crate::provider::ToolCall {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: "{}".to_string(),
                    }
                }
            })
            .collect();
        let tool_calls_json = has_tool_calls.then(|| serialize_tool_calls_openai(&storable_calls));
        let provider_state_json = result
            .provider_state
            .as_ref()
            .map(|state| state.to_storage_json())
            .transpose()?;
        complete_assistant(
            pool,
            &assistant_msg_id,
            &result.text,
            (!result.reasoning.is_empty()).then_some(result.reasoning.as_str()),
            tool_calls_json.as_deref(),
            provider_state_json.as_deref(),
            row_usage(result.usage.as_ref()),
        )
        .await?;

        last_assistant_text = result.text.clone();

        // `pause_turn`: the upstream stopped while one of its own tools was
        // running long, and wants the same request back with this round's
        // blocks appended so it can carry on. Not a tool call — nothing here
        // runs — and not an ending: the reader has no answer yet. The blocks
        // travel in `provider_state`, which is what the adapter replays.
        if !has_tool_calls && result.finish_reason.as_deref() == Some("pause_turn") && pauses < MAX_PAUSE_CONTINUATIONS
        {
            pauses += 1;
            let mut assistant = ChatMessage::assistant(&result.text);
            assistant.provider_state = result.provider_state.clone();
            chat_messages.push(assistant);
            continue;
        }

        if !has_tool_calls {
            // The answer is written. Anything typed while it was being written
            // is still in the inbox, and the drain at the bottom of this loop is
            // past the tool dispatch — unreachable from here. Without this, a
            // message sent during the last few seconds of a reply is accepted,
            // acknowledged, and then never read by anyone.
            //
            // The cap is not a drain: reaching it must leave the messages where
            // they are, so `close()` can hand them back and the caller can say
            // what happened to them. Draining and then stopping would make them
            // vanish.
            let steered = match ports.steering {
                Some(s) if continuations < MAX_TAIL_CONTINUATIONS => s.drain().await,
                _ => Vec::new(),
            };
            if steered.is_empty() {
                break;
            }
            continuations += 1;
            // Before the steering, and this is the whole reason the tail case
            // cannot just reuse the code below: the push that puts a reply in
            // front of the next request lives in the tool branch. Skipping it
            // would have the model answer as though it had said nothing.
            let mut assistant = ChatMessage::assistant(&result.text);
            assistant.provider_state = result.provider_state.clone();
            chat_messages.push(assistant);
            inject_steering(
                pool,
                &conversation_id,
                &turn_id,
                steered,
                &mut parent_cursor,
                &mut chat_messages,
                files_root.as_deref(),
            )
            .await?;
            narrow_offered(&mut offered, ports.steering);
            whisper_conversation_updated();
            continue;
        }

        // Only the final iteration's text is returned to the caller, so on a
        // runner that does not stream, everything said on the way to a tool call
        // would otherwise exist solely in the database.
        if !result.text.is_empty()
            && let Some(interim) = ports.interim
        {
            interim.say(result.text.clone()).await;
        }

        let mut assistant_msg = ChatMessage::assistant_with_tools(
            &result.text,
            (!result.reasoning.is_empty()).then(|| result.reasoning.clone()),
            result.tool_calls.clone(),
        );
        assistant_msg.provider_state = result.provider_state.clone();
        chat_messages.push(assistant_msg);

        let ordered_calls = plan_batch_order(&result.tool_calls);
        let segments = super::parallel::partition_tool_calls(
            &ordered_calls,
            services.tools,
            &offered,
            &tool_context,
            &approval_rule,
            ports.surface_tools,
            mode,
            ports.sub_agents,
        );
        let mut plan_update_failed = false;
        for segment in &segments {
            if cancel.is_cancelled() || turn_aborted {
                break;
            }

            if let super::parallel::Segment::Parallel(parallel_calls) = segment {
                // --- Parallel dispatch: concurrent execution of read-only, no-approval tools ---

                // Pre-scan loop guard in wire order. If any call triggers a
                // warning or abort, fall back to serial for the remaining calls
                // in this segment.
                let mut parallel_ok = Vec::new();
                let mut fallback_serial = Vec::new();
                let mut hit_guard = false;
                for &tc in parallel_calls {
                    if hit_guard {
                        fallback_serial.push(tc);
                    } else {
                        let verdict = loop_guard.observe(&tc.name, &tc.arguments);
                        match verdict {
                            crate::agent::LoopVerdict::Proceed => parallel_ok.push(tc),
                            _ => {
                                hit_guard = true;
                                fallback_serial.push(tc);
                            }
                        }
                    }
                }

                if !parallel_ok.is_empty() {
                    // Emit all ToolCall events upfront.
                    for tc in &parallel_ok {
                        announce(ChatStreamEvent::ToolCall {
                            call_id: tc.id.clone(),
                            tool_name: tc.name.clone(),
                            arguments: tc.arguments.clone(),
                            message_id: assistant_msg_id.clone(),
                            conversation_id: conversation_id.clone(),
                        })?;
                    }

                    // Phase: mark as running tool for crash safety.
                    crate::agent::turn_record::note_phase(pool, &turn_id, TurnPhase::RunningTool, None).await;

                    // Spawn all calls concurrently. Each future resolves to
                    // (output, outcome) — the same shape the serial path produces.
                    let mut in_flight = futures::stream::FuturesOrdered::new();
                    for tc in &parallel_ok {
                        let cancel_inner = cancel.clone();

                        if tc.name == crate::agent::sub_agents::RUN_AGENT_TOOL {
                            // Sub-agent dispatch.
                            let msg_id = assistant_msg_id.clone();
                            let call_id = tc.id.clone();
                            match parse_sub_agent(&tc.arguments, &msg_id, &call_id) {
                                Err(e) => {
                                    in_flight
                                        .push_back(futures::future::ready((format!("Error: {e}"), "error")).boxed());
                                }
                                Ok(spec) => {
                                    let sub = ports.sub_agents.expect("partition guarantees sub_agents port");
                                    in_flight.push_back(
                                        async move {
                                            if cancel_inner.is_cancelled() {
                                                return ("Cancelled".to_string(), "error");
                                            }
                                            match sub.run(spec).await {
                                                Ok(report) => {
                                                    let outcome = report.status.outcome();
                                                    (sub_agent_result(&report), outcome)
                                                }
                                                Err(e) => (format!("Error: {e}"), "error"),
                                            }
                                        }
                                        .boxed(),
                                    );
                                }
                            }
                        } else {
                            // Registry tool dispatch.
                            let tool = services
                                .tools
                                .get(&tc.name)
                                .expect("partition guarantees registry tool");
                            let args = match parse_tool_arguments(&tc.arguments) {
                                Ok(a) => a,
                                Err(e) => {
                                    in_flight
                                        .push_back(futures::future::ready((format!("Error: {e}"), "error")).boxed());
                                    continue;
                                }
                            };
                            let ctx = tool_context.clone();
                            in_flight.push_back(
                                async move {
                                    if cancel_inner.is_cancelled() {
                                        return ("Cancelled".to_string(), "error");
                                    }
                                    match tool.execute(args, &ctx).await {
                                        Ok(o) => (o, "success"),
                                        Err(e) => (format!("Error: {e}"), "error"),
                                    }
                                }
                                .boxed(),
                            );
                        }
                    }

                    // Collect results in wire order.
                    use futures::StreamExt;
                    let mut results: Vec<(String, &str)> = Vec::with_capacity(parallel_ok.len());
                    while let Some(res) = in_flight.next().await {
                        results.push(res);
                    }

                    // Restore phase.
                    crate::agent::turn_record::note_phase(pool, &turn_id, TurnPhase::Streaming, None).await;

                    // Post-process each result sequentially in wire order.
                    for (tc, (output, outcome)) in parallel_ok.iter().zip(results) {
                        let output =
                            crate::agent::formatted_truncate_text(&output, crate::agent::TOOL_OUTPUT_TRUNCATION);

                        let event_outcome = match outcome {
                            "success" => ToolOutcome::Success,
                            _ => ToolOutcome::Error,
                        };
                        announce(ChatStreamEvent::ToolResult {
                            call_id: tc.id.clone(),
                            result: output.clone(),
                            outcome: event_outcome,
                            message_id: assistant_msg_id.clone(),
                            conversation_id: conversation_id.clone(),
                        })?;

                        if let Some(id) = append_tool_result(
                            pool,
                            &conversation_id,
                            &turn_id,
                            &tc.id,
                            &output,
                            outcome,
                            parent_cursor.as_deref(),
                        )
                        .await
                        {
                            parent_cursor = Some(id);
                        }

                        chat_messages.push(match event_outcome {
                            ToolOutcome::Success => ChatMessage::tool_result(&tc.id, &output),
                            ToolOutcome::Denied | ToolOutcome::Error => ChatMessage::tool_error(&tc.id, &output),
                        });
                    }
                }

                // Fall-through: any calls that hit the loop guard are handled
                // serially below, inline with the Serial path.
                for tc in fallback_serial {
                    if cancel.is_cancelled() || turn_aborted {
                        break;
                    }
                    // Reuse the serial dispatch path (duplicated from the
                    // Serial branch below to avoid an extraction that would
                    // touch 400 lines of parameters). The loop guard already
                    // observed these calls above, so re-observe here to get
                    // the verdict back.
                    let verdict = loop_guard.observe(&tc.name, &tc.arguments);

                    announce(ChatStreamEvent::ToolCall {
                        call_id: tc.id.clone(),
                        tool_name: tc.name.clone(),
                        arguments: tc.arguments.clone(),
                        message_id: assistant_msg_id.clone(),
                        conversation_id: conversation_id.clone(),
                    })?;

                    let (output, outcome): (String, &'static str) = if let crate::agent::LoopVerdict::Warn(n) = verdict
                    {
                        (crate::agent::loop_warning_message(&tc.name, n), "error")
                    } else if let crate::agent::LoopVerdict::Abort(n) = verdict {
                        turn_aborted = true;
                        (crate::agent::loop_abort_message(&tc.name, n), "error")
                    } else {
                        unreachable!("fallback_serial only contains guard-hit calls");
                    };

                    let output = crate::agent::formatted_truncate_text(&output, crate::agent::TOOL_OUTPUT_TRUNCATION);

                    let event_outcome = match outcome {
                        "success" => ToolOutcome::Success,
                        "denied" => ToolOutcome::Denied,
                        "error" => ToolOutcome::Error,
                        other => return Err(format!("unknown tool outcome `{other}`")),
                    };
                    announce(ChatStreamEvent::ToolResult {
                        call_id: tc.id.clone(),
                        result: output.clone(),
                        outcome: event_outcome,
                        message_id: assistant_msg_id.clone(),
                        conversation_id: conversation_id.clone(),
                    })?;

                    if let Some(id) = append_tool_result(
                        pool,
                        &conversation_id,
                        &turn_id,
                        &tc.id,
                        &output,
                        outcome,
                        parent_cursor.as_deref(),
                    )
                    .await
                    {
                        parent_cursor = Some(id);
                    }

                    chat_messages.push(match event_outcome {
                        ToolOutcome::Success => ChatMessage::tool_result(&tc.id, &output),
                        ToolOutcome::Denied | ToolOutcome::Error => ChatMessage::tool_error(&tc.id, &output),
                    });

                    if turn_aborted {
                        break;
                    }
                }

                continue;
            }

            // --- Serial dispatch: the original single-call path ---
            let tc = match segment {
                super::parallel::Segment::Serial(tc) => tc,
                super::parallel::Segment::Parallel(_) => unreachable!(),
            };

            announce(ChatStreamEvent::ToolCall {
                call_id: tc.id.clone(),
                tool_name: tc.name.clone(),
                arguments: tc.arguments.clone(),
                message_id: assistant_msg_id.clone(),
                conversation_id: conversation_id.clone(),
            })?;

            let allowed = offered.contains(&tc.name);
            let is_mcp = tc.name.starts_with("mcp__");
            let surface = ports.surface_tools.filter(|s| s.owns(&tc.name));
            let tool = if allowed && !is_mcp && surface.is_none() {
                services.tools.get(&tc.name)
            } else {
                None
            };
            // Ahead of any approval, so a stuck model cannot spend a person's
            // attention on the same dialog forty times.
            let verdict = loop_guard.observe(&tc.name, &tc.arguments);

            let (output, outcome): (String, &'static str) = if let crate::agent::LoopVerdict::Warn(n) = verdict {
                (crate::agent::loop_warning_message(&tc.name, n), "error")
            } else if let crate::agent::LoopVerdict::Abort(n) = verdict {
                turn_aborted = true;
                (crate::agent::loop_abort_message(&tc.name, n), "error")
            } else if !allowed {
                (withheld.say(&tc.name), "error")
            } else if let Some(surface) = surface {
                // Read-only query tools are scope-locked and go straight
                // through; the ones that change a group ask first.
                let decision = if surface.requires_approval(&tc.name) {
                    ports.approvals.ask(&assistant_msg_id, tc, None).await?
                } else {
                    Some(ApprovalDecision::Approved)
                };
                match decision {
                    Some(ApprovalDecision::Approved) => {
                        let ran = in_phase(
                            pool,
                            &turn_id,
                            TurnPhase::RunningTool,
                            Some(&tc.name),
                            surface.execute(&tc.name, &tc.arguments),
                        )
                        .await;
                        match ran {
                            Ok(o) => (o, "success"),
                            Err(e) => (format!("Error: {e}"), "error"),
                        }
                    }
                    Some(ApprovalDecision::Denied(Some(reason))) => {
                        (format!("Tool call denied by user. Reason: {reason}"), "denied")
                    }
                    Some(ApprovalDecision::Denied(None)) => ("Tool call denied by user.".to_string(), "denied"),
                    // Nobody answered — not a denial. See `UNANSWERED_APPROVAL`.
                    _ => (UNANSWERED_APPROVAL.to_string(), "denied"),
                }
            } else if tc.name == "ask_user" {
                match ports.approvals.ask(&assistant_msg_id, tc, None).await? {
                    Some(ApprovalDecision::Response(text)) => (text, "success"),
                    _ => ("User did not respond.".to_string(), "denied"),
                }
            } else if let (crate::agent::sub_agents::RUN_AGENT_TOOL, Some(sub_agents)) =
                (tc.name.as_str(), ports.sub_agents)
            {
                // The phase is the parent's: for as long as the sub-agent
                // runs, this turn is running a tool called `run_agent`. A
                // crash here reads as "that call may have half-happened",
                // which is exactly what it means.
                match parse_sub_agent(&tc.arguments, &assistant_msg_id, &tc.id) {
                    Err(e) => (format!("Error: {e}"), "error"),
                    Ok(spec) => {
                        let ran = in_phase(
                            pool,
                            &turn_id,
                            TurnPhase::RunningTool,
                            Some(&tc.name),
                            sub_agents.run(spec),
                        )
                        .await;
                        match ran {
                            Ok(report) => {
                                let outcome = report.status.outcome();
                                (sub_agent_result(&report), outcome)
                            }
                            Err(e) => (format!("Error: {e}"), "error"),
                        }
                    }
                }
            } else if let Some(transition_ports) = ports.transitions
                && mode.id == crate::agent::modes::PLAN_MODE
                && tc.name == crate::agent::modes::READ_PLAN_TOOL
            {
                match transitions::parse_empty_plan_arguments(&tc.name, &tc.arguments) {
                    Err(error) => (format!("Error: {error}"), "error"),
                    Ok(()) => match in_phase(
                        pool,
                        &turn_id,
                        TurnPhase::RunningTool,
                        Some(&tc.name),
                        transition_ports.read_plan(),
                    )
                    .await
                    {
                        Ok(result) => match serde_json::to_string(&result) {
                            Ok(output) => (output, "success"),
                            Err(error) => (format!("Error: could not encode read_plan result: {error}"), "error"),
                        },
                        Err(error) => (format!("Error: {error}"), "error"),
                    },
                }
            } else if let Some(transition_ports) = ports.transitions
                && mode.id == crate::agent::modes::PLAN_MODE
                && tc.name == crate::agent::modes::UPDATE_PLAN_TOOL
            {
                match transitions::parse_update_plan_arguments(&tc.arguments, &assistant_msg_id, &tc.id) {
                    Err(error) => (format!("Error: {error}"), "error"),
                    Ok(request) => match in_phase(
                        pool,
                        &turn_id,
                        TurnPhase::RunningTool,
                        Some(&tc.name),
                        transition_ports.update_plan(request),
                    )
                    .await
                    {
                        Ok(result) => match serde_json::to_string(&result) {
                            Ok(output) => (output, "success"),
                            Err(error) => (format!("Error: could not encode update_plan result: {error}"), "error"),
                        },
                        Err(error) => (format!("Error: {error}"), "error"),
                    },
                }
            } else if let Some(target) = ports
                .transitions
                .and_then(|_| crate::agent::modes::by_enter_tool(&tc.name))
            {
                // Guarded on the port, and the tool set is too: a runner
                // with no transitions is offered no transition tool
                // (`Modes::Fixed`), so the model has no way to name one and
                // this is the only path that ever reaches one. The guard
                // stays because the two are decided in different places, and
                // falling past it lands on the registry tool, whose refusal
                // to be called outside the loop would be read as a result.
                let decision = ports.approvals.ask(&assistant_msg_id, tc, None).await?;
                transitions::enter(
                    pool,
                    ports.transitions.expect("guarded above"),
                    emit,
                    &conversation_id,
                    target,
                    decision,
                )
                .await?
                .apply(&mut mode, &mut chat_messages, &mut tool_defs, &mut offered)
            } else if let Some(transition_ports) = ports.transitions
                && mode.exit_tool == Some(tc.name.as_str())
            {
                if plan_update_failed {
                    (
                        "Error: exit_plan was not submitted because an update_plan in the same batch failed. \
                         Read the current plan, apply one corrected patch, and call exit_plan again."
                            .into(),
                        "error",
                    )
                } else {
                    let request = transitions::SubmitPlanRequest {
                        turn_id: turn_id.clone(),
                        assistant_message_id: assistant_msg_id.clone(),
                        provider_call_id: tc.id.clone(),
                    };
                    match in_phase(
                        pool,
                        &turn_id,
                        TurnPhase::RunningTool,
                        Some(&tc.name),
                        transitions::submit(transition_ports, &tc.arguments, request),
                    )
                    .await
                    {
                        Ok(event) => {
                            // Durable state first. An event is only an
                            // invalidation hint and a failing sink must not
                            // turn a committed WaitingReview boundary into a
                            // failed turn that can never be decided.
                            progress.waiting_review = Some(event.review_id.clone());
                            if let Some(emitter) = emit
                                && let Err(error) = emitter.emit_plan_review_requested(event.clone())
                            {
                                tracing::warn!(%error, review_id = %event.review_id, "could not publish durable plan-review request");
                            }
                            break;
                        }
                        Err(error) => (format!("Error: {error}"), "error"),
                    }
                }
            } else if is_mcp {
                match parse_tool_arguments(&tc.arguments) {
                    Err(error) => (format!("Error: {error}"), "error"),
                    Ok(args) => {
                        // External tools ask, always. They are the one class the
                        // authorizer knows nothing about.
                        match ports.approvals.ask(&assistant_msg_id, tc, None).await? {
                            Some(ApprovalDecision::Approved) => {
                                // Awaited with nothing locked: the registry hands
                                // back a handle and the call runs outside it.
                                let called = in_phase(
                                    pool,
                                    &turn_id,
                                    TurnPhase::RunningTool,
                                    Some(&tc.name),
                                    services.mcp.call_tool(&tc.name, args),
                                )
                                .await;
                                match called {
                                    Ok(o) => (o, "success"),
                                    Err(e) => (format!("MCP error: {e}"), "error"),
                                }
                            }
                            Some(ApprovalDecision::Denied(Some(reason))) => {
                                (format!("Tool call denied by user. Reason: {reason}"), "denied")
                            }
                            Some(ApprovalDecision::Denied(None)) => ("Tool call denied by user.".to_string(), "denied"),
                            // Nobody answered, which the port's contract says is not a
                            // denial — see `UNANSWERED_APPROVAL`.
                            _ => (UNANSWERED_APPROVAL.to_string(), "denied"),
                        }
                    }
                }
            } else if let Some(tool) = tool {
                match parse_tool_arguments(&tc.arguments) {
                    Err(error) => (format!("Error: {error}"), "error"),
                    Ok(args) => {
                        let permission = tool.default_permission();
                        let must_ask = match &approval_rule {
                            // `reach` is advisory: it decides whether to prompt, not
                            // what the tool may touch. `tools::verified` enforces
                            // that against the handle when the I/O happens.
                            ApprovalRule::ByReach { accept_edits } => tools::reach::needs_approval(
                                permission,
                                tool.reach(&args, &tool_context),
                                *accept_edits,
                            ),
                            ApprovalRule::ByPermission => permission == tools::Permission::Ask,
                        };
                        // Three answers, not two. `None` is nobody answering — a card
                        // that expired, or a turn that outlived its question — and the
                        // `Approvals` contract says that is never a denial. Telling the
                        // model "denied by user" there attributes a decision to a
                        // person who made none, and the model acts on it: apologising
                        // for something nobody objected to, or not asking again when
                        // asking again is exactly what the user would want.
                        enum Authorised {
                            Yes,
                            Refused(Option<String>),
                            Unanswered,
                        }
                        let authorised = if permission == tools::Permission::Never {
                            Authorised::Refused(None)
                        } else if !must_ask {
                            Authorised::Yes
                        } else {
                            match ports.approvals.ask(&assistant_msg_id, tc, None).await? {
                                Some(ApprovalDecision::Approved) => Authorised::Yes,
                                Some(ApprovalDecision::Denied(reason)) => Authorised::Refused(reason),
                                // Typed words are an answer to a question, and
                                // only `ask_user` asked one. Reaching here with
                                // some means the card was answered by something
                                // that had no permission to grant, so it is not
                                // one — and it is not a user's refusal either.
                                Some(ApprovalDecision::Response(_)) | None => Authorised::Unanswered,
                            }
                        };
                        match authorised {
                            Authorised::Refused(Some(reason)) => {
                                (format!("Tool call denied by user. Reason: {reason}"), "denied")
                            }
                            Authorised::Refused(None) => ("Tool call denied by user.".to_string(), "denied"),
                            Authorised::Unanswered => (UNANSWERED_APPROVAL.to_string(), "denied"),
                            Authorised::Yes => {
                                // The one phase that describes something outside the
                                // database. A turn found dead here may already have
                                // written the file or run the command.
                                let executed = in_phase(
                                    pool,
                                    &turn_id,
                                    TurnPhase::RunningTool,
                                    Some(&tc.name),
                                    tool.execute(args.clone(), &tool_context),
                                )
                                .await;
                                match executed {
                                    Ok(o) => (o, "success"),
                                    Err(e) => match tools::decode_sandbox_denied(&e) {
                                        None => (format!("Error: {e}"), "error"),
                                        Some(blocked) => {
                                            // The same call under the same id: it is the
                                            // approval that is new, and that has an
                                            // identity of its own.
                                            let retry =
                                                ports.approvals.ask(&assistant_msg_id, tc, Some(blocked)).await?;
                                            if matches!(retry, Some(ApprovalDecision::Approved)) {
                                                let escalated = tool_context.without_sandbox();
                                                let retried = in_phase(
                                                    pool,
                                                    &turn_id,
                                                    TurnPhase::RunningTool,
                                                    Some(&tc.name),
                                                    tool.execute(args, &escalated),
                                                )
                                                .await;
                                                match retried {
                                                    Ok(o) => (o, "success"),
                                                    Err(e2) => (format!("Error: {e2}"), "error"),
                                                }
                                            } else {
                                                (
                                                    format!(
                                                        "{blocked}\n[blocked by sandbox; user declined to retry without sandbox]"
                                                    ),
                                                    "denied",
                                                )
                                            }
                                        }
                                    },
                                }
                            }
                        }
                    }
                }
            } else {
                (format!("Unknown tool: {}", tc.name), "error")
            };

            if tc.name == crate::agent::modes::UPDATE_PLAN_TOOL && outcome != "success" {
                plan_update_failed = true;
            }

            let output = crate::agent::formatted_truncate_text(&output, crate::agent::TOOL_OUTPUT_TRUNCATION);

            let event_outcome = match outcome {
                "success" => ToolOutcome::Success,
                "denied" => ToolOutcome::Denied,
                "error" => ToolOutcome::Error,
                other => return Err(format!("unknown tool outcome `{other}`")),
            };
            announce(ChatStreamEvent::ToolResult {
                call_id: tc.id.clone(),
                result: output.clone(),
                outcome: event_outcome,
                message_id: assistant_msg_id.clone(),
                conversation_id: conversation_id.clone(),
            })?;

            // `None` leaves the cursor where it is: the tool already ran, so the
            // row is worth less than the turn, and the next write hangs off the
            // last one that did land.
            if let Some(id) = append_tool_result(
                pool,
                &conversation_id,
                &turn_id,
                &tc.id,
                &output,
                outcome,
                parent_cursor.as_deref(),
            )
            .await
            {
                parent_cursor = Some(id);
            }

            chat_messages.push(match event_outcome {
                ToolOutcome::Success => ChatMessage::tool_result(&tc.id, &output),
                ToolOutcome::Denied | ToolOutcome::Error => ChatMessage::tool_error(&tc.id, &output),
            });

            if turn_aborted {
                break;
            }
        }

        if cancel.is_cancelled() || turn_aborted || progress.waiting_review.is_some() {
            break;
        }

        // Between rounds, never inside one: a request must not be assembled from
        // a history something else is appending to. Injecting only appends, so
        // the prompt prefix the cache is keyed on stays exactly where it was.
        if let Some(steering) = ports.steering {
            let items = steering.drain().await;
            if !items.is_empty() {
                inject_steering(
                    pool,
                    &conversation_id,
                    &turn_id,
                    items,
                    &mut parent_cursor,
                    &mut chat_messages,
                    files_root.as_deref(),
                )
                .await?;
                narrow_offered(&mut offered, ports.steering);
                whisper_conversation_updated();
            }
        }

        compaction
            .between_rounds(Compacting {
                messages: &mut chat_messages,
                budget: &mut budget,
                provider,
                params: &params,
                keep_recent,
                context_limit,
                emit,
                conversation_id: &conversation_id,
            })
            .await;
    }

    progress.aborted = turn_aborted;
    progress.final_cursor = parent_cursor;
    Ok(last_assistant_text)
}

/// Write what arrived mid-turn, and put it in front of the model.
///
/// Both callers advance `parent_cursor` through it, which is what keeps the
/// rows on one path. A write that fails leaves the cursor alone and the message
/// still goes to the model: losing a row is worse than losing a turn, but not
/// worse than ignoring what somebody said.
/// Apply whatever the arrival of those messages did to this turn's authority.
///
/// Called after every injection, both of them, because either one can be the
/// point where somebody else joins. Intersects rather than assigns: a port that
/// answers may only take tools away, never hand back one the turn never had.
fn narrow_offered(offered: &mut HashSet<String>, steering: Option<&dyn Steering>) {
    let Some(narrowed) = steering.and_then(|s| s.narrowed()) else {
        return;
    };
    let before = offered.len();
    offered.retain(|name| narrowed.contains(name));
    if offered.len() != before {
        tracing::info!(
            withdrawn = before - offered.len(),
            "someone with less authority joined the turn; tools withdrawn for the rest of it"
        );
    }
}

async fn inject_steering(
    pool: &crate::db::DbPool,
    conversation_id: &str,
    turn_id: &str,
    items: Vec<Steered>,
    parent_cursor: &mut Option<String>,
    chat_messages: &mut Vec<ChatMessage>,
    files_root: Option<&std::path::Path>,
) -> Result<(), String> {
    for item in items {
        let sender = match &item.origin {
            SteeredOrigin::User(Some(s)) => Some(s.user_id),
            _ => None,
        };
        // A message that already has a row is not written a second time. Its id
        // still advances the cursor, because it is on the path either way and
        // the next row has to hang off it — see [`Steered::row`].
        let written = match &item.row {
            Some(id) => Some(id.clone()),
            None => {
                append_steering(
                    pool,
                    conversation_id,
                    turn_id,
                    &item.text,
                    sender,
                    parent_cursor.as_deref(),
                )
                .await
            }
        };
        if let Some(id) = written {
            *parent_cursor = Some(id);
        }
        // The turn's first resolve pass ran before this existed, so any image
        // parts inside it need their own.
        let mut injected = vec![match item.origin {
            SteeredOrigin::User(Some(s)) => ChatMessage::user_from(&item.text, s),
            // A person with no chat identity is still a person. Sending this as
            // context would have the model weigh it as ambient noise.
            SteeredOrigin::User(None) => ChatMessage::user(&item.text),
            SteeredOrigin::System => ChatMessage::system_context(&item.text),
        }];
        crate::agent::resolve_file_uris_in_messages(&mut injected, files_root)?;
        chat_messages.extend(injected);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::Emit;
    use super::super::ports::{Approvals, Steered, Steering, SurfaceTools};
    use super::*;
    use crate::agent::turn_config::TurnConfig;
    use crate::db::test_db;
    use crate::provider::{ProviderError, SenderRef, StreamEvent, TokenUsage, ToolCall};
    use crate::turn::TurnOrigin;
    use diesel::connection::SimpleConnection;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    // --- the model -------------------------------------------------------

    /// One request's worth of stream.
    fn says(text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::Text {
                content: text.to_string(),
            },
            StreamEvent::Stop {
                reason: "stop".into(),
                usage: None,
            },
        ]
    }

    fn calls(id: &str, name: &str, arguments: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolCallStart {
                index: 0,
                id: id.into(),
                name: name.into(),
            },
            StreamEvent::ToolCallDone {
                index: 0,
                arguments: arguments.into(),
            },
            StreamEvent::Stop {
                reason: "tool_calls".into(),
                usage: None,
            },
        ]
    }

    fn call_batch(calls: &[(&str, &str, &str)]) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        for (index, (id, name, arguments)) in calls.iter().enumerate() {
            events.push(StreamEvent::ToolCallStart {
                index,
                id: (*id).into(),
                name: (*name).into(),
            });
            events.push(StreamEvent::ToolCallDone {
                index,
                arguments: (*arguments).into(),
            });
        }
        events.push(StreamEvent::Stop {
            reason: "tool_calls".into(),
            usage: None,
        });
        events
    }

    #[test]
    fn plan_batch_barrier_runs_updates_before_an_earlier_exit() {
        let calls = vec![
            ToolCall {
                id: "exit".into(),
                name: crate::agent::modes::EXIT_PLAN_TOOL.into(),
                arguments: "{}".into(),
            },
            ToolCall {
                id: "read".into(),
                name: crate::agent::modes::READ_PLAN_TOOL.into(),
                arguments: "{}".into(),
            },
            ToolCall {
                id: "update-1".into(),
                name: crate::agent::modes::UPDATE_PLAN_TOOL.into(),
                arguments: "{}".into(),
            },
            ToolCall {
                id: "update-2".into(),
                name: crate::agent::modes::UPDATE_PLAN_TOOL.into(),
                arguments: "{}".into(),
            },
        ];

        let ordered: Vec<&str> = plan_batch_order(&calls)
            .into_iter()
            .map(|call| call.id.as_str())
            .collect();
        assert_eq!(ordered, ["read", "update-1", "update-2", "exit"]);

        let ordinary = vec![calls[1].clone(), calls[2].clone()];
        let unchanged: Vec<&str> = plan_batch_order(&ordinary)
            .into_iter()
            .map(|call| call.id.as_str())
            .collect();
        assert_eq!(unchanged, ["read", "update-1"]);
    }

    /// Answers from a script, one entry per request, and keeps every request it
    /// was given. What the loop *sent* is half of what these tests are about:
    /// "the new tools take effect immediately" and "steering does not disturb
    /// the prefix" are both claims about request N+1.
    #[derive(Default)]
    struct Scripted {
        script: Mutex<VecDeque<Vec<StreamEvent>>>,
        sent: Mutex<Vec<(Vec<ChatMessage>, Vec<String>)>>,
        /// The output allowance each request actually asked for.
        ceilings: Mutex<Vec<Option<i32>>>,
        /// Never let a round run out, so cancelling it means something.
        ///
        /// `consume_stream` reads its cancellation token and its stream in one
        /// `select!`, which picks at random among *ready* branches. A finite
        /// stream is always ready, so a cancelled read can still poll its way to
        /// the end — and the end is what sets `ran_to_completion`. A test that
        /// cancels against one is testing a coin toss.
        stalls: bool,
        /// What a summarisation request comes back with. Compaction runs against
        /// the turn's own provider, so a test about compaction has to answer for
        /// it too.
        summary: Option<String>,
    }

    impl Scripted {
        fn of(rounds: Vec<Vec<StreamEvent>>) -> Self {
            Self {
                script: Mutex::new(rounds.into()),
                ..Default::default()
            }
        }
        fn summarising_to(self, summary: &str) -> Self {
            Self {
                summary: Some(summary.to_string()),
                ..self
            }
        }
        /// Says its piece and then nothing, the way a provider that has stopped
        /// sending does. The only way to leave cancellation as the sole branch
        /// that can fire.
        fn stalling(rounds: Vec<Vec<StreamEvent>>) -> Self {
            Self {
                stalls: true,
                ..Self::of(rounds)
            }
        }
        fn requests(&self) -> Vec<(Vec<ChatMessage>, Vec<String>)> {
            self.sent.lock().unwrap().clone()
        }
        fn rounds(&self) -> usize {
            self.sent.lock().unwrap().len()
        }
        fn ceilings(&self) -> Vec<Option<i32>> {
            self.ceilings.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl ChatProvider for Scripted {
        fn adapter_name(&self) -> &'static str {
            "Scripted"
        }

        async fn stream_chat_with_tools(
            &self,
            messages: Vec<ChatMessage>,
            tools: Vec<ToolDefinition>,
            params: ChatParams,
        ) -> Result<crate::provider::ChatStream, ProviderError> {
            self.sent
                .lock()
                .unwrap()
                .push((messages, tools.into_iter().map(|t| t.name).collect()));
            self.ceilings.lock().unwrap().push(params.max_tokens);
            match self.script.lock().unwrap().pop_front() {
                Some(events) => {
                    let said = futures::stream::iter(events.into_iter().map(Ok));
                    Ok(if self.stalls {
                        Box::pin(futures::StreamExt::chain(said, futures::stream::pending()))
                    } else {
                        Box::pin(said)
                    })
                }
                // A loop that asked one more time than the test scripted has
                // gone somewhere the test does not describe. Say so rather than
                // hanging or quietly answering nothing.
                None => Err(ProviderError::Parse("the script ran out".into())),
            }
        }

        async fn chat(&self, _messages: Vec<ChatMessage>, _params: ChatParams) -> Result<String, ProviderError> {
            Err(ProviderError::NotImplemented("the summariser needs usage back".into()))
        }

        /// Mid-turn compaction goes through here, empty tool list and all,
        /// because this is the call that reports what the summary cost.
        async fn chat_with_tools(
            &self,
            _messages: Vec<ChatMessage>,
            _tools: Vec<ToolDefinition>,
            _params: ChatParams,
        ) -> Result<crate::provider::AgentResponse, ProviderError> {
            match self.summary {
                Some(ref s) => Ok(crate::provider::AgentResponse {
                    text: s.clone(),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    usage: None,
                    provider_state: None,
                }),
                None => Err(ProviderError::NotImplemented("not used by the loop".into())),
            }
        }
    }

    // --- the ports -------------------------------------------------------

    #[derive(Default)]
    struct Recorder(Mutex<Vec<(String, serde_json::Value)>>);

    impl Recorder {
        fn kinds(&self) -> Vec<String> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|(channel, p)| match p.get("type").and_then(|t| t.as_str()) {
                    Some(t) => t.to_string(),
                    None => channel.clone(),
                })
                .collect()
        }
    }

    impl Emit for Recorder {
        fn emit(&self, channel: &str, payload: serde_json::Value) -> Result<(), String> {
            self.0.lock().unwrap().push((channel.to_string(), payload));
            Ok(())
        }
    }

    /// The desktop's reading: a send that fails takes the turn with it. Narrowed
    /// to one event type so a test can say *which* send it means.
    struct Broken(&'static str);
    impl Emit for Broken {
        fn emit(&self, _channel: &str, payload: serde_json::Value) -> Result<(), String> {
            if payload["type"] == self.0 {
                return Err("the window is gone".into());
            }
            Ok(())
        }
    }

    /// OneBot's reading: the events are a courtesy, and losing one is nothing.
    struct Deaf;
    impl Emit for Deaf {
        fn emit(&self, _channel: &str, _payload: serde_json::Value) -> Result<(), String> {
            Ok(())
        }
    }

    struct Answers {
        answer: Option<ApprovalDecision>,
        asked: Mutex<Vec<(String, Option<String>)>>,
    }

    impl Answers {
        fn saying(answer: Option<ApprovalDecision>) -> Self {
            Self {
                answer,
                asked: Mutex::new(Vec::new()),
            }
        }
        fn nobody() -> Self {
            Self::saying(None)
        }
    }

    #[async_trait::async_trait]
    impl Approvals for Answers {
        async fn ask(
            &self,
            _assistant_message_id: &str,
            call: &ToolCall,
            retry_reason: Option<&str>,
        ) -> Result<Option<ApprovalDecision>, String> {
            self.asked
                .lock()
                .unwrap()
                .push((call.name.clone(), retry_reason.map(str::to_string)));
            Ok(self.answer.clone())
        }
    }

    /// A tool the test owns outright: no registry, no filesystem, and a hook to
    /// make something happen at exactly the moment it runs.
    struct Fixture {
        output: String,
        needs_approval: bool,
        on_call: Option<Box<dyn Fn() + Send + Sync>>,
        ran: Mutex<Vec<String>>,
    }

    impl Fixture {
        fn returning(output: &str) -> Self {
            Self {
                output: output.to_string(),
                needs_approval: false,
                on_call: None,
                ran: Mutex::new(Vec::new()),
            }
        }
        fn asking_first(mut self) -> Self {
            self.needs_approval = true;
            self
        }
        fn doing(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
            self.on_call = Some(Box::new(f));
            self
        }
    }

    #[async_trait::async_trait]
    impl SurfaceTools for Fixture {
        fn owns(&self, name: &str) -> bool {
            name == "fixture"
        }
        fn requires_approval(&self, _name: &str) -> bool {
            self.needs_approval
        }
        async fn execute(&self, _name: &str, arguments: &str) -> Result<String, String> {
            self.ran.lock().unwrap().push(arguments.to_string());
            if let Some(f) = &self.on_call {
                f();
            }
            Ok(self.output.clone())
        }
    }

    /// Hands over its queue once and is empty afterwards, the way a real inbox
    /// behaves across rounds.
    struct Inbox(Mutex<Vec<Steered>>);

    #[async_trait::async_trait]
    impl Steering for Inbox {
        async fn drain(&self) -> Vec<Steered> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }

    struct Rebuilt(TurnConfig);

    #[async_trait::async_trait]
    impl transitions::Transitions for Rebuilt {
        async fn rebuild(&self, _mode: &'static ModeSpec) -> Result<Result<TurnConfig, String>, String> {
            Ok(Ok(TurnConfig {
                tool_defs: self.0.tool_defs.clone(),
                system_prompt: self.0.system_prompt.clone(),
                offered: self.0.offered.clone(),
            }))
        }
    }

    #[derive(Default)]
    struct FailingPlanTransitions {
        updates: std::sync::atomic::AtomicUsize,
        submissions: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl transitions::Transitions for FailingPlanTransitions {
        async fn rebuild(&self, _mode: &'static ModeSpec) -> Result<Result<TurnConfig, String>, String> {
            Err("not used".into())
        }

        async fn update_plan(
            &self,
            _request: transitions::UpdatePlanRequest,
        ) -> Result<transitions::PlanUpdateResult, String> {
            self.updates.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err("stale plan generation".into())
        }

        async fn submit_plan(
            &self,
            request: transitions::SubmitPlanRequest,
        ) -> Result<crate::events::PlanReviewEvent, String> {
            self.submissions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::events::PlanReviewEvent {
                review_id: "review-that-must-not-exist".into(),
                conversation_id: "c1".into(),
                document_id: "document-1".into(),
                revision_id: "revision-1".into(),
                turn_id: request.turn_id,
                status: "pending".into(),
                lock_version: 0,
                delivery_state: None,
            })
        }
    }

    // --- the fixtures ----------------------------------------------------

    fn conversation(pool: &DbPool) {
        let mut conn = pool.get().unwrap();
        crate::db::ops::conversation::create_conversation(&mut conn, "c1", Some("t"), None, None, 1).unwrap();
        crate::db::ops::turn::begin(&mut conn, "t1", "c1", TurnOrigin::Desktop, None, 1000).unwrap();
    }

    fn rows(pool: &DbPool) -> Vec<crate::db::models::message::MessageRow> {
        let mut conn = pool.get().unwrap();
        crate::db::ops::message::list_messages(&mut conn, "c1").unwrap()
    }

    fn def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: String::new(),
            parameters: serde_json::json!({}),
        }
    }

    fn system(content: &str) -> ChatMessage {
        let mut m = ChatMessage::user(content);
        m.role = "system".into();
        m
    }

    fn registry() -> ToolRegistry {
        ToolRegistry::new(
            std::path::PathBuf::from("/nonexistent"),
            std::path::PathBuf::from("/nonexistent"),
        )
    }

    fn context(pool: &DbPool, cancel: &CancellationToken) -> ToolContext {
        ToolContext {
            working_directory: None,
            shell: tools::ShellType::default_for_platform(),
            file_access: tools::FileAccess::default(),
            project_id: None,
            conversation_id: Some("c1".into()),
            turn_id: Some("t1".into()),
            assistant_id: None,
            db_pool: Some(pool.clone()),
            #[cfg(not(target_os = "android"))]
            sandbox_policy: None,
            tool_secrets: Default::default(),
            cancel: cancel.clone(),
            journal: None,
        }
    }

    fn setup<'a>(provider: &'a Scripted, pool: &DbPool, cancel: &CancellationToken, offered: &[&str]) -> TurnSetup<'a> {
        TurnSetup {
            provider,
            params: ChatParams {
                model: "m".into(),
                ..Default::default()
            },
            chat_messages: vec![system("you are helpful"), ChatMessage::user("do the thing")],
            tool_defs: offered.iter().map(|n| def(n)).collect(),
            offered: offered.iter().map(|n| n.to_string()).collect(),
            mode: crate::agent::modes::resolve(None).unwrap(),
            tool_context: context(pool, cancel),
            budget: TokenBudget::new("openai", "m", 128_000, 4096, None),
            turn_id: "t1".into(),
            conversation_id: "c1".into(),
            // No `providers` row in the harness, and the column has a foreign
            // key — a made-up id would take every test in this module down on
            // the placeholder insert.
            provider_id: None,
            provider_name: None,
            parent_cursor: None,
            cancel: cancel.clone(),
            keep_recent: 10,
            context_limit: 128_000,
            approval_rule: ApprovalRule::ByReach { accept_edits: false },
            withheld: WithheldWording::Explained,
            files_root: None,
            interrupted: None,
            compaction: CompactionPolicy::OneBot,
            pricing: None,
        }
    }

    fn ports<'a>(approvals: &'a Answers, emit: Option<&'a dyn Emit>) -> TurnPorts<'a> {
        TurnPorts {
            emit,
            approvals,
            interim: None,
            surface_tools: None,
            steering: None,
            transitions: None,
            sub_agents: None,
        }
    }

    fn services<'a>(pool: &'a DbPool, tools: &'a ToolRegistry, mcp: &'a McpRegistry) -> TurnServices<'a> {
        TurnServices { pool, tools, mcp }
    }

    /// A `SubAgents` port that keeps what it was asked for and answers from a
    /// fixed script.
    struct Delegate {
        seen: Mutex<Vec<SubAgentSpec>>,
        reply: Result<(SubAgentStatus, &'static str, usize), String>,
        stranded: super::super::ports::Stranded,
    }

    impl Delegate {
        fn returning(status: SubAgentStatus, text: &'static str, steps: usize) -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
                reply: Ok((status, text, steps)),
                stranded: Default::default(),
            }
        }
        fn failing(error: &str) -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
                reply: Err(error.to_string()),
                stranded: Default::default(),
            }
        }
        /// A run that was sent messages after it stopped reading.
        fn stranding(accepted: usize, unrecorded: usize) -> Self {
            Self {
                stranded: super::super::ports::Stranded { accepted, unrecorded },
                ..Self::returning(SubAgentStatus::Done, "had a look", 2)
            }
        }
        fn asked(&self) -> usize {
            self.seen.lock().unwrap().len()
        }
    }

    #[async_trait::async_trait]
    impl super::super::ports::SubAgents for Delegate {
        async fn run(&self, spec: SubAgentSpec) -> Result<SubAgentReport, String> {
            self.seen.lock().unwrap().push(spec);
            match &self.reply {
                Ok((status, text, steps)) => Ok(SubAgentReport {
                    status: *status,
                    reply: (*text).to_string(),
                    steps: *steps,
                    stranded: self.stranded,
                }),
                Err(e) => Err(e.clone()),
            }
        }
    }

    fn delegating<'a>(approvals: &'a Answers, sub_agents: &'a Delegate) -> TurnPorts<'a> {
        TurnPorts {
            sub_agents: Some(sub_agents),
            ..ports(approvals, None)
        }
    }

    fn run_agent_call(id: &str, args: &str) -> Vec<StreamEvent> {
        calls(id, crate::agent::sub_agents::RUN_AGENT_TOOL, args)
    }

    const ERRAND: &str = r#"{"agent":"explore","description":"find the caller","prompt":"Find every caller of resolve_head and say what each one does with the answer."}"#;

    #[tokio::test]
    async fn provider_boundary_caps_user_context_even_below_a_large_windows_trim_threshold() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![says("done")]);
        let approvals = Answers::nobody();
        let mut turn = setup(&provider, &pool, &cancel, &[]);
        turn.context_limit = 200_000;
        turn.budget = TokenBudget::new("openai", "m", turn.context_limit, 4_096, None);
        let shell = crate::workspace::reference::render_context_item(
            crate::workspace::reference::MessageContextKind::ShellOutput,
            None,
            None,
            None,
            &"界".repeat(crate::workspace::reference::MAX_MODEL_SHELL_CONTEXT_BYTES),
            false,
        );
        for _ in 0..5 {
            turn.chat_messages.push(ChatMessage::user_provided_context(&shell));
        }
        let before = turn
            .chat_messages
            .iter()
            .filter(|message| message.origin == crate::provider::MessageOrigin::UserProvidedContext)
            .map(|message| crate::agent::context::estimate_tokens(&message.content))
            .sum::<usize>();
        assert!(before > 25_000);
        assert!(
            before < turn.context_limit * 4 / 5,
            "the regression must stay below trim pressure"
        );

        let outcome = run_turn(&services(&pool, &tools, &mcp), turn, ports(&approvals, None)).await;

        assert_eq!(outcome.reply.as_deref(), Ok("done"));
        let sent = &provider.requests()[0].0;
        let sent_context = sent
            .iter()
            .filter(|message| message.origin == crate::provider::MessageOrigin::UserProvidedContext)
            .map(|message| crate::agent::context::estimate_tokens(&message.content))
            .sum::<usize>();
        assert!(
            sent_context <= 25_000,
            "provider received {sent_context} user-context tokens"
        );
    }

    /// The whole point of the port: the loop recognises the name, hands over,
    /// and puts the answer back where a tool result goes.
    #[tokio::test]
    async fn a_delegated_run_reaches_the_port_and_its_answer_reaches_the_model() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![run_agent_call("c1", ERRAND), says("thanks")]);
        let approvals = Answers::nobody();
        let delegate = Delegate::returning(SubAgentStatus::Done, "Three callers.", 4);

        let outcome = run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &[crate::agent::sub_agents::RUN_AGENT_TOOL]),
            delegating(&approvals, &delegate),
        )
        .await;

        assert_eq!(outcome.reply.as_deref(), Ok("thanks"));
        let seen = delegate.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].kind, crate::agent::sub_agents::SubAgentKind::Explore);
        assert_eq!(seen[0].description, "find the caller");
        assert!(seen[0].prompt.contains("resolve_head"));
        assert_eq!(seen[0].parent_call_id, "c1");
        assert!(seen[0].model.is_none(), "an omitted model stays omitted");
        drop(seen);

        // The verdict and the count travel with the text; the model is not left
        // to infer either from prose.
        let tool_row = rows(&pool).into_iter().find(|r| r.role == "tool").unwrap();
        assert!(
            tool_row.content.contains("finished after 4 steps"),
            "{}",
            tool_row.content
        );
        assert!(tool_row.content.contains("Three callers."));
        assert_eq!(tool_row.tool_outcome.as_deref(), Some("success"));
    }

    /// A run somebody stopped returns whatever text had been written, exactly as
    /// a finished one does. Without a verdict in front of it the model reads
    /// half an answer as the answer.
    #[tokio::test]
    async fn a_stopped_sub_agent_does_not_come_back_looking_like_a_conclusion() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![run_agent_call("c1", ERRAND), says("ok")]);
        let approvals = Answers::nobody();
        let delegate = Delegate::returning(SubAgentStatus::Cancelled, "I found two so far", 2);

        run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &[crate::agent::sub_agents::RUN_AGENT_TOOL]),
            delegating(&approvals, &delegate),
        )
        .await;

        let tool_row = rows(&pool).into_iter().find(|r| r.role == "tool").unwrap();
        assert!(tool_row.content.contains("stopped"), "{}", tool_row.content);
        assert!(tool_row.content.contains("do not treat it as a conclusion"));
        assert!(
            tool_row.content.contains("I found two so far"),
            "the partial text is still there"
        );
        assert_eq!(
            tool_row.tool_outcome.as_deref(),
            Some("error"),
            "a run that was stopped is not a successful call",
        );
    }

    /// A delegation that could not start is an ordinary tool failure. Ending the
    /// turn over it would throw away everything the parent had already done.
    #[tokio::test]
    async fn a_port_that_refuses_is_a_tool_result_and_the_turn_carries_on() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![run_agent_call("c1", ERRAND), says("I will do it myself")]);
        let approvals = Answers::nobody();
        let delegate = Delegate::failing("no model configured for sub-agents");

        let outcome = run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &[crate::agent::sub_agents::RUN_AGENT_TOOL]),
            delegating(&approvals, &delegate),
        )
        .await;

        assert_eq!(outcome.reply.as_deref(), Ok("I will do it myself"));
        assert_eq!(provider.rounds(), 2, "the loop went round again");
        let tool_row = rows(&pool).into_iter().find(|r| r.role == "tool").unwrap();
        assert!(tool_row.content.contains("no model configured"), "{}", tool_row.content);
        assert_eq!(tool_row.tool_outcome.as_deref(), Some("error"));
    }

    /// Arguments the loop cannot read never reach the port, and what comes back
    /// says what to write instead. Naming an agent that does not exist is a
    /// mistake the model can fix on the next round.
    #[tokio::test]
    async fn unusable_arguments_are_answered_rather_than_acted_on() {
        for (args, expected) in [
            (r#"{"agent":"researcher","description":"d","prompt":"p"}"#, "explore"),
            (r#"{"agent":"explore","description":"d"}"#, "`prompt` is required"),
            (
                r#"{"agent":"explore","description":"  ","prompt":"p"}"#,
                "`description` is required",
            ),
            ("not json at all", "not valid JSON"),
        ] {
            let pool = test_db();
            conversation(&pool);
            let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
            let provider = Scripted::of(vec![run_agent_call("c1", args), says("fine")]);
            let approvals = Answers::nobody();
            let delegate = Delegate::returning(SubAgentStatus::Done, "never runs", 1);

            run_turn(
                &services(&pool, &tools, &mcp),
                setup(&provider, &pool, &cancel, &[crate::agent::sub_agents::RUN_AGENT_TOOL]),
                delegating(&approvals, &delegate),
            )
            .await;

            assert_eq!(delegate.asked(), 0, "nothing was started for `{args}`");
            let tool_row = rows(&pool).into_iter().find(|r| r.role == "tool").unwrap();
            assert!(
                tool_row.content.contains(expected),
                "for `{args}` expected {expected:?} in {:?}",
                tool_row.content,
            );
        }
    }

    /// With no port, the name is not offered, so it never reaches dispatch at
    /// all — the withholding wording answers first. This is the property that
    /// keeps a sub-agent from delegating to a sub-agent.
    #[tokio::test]
    async fn without_a_port_the_name_is_withheld_and_nothing_is_started() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![run_agent_call("c1", ERRAND), says("understood")]);
        let approvals = Answers::nobody();

        run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &[]),
            ports(&approvals, None),
        )
        .await;

        let tool_row = rows(&pool).into_iter().find(|r| r.role == "tool").unwrap();
        assert_eq!(tool_row.tool_outcome.as_deref(), Some("error"));
        assert!(
            !tool_row.content.contains("must be handled by the agent loop"),
            "the registry's placeholder must never reach the model: {}",
            tool_row.content,
        );
    }

    // --- the contract ----------------------------------------------------

    #[tokio::test]
    async fn a_turn_with_nothing_to_run_asks_once_and_answers() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![says("here you go")]);
        let approvals = Answers::nobody();
        let emit = Recorder::default();

        let outcome = run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &[]),
            ports(&approvals, Some(&emit)),
        )
        .await;

        assert_eq!(outcome.reply.as_deref(), Ok("here you go"));
        assert_eq!(outcome.stop_reason(), "end_turn");
        assert!(!outcome.progress.aborted);
        assert_eq!(provider.rounds(), 1);
        assert_eq!(emit.kinds(), ["message_start", "text"]);

        let rows = rows(&pool);
        assert_eq!(rows.len(), 1, "one assistant row and nothing else");
        assert_eq!(rows[0].content, "here you go");
        assert_eq!(outcome.progress.message_id.as_deref(), Some(rows[0].id.as_str()));
    }

    /// The tool result has to reach the *next* request, or the model answers
    /// without ever seeing what it asked for.
    #[tokio::test]
    async fn a_tool_call_runs_and_its_answer_goes_into_the_next_request() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![calls("call-1", "fixture", r#"{"x":1}"#), says("that worked")]);
        let approvals = Answers::nobody();
        let fixture = Fixture::returning("42");
        let emit = Recorder::default();

        let outcome = run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &["fixture"]),
            TurnPorts {
                surface_tools: Some(&fixture),
                ..ports(&approvals, Some(&emit))
            },
        )
        .await;

        assert_eq!(outcome.reply.as_deref(), Ok("that worked"));
        assert_eq!(*fixture.ran.lock().unwrap(), [r#"{"x":1}"#]);
        assert!(
            approvals.asked.lock().unwrap().is_empty(),
            "a read-only tool does not ask"
        );
        assert_eq!(provider.rounds(), 2);

        let second = &provider.requests()[1].0;
        assert_eq!(second.last().unwrap().content, "42");
        assert_eq!(second.last().unwrap().role, "tool");

        let rows = rows(&pool);
        assert_eq!(
            rows.iter().map(|r| r.role.as_str()).collect::<Vec<_>>(),
            ["assistant", "tool", "assistant"],
        );
        assert_eq!(rows[1].parent_id.as_deref(), Some(rows[0].id.as_str()));
        assert_eq!(rows[2].parent_id.as_deref(), Some(rows[1].id.as_str()));
        assert_eq!(
            emit.kinds(),
            ["message_start", "tool_call", "tool_result", "message_start", "text",]
        );
    }

    /// Cancelling is a decision, not a failure: whatever the model managed to
    /// say is still the reply, and the caller reports it as a stop rather than
    /// as an error.
    #[tokio::test]
    async fn cancelling_ends_the_turn_without_making_it_an_error() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![calls("call-1", "fixture", "{}"), says("never asked for")]);
        let approvals = Answers::nobody();
        let stop = cancel.clone();
        let fixture = Fixture::returning("done").doing(move || stop.cancel());

        let outcome = run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &["fixture"]),
            TurnPorts {
                surface_tools: Some(&fixture),
                ..ports(&approvals, None)
            },
        )
        .await;

        assert!(outcome.reply.is_ok(), "cancelling is not an error");
        assert_eq!(outcome.stop_reason(), "end_turn");
        assert!(!outcome.progress.aborted, "and it is not the loop guard either");
        assert_eq!(provider.rounds(), 1, "the second request is never made");
        // The tool did run and its row is written: the world had already
        // changed by the time the token was cancelled.
        assert_eq!(rows(&pool).iter().filter(|r| r.role == "tool").count(), 1);
    }

    /// The guard exists so a stuck model cannot spend a person's attention, or
    /// the machine's, on the same call forever. It ends the turn as a success
    /// carrying whatever was said — an error would lose that.
    #[tokio::test]
    async fn a_model_repeating_itself_is_stopped_and_the_caller_is_told_why() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let rounds: Vec<_> = (0..crate::agent::loop_guard::LOOP_ABORT_AFTER + 2)
            .map(|_| calls("call-1", "fixture", r#"{"same":true}"#))
            .collect();
        let provider = Scripted::of(rounds);
        let approvals = Answers::nobody();
        let fixture = Fixture::returning("again");

        let outcome = run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &["fixture"]),
            TurnPorts {
                surface_tools: Some(&fixture),
                ..ports(&approvals, None)
            },
        )
        .await;

        assert!(outcome.reply.is_ok());
        assert!(outcome.progress.aborted);
        assert_eq!(outcome.stop_reason(), "loop_detected");
        assert_eq!(
            provider.rounds(),
            crate::agent::loop_guard::LOOP_ABORT_AFTER as usize,
            "it stops on the call that trips the guard, not a round later",
        );
        assert!(
            fixture.ran.lock().unwrap().len() < crate::agent::loop_guard::LOOP_ABORT_AFTER as usize,
            "and the call it aborted on is not executed"
        );
    }

    /// The one write the loop is allowed to lose. By the time it runs the tool
    /// has already touched the world, so the row is worth less than the turn.
    #[tokio::test]
    async fn a_tool_row_the_database_refuses_does_not_stop_the_turn_or_move_the_cursor() {
        let pool = test_db();
        conversation(&pool);
        {
            let mut conn = pool.get().unwrap();
            conn.batch_execute(
                "CREATE TRIGGER no_tool_rows BEFORE INSERT ON messages \
                 WHEN NEW.role = 'tool' \
                 BEGIN SELECT RAISE(ABORT, 'refused'); END",
            )
            .unwrap();
        }
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![calls("call-1", "fixture", "{}"), says("carried on")]);
        let approvals = Answers::nobody();
        let fixture = Fixture::returning("the tool still ran");

        let outcome = run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &["fixture"]),
            TurnPorts {
                surface_tools: Some(&fixture),
                ..ports(&approvals, None)
            },
        )
        .await;

        assert_eq!(outcome.reply.as_deref(), Ok("carried on"));
        assert_eq!(provider.rounds(), 2);
        // The model still sees the result — only the transcript lost it.
        assert_eq!(provider.requests()[1].0.last().unwrap().content, "the tool still ran");

        let rows = rows(&pool);
        assert_eq!(
            rows.iter().map(|r| r.role.as_str()).collect::<Vec<_>>(),
            ["assistant", "assistant"]
        );
        assert_eq!(
            rows[1].parent_id.as_deref(),
            Some(rows[0].id.as_str()),
            "the cursor stayed on the last row that landed, so the chain is intact",
        );
    }

    /// The two runners disagree about this on purpose, and the disagreement is
    /// the reason `Emit` returns a `Result` at all.
    ///
    /// The send under test is one the loop makes itself, not one the stream
    /// reader makes: a failing `Emit` also takes the stream down, and a test that
    /// let it would be re-checking `engine::stream` and calling it this. So it
    /// fails only on `tool_call`, which nothing but the loop sends — and the
    /// assertion that the tool never ran is what makes the failure's *position*
    /// part of the contract rather than just its existence.
    #[tokio::test]
    async fn a_send_the_loop_makes_ends_one_runners_turn_and_not_the_others() {
        for (label, emit, ends) in [
            ("desktop", &Broken("tool_call") as &dyn Emit, true),
            ("onebot", &Deaf as &dyn Emit, false),
        ] {
            let pool = test_db();
            conversation(&pool);
            let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
            let provider = Scripted::of(vec![calls("call-1", "fixture", "{}"), says("carried on")]);
            let approvals = Answers::nobody();
            let fixture = Fixture::returning("ok");

            let outcome = run_turn(
                &services(&pool, &tools, &mcp),
                setup(&provider, &pool, &cancel, &["fixture"]),
                TurnPorts {
                    surface_tools: Some(&fixture),
                    ..ports(&approvals, Some(emit))
                },
            )
            .await;

            assert_eq!(outcome.reply.is_err(), ends, "{label}");
            assert_eq!(
                fixture.ran.lock().unwrap().is_empty(),
                ends,
                "{label}: the card was never drawn, so the call must not have happened",
            );
            // Either way the row was opened, so the caller can name it.
            assert!(outcome.progress.message_id.is_some(), "{label}");
        }
    }

    /// A turn that failed still has to say what it was writing: the front end
    /// hangs its terminal event off that id, and without one it sits on the
    /// `streaming` flag its optimistic send set.
    #[tokio::test]
    async fn a_failed_turn_still_reports_what_it_had_got_done() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![]);
        let approvals = Answers::nobody();

        let outcome = run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &[]),
            ports(&approvals, None),
        )
        .await;

        assert!(outcome.reply.is_err());
        assert_eq!(outcome.stop_reason(), "error");
        assert_eq!(
            outcome.progress.message_id.as_deref(),
            Some(rows(&pool)[0].id.as_str()),
            "the row it had opened",
        );
    }

    /// Approving a mode switch has to change the next request, not the one
    /// after it. A tool set that lags by a round is a turn told it may edit
    /// while it still cannot.
    #[tokio::test]
    async fn a_mode_switch_reaches_the_very_next_request() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![
            calls("call-1", crate::agent::modes::ENTER_PLAN_TOOL, "{}"),
            says("planning now"),
        ]);
        let approvals = Answers::saying(Some(ApprovalDecision::Approved));
        let rebuilt = Rebuilt(TurnConfig {
            tool_defs: vec![def("read_file")],
            system_prompt: "# Plan mode\n\nyou are planning".into(),
            offered: ["read_file".to_string()].into_iter().collect(),
        });

        let outcome = run_turn(
            &services(&pool, &tools, &mcp),
            setup(
                &provider,
                &pool,
                &cancel,
                &[crate::agent::modes::ENTER_PLAN_TOOL, "write_file"],
            ),
            TurnPorts {
                transitions: Some(&rebuilt),
                ..ports(&approvals, None)
            },
        )
        .await;

        assert_eq!(outcome.reply.as_deref(), Ok("planning now"));
        let requests = provider.requests();
        assert_eq!(requests[1].1, ["read_file"], "the new tool set, one request later");
        assert_eq!(requests[1].0[0].content, "# Plan mode\n\nyou are planning");
        assert_eq!(requests[0].0[0].content, "you are helpful", "and the old one before it");
    }

    #[tokio::test]
    async fn a_failed_update_in_the_batch_prevents_exit_from_submitting_a_review() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let patch = serde_json::json!({
            "base_generation": 1,
            "base_sha256": "0".repeat(64),
            "patch": "*** Begin Patch\n*** Update File: plan.md\n@@\n-old\n+new\n*** End Patch"
        })
        .to_string();
        let provider = Scripted::of(vec![
            call_batch(&[
                ("exit-first", crate::agent::modes::EXIT_PLAN_TOOL, "{}"),
                ("update-second", crate::agent::modes::UPDATE_PLAN_TOOL, &patch),
            ]),
            says("I will read and retry the patch."),
        ]);
        let approvals = Answers::nobody();
        let transitions = FailingPlanTransitions::default();
        let mut turn = setup(
            &provider,
            &pool,
            &cancel,
            &[
                crate::agent::modes::UPDATE_PLAN_TOOL,
                crate::agent::modes::EXIT_PLAN_TOOL,
            ],
        );
        turn.mode = crate::agent::modes::resolve(Some(crate::agent::modes::PLAN_MODE)).unwrap();

        let outcome = run_turn(
            &services(&pool, &tools, &mcp),
            turn,
            TurnPorts {
                transitions: Some(&transitions),
                ..ports(&approvals, None)
            },
        )
        .await;

        assert_eq!(outcome.reply.as_deref(), Ok("I will read and retry the patch."));
        assert_eq!(transitions.updates.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            transitions.submissions.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "exit_plan must not create a review after any same-batch update failure"
        );
        assert!(outcome.progress.waiting_review.is_none());
        let tool_rows = rows(&pool)
            .into_iter()
            .filter(|row| row.role == "tool")
            .collect::<Vec<_>>();
        assert_eq!(tool_rows.len(), 2);
        assert!(tool_rows[0].content.contains("stale plan generation"));
        assert!(tool_rows[1].content.contains("was not submitted"));
    }

    /// Steering appends and only appends. Everything ahead of the first user
    /// message is the prefix the prompt cache is keyed on; moving it costs the
    /// cache on every following request of the turn.
    #[tokio::test]
    async fn steering_lands_at_the_end_and_leaves_the_prefix_alone() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![calls("call-1", "fixture", "{}"), says("noted")]);
        let approvals = Answers::nobody();
        let fixture = Fixture::returning("ok");
        let inbox = Inbox(Mutex::new(vec![
            Steered {
                text: "one more thing".into(),
                origin: SteeredOrigin::User(Some(SenderRef {
                    user_id: 7,
                    nickname: None,
                })),
                row: None,
            },
            Steered {
                text: "they left the group".into(),
                origin: SteeredOrigin::System,
                row: None,
            },
        ]));

        run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &["fixture"]),
            TurnPorts {
                surface_tools: Some(&fixture),
                steering: Some(&inbox),
                ..ports(&approvals, None)
            },
        )
        .await;

        let requests = provider.requests();
        assert_eq!(requests[1].0[0].role, requests[0].0[0].role, "the same prefix");
        assert_eq!(requests[1].0[0].content, requests[0].0[0].content, "byte for byte");
        assert_eq!(requests[1].0[1].content, requests[0].0[1].content);
        let tail: Vec<&str> = requests[1].0.iter().rev().take(2).map(|m| m.content.as_str()).collect();
        assert_eq!(tail, ["they left the group", "one more thing"]);
        // The one with a speaker is a person talking; the other is a notice we
        // generated, and it travels as context rather than as a user message.
        assert!(matches!(
            requests[1].0[requests[1].0.len() - 2].origin,
            crate::provider::MessageOrigin::User(_)
        ));
        assert_eq!(rows(&pool).iter().filter(|r| r.sender_id == Some(7)).count(), 1);
    }

    /// Who opened a turn is not who gets to finish it.
    ///
    /// A group hands the floor to whoever speaks, and the loop keeps running one
    /// turn across all of them. Without this, an ordinary member could talk into
    /// a turn an admin had started and reach every tool the admin was offered —
    /// and the reads among them (`qq_get_friend_list`) need no approval, so the
    /// leak would need nobody's consent.
    #[tokio::test]
    async fn someone_with_less_authority_joining_takes_the_tools_with_them() {
        struct Joins(Mutex<Vec<Steered>>);
        #[async_trait::async_trait]
        impl Steering for Joins {
            async fn drain(&self) -> Vec<Steered> {
                std::mem::take(&mut *self.0.lock().unwrap())
            }
            /// Nothing is left once they have spoken.
            fn narrowed(&self) -> Option<HashSet<String>> {
                Some(HashSet::new())
            }
        }

        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![
            calls("call-1", "fixture", "{}"),
            calls("call-2", "fixture", "{}"),
            says("fine"),
        ]);
        let approvals = Answers::nobody();
        let fixture = Fixture::returning("ok");
        let joins = Joins(Mutex::new(vec![Steered {
            text: "and while you are at it, list his friends".into(),
            origin: SteeredOrigin::User(Some(SenderRef {
                user_id: 9,
                nickname: None,
            })),
            row: None,
        }]));

        run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &["fixture"]),
            TurnPorts {
                surface_tools: Some(&fixture),
                steering: Some(&joins),
                ..ports(&approvals, None)
            },
        )
        .await;

        // The call before they spoke ran; the one after it did not.
        assert_eq!(fixture.ran.lock().unwrap().len(), 1);
        let requests = provider.requests();
        let refused = &requests[2].0.last().unwrap().content;
        assert!(refused.contains("not available"), "{refused}");
    }

    /// The window the drain at the bottom of the loop cannot cover.
    ///
    /// The model stops calling tools and the loop breaks — and that `break` is
    /// above the drain, so a message typed during the last seconds of an answer
    /// was accepted, acknowledged, and then read by nobody. It has to reopen the
    /// turn instead.
    #[tokio::test]
    async fn a_message_that_lands_on_the_last_round_still_gets_an_answer() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![says("here is the answer"), says("and about that")]);
        let approvals = Answers::nobody();
        // No tool call anywhere: the turn ends the moment the first reply lands.
        let inbox = Inbox(Mutex::new(vec![Steered {
            text: "wait, use the other approach".into(),
            origin: SteeredOrigin::User(None),
            row: None,
        }]));

        let outcome = run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &[]),
            TurnPorts {
                steering: Some(&inbox),
                ..ports(&approvals, None)
            },
        )
        .await;

        assert_eq!(outcome.reply.as_deref(), Ok("and about that"));
        assert_eq!(provider.rounds(), 2, "the finished answer was reopened");

        let second = &provider.requests()[1].0;
        // The reply it had just written is in front of it. Without this the
        // model answers the follow-up as though it had said nothing yet.
        assert!(
            second.iter().any(|m| m.content == "here is the answer"),
            "{:?}",
            second.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
        );
        // And a person typed it, so it arrives as a person talking — as the
        // desktop's implicit single speaker, since they have no chat identity.
        // `SystemContext` here is the bug `SteeredOrigin` exists to stop: the
        // model weighs that as ambient noise rather than as an instruction.
        let last = second.last().unwrap();
        assert_eq!(last.content, "wait, use the other approach");
        assert!(
            matches!(last.origin, crate::provider::MessageOrigin::LegacyUser),
            "{:?}",
            last.origin,
        );
    }

    /// A message that arrives already written does not get a second row.
    ///
    /// The durable queue takes the item and writes the row in one transaction,
    /// because a kill in between must leave one of two states and not a third.
    /// That only works if the loop believes it: writing its own row here would
    /// put the same sentence in the transcript twice, and hang the rest of the
    /// turn off a row the queue has never heard of.
    #[tokio::test]
    async fn a_steered_message_that_already_has_a_row_is_not_written_again() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![says("here is the answer"), says("and about that")]);
        let approvals = Answers::nobody();

        // What the queue would have written, in its own transaction.
        let existing =
            crate::agent::engine::transcript::write_steering(&pool, "c1", "t1", "already on the record", None, None)
                .await
                .expect("the queue wrote it");

        let before = rows(&pool).len();
        let inbox = Inbox(Mutex::new(vec![Steered {
            text: "already on the record".into(),
            origin: SteeredOrigin::User(None),
            row: Some(existing.clone()),
        }]));

        let outcome = run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &[]),
            TurnPorts {
                steering: Some(&inbox),
                ..ports(&approvals, None)
            },
        )
        .await;
        assert_eq!(outcome.reply.as_deref(), Ok("and about that"));

        let after = rows(&pool);
        assert_eq!(
            after.iter().filter(|r| r.content == "already on the record").count(),
            1,
            "one row for one message"
        );
        // And the turn carried on from it: the row written after the
        // interjection hangs off it, so the transcript is one path.
        assert!(
            after.iter().any(|r| r.parent_id.as_deref() == Some(existing.as_str())),
            "the turn continued from the row it was handed"
        );
        // The model still saw it, which is the other half of not writing it.
        let second = &provider.requests()[1].0;
        assert_eq!(second.last().unwrap().content, "already on the record");
        assert!(after.len() > before);
    }

    /// The cap is not a drain. Reaching it has to leave the messages where they
    /// are, so whoever closes the inbox can hand them back and say what happened
    /// to them — draining and then stopping would make them disappear.
    #[tokio::test]
    async fn the_continuation_cap_leaves_the_inbox_alone() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        // Enough replies for every continuation plus the one that stops.
        let provider = Scripted::of((0..MAX_TAIL_CONTINUATIONS + 1).map(|_| says("ok")).collect::<Vec<_>>());
        let approvals = Answers::nobody();
        // Never empties: something new is waiting every single time.
        struct Endless(Mutex<usize>);
        #[async_trait::async_trait]
        impl Steering for Endless {
            async fn drain(&self) -> Vec<Steered> {
                *self.0.lock().unwrap() += 1;
                vec![Steered {
                    text: "and another".into(),
                    origin: SteeredOrigin::User(None),
                    row: None,
                }]
            }
        }
        let endless = Endless(Mutex::new(0));

        let outcome = run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &[]),
            TurnPorts {
                steering: Some(&endless),
                ..ports(&approvals, None)
            },
        )
        .await;

        assert!(outcome.reply.is_ok());
        assert_eq!(
            provider.rounds(),
            MAX_TAIL_CONTINUATIONS + 1,
            "it stops rather than being kept alive by whoever is typing",
        );
        // The round that gives up must not have taken anything with it. One
        // drain per continuation, and none for the round that stops.
        assert_eq!(
            *endless.0.lock().unwrap(),
            MAX_TAIL_CONTINUATIONS,
            "the last round took messages it was never going to deliver",
        );
    }

    /// The cursor a caller has to hang anything else off.
    ///
    /// Not the assistant row: a turn that ended on a tool call has that result
    /// as its last reachable row, and attaching to the assistant row above it
    /// opens a branch that pushes the result off the active path.
    #[tokio::test]
    async fn the_final_cursor_is_the_last_row_that_landed_not_the_last_reply() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![calls("call-1", "fixture", "{}"), says("never asked")]);
        let approvals = Answers::nobody();
        // Stopped while the tool was running, so the turn's last reachable row
        // is the tool result rather than an assistant row. This is the shape
        // that tells the two candidates apart.
        let stopper = cancel.clone();
        let fixture = Fixture::returning("ok").doing(move || stopper.cancel());

        let outcome = run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &["fixture"]),
            TurnPorts {
                surface_tools: Some(&fixture),
                ..ports(&approvals, None)
            },
        )
        .await;

        let rows = rows(&pool);
        let last = rows.last().unwrap();
        assert_eq!(
            outcome.progress.final_cursor.as_deref(),
            Some(last.id.as_str()),
            "rows: {:?}",
            rows.iter().map(|r| r.role.as_str()).collect::<Vec<_>>(),
        );
        // And every row hangs off the one before it, so there is one path.
        for pair in rows.windows(2) {
            assert_eq!(pair[1].parent_id.as_deref(), Some(pair[0].id.as_str()));
        }
    }

    /// Something the user said and the run never saw is the likeliest thing the
    /// parent is about to be asked why it ignored. It gets told.
    #[tokio::test]
    async fn what_a_run_never_saw_is_reported_to_the_parent() {
        for (accepted, unrecorded, expect) in [
            (2usize, 0usize, "They are in its transcript."),
            (2, 2, "None of them could be written down"),
            (3, 1, "1 of them could not be written down"),
        ] {
            let pool = test_db();
            conversation(&pool);
            let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
            let provider = Scripted::of(vec![run_agent_call("call-1", ERRAND), says("understood")]);
            let approvals = Answers::nobody();
            let delegate = Delegate::stranding(accepted, unrecorded);

            run_turn(
                &services(&pool, &tools, &mcp),
                setup(&provider, &pool, &cancel, &[crate::agent::sub_agents::RUN_AGENT_TOOL]),
                delegating(&approvals, &delegate),
            )
            .await;

            let result = provider.requests()[1].0.last().unwrap().content.clone();
            assert!(result.contains(&format!("sent {accepted} message(s)")), "{result}");
            assert!(result.contains(expect), "{result}");
            assert!(result.contains("never saw them"), "{result}");
        }
    }

    /// The ordinary case says nothing about it, because there is nothing to say.
    #[tokio::test]
    async fn a_run_that_saw_everything_is_not_reported_as_having_missed_something() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![run_agent_call("call-1", ERRAND), says("understood")]);
        let approvals = Answers::nobody();
        let delegate = Delegate::returning(SubAgentStatus::Done, "had a look", 2);

        run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &[crate::agent::sub_agents::RUN_AGENT_TOOL]),
            delegating(&approvals, &delegate),
        )
        .await;

        let result = provider.requests()[1].0.last().unwrap().content.clone();
        assert!(!result.contains("never saw"), "{result}");
    }

    fn owed(pool: &DbPool, asking: &str) -> Option<crate::agent::interrupted::Report> {
        let mut conn = pool.get().unwrap();
        let idle = crate::turn::TurnCoordinator::default();
        crate::agent::interrupted::block(&mut conn, &idle, "c1", Some(asking)).unwrap()
    }

    fn ended(pool: &DbPool, turn: &str, status: crate::db::models::turn::TurnStatus, at: i64) {
        let mut conn = pool.get().unwrap();
        crate::db::ops::turn::finish(&mut conn, turn, status, None, at).unwrap();
    }

    /// The notice that an earlier turn may have left a tool half-run is retired
    /// by a reply that was read all the way to the end, and by nothing else.
    ///
    /// The case that matters is not a request that failed to go out — that one
    /// is obvious. It is a stream that opened, said something, and was then
    /// stopped: `consume_stream` hands that back as `Ok`, and reading `Ok` as
    /// "the model received the warning" would retire it in favour of a request
    /// nobody finished reading. What the model does next is the thing the
    /// warning was protecting.
    #[tokio::test]
    async fn a_stopped_reply_does_not_retire_the_interruption_notice() {
        let pool = test_db();
        conversation(&pool);
        {
            let mut conn = pool.get().unwrap();
            crate::db::ops::turn::begin(&mut conn, "t0", "c1", TurnOrigin::Desktop, None, 500).unwrap();
        }
        let report = owed(&pool, "t1").expect("t0 is running and held by nobody, so it counts as cut off");
        let (tools, mcp) = (registry(), McpRegistry::new());
        let approvals = Answers::nobody();

        // Round one is stopped the moment the first chunk lands.
        let cancel = CancellationToken::new();
        let stopper = StopOnText(cancel.clone());
        let interrupted_run = Scripted::stalling(vec![vec![
            StreamEvent::Text {
                content: "I was about to".into(),
            },
            StreamEvent::Text {
                content: "never sent".into(),
            },
        ]]);
        let mut first = setup(&interrupted_run, &pool, &cancel, &[]);
        first.interrupted = Some(report);
        let outcome = run_turn(&services(&pool, &tools, &mcp), first, ports(&approvals, Some(&stopper))).await;
        assert!(outcome.reply.is_ok(), "being stopped is not a failure");

        ended(&pool, "t1", crate::db::models::turn::TurnStatus::Cancelled, 1500);
        let still_owed = owed(&pool, "t2");
        assert!(
            still_owed.is_some(),
            "the reply was never read to the end, so it consumed nothing",
        );

        // Round two reads one all the way through.
        let cancel = CancellationToken::new();
        let answering = Scripted::of(vec![says("understood")]);
        let mut second = setup(&answering, &pool, &cancel, &[]);
        second.interrupted = still_owed;
        second.turn_id = "t2".into();
        {
            let mut conn = pool.get().unwrap();
            crate::db::ops::turn::begin(&mut conn, "t2", "c1", TurnOrigin::Desktop, None, 2000).unwrap();
        }
        assert!(
            run_turn(&services(&pool, &tools, &mcp), second, ports(&approvals, None))
                .await
                .reply
                .is_ok()
        );

        ended(&pool, "t2", crate::db::models::turn::TurnStatus::Done, 2500);
        assert!(owed(&pool, "t3").is_none(), "and that one does retire it");
    }

    /// Stops the turn from inside the stream, which is the only way to reach
    /// "read part of a reply and then stopped" without a race.
    struct StopOnText(CancellationToken);

    impl Emit for StopOnText {
        fn emit(&self, _channel: &str, payload: serde_json::Value) -> Result<(), String> {
            if payload["type"] == "text" {
                self.0.cancel();
            }
            Ok(())
        }
    }

    /// The prompt and the output allowance are charged against one window by
    /// most providers, so asking for the model's advertised maximum on top of a
    /// long conversation is a request that has to be refused — while the
    /// conversation itself would have fitted.
    #[tokio::test]
    async fn the_output_allowance_is_trimmed_to_what_the_prompt_left() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![says("brief")]);
        let approvals = Answers::nobody();

        let mut s = setup(&provider, &pool, &cancel, &[]);
        s.params.max_tokens = Some(128_000);
        s.budget = TokenBudget::new("openai", "m", 256_000, 128_000, None);
        // A prompt big enough that the full allowance would not fit behind it.
        s.chat_messages.push(ChatMessage::user(&"word ".repeat(210_000)));

        run_turn(&services(&pool, &tools, &mcp), s, ports(&approvals, None)).await;

        let asked = provider.ceilings()[0].expect("a ceiling is always sent");
        assert!(asked < 128_000, "still asked for the advertised maximum: {asked}");
        assert!(asked > 0);
        // And what it asked for is what was actually left.
        let prompt = TokenBudget::new("openai", "m", 256_000, 128_000, None)
            .counter
            .count_messages(&provider.requests()[0].0);
        assert!(
            prompt + asked as usize <= 256_000,
            "prompt {prompt} + reply {asked} still overruns the window",
        );
    }

    /// The other end of the same rule. A prompt that already fills the window
    /// has nowhere to put an answer, and no output allowance rescues it — least
    /// of all a small one invented to avoid sending a zero. So the request is
    /// not made at all, and what comes back names the numbers.
    #[tokio::test]
    async fn a_prompt_that_fills_the_window_is_never_sent() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![says("never reached")]);
        let approvals = Answers::nobody();

        let mut s = setup(&provider, &pool, &cancel, &[]);
        s.params.max_tokens = Some(8_000);
        s.budget = TokenBudget::new("openai", "m", 32_000, 8_000, None);
        s.context_limit = 32_000;
        // One message larger than the whole window: nothing compaction does to
        // the list around it can make room.
        s.chat_messages.push(ChatMessage::user(&"word ".repeat(40_000)));

        let out = run_turn(&services(&pool, &tools, &mcp), s, ports(&approvals, None)).await;

        let err = out
            .reply
            .expect_err("a turn with no room to answer in is not a success");
        assert!(err.contains("fills the context window"), "unhelpful: {err}");
        assert!(
            provider.ceilings().is_empty(),
            "a request was sent anyway: {:?}",
            provider.ceilings()
        );
    }

    /// Recovery gets one attempt, so a second request that cannot be served
    /// spends it on nothing. Reachable because compaction is not guaranteed to
    /// shrink anything: a summariser that returns more than it replaced leaves
    /// the turn worse off than the refusal did.
    #[tokio::test]
    async fn recovery_that_frees_nothing_fails_instead_of_asking_again() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![vec![StreamEvent::Error {
            message: "context_length_exceeded".into(),
        }]])
        .summarising_to(&"word ".repeat(40_000));
        let approvals = Answers::nobody();

        let mut s = setup(&provider, &pool, &cancel, &[]);
        s.params.max_tokens = Some(8_000);
        s.budget = TokenBudget::new("openai", "m", 32_000, 8_000, None);
        s.context_limit = 32_000;
        s.keep_recent = 2;
        s.compaction = CompactionPolicy::Desktop {
            enabled: true,
            breaker: std::sync::Arc::new(crate::agent::CompactCircuitBreaker::new()),
        };
        // Over the threshold so the summariser is what recovery reaches for, and
        // under the window so the first request is legitimately made.
        for _ in 0..7 {
            s.chat_messages.push(ChatMessage::user(&"word ".repeat(4_000)));
        }

        let out = run_turn(&services(&pool, &tools, &mcp), s, ports(&approvals, None)).await;

        let err = out.reply.expect_err("there was no room for the second request either");
        assert!(err.contains("recovery failed"), "not attributed to recovery: {err}");
        assert!(err.contains("fills the context window"), "unhelpful: {err}");
        assert_eq!(provider.rounds(), 1, "asked again with nowhere to put the answer");
    }

    /// A short conversation is not penalised for the long ones' sake.
    #[tokio::test]
    async fn a_short_prompt_still_gets_the_whole_allowance() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![says("hi")]);
        let approvals = Answers::nobody();

        let mut s = setup(&provider, &pool, &cancel, &[]);
        s.params.max_tokens = Some(8_000);
        s.budget = TokenBudget::new("openai", "m", 256_000, 8_000, None);

        run_turn(&services(&pool, &tools, &mcp), s, ports(&approvals, None)).await;

        assert_eq!(provider.ceilings(), [Some(8_000)]);
    }

    /// `pause_turn` is the Messages API stopping while its own tool runs long.
    /// The same request goes back with the round's blocks appended, and the
    /// blocks travel in `provider_state` — so the resumed request has to
    /// carry the assistant row that holds them, or the search starts over.
    #[tokio::test]
    async fn a_pause_turn_is_resumed_with_the_rounds_blocks_on_the_request() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let paused = vec![
            StreamEvent::ProviderStateUpdate {
                update: crate::provider::state::ProviderStateUpdate::AnthropicContentBlock {
                    model: "claude-opus-5".into(),
                    position: 0,
                    block_json:
                        r#"{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{"query":"x"}}"#
                            .into(),
                },
            },
            StreamEvent::Stop {
                reason: "pause_turn".into(),
                usage: None,
            },
        ];
        let provider = Scripted::of(vec![paused, says("found it")]);
        let approvals = Answers::nobody();

        let outcome = run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &["run_command"]),
            ports(&approvals, None),
        )
        .await;
        assert_eq!(outcome.reply.as_deref(), Ok("found it"));
        assert_eq!(provider.rounds(), 2, "resumed once, then answered");

        let (resumed_with, _) = &provider.requests()[1];
        let paused_row = resumed_with
            .iter()
            .rev()
            .find(|m| m.role == "assistant")
            .expect("the paused round goes back on the request");
        assert!(
            paused_row
                .provider_state
                .as_ref()
                .is_some_and(|s| s.anthropic_blocks_for("claude-opus-5").is_some()),
            "with the blocks the adapter replays"
        );
        assert_eq!(outcome.chat_stop_reason(), ChatStopReason::EndTurn);
    }

    /// A server that pauses on every round is not a conversation; the cap is
    /// what keeps it from being a loop the loop guard cannot see.
    #[tokio::test]
    async fn pause_turn_resumption_is_bounded() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let pause = || {
            vec![StreamEvent::Stop {
                reason: "pause_turn".into(),
                usage: None,
            }]
        };
        let provider = Scripted::of((0..MAX_PAUSE_CONTINUATIONS + 5).map(|_| pause()).collect());
        let approvals = Answers::nobody();

        let outcome = run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &["run_command"]),
            ports(&approvals, None),
        )
        .await;
        assert!(outcome.reply.is_ok());
        assert_eq!(provider.rounds(), MAX_PAUSE_CONTINUATIONS + 1);
    }

    /// A refusal and a length cut-off are stops with a name, not errors and
    /// not ordinary ends: the reader is told which.
    #[tokio::test]
    async fn a_refusal_and_a_truncation_are_reported_by_name() {
        for (reason, expected) in [
            ("refusal", ChatStopReason::Refusal),
            ("content_filter", ChatStopReason::Refusal),
            ("max_tokens", ChatStopReason::MaxTokens),
            ("length", ChatStopReason::MaxTokens),
            ("end_turn", ChatStopReason::EndTurn),
        ] {
            let pool = test_db();
            conversation(&pool);
            let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
            let provider = Scripted::of(vec![vec![StreamEvent::Stop {
                reason: reason.into(),
                usage: None,
            }]]);
            let approvals = Answers::nobody();
            let outcome = run_turn(
                &services(&pool, &tools, &mcp),
                setup(&provider, &pool, &cancel, &["run_command"]),
                ports(&approvals, None),
            )
            .await;
            assert!(outcome.reply.is_ok(), "{reason} is not an error");
            assert_eq!(outcome.chat_stop_reason(), expected, "{reason}");
        }
    }

    /// A stream cut short mid-argument would otherwise be persisted as a
    /// fragment and fail every later context rebuild of the conversation. The
    /// stored copy is normalised to an empty object — which the strict read
    /// contract accepts — while dispatch still sees the original arguments and
    /// answers the call as the ordinary invalid-arguments error the model
    /// already recovers from.
    #[tokio::test]
    async fn a_call_whose_arguments_never_arrived_intact_is_stored_empty() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let truncated = vec![
            StreamEvent::ToolCallStart {
                index: 0,
                id: "c1".into(),
                name: "run_command".into(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                arguments: r#"{"command":"echo half"#.into(),
            },
            StreamEvent::Stop {
                reason: "stop".into(),
                usage: None,
            },
        ];
        let provider = Scripted::of(vec![truncated, says("let me retry")]);
        let approvals = Answers::nobody();

        let outcome = run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &["run_command"]),
            ports(&approvals, None),
        )
        .await;
        assert!(outcome.reply.is_ok());

        // The row on disk is parseable under the read contract.
        let assistant = rows(&pool)
            .into_iter()
            .find(|m| m.role == "assistant" && m.tool_calls.is_some())
            .unwrap();
        let stored = crate::agent::tool_calls::parse_stored_tool_calls(
            assistant.schema_version,
            assistant.tool_calls.as_deref(),
        )
        .expect("the stored tool_calls must satisfy the read contract");
        assert_eq!(stored[0].arguments, "{}", "the fragment is not persisted");

        // And the original arguments reached dispatch, which answered the call
        // as invalid rather than running it.
        let tool_row = rows(&pool).into_iter().find(|m| m.role == "tool").unwrap();
        assert!(
            tool_row.content.contains("invalid tool arguments JSON"),
            "dispatch answered the original arguments: {:?}",
            tool_row.content
        );
        assert_eq!(tool_row.tool_outcome.as_deref(), Some("error"));
    }

    /// Usage is accumulated across every round, not taken from the last one.
    #[tokio::test]
    async fn the_tokens_of_every_round_are_added_up() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let used = |p: i32, c: i32| StreamEvent::Stop {
            reason: "stop".into(),
            usage: Some(TokenUsage {
                prompt_tokens: Some(p),
                completion_tokens: Some(c),
                ..Default::default()
            }),
        };
        let provider = Scripted::of(vec![
            vec![
                StreamEvent::ToolCallStart {
                    index: 0,
                    id: "c".into(),
                    name: "fixture".into(),
                },
                StreamEvent::ToolCallDone {
                    index: 0,
                    arguments: "{}".into(),
                },
                used(100, 10),
            ],
            vec![StreamEvent::Text { content: "done".into() }, used(200, 20)],
        ]);
        let approvals = Answers::nobody();
        let fixture = Fixture::returning("ok");

        let outcome = run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &["fixture"]),
            TurnPorts {
                surface_tools: Some(&fixture),
                ..ports(&approvals, None)
            },
        )
        .await;

        assert_eq!(outcome.progress.input_tokens, 300);
        assert_eq!(outcome.progress.output_tokens, 30);
    }

    /// The whole chain in one test: a provider reports a cache hit, the loop
    /// carries it past the two places that used to drop it, and the row says so
    /// afterwards.
    ///
    /// Two rounds with different numbers, because a row records its own round
    /// while `TurnProgress` records the turn. A version that stored the turn
    /// total on every row would pass a single-round test.
    #[tokio::test]
    async fn a_cache_hit_reaches_the_row_that_got_it() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let used = |p: i32, c: i32, read: i32| StreamEvent::Stop {
            reason: "stop".into(),
            usage: Some(TokenUsage {
                prompt_tokens: Some(p),
                completion_tokens: Some(c),
                cache_read_tokens: Some(read),
                ..Default::default()
            }),
        };
        let provider = Scripted::of(vec![
            vec![
                StreamEvent::ToolCallStart {
                    index: 0,
                    id: "c".into(),
                    name: "fixture".into(),
                },
                StreamEvent::ToolCallDone {
                    index: 0,
                    arguments: "{}".into(),
                },
                used(100, 10, 0),
            ],
            vec![StreamEvent::Text { content: "done".into() }, used(200, 20, 180)],
        ]);
        let approvals = Answers::nobody();
        let fixture = Fixture::returning("ok");

        let outcome = run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &["fixture"]),
            TurnPorts {
                surface_tools: Some(&fixture),
                ..ports(&approvals, None)
            },
        )
        .await;
        assert!(outcome.reply.is_ok());

        let assistant: Vec<_> = rows(&pool).into_iter().filter(|m| m.role == "assistant").collect();
        assert_eq!(assistant.len(), 2, "one row per round");
        // The cold round is stored as a reported zero, not as absent: the
        // provider said nothing was cached, which is a different claim from
        // saying nothing at all.
        assert_eq!(assistant[0].cache_read_tokens, Some(0));
        assert_eq!(assistant[1].cache_read_tokens, Some(180));
        assert_eq!(
            assistant[1].input_tokens,
            Some(200),
            "the read is a subset of the prompt, not an addition to it",
        );
        assert_eq!(outcome.progress.cache_read_tokens, 180, "summed across rounds");
        assert_eq!(outcome.progress.cache_write_tokens, 0);
    }

    /// An upstream that says nothing about caching leaves the columns empty
    /// rather than zero. A hit rate that cannot tell the two apart reports every
    /// such reply as a total cache miss — a claim about the provider rather than
    /// about the data.
    #[tokio::test]
    async fn a_provider_that_says_nothing_about_caching_stores_nothing() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![vec![
            StreamEvent::Text { content: "hi".into() },
            StreamEvent::Stop {
                reason: "stop".into(),
                usage: Some(TokenUsage {
                    prompt_tokens: Some(50),
                    completion_tokens: Some(5),
                    ..Default::default()
                }),
            },
        ]]);
        let approvals = Answers::nobody();

        run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &[]),
            ports(&approvals, None),
        )
        .await;

        let row = rows(&pool).into_iter().find(|m| m.role == "assistant").unwrap();
        assert_eq!(row.input_tokens, Some(50));
        assert_eq!(row.cache_read_tokens, None);
        assert_eq!(row.cache_write_tokens, None);
    }

    /// A tool the turn did not offer is refused by the loop, not by the
    /// registry. The registry still has it; the mode's pruning would be
    /// decorative if naming it anyway worked.
    #[tokio::test]
    async fn a_tool_that_was_not_offered_is_refused_before_anything_runs() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![calls("call-1", "fixture", "{}"), says("fine then")]);
        let approvals = Answers::nobody();
        let fixture = Fixture::returning("should not happen");

        let outcome = run_turn(
            &services(&pool, &tools, &mcp),
            // Deliberately not offered.
            setup(&provider, &pool, &cancel, &[]),
            TurnPorts {
                surface_tools: Some(&fixture),
                ..ports(&approvals, None)
            },
        )
        .await;

        assert!(outcome.reply.is_ok());
        assert!(fixture.ran.lock().unwrap().is_empty());
        let requests = provider.requests();
        let told = &requests[1].0.last().unwrap().content;
        assert!(told.contains("not available"), "{told}");
        assert!(
            told.contains("another tool"),
            "and it is told not to route around it: {told}"
        );
    }

    /// Answering a question is not granting permission.
    ///
    /// `Response` and `Approved` both mean somebody did something rather than
    /// nothing, which is what makes them easy to conflate — and conflating them
    /// turns a typed sentence into permission to run a command. Only `ask_user`
    /// asked a question, so only `ask_user` may read one as an answer.
    ///
    /// Sandbox escalation obeys the same rule and is not covered here: reaching
    /// it needs a registry tool that fails with a sandbox denial, and every
    /// fixture in this module is a surface tool. It is written the same way, and
    /// the port's own documentation is what holds it.
    #[tokio::test]
    async fn typed_words_answer_a_question_and_authorise_nothing_else() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![
            calls("call-1", "fixture", "{}"),
            calls("call-2", "mcp__server__do", "{}"),
            calls("call-3", "ask_user", "{}"),
            says("fine"),
        ]);
        let approvals = Answers::saying(Some(ApprovalDecision::Response("go on then".into())));
        let fixture = Fixture::returning("ran anyway").asking_first();

        run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &["fixture", "mcp__server__do", "ask_user"]),
            TurnPorts {
                surface_tools: Some(&fixture),
                ..ports(&approvals, None)
            },
        )
        .await;

        let said = |round: usize| provider.requests()[round].0.last().unwrap().content.clone();

        assert!(
            fixture.ran.lock().unwrap().is_empty(),
            "a surface tool is not authorised"
        );
        // Not authorised — and not reported as a user's refusal either, since
        // a stray `Response` is no more a denial than silence is.
        assert_eq!(said(1), UNANSWERED_APPROVAL);
        // Never reached the registry, so the refusal is the approval's and not
        // an "unknown MCP server" from further down.
        assert_eq!(said(2), UNANSWERED_APPROVAL, "an MCP tool is not authorised either");
        assert_eq!(said(3), "go on then", "but the question that was asked gets its answer");
    }

    /// An unanswered card is not a yes. The distinction matters most on the
    /// path where saying yes runs a command.
    #[tokio::test]
    async fn a_tool_nobody_approved_is_not_run() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![calls("call-1", "fixture", "{}"), says("fine")]);
        let approvals = Answers::nobody();
        let fixture = Fixture::returning("ran anyway").asking_first();

        run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &["fixture"]),
            TurnPorts {
                surface_tools: Some(&fixture),
                ..ports(&approvals, None)
            },
        )
        .await;

        assert_eq!(*approvals.asked.lock().unwrap(), [("fixture".to_string(), None)]);
        assert!(fixture.ran.lock().unwrap().is_empty());
        // The tool did not run, and the model is told *why it did not run*
        // truthfully: nobody answered. "Denied by user" here was an expiry
        // being attributed to a person who made no decision — with a TTL on
        // every card, that misattribution would fire on every timeout.
        let told = provider.requests()[1].0.last().unwrap().content.clone();
        assert_eq!(told, UNANSWERED_APPROVAL);
        assert!(
            !told.contains("denied by user"),
            "an unanswered question must not be reported as a user's decision"
        );
    }

    /// The same truth on the ordinary registry path, which is the one that
    /// runs commands. The surface test above cannot stand in for it: the two
    /// branches carry separate wording, and a fix to one leaves the other
    /// still telling the model a person refused.
    #[tokio::test]
    async fn an_unanswered_registry_tool_is_not_reported_as_a_users_refusal() {
        let pool = test_db();
        conversation(&pool);
        let (tools, mcp, cancel) = (registry(), McpRegistry::new(), CancellationToken::new());
        let provider = Scripted::of(vec![
            calls("call-1", "run_command", r#"{"command":"echo hi"}"#),
            says("fine"),
        ]);
        let approvals = Answers::nobody();

        run_turn(
            &services(&pool, &tools, &mcp),
            setup(&provider, &pool, &cancel, &["run_command"]),
            ports(&approvals, None),
        )
        .await;

        assert_eq!(
            *approvals.asked.lock().unwrap(),
            [("run_command".to_string(), None)],
            "the question was asked, once, with no retry framing"
        );
        let told = provider.requests()[1].0.last().unwrap().content.clone();
        assert_eq!(told, UNANSWERED_APPROVAL);
    }
}

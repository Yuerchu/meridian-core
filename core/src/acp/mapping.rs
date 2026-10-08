//! Turning what the agent narrates into what this app draws.
//!
//! Kept pure and separate from [`super::session`] so the translation can be
//! tested without a child process, a database or a window — which matters
//! because it is the layer most likely to drift when the adapter adds an update
//! kind.
//!
//! The target shapes are not ours to choose: `chat-stream` already has a
//! vocabulary that `chat-view` understands (`turn.rs` emits `message_start`,
//! `text`, `tool_call`, `tool_result`; `stream.rs` emits `reasoning`). Inventing
//! a parallel one for ACP would mean a second renderer.

use super::protocol::{PlanEntry, SessionConfigOption, SessionFailureRecord, SessionUpdate, ToolCall, Usage};
use crate::events::{AcpNoticeAction, AcpNoticeCategory, AcpNoticeSeverity, ToolCallDiff, ToolOutcome};

/// What one `session/update` means here.
///
/// One update maps to at most one effect through [`effect_of`], with the one
/// exception [`effects_of`] exists for; the variants this step does not draw
/// collapse to [`Effect::Ignored`] rather than being an error, for the same
/// reason the parse is lax.
#[derive(Debug, PartialEq)]
pub enum Effect {
    /// Visible prose from the agent.
    Text { message_id: Option<String>, text: String },
    /// Its thinking, drawn separately.
    Reasoning { message_id: Option<String>, text: String },
    /// Something a *person* said.
    ///
    /// On the live path this is the agent echoing the prompt back, and the
    /// session drops it — that row was written before the prompt was ever sent.
    /// On a replay it is the other half of the transcript and the only record
    /// of it there is. The difference is a fact about the caller, not about the
    /// update, which is why it is no longer decided here.
    UserText { message_id: Option<String>, text: String },
    /// A call has started. `arguments` is JSON, because that is what the tool
    /// card renders and what `PendingApproval` stores.
    ToolCall {
        call_id: String,
        tool_name: String,
        arguments: String,
    },
    /// A call already announced has better information about itself.
    ///
    /// The adapter emits a call as soon as it knows one is coming, which can be
    /// before the arguments have finished streaming — a `Bash` call arrives
    /// titled "Terminal" with `rawInput: {}` and is filled in a moment later.
    /// Without this the card keeps the placeholder for ever.
    ToolCallRevised {
        call_id: String,
        tool_name: String,
        arguments: String,
    },
    /// A call has finished, one way or the other.
    ToolResult {
        call_id: String,
        result: String,
        /// `success` or `error`, matching `messages.tool_outcome`.
        outcome: ToolOutcome,
    },
    /// The agent's own todo list.
    Plan(Vec<PlanItem>),
    /// Context usage. Reported, never priced — see `Usage`'s `cost`.
    Usage { used: u64, size: u64 },
    /// The agent has described its configuration knobs.
    ///
    /// Carries the whole set rather than the model alone, because the composer
    /// offers them: the values a `select` accepts are the agent's to decide and
    /// change under us — picking a model re-derives which modes exist. The
    /// model is read back out of this by the session, which is also what keeps
    /// there from being two sources for it.
    ConfigOptions(Vec<SessionConfigOption>),
    /// The agent reported an incident about itself — a failure, or a warning
    /// on the way to one — through the AIR `sessionFailure` extension.
    SessionNotice(SessionNoticeRecord),
    /// The agent named the conversation. Arrives once it has generated a
    /// title, and again on later turns only if the title changed.
    SessionTitle(String),
    /// The agent reported what an Edit or Write actually changed: one hunk
    /// per block, the whole list for the call, sent after the tool ran and
    /// before the call is marked complete. Replaces anything held for the
    /// call — the adapter's `content` is a whole-array replacement.
    ToolCallDiff { call_id: String, diffs: Vec<ToolCallDiff> },
    /// A call that has been announced but has not finished, and everything this
    /// step does not draw.
    Ignored,
}

#[derive(Debug, PartialEq, serde::Serialize)]
pub struct PlanItem {
    pub content: String,
    pub status: String,
}

/// One incident, in this app's closed vocabulary. The wire form
/// ([`SessionFailureRecord`]) carries strings; this is what survives
/// [`notice_of`], so nothing downstream has to re-validate a category.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionNoticeRecord {
    /// The adapter's own id for the incident, stable across revisions.
    pub notice_id: String,
    pub revision: u32,
    pub category: AcpNoticeCategory,
    pub severity: AcpNoticeSeverity,
    pub title: String,
    pub details: Option<String>,
    pub reason: Option<String>,
    pub actions: Vec<AcpNoticeAction>,
}

/// Read an AIR incident record into the closed form, or refuse it.
///
/// Refused rather than defaulted: an unknown `category` stored as `unknown`
/// would pass the database's own CHECK and then be drawn as something it is
/// not, and an unknown `severity` has no honest default at all — `warning`
/// hides a failure, `error` fails a turn that finished. An unknown *action*
/// is different: the list is advice, and the spec tells a client to drop
/// what it does not know, so that one is dropped with a warning.
///
/// Shared by the two carriers — a `session_info_update` and the reply to
/// `session/prompt` — so they cannot disagree about what a record means.
pub fn notice_of(record: &SessionFailureRecord) -> Option<SessionNoticeRecord> {
    let notice_id = record.id.trim();
    if notice_id.is_empty() {
        tracing::warn!("an ACP session failure record has no id; dropped");
        return None;
    }
    let category = match AcpNoticeCategory::parse(&record.category) {
        Ok(category) => category,
        Err(error) => {
            tracing::warn!(%error, notice_id, "an ACP session failure record was dropped");
            return None;
        }
    };
    let severity = match AcpNoticeSeverity::parse(&record.severity) {
        Ok(severity) => severity,
        Err(error) => {
            tracing::warn!(%error, notice_id, "an ACP session failure record was dropped");
            return None;
        }
    };
    let actions = record
        .actions
        .iter()
        .filter_map(|action| match AcpNoticeAction::parse(action) {
            Ok(action) => Some(action),
            Err(error) => {
                tracing::warn!(%error, notice_id, "an ACP notice action was dropped");
                None
            }
        })
        .collect();
    Some(SessionNoticeRecord {
        notice_id: notice_id.to_string(),
        revision: record.revision,
        category,
        severity,
        title: record.title.trim().to_string(),
        details: record
            .details
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_string),
        reason: record
            .reason
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(str::to_string),
        actions,
    })
}

/// Everything one update means, in the order it has to be applied.
///
/// One effect, except for a `tool_call` that arrives already finished. ACP
/// lets a call's first announcement carry a terminal `status`, and the adapter
/// uses it: `memory_recall` is announced once, `completed`, and never updated;
/// a compaction whose start was missed is announced at its end. Read as a bare
/// call, that is a card no result will ever close and a round with more calls
/// than results — which never settles, so every later round of the turn lands
/// on the same row and the transcript reader draws the finished turn as
/// `interrupted`. So the call and its result are both said here, call first.
///
/// **An announcement can carry the diff too**, and for an AIR client it is the
/// only place an Edit's text is before it runs: from 0.82 its `rawInput` leaves
/// out `old_string` and `new_string` ("the file text of an edit only in the
/// diff"). Read as a bare call, the approval card for an edit showed nothing
/// of what it would change. The diff is said after the call, so it lands on the
/// call it belongs to.
///
/// **And an update can carry the diff beside the finished arguments.** Without
/// the AIR envelope, the frame that completes an Edit's `rawInput` — the first
/// with `new_string` in it — is the one that brings the diff. Read as a diff
/// alone, the arguments stored for the call stopped at the frame before, with
/// `old_string` and no `new_string`. So that frame is a revision and then a
/// diff, in that order.
///
/// **And an update is every one of the things it carries, too.** From 0.82 an
/// AIR client is sent no `rawInput` while the input streams; it comes once,
/// complete, on a `tool_call_update` that also carries the diff. Read as one
/// effect it was only the diff, so `file_path` never reached the call and
/// every hosted edit showed a change with no file named. That was the third
/// time one AIR frame carried two facts and one was dropped — the tool name
/// and the first announcement's diff were the others — so this reads each part
/// of an update rather than choosing one: the revision, then the diff, then
/// the result, which is the order they have to land in. See
/// `tests::no_part_of_a_tool_call_update_is_dropped` for the gate.
pub fn effects_of(update: SessionUpdate) -> Vec<Effect> {
    if let SessionUpdate::ToolCallUpdate(call) = &update {
        let mut effects = Vec::new();
        if revises(call) {
            effects.push(Effect::ToolCallRevised {
                call_id: call.tool_call_id.clone(),
                tool_name: explicit_tool_name(call).unwrap_or_default().to_string(),
                arguments: arguments_of(call),
            });
        }
        if has_diff(call) {
            effects.push(Effect::ToolCallDiff {
                call_id: call.tool_call_id.clone(),
                diffs: diffs_of(call),
            });
        }
        effects.extend(result_of(call));
        if effects.is_empty() {
            effects.push(Effect::Ignored);
        }
        return effects;
    }
    let (diffs, finished) = match &update {
        SessionUpdate::ToolCall(call) => {
            let diffs = diffs_of(call);
            let diffs = (!diffs.is_empty()).then(|| Effect::ToolCallDiff {
                call_id: call.tool_call_id.clone(),
                diffs,
            });
            (diffs, result_of(call))
        }
        _ => (None, None),
    };
    let mut effects = vec![effect_of(update)];
    effects.extend(diffs);
    effects.extend(finished);
    effects
}

/// Whether an update says something new about the call itself: input that is
/// more than `{}`, or — on a frame that is nothing else — the call's name.
///
/// Not every `_meta` is a revision. The adapter repeats the tool name on
/// most frames, the finishing one included, and a "revision" of `{}` there
/// is not harmless downstream of `revise`: anything reading the last revision
/// as the call's arguments reads it as the call having had none.
fn revises(call: &ToolCall) -> bool {
    let input = call
        .raw_input
        .as_ref()
        .is_some_and(|v| !(v.is_null() || v.as_object().is_some_and(|o| o.is_empty())));
    let renamed = explicit_tool_name(call).is_some_and(|n| !n.is_empty());
    input || (renamed && !has_diff(call) && result_of(call).is_none())
}

/// Whether an update carries what an Edit or Write changed.
fn has_diff(call: &ToolCall) -> bool {
    call.content.iter().any(|b| b.kind == "diff")
}

/// The result a call's status reports, if it reports one.
///
/// **A refusal is `failed` on the wire and `Denied` here.** The adapter sends
/// a call the user or a permission rule refused exactly as it sends one that
/// ran and broke, and says which in `_meta.claudeCode.nonExecutionKind`. Read
/// as an error, a "no" the user clicked a moment ago was drawn as the tool
/// failing. The other kinds (`interrupted`, `cancelled`, whatever ships next)
/// stay errors: the tool did not run, and nobody declined it.
fn result_of(call: &ToolCall) -> Option<Effect> {
    let outcome = match call.status.as_deref() {
        Some("completed") => ToolOutcome::Success,
        Some("failed") if refused(call) => ToolOutcome::Denied,
        Some("failed") => ToolOutcome::Error,
        _ => return None,
    };
    Some(Effect::ToolResult {
        call_id: call.tool_call_id.clone(),
        result: output_of(call),
        outcome,
    })
}

/// The first thing an update means. See [`effects_of`] for the one update that
/// means two things, and use that on every path that applies effects.
pub fn effect_of(update: SessionUpdate) -> Effect {
    match update {
        SessionUpdate::AgentMessageChunk { content, message_id } => match content.as_text() {
            Some(text) if !text.is_empty() => Effect::Text {
                message_id,
                text: text.to_string(),
            },
            _ => Effect::Ignored,
        },
        SessionUpdate::AgentThoughtChunk { content, message_id } => match content.as_text() {
            Some(text) if !text.is_empty() => Effect::Reasoning {
                message_id,
                text: text.to_string(),
            },
            _ => Effect::Ignored,
        },
        SessionUpdate::UserMessageChunk { content, message_id } => match content.as_text() {
            Some(text) if !text.is_empty() => Effect::UserText {
                message_id,
                text: text.to_string(),
            },
            _ => Effect::Ignored,
        },
        SessionUpdate::ToolCall(call) => Effect::ToolCall {
            call_id: call.tool_call_id.clone(),
            tool_name: tool_name_of(&call),
            arguments: arguments_of(&call),
        },
        SessionUpdate::ToolCallUpdate(call) => match result_of(&call) {
            Some(result) => result,
            // What an Edit or Write actually changed, once it has run: no
            // status, `content` replaced by one diff block per hunk. Asked
            // before the revision arm below, because this frame carries
            // `_meta` too and read as a revision it is one with `{}` for
            // arguments — which `revise` then ignores, and the diff with it.
            _ if has_diff(&call) => Effect::ToolCallDiff {
                call_id: call.tool_call_id.clone(),
                diffs: diffs_of(&call),
            },
            // Still running. Usually there is nothing to say — but this is also
            // how a call announced before its arguments were known gets them,
            // and how the adapter's second source revises one it did not emit.
            // An update carrying neither is the ordinary "still going" beat.
            _ if revises(&call) => Effect::ToolCallRevised {
                call_id: call.tool_call_id.clone(),
                // Empty is "unchanged", which is what `revise` reads it as.
                tool_name: explicit_tool_name(&call).unwrap_or_default().to_string(),
                arguments: arguments_of(&call),
            },
            _ => Effect::Ignored,
        },
        SessionUpdate::Plan { entries } => Effect::Plan(entries.iter().map(plan_item).collect()),
        SessionUpdate::UsageUpdate(Usage { used, size, .. }) => Effect::Usage { used, size },
        // Passed through whole, empty included: the schema defines this as "the
        // full set of configuration options", so an empty one says the agent
        // now offers none, and the session replaces what it held.
        SessionUpdate::ConfigOptionUpdate { config_options } => Effect::ConfigOptions(config_options),
        // An incident outranks a title. The adapter sends them in separate
        // updates, so this is only a rule about which to keep if that changes;
        // a title arriving beside a failure record is logged and waits for
        // the next turn end, when the adapter re-sends it if it changed.
        SessionUpdate::SessionInfoUpdate { meta, title, .. } => {
            if let Some(record) = meta.as_ref().and_then(|m| m.session_failure()) {
                if title.is_some() {
                    tracing::debug!(
                        "a session_info_update carried both a title and an incident; the title was skipped"
                    );
                }
                return notice_of(record).map_or(Effect::Ignored, Effect::SessionNotice);
            }
            match title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                Some(title) => Effect::SessionTitle(title.to_string()),
                None => Effect::Ignored,
            }
        }
        SessionUpdate::Unhandled => Effect::Ignored,
    }
}

/// The tool's name, when this message says it — and nothing else.
///
/// A revision has to use this rather than [`tool_name_of`]. ACP merges an
/// update into the call it names, a field left out means "unchanged", and from
/// 0.82 the adapter leaves out every field and `_meta` key that has not changed
/// for an AIR client. Falling back to `title`, `kind` and finally `"tool"`
/// there turned every card that received an update into one called "tool".
pub fn explicit_tool_name(call: &ToolCall) -> Option<&str> {
    call.meta
        .as_ref()
        .and_then(|m| m.claude_code.as_ref())
        .and_then(|c| c.tool_name.as_deref())
        .or(call.name.as_deref())
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

/// What to label the tool card with.
///
/// ACP's own fields cannot answer this: `title` is prose written for a person
/// and `kind` is one of five categories. The adapter puts the real name in
/// `_meta.claudeCode.toolName`, so that wins.
///
/// Reading `title` instead — which this did — produced "Terminal" on every
/// shell command, because that is the adapter's placeholder for a `Bash` call
/// whose input has not finished streaming (`tools.ts`: `input?.command ?
/// input.command : "Terminal"`). Once the input lands the title becomes the
/// command itself, which is not a tool name either: it belongs in the
/// arguments, and the card already renders those.
///
/// Public because an approval card labels the same call, and the two must not
/// disagree — a question naming the tool differently from the block it belongs
/// to reads as being about something else.
///
/// **`name` is the second place to look, and for a permission request usually
/// the only one.** The adapter puts `_meta.claudeCode.toolName` on a permission
/// request's call only for a sub-agent or an MCP server; an ordinary `Bash`
/// arrives with its name in `name` and its command as `title`. Falling
/// through to `title` there labelled the approval with the command line, so a
/// top-level `ExitPlanMode` — titled "Ready to code?" — never reached the plan
/// review, and the notification stack could not recognise a `Read` as a read.
pub fn tool_name_of(call: &ToolCall) -> String {
    explicit_tool_name(call)
        .or(call.title.as_deref())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .or(call.kind.as_deref())
        .unwrap_or("tool")
        .to_string()
}

/// The call's input, as the JSON string the card expects.
///
/// `rawInput` is optional in the protocol and absent in practice for some
/// adapters, so an empty object stands in — a card with no arguments is worth
/// drawing, and `arguments` is also what an approval quotes back to the user.
pub fn arguments_of(call: &ToolCall) -> String {
    call.raw_input
        .as_ref()
        .map(|v| v.to_string())
        .unwrap_or_else(|| "{}".to_string())
}

/// Everything textual the call produced, in order.
///
/// Non-text blocks (diffs, terminal handles, images) are skipped rather than
/// described. They are drawn from the tool card's own data in a later step; a
/// placeholder like `[diff]` in the result text would be indistinguishable from
/// output a command actually printed.
/// The diff blocks of a call, as hunks.
///
/// `locations` is the adapter's parallel list — one `{path, line}` per hunk,
/// `line` the hunk's first line after the edit — so block *i* takes location
/// *i*'s line only when the two lists are the same length and name the same
/// file. Anything less and the hunk goes unnumbered rather than numbered off
/// a location that belongs to a different hunk. A block with no `path` or no
/// `newText` is not a diff this app can draw and is left out.
pub fn diffs_of(call: &ToolCall) -> Vec<ToolCallDiff> {
    let blocks: Vec<&super::protocol::ToolCallContent> = call.content.iter().filter(|b| b.kind == "diff").collect();
    let lined = blocks.len() == call.locations.len();
    blocks
        .iter()
        .enumerate()
        .filter_map(|(i, block)| {
            let path = block.path.clone()?;
            let new_text = block.new_text.clone()?;
            let line = lined
                .then(|| &call.locations[i])
                .filter(|location| location.path == path)
                .and_then(|location| location.line);
            Some(ToolCallDiff {
                path,
                old_text: block.old_text.clone(),
                new_text,
                line,
            })
        })
        .collect()
}

fn refused(call: &ToolCall) -> bool {
    let kind = call
        .meta
        .as_ref()
        .and_then(|m| m.claude_code.as_ref())
        .and_then(|c| c.non_execution_kind.as_deref());
    matches!(kind, Some("user-rejected" | "permission-rule"))
}

/// What the call returned. `rawOutput` when it is a string, because that is
/// what the model saw — the same thing a native tool's result row holds —
/// where `content` is the adapter's display copy: a command's output fenced as
/// `console`, a Read fenced bare, a web search re-set as a list.
/// Stored fenced, every reader downstream (the card, the audit copy, search)
/// would have had to know to strip it. A structured `rawOutput` has no single
/// text to keep, so it falls back to `content`, as does an AIR client, which
/// gets `rawOutput` only when `content` is empty.
fn output_of(call: &ToolCall) -> String {
    if let Some(serde_json::Value::String(raw)) = &call.raw_output {
        return raw.clone();
    }
    let mut out = String::new();
    for block in &call.content {
        if let Some(text) = block.content.as_ref().and_then(|c| c.as_text()) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

fn plan_item(entry: &PlanEntry) -> PlanItem {
    PlanItem {
        content: entry.content.clone(),
        // The protocol's own default. An entry with no status is one that has
        // not been started, not one in an unknown state.
        status: entry.status.clone().unwrap_or_else(|| "pending".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::protocol::SessionNotification;

    fn update(raw: &str) -> SessionUpdate {
        let wrapped = format!(r#"{{"sessionId":"s1","update":{raw}}}"#);
        serde_json::from_str::<SessionNotification>(&wrapped).unwrap().update
    }

    #[test]
    fn prose_and_thinking_land_on_different_channels() {
        assert_eq!(
            effect_of(update(
                r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello"}}"#
            )),
            Effect::Text {
                message_id: None,
                text: "hello".into()
            }
        );
        assert_eq!(
            effect_of(update(
                r#"{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"hmm"}}"#
            )),
            Effect::Reasoning {
                message_id: None,
                text: "hmm".into()
            }
        );
    }

    /// What a person said is carried, not dropped. Whether to *draw* it is the
    /// caller's question and the two callers answer it differently: a live turn
    /// wrote that row before the prompt was sent and would double it, a replay
    /// has no other record of it. See `Shared::absorb`.
    #[test]
    fn what_the_user_said_is_carried_with_the_message_it_belongs_to() {
        assert_eq!(
            effect_of(update(
                r#"{"sessionUpdate":"user_message_chunk","messageId":"u-1",
                    "content":{"type":"text","text":"do it"}}"#
            )),
            Effect::UserText {
                message_id: Some("u-1".into()),
                text: "do it".into()
            }
        );
    }

    /// Which message a chunk belongs to, which is what tells one replayed row
    /// from the next. Nothing else in a replay marks the boundary.
    #[test]
    fn a_chunk_carries_the_id_of_the_message_it_belongs_to() {
        assert_eq!(
            effect_of(update(
                r#"{"sessionUpdate":"agent_message_chunk","messageId":"msg_01",
                    "content":{"type":"text","text":"hello"}}"#
            )),
            Effect::Text {
                message_id: Some("msg_01".into()),
                text: "hello".into()
            }
        );
    }

    /// An empty chunk is a keepalive, not a paragraph break. Emitted, it would
    /// be an empty bubble.
    #[test]
    fn an_empty_chunk_produces_nothing() {
        assert_eq!(
            effect_of(update(
                r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":""}}"#
            )),
            Effect::Ignored
        );
    }

    /// The real frames, as `@zed-industries/claude-code-acp` 0.16.2 sent them
    /// for one `Bash` call. Two `tool_call`s under one id, the first a
    /// placeholder — which is what drew every shell command twice, once as
    /// "Terminal" and once as itself.
    #[test]
    fn the_adapters_own_frames_name_the_tool_not_the_placeholder() {
        let announced = update(
            r#"{"_meta":{"claudeCode":{"toolName":"Bash"}},"toolCallId":"toolu_01Vq",
                "sessionUpdate":"tool_call","rawInput":{},"status":"pending",
                "title":"Terminal","kind":"execute","content":[]}"#,
        );
        assert_eq!(
            effect_of(announced),
            Effect::ToolCall {
                call_id: "toolu_01Vq".into(),
                // Not "Terminal": that is the adapter's stand-in for a command
                // it has not been told yet.
                tool_name: "Bash".into(),
                arguments: "{}".into(),
            }
        );

        // The same id again, now filled in. Must revise rather than announce.
        let filled = update(
            r#"{"_meta":{"claudeCode":{"toolName":"Bash"}},"toolCallId":"toolu_01Vq",
                "sessionUpdate":"tool_call_update","rawInput":{"command":"git status"},
                "status":"pending","title":"`git status`","kind":"execute"}"#,
        );
        assert_eq!(
            effect_of(filled),
            Effect::ToolCallRevised {
                call_id: "toolu_01Vq".into(),
                tool_name: "Bash".into(),
                arguments: r#"{"command":"git status"}"#.into(),
            }
        );
    }

    /// The refinement 0.84.0 sends an AIR client once an Edit's input is
    /// complete: the input (text fields left out, `file_path` kept) *and* the
    /// diff, on one update. Both have to land — the path is the only place the
    /// card learns which file it is changing.
    #[test]
    fn an_edit_refinement_names_the_file_as_well_as_the_change() {
        let refinement = update(
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"toolu_E",
                "_meta":{"claudeCode":{"toolName":"Edit"}},
                "rawInput":{"file_path":"C:\\repo\\src\\lib.rs","replace_all":false},
                "title":"Edit lib.rs","kind":"edit",
                "content":[{"type":"diff","path":"C:\\repo\\src\\lib.rs","oldText":"a","newText":"b"}]}"#,
        );
        let effects = effects_of(refinement);
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::ToolCallRevised { arguments, .. }, Effect::ToolCallDiff { .. }]
                    if arguments.contains("file_path")
            ),
            "the input, then the diff: {effects:?}"
        );
    }

    /// **The gate for the class, not the instance.** Every combination of the
    /// three things a `tool_call_update` can carry — input, a diff, a
    /// terminal status — has to come out as every one of them, in the order
    /// they land. Three bugs shipped from a frame that carried two of these and
    /// was read as one.
    #[test]
    fn no_part_of_a_tool_call_update_is_dropped() {
        for input in [false, true] {
            for diff in [false, true] {
                for done in [false, true] {
                    let mut frame = serde_json::json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t" });
                    if input {
                        frame["rawInput"] = serde_json::json!({ "file_path": "/repo/a.rs" });
                    }
                    if diff {
                        frame["content"] = serde_json::json!([{ "type": "diff", "path": "/repo/a.rs", "oldText": "a", "newText": "b" }]);
                    }
                    if done {
                        frame["status"] = serde_json::json!("completed");
                    }
                    let effects = effects_of(update(&frame.to_string()));
                    let kinds: Vec<&str> = effects
                        .iter()
                        .map(|e| match e {
                            Effect::ToolCallRevised { .. } => "revised",
                            Effect::ToolCallDiff { .. } => "diff",
                            Effect::ToolResult { .. } => "result",
                            Effect::Ignored => "ignored",
                            _ => "other",
                        })
                        .collect();
                    let mut expected: Vec<&str> = Vec::new();
                    if input {
                        expected.push("revised");
                    }
                    if diff {
                        expected.push("diff");
                    }
                    if done {
                        expected.push("result");
                    }
                    if expected.is_empty() {
                        expected.push("ignored");
                    }
                    assert_eq!(kinds, expected, "input={input} diff={diff} done={done}");
                }
            }
        }
    }

    /// A beat that carries nothing new is still just a beat. Treated as a
    /// revision it would blank the arguments already on the card.
    #[test]
    fn a_bare_progress_update_revises_nothing() {
        assert_eq!(
            effect_of(update(
                r#"{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"in_progress"}"#
            )),
            Effect::Ignored
        );
    }

    #[test]
    fn a_tool_call_is_labelled_with_its_title() {
        let effect = effect_of(update(
            r#"{"sessionUpdate":"tool_call","toolCallId":"t1","title":"Run npm test",
                "kind":"execute","status":"pending","rawInput":{"command":"npm test"}}"#,
        ));
        assert_eq!(
            effect,
            Effect::ToolCall {
                call_id: "t1".into(),
                tool_name: "Run npm test".into(),
                arguments: r#"{"command":"npm test"}"#.into(),
            }
        );
    }

    /// Without a title there is still a card to draw, and without rawInput the
    /// card still needs valid JSON — an approval quotes this back to the user.
    #[test]
    fn a_tool_call_missing_its_optional_fields_still_produces_a_card() {
        let effect = effect_of(update(
            r#"{"sessionUpdate":"tool_call","toolCallId":"t1","kind":"read","status":"pending"}"#,
        ));
        assert_eq!(
            effect,
            Effect::ToolCall {
                call_id: "t1".into(),
                tool_name: "read".into(),
                arguments: "{}".into(),
            }
        );

        let effect = effect_of(update(
            r#"{"sessionUpdate":"tool_call","toolCallId":"t2","status":"pending"}"#,
        ));
        match effect {
            Effect::ToolCall { tool_name, .. } => assert_eq!(tool_name, "tool"),
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    /// Only a finished call produces a result. `in_progress` arrives for the
    /// same id first, and turning that into a result would close the card while
    /// the command is still running.
    #[test]
    fn only_a_finished_call_produces_a_result() {
        assert_eq!(
            effect_of(update(
                r#"{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"in_progress"}"#
            )),
            Effect::Ignored
        );

        assert_eq!(
            effect_of(update(
                r#"{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed",
                    "content":[{"type":"content","content":{"type":"text","text":"3 passed"}}]}"#
            )),
            Effect::ToolResult {
                call_id: "t1".into(),
                result: "3 passed".into(),
                outcome: ToolOutcome::Success,
            }
        );

        assert_eq!(
            effect_of(update(
                r#"{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"failed",
                    "content":[{"type":"content","content":{"type":"text","text":"exit 1"}}]}"#
            )),
            Effect::ToolResult {
                call_id: "t1".into(),
                result: "exit 1".into(),
                outcome: ToolOutcome::Error,
            }
        );
    }

    /// The three frames `claude-agent-acp` 0.76.0 sends for a `Write` over a
    /// file that exists. The first is the announcement with an optimistic diff
    /// (no line, `oldText` null); it stays an ordinary call, since the card
    /// draws that from the arguments already. The second is the PostToolUse
    /// refinement — no status, one diff block per hunk, a location per hunk —
    /// and is the one worth keeping. The third completes the call and carries
    /// no diff, so it must not erase what the second brought.
    #[test]
    fn the_adapters_write_frames_yield_one_diff_effect_with_its_line() {
        let announced = update(
            r#"{"sessionUpdate":"tool_call","toolCallId":"toolu_w","_meta":{"claudeCode":{"toolName":"Write"}},
                "title":"Write src/lib.rs","kind":"edit","status":"pending",
                "rawInput":{"file_path":"/w/src/lib.rs","content":"line1\nNEW line2\nline3"},
                "content":[{"type":"diff","path":"/w/src/lib.rs","oldText":null,"newText":"line1\nNEW line2\nline3"}],
                "locations":[{"path":"/w/src/lib.rs"}]}"#,
        );
        assert!(matches!(effect_of(announced), Effect::ToolCall { .. }));

        let refined = update(
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"toolu_w",
                "_meta":{"claudeCode":{"toolName":"Write","toolResponse":{"type":"update"}}},
                "content":[{"type":"diff","path":"/w/src/lib.rs","oldText":"line1\nold line2\nline3","newText":"line1\nNEW line2\nline3"}],
                "locations":[{"path":"/w/src/lib.rs","line":1}]}"#,
        );
        assert_eq!(
            effect_of(refined),
            Effect::ToolCallDiff {
                call_id: "toolu_w".into(),
                diffs: vec![ToolCallDiff {
                    path: "/w/src/lib.rs".into(),
                    old_text: Some("line1\nold line2\nline3".into()),
                    new_text: "line1\nNEW line2\nline3".into(),
                    line: Some(1),
                }],
            }
        );

        let completed = update(
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"toolu_w",
                "_meta":{"claudeCode":{"toolName":"Write"}},"status":"completed"}"#,
        );
        assert!(matches!(effect_of(completed), Effect::ToolResult { .. }));
    }

    /// An `Edit` with `replace_all` comes back as several hunks with a
    /// location each; the lines pair up by position. When the lists disagree
    /// in length nothing is numbered, since a wrong number is worse than none.
    #[test]
    fn hunks_take_their_line_from_the_matching_location_only() {
        let two = update(
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"toolu_e","_meta":{"claudeCode":{"toolName":"Edit"}},
                "content":[{"type":"diff","path":"/w/f.ts","oldText":"foo","newText":"bar"},
                           {"type":"diff","path":"/w/f.ts","oldText":"foo","newText":"bar"}],
                "locations":[{"path":"/w/f.ts","line":3},{"path":"/w/f.ts","line":15}]}"#,
        );
        let Effect::ToolCallDiff { diffs, .. } = effect_of(two) else {
            panic!("expected a diff");
        };
        assert_eq!(diffs.iter().map(|d| d.line).collect::<Vec<_>>(), [Some(3), Some(15)]);

        let mismatched = update(
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"toolu_e","_meta":{"claudeCode":{"toolName":"Edit"}},
                "content":[{"type":"diff","path":"/w/f.ts","oldText":"foo","newText":"bar"},
                           {"type":"diff","path":"/w/f.ts","oldText":"foo","newText":"bar"}],
                "locations":[{"path":"/w/f.ts","line":3}]}"#,
        );
        let Effect::ToolCallDiff { diffs, .. } = effect_of(mismatched) else {
            panic!("expected a diff");
        };
        assert_eq!(diffs.iter().map(|d| d.line).collect::<Vec<_>>(), [None, None]);
    }

    /// Several content blocks are one result, joined in order. A diff block in
    /// the middle is skipped rather than described — a `[diff]` marker would be
    /// indistinguishable from something a command printed.
    #[test]
    fn a_result_joins_its_text_blocks_and_skips_the_rest() {
        let effect = effect_of(update(
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed","content":[
                {"type":"content","content":{"type":"text","text":"first"}},
                {"type":"diff","path":"a.rs","oldText":"x","newText":"y"},
                {"type":"content","content":{"type":"text","text":"second"}}]}"#,
        ));
        assert_eq!(
            effect,
            Effect::ToolResult {
                call_id: "t1".into(),
                result: "first\nsecond".into(),
                outcome: ToolOutcome::Success,
            }
        );
    }

    #[test]
    fn a_plan_entry_with_no_status_is_pending() {
        let effect = effect_of(update(
            r#"{"sessionUpdate":"plan","entries":[
                {"content":"read the code","priority":"high","status":"completed"},
                {"content":"write the fix"}]}"#,
        ));
        assert_eq!(
            effect,
            Effect::Plan(vec![
                PlanItem {
                    content: "read the code".into(),
                    status: "completed".into()
                },
                PlanItem {
                    content: "write the fix".into(),
                    status: "pending".into()
                },
            ])
        );
    }

    #[test]
    fn usage_is_carried_without_its_cost() {
        assert_eq!(
            effect_of(update(
                r#"{"sessionUpdate":"usage_update","used":1200,"size":200000,
                    "cost":{"amount":0.03,"currency":"USD"}}"#
            )),
            Effect::Usage {
                used: 1200,
                size: 200000
            }
        );
    }

    /// The adapter sends `total_cost_usd`, a sum of doubles, and JavaScript
    /// prints a double in its shortest round-tripping form — which for an
    /// ordinary small bill has nineteen decimal places or an exponent. The
    /// usage beside it must survive whatever spelling the cost arrives in.
    #[test]
    fn usage_survives_a_cost_spelled_the_way_a_double_prints() {
        for amount in ["0.0031200000000000004", "1.2e-7", "2.1e-05"] {
            let frame = format!(
                r#"{{"sessionUpdate":"usage_update","used":1200,"size":200000,
                    "cost":{{"amount":{amount},"currency":"USD"}}}}"#
            );
            let parsed: Result<SessionNotification, _> =
                serde_json::from_str(&format!(r#"{{"sessionId":"s","update":{frame}}}"#));
            assert!(parsed.is_ok(), "{amount}: {:?}", parsed.err());
        }
    }

    /// ACP has no model field. It reports the model as one of the session's
    /// configuration options — and the whole set travels, because the composer
    /// offers the others.
    #[test]
    fn the_whole_option_set_travels_and_the_model_is_read_out_of_it() {
        let effect = effect_of(update(
            r#"{"sessionUpdate":"config_option_update","configOptions":[
                {"id":"mode","name":"Mode","category":"mode","type":"select",
                 "currentValue":"code","options":[{"value":"code","name":"Code"},
                                                  {"value":"plan","name":"Plan"}]},
                {"id":"model","name":"Model","category":"model","type":"select",
                 "currentValue":"claude-sonnet-4-5","options":[]}]}"#,
        ));
        let Effect::ConfigOptions(options) = effect else {
            panic!("expected the option set, got {effect:?}");
        };
        assert_eq!(options.len(), 2, "the mode knob travels too, not just the model");
        assert_eq!(options.iter().find_map(|o| o.as_model()), Some("claude-sonnet-4-5"));
        // And what a select may be set to, which is the half a picker needs.
        let mode = options.iter().find(|o| o.names_a("mode")).expect("the mode option");
        assert!(mode.is_select());
        assert_eq!(mode.options.len(), 2);
        assert_eq!(mode.current_str(), Some("code"));
    }

    /// `"default"` is what the knob says when nobody picked a model, and it is
    /// not one. Taken at face value it would be written into `model_id` on
    /// every row — including, for an import, several hundred at once — where a
    /// reader has no way to tell it from a model actually called that.
    #[test]
    fn the_model_knob_saying_default_is_the_same_as_not_saying() {
        let effect = effect_of(update(
            r#"{"sessionUpdate":"config_option_update","configOptions":[
                {"id":"model","name":"Model","category":"model","type":"select","currentValue":"default"}]}"#,
        ));
        let Effect::ConfigOptions(options) = effect else {
            panic!("expected the option set, got {effect:?}");
        };
        assert_eq!(options.iter().find_map(|o| o.as_model()), None);
        // And the knob itself still travels, because the composer offers it.
        assert!(options[0].names_a("model"));
    }

    /// `category` is documented as advisory and an agent may leave it out. The
    /// id is the fallback, and nothing else is guessed at.
    #[test]
    fn a_model_option_without_a_category_is_still_recognised() {
        let effect = effect_of(update(
            r#"{"sessionUpdate":"config_option_update","configOptions":[
                {"id":"model","name":"Model","type":"select","currentValue":"opus-4"}]}"#,
        ));
        let Effect::ConfigOptions(options) = effect else {
            panic!("expected the option set, got {effect:?}");
        };
        assert_eq!(options.iter().find_map(|o| o.as_model()), Some("opus-4"));

        // A toggle, whose `currentValue` is a boolean. Still passed through —
        // the merge wants it — but it names no model and is not a picker.
        let effect = effect_of(update(
            r#"{"sessionUpdate":"config_option_update","configOptions":[
                {"id":"thinking","name":"Thinking","type":"boolean","currentValue":true}]}"#,
        ));
        let Effect::ConfigOptions(options) = effect else {
            panic!("expected the option set, got {effect:?}");
        };
        assert_eq!(options.iter().find_map(|o| o.as_model()), None);
        assert!(!options[0].is_select());
    }

    /// The update is the full set, so an empty one says there are no options
    /// now — it has to reach the session, which replaces what it held.
    #[test]
    fn an_empty_option_set_is_still_the_whole_set() {
        assert_eq!(
            effect_of(update(r#"{"sessionUpdate":"config_option_update","configOptions":[]}"#)),
            Effect::ConfigOptions(Vec::new())
        );
    }

    /// A call announced already finished is the call *and* its result. The
    /// adapter's `memory_recall` frame, as it sends it: one `tool_call`,
    /// `completed`, never updated.
    #[test]
    fn an_edit_announced_with_its_diff_carries_it() {
        // An Edit as adapter 0.84 announces it to an AIR client: the text only
        // in the diff, none of it in `rawInput`.
        let effects = effects_of(update(
            r#"{"sessionUpdate":"tool_call","toolCallId":"e-1","title":"Edit a.rs","kind":"edit",
                "status":"pending","rawInput":{"file_path":"a.rs"},
                "content":[{"type":"diff","path":"a.rs","oldText":"x","newText":"y"}],
                "_meta":{"claudeCode":{"toolName":"Edit"}}}"#,
        ));
        assert_eq!(effects.len(), 2, "{effects:?}");
        let Effect::ToolCallDiff { call_id, diffs } = &effects[1] else {
            panic!("the diff came after the call: {effects:?}");
        };
        assert_eq!(call_id, "e-1");
        assert_eq!(diffs[0].new_text, "y");
    }

    #[test]
    fn a_call_announced_finished_carries_its_result() {
        let effects = effects_of(update(
            r#"{"sessionUpdate":"tool_call","toolCallId":"mem-1","title":"Recalled memory","kind":"read",
                "status":"completed","content":[{"type":"content","content":{"type":"text","text":"remembered"}}],
                "_meta":{"claudeCode":{"toolName":"memory_recall"}}}"#,
        ));
        assert_eq!(effects.len(), 2, "{effects:?}");
        assert!(matches!(&effects[0], Effect::ToolCall { call_id, .. } if call_id == "mem-1"));
        assert_eq!(
            effects[1],
            Effect::ToolResult {
                call_id: "mem-1".into(),
                result: "remembered".into(),
                outcome: ToolOutcome::Success,
            }
        );

        let pending = effects_of(update(
            r#"{"sessionUpdate":"tool_call","toolCallId":"b-1","title":"Terminal","status":"pending"}"#,
        ));
        assert_eq!(pending.len(), 1, "a call still running is only a call: {pending:?}");
    }

    /// The adapter's own frame for a warning on the way to a failure (an
    /// `api_retry`, as `claude-agent-acp` 0.76.0 publishes it): a
    /// `session_info_update` carrying nothing but `_meta`.
    #[test]
    fn a_session_failure_warning_is_a_notice() {
        let effect = effect_of(update(
            r#"{"sessionUpdate":"session_info_update","_meta":{"jetbrains":{"air":{"version":1,
                "sessionFailure":{"id":"prompt-1:error","revision":1,"category":"limit",
                "severity":"warning","title":"Retrying Claude, attempt 1 of 5.","actions":[]}}}}}"#,
        ));
        assert_eq!(
            effect,
            Effect::SessionNotice(SessionNoticeRecord {
                notice_id: "prompt-1:error".into(),
                revision: 1,
                category: AcpNoticeCategory::Limit,
                severity: AcpNoticeSeverity::Warning,
                title: "Retrying Claude, attempt 1 of 5.".into(),
                details: None,
                reason: None,
                actions: vec![],
            })
        );
    }

    /// The same incident again at a higher revision, now terminal: same id,
    /// new severity, and the recommended actions this time. The `reason`
    /// field is on the wire but not in the extension's own table; it travels.
    #[test]
    fn a_later_revision_of_an_incident_keeps_its_id() {
        let effect = effect_of(update(
            r#"{"sessionUpdate":"session_info_update","_meta":{"jetbrains":{"air":{"version":1,
                "sessionFailure":{"id":"prompt-1:error","revision":2,"category":"limit",
                "severity":"error","title":"Rate limit reached.","details":"Try again in a minute.",
                "reason":"rate_limit","actions":["retry","new_session"]}}}}}"#,
        ));
        let Effect::SessionNotice(record) = effect else {
            panic!("expected a notice, got {effect:?}");
        };
        assert_eq!(record.notice_id, "prompt-1:error");
        assert_eq!(record.revision, 2);
        assert_eq!(record.severity, AcpNoticeSeverity::Error);
        assert_eq!(record.details.as_deref(), Some("Try again in a minute."));
        assert_eq!(record.reason.as_deref(), Some("rate_limit"));
        assert_eq!(
            record.actions,
            vec![AcpNoticeAction::Retry, AcpNoticeAction::NewSession]
        );
    }

    /// A category this build has never heard of is not stored under a guessed
    /// one; an action it has never heard of is dropped from the list, which
    /// is what the extension tells a client to do with it.
    #[test]
    fn an_unknown_category_drops_the_record_and_an_unknown_action_drops_itself() {
        assert_eq!(
            effect_of(update(
                r#"{"sessionUpdate":"session_info_update","_meta":{"jetbrains":{"air":{"version":1,
                    "sessionFailure":{"id":"x","revision":1,"category":"weather",
                    "severity":"error","title":"t","actions":[]}}}}}"#
            )),
            Effect::Ignored
        );
        let effect = effect_of(update(
            r#"{"sessionUpdate":"session_info_update","_meta":{"jetbrains":{"air":{"version":1,
                "sessionFailure":{"id":"x","revision":1,"category":"service",
                "severity":"error","title":"t","actions":["retry","teleport"]}}}}}"#,
        ));
        let Effect::SessionNotice(record) = effect else {
            panic!("expected a notice, got {effect:?}");
        };
        assert_eq!(record.actions, vec![AcpNoticeAction::Retry]);
    }

    /// The adapter's title frame, exactly as `session-titles.ts` sends it.
    /// A `session_info_update` with neither a title nor an incident — a goal
    /// snapshot, say — is nothing to draw.
    #[test]
    fn a_session_title_travels_and_an_empty_info_update_does_not() {
        assert_eq!(
            effect_of(update(
                r#"{"sessionUpdate":"session_info_update","title":"Fix the flaky title test",
                    "updatedAt":"2026-09-12T02:00:00.000Z"}"#
            )),
            Effect::SessionTitle("Fix the flaky title test".into())
        );
        assert_eq!(
            effect_of(update(r#"{"sessionUpdate":"session_info_update","title":"   "}"#)),
            Effect::Ignored
        );
        assert_eq!(
            effect_of(update(
                r#"{"sessionUpdate":"session_info_update","_meta":{"goal":{"objective":"ship","status":"active"}}}"#
            )),
            Effect::Ignored
        );
    }

    /// The reason the parse is lax, stated as behaviour: a variant this build
    /// has never heard of costs nothing.
    #[test]
    fn an_unknown_variant_is_ignored_rather_than_fatal() {
        assert_eq!(
            effect_of(update(
                r#"{"sessionUpdate":"current_mode_update","currentModeId":"plan"}"#
            )),
            Effect::Ignored
        );
    }

    /// The recordings `tests/acp_tool_probe.rs` made against the real adapter
    /// (0.84.0), replayed through the mapping. Each holds one session's frames
    /// in both directions; only what came in is fed here.
    const PROBE: &str = include_str!("../../tests/fixtures/acp/tool-probe-no-air.jsonl");
    const PROBE_AIR: &str = include_str!("../../tests/fixtures/acp/tool-probe.jsonl");

    struct Replayed {
        /// `(tool name, result, outcome)`, in the order the calls finished.
        results: Vec<(String, String, ToolOutcome)>,
        /// The last arguments seen for each call, by tool name.
        arguments: Vec<(String, String)>,
    }

    fn replay(recording: &str) -> Replayed {
        let mut names = std::collections::HashMap::new();
        let mut replayed = Replayed {
            results: Vec::new(),
            arguments: Vec::new(),
        };
        for line in recording.lines().filter(|l| !l.trim().is_empty()) {
            let frame: serde_json::Value = serde_json::from_str(line).unwrap();
            if frame["dir"] != "in" || frame["msg"]["method"] != "session/update" {
                continue;
            }
            let notification: SessionNotification = serde_json::from_value(frame["msg"]["params"].clone()).unwrap();
            for effect in effects_of(notification.update) {
                match effect {
                    Effect::ToolCall {
                        call_id,
                        tool_name,
                        arguments,
                    } => {
                        replayed.arguments.push((tool_name.clone(), arguments));
                        names.insert(call_id, tool_name);
                    }
                    Effect::ToolCallRevised {
                        call_id,
                        tool_name,
                        arguments,
                    } => {
                        let name = if tool_name.is_empty() {
                            names.get(&call_id).cloned().unwrap_or_default()
                        } else {
                            tool_name
                        };
                        replayed.arguments.push((name.clone(), arguments));
                        names.insert(call_id, name);
                    }
                    Effect::ToolResult {
                        call_id,
                        result,
                        outcome,
                    } => {
                        let name = names.get(&call_id).cloned().unwrap_or_default();
                        replayed.results.push((name, result, outcome));
                    }
                    _ => {}
                }
            }
        }
        replayed
    }

    fn results_of<'a>(replayed: &'a Replayed, tool: &str) -> Vec<(&'a str, ToolOutcome)> {
        replayed
            .results
            .iter()
            .filter(|(name, ..)| name == tool)
            .map(|(_, result, outcome)| (result.as_str(), *outcome))
            .collect()
    }

    /// What a hosted call stores is what the model saw, not the adapter's
    /// display copy: a Read keeps its line numbers and loses its fence, a
    /// command's output is bare, and a structured `rawOutput` (`ToolSearch`)
    /// falls back to the text.
    #[test]
    fn a_recorded_result_is_what_the_model_saw() {
        let replayed = replay(PROBE);
        assert!(
            replayed.results.iter().all(|(_, result, _)| !result.contains("```")),
            "{:?}",
            replayed.results
        );
        assert_eq!(
            results_of(&replayed, "Read"),
            [
                (
                    "1\tone alpha\n2\ttwo\n3\tthree\n4\tfour needle\n5\tfive\n6\t",
                    ToolOutcome::Success
                ),
                ("2\ttwo\n3\tthree", ToolOutcome::Success),
            ]
        );
        assert_eq!(
            results_of(&replayed, "Bash"),
            [
                ("hello", ToolOutcome::Success),
                ("Exit code 3", ToolOutcome::Error),
                ("(Bash completed with no output)", ToolOutcome::Success),
            ]
        );
        assert_eq!(
            results_of(&replayed, "ToolSearch"),
            [("Tool: WebSearch", ToolOutcome::Success)]
        );
    }

    /// The refused Write in the recording is `failed` on the wire with
    /// `nonExecutionKind: permission-rule`, and the Bash that exited 3 is
    /// `failed` with none. Only the first is a refusal.
    #[test]
    fn a_recorded_refusal_is_denied_and_a_failure_is_an_error() {
        let replayed = replay(PROBE);
        assert_eq!(
            results_of(&replayed, "Write"),
            [
                (
                    "File created successfully at: <WORKSPACE>\\new.txt (file state is current in your context — no need to Read it back)",
                    ToolOutcome::Success
                ),
                ("User refused permission to run tool", ToolOutcome::Denied),
            ]
        );
        let interrupted = effect_of(update(
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"failed","rawOutput":"Interrupted",
                "_meta":{"claudeCode":{"toolName":"Bash","nonExecutionKind":"interrupted"}}}"#,
        ));
        assert!(
            matches!(
                interrupted,
                Effect::ToolResult {
                    outcome: ToolOutcome::Error,
                    ..
                }
            ),
            "nobody declined an interrupted call: {interrupted:?}"
        );
        let rejected = effect_of(update(
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"t2","status":"failed",
                "_meta":{"claudeCode":{"toolName":"Bash","nonExecutionKind":"user-rejected"}}}"#,
        ));
        assert!(
            matches!(
                rejected,
                Effect::ToolResult {
                    outcome: ToolOutcome::Denied,
                    ..
                }
            ),
            "{rejected:?}"
        );
    }

    /// Without the AIR envelope an Edit and a Write carry their file text in
    /// `rawInput`, so the card and the approval can show it from the call.
    #[test]
    fn a_recorded_edit_carries_its_text() {
        let replayed = replay(PROBE);
        let edit = replayed
            .arguments
            .iter()
            .rev()
            .find(|(name, _)| name == "Edit")
            .unwrap();
        let edit: serde_json::Value = serde_json::from_str(&edit.1).unwrap();
        assert_eq!(
            (edit["old_string"].as_str(), edit["new_string"].as_str()),
            (Some("alpha"), Some("beta"))
        );
        let write = replayed
            .arguments
            .iter()
            .rev()
            .find(|(name, arguments)| name == "Write" && arguments.contains("new.txt"))
            .unwrap();
        let write: serde_json::Value = serde_json::from_str(&write.1).unwrap();
        assert_eq!(write["content"].as_str(), Some("x\n"));
    }

    /// Why the envelope is gone (`protocol::ClientCapabilities`): declared,
    /// the same Read comes back with no text anywhere, `rawOutput` included.
    #[test]
    fn under_the_air_envelope_a_read_has_no_text() {
        let replayed = replay(PROBE_AIR);
        let reads = results_of(&replayed, "Read");
        assert_eq!(reads.len(), 2, "{:?}", replayed.results);
        assert!(reads.iter().all(|(result, _)| result.is_empty()), "{reads:?}");
    }
}

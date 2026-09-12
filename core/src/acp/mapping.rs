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
use crate::events::{AcpNoticeAction, AcpNoticeCategory, AcpNoticeSeverity, ToolOutcome};

/// What one `session/update` means here.
///
/// One update maps to at most one effect; the variants this step does not draw
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
    /// Context usage. Reported, never priced — see [`Usage::cost`].
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
        SessionUpdate::ToolCallUpdate(call) => match call.status.as_deref() {
            Some("completed") => Effect::ToolResult {
                call_id: call.tool_call_id.clone(),
                result: output_of(&call),
                outcome: ToolOutcome::Success,
            },
            Some("failed") => Effect::ToolResult {
                call_id: call.tool_call_id.clone(),
                result: output_of(&call),
                outcome: ToolOutcome::Error,
            },
            // Still running. Usually there is nothing to say — but this is also
            // how a call announced before its arguments were known gets them,
            // and how the adapter's second source revises one it did not emit.
            // An update carrying neither is the ordinary "still going" beat.
            _ if call.raw_input.is_some() || call.meta.is_some() || call.title.is_some() => Effect::ToolCallRevised {
                call_id: call.tool_call_id.clone(),
                tool_name: tool_name_of(&call),
                arguments: arguments_of(&call),
            },
            _ => Effect::Ignored,
        },
        SessionUpdate::Plan { entries } => Effect::Plan(entries.iter().map(plan_item).collect()),
        SessionUpdate::UsageUpdate(Usage { used, size, .. }) => Effect::Usage { used, size },
        // Passed through whole, including an update that changes nothing this
        // app reads: the set is merged rather than replaced, so an option
        // carrying only a new `currentValue` still has to reach the merge.
        SessionUpdate::ConfigOptionUpdate { config_options } if !config_options.is_empty() => {
            Effect::ConfigOptions(config_options)
        }
        SessionUpdate::ConfigOptionUpdate { .. } => Effect::Ignored,
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
pub fn tool_name_of(call: &ToolCall) -> String {
    let from_meta = call
        .meta
        .as_ref()
        .and_then(|m| m.claude_code.as_ref())
        .and_then(|c| c.tool_name.as_deref());

    from_meta
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
fn output_of(call: &ToolCall) -> String {
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

    /// An update with nothing in it is not a change to merge.
    #[test]
    fn an_empty_option_set_is_ignored() {
        assert_eq!(
            effect_of(update(r#"{"sessionUpdate":"config_option_update","configOptions":[]}"#)),
            Effect::Ignored
        );
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
}

//! Moving a conversation between collaboration modes, in the middle of a turn.
//!
//! A switch is not one write. The conversation row changes, the turn is
//! re-resolved against the new mode, and four things in the running loop have to
//! change together: which mode it thinks it is in, the tool definitions the next
//! request carries, the set that authorises a tool call, and the system prompt.
//! Land three of the four and the turn spends the rest of itself being told that
//! the tools it just gained do not exist.
//!
//! So nothing here changes the loop directly. Both paths hand back a
//! [`TransitionEffect`], and applying one is a single call that does all four or
//! none of them. The half-done switch — row written, rebuild refused — is a real
//! outcome with its own wording, not an error, and it deliberately leaves the
//! tool set exactly where it was.
//!
//! The port is one method wide because that is all that needs the outside: the
//! assistant row, its persona, the context blocks, the MCP snapshot and the tool
//! registry are assembled long before the loop starts. A runner with no modes
//! passes no [`Transitions`] at all, which is what keeps OneBot's current
//! behaviour — see the drift list.

use std::collections::HashSet;

use crate::agent::modes::ModeSpec;
use crate::agent::turn_config::TurnConfig;
use crate::db;
use crate::db::DbPool;
use crate::provider::{ChatMessage, ToolDefinition};
use crate::util::{get_conn, now_ms};

use super::{ApprovalDecision, Emit};

/// Re-resolving the turn for another mode.
#[async_trait::async_trait]
pub trait Transitions: Send + Sync {
    /// The outer `Err` is the worker never coming back, and ends the turn — the
    /// same as it does today. A rebuild that merely fails is `Ok(Err(_))`: the
    /// model is told, and nothing in the loop moves. The two are not the same
    /// failure and folding them together would turn a panicked worker into a
    /// sentence the model reads and carries on from.
    async fn rebuild(&self, mode: &'static ModeSpec) -> Result<Result<TurnConfig, String>, String>;

    /// Read the durable private plan document. Defaulting to an error keeps
    /// non-desktop runners honest; their fixed work mode never offers the tool.
    async fn read_plan(&self) -> Result<PlanReadResult, String> {
        Err("this runner has no durable plan document".into())
    }

    /// Apply one optimistic patch to the durable plan document.
    async fn update_plan(&self, _request: UpdatePlanRequest) -> Result<PlanUpdateResult, String> {
        Err("this runner has no durable plan document".into())
    }

    /// Seal the current head as a review and move the recorded turn to
    /// `waiting_review`. This does not wait for a person and does not return a
    /// tool result; a later explicit continuation settles that pending call.
    async fn submit_plan(&self, _request: SubmitPlanRequest) -> Result<crate::events::PlanReviewEvent, String> {
        Err("this runner has no durable plan review surface".into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PlanReadResult {
    pub content: String,
    pub generation: i64,
    pub sha256: String,
    pub file_sync_state: crate::db::models::plan_review::PlanMaterializationState,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdatePlanRequest {
    pub base_generation: i64,
    pub base_sha256: String,
    pub patch: String,
    /// The assistant row and provider call identify the revision in the
    /// transcript. They are supplied by the loop, never accepted from the
    /// model's JSON.
    #[serde(skip)]
    pub source_message_id: String,
    #[serde(skip)]
    pub source_call_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PlanUpdateResult {
    pub generation: i64,
    pub sha256: String,
    pub applied_diff: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitPlanRequest {
    pub turn_id: String,
    pub assistant_message_id: String,
    pub provider_call_id: String,
}

pub(crate) fn parse_update_plan_arguments(
    arguments: &str,
    source_message_id: &str,
    source_call_id: &str,
) -> Result<UpdatePlanRequest, String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Arguments {
        base_generation: i64,
        base_sha256: String,
        patch: String,
    }

    let arguments: Arguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid update_plan arguments: {error}"))?;
    if arguments.base_generation < 0 {
        return Err("invalid update_plan arguments: base_generation must be non-negative".into());
    }
    if arguments.base_sha256.len() != 64
        || !arguments
            .base_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err("invalid update_plan arguments: base_sha256 must be 64 lowercase hexadecimal characters".into());
    }
    if arguments.patch.trim().is_empty() {
        return Err("invalid update_plan arguments: patch must not be empty".into());
    }
    Ok(UpdatePlanRequest {
        base_generation: arguments.base_generation,
        base_sha256: arguments.base_sha256,
        patch: arguments.patch,
        source_message_id: source_message_id.to_string(),
        source_call_id: source_call_id.to_string(),
    })
}

pub(crate) fn parse_empty_plan_arguments(tool: &str, arguments: &str) -> Result<(), String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Empty {}
    serde_json::from_str::<Empty>(arguments)
        .map(|_| ())
        .map_err(|error| format!("invalid {tool} arguments: {error}"))
}

/// A switch that went all the way through: conversation row written *and* turn
/// re-resolved.
///
/// The two travel as one value rather than as two `Option` fields so that a
/// mode without its config cannot be built in the first place. As two fields the
/// pairing was a comment, and `apply` had to be trusted to honour it; the shape
/// that carries a mode nobody can act on simply does not exist now.
struct TransitionNext {
    mode: &'static ModeSpec,
    config: TurnConfig,
}

/// What a transition did: what the model is told, and what the loop has to
/// change to match.
pub(crate) struct TransitionEffect {
    pub result: String,
    pub outcome: &'static str,
    next: Option<TransitionNext>,
}

impl TransitionEffect {
    /// A transition that did not happen, whether because the user said no or
    /// because a write did. All that is left is what to say.
    fn said(result: impl Into<String>, outcome: &'static str) -> Self {
        Self {
            result: result.into(),
            outcome,
            next: None,
        }
    }

    /// A switch that landed, with the turn it resolved to.
    fn switched(result: impl Into<String>, mode: &'static ModeSpec, config: TurnConfig) -> Self {
        Self {
            result: result.into(),
            outcome: "success",
            next: Some(TransitionNext { mode, config }),
        }
    }

    /// Put the switch into effect, if there was one, and hand back the tool
    /// result.
    pub(crate) fn apply(
        self,
        mode: &mut &'static ModeSpec,
        chat_messages: &mut [ChatMessage],
        tool_defs: &mut Vec<ToolDefinition>,
        offered: &mut HashSet<String>,
    ) -> (String, &'static str) {
        if let Some(next) = self.next {
            *mode = next.mode;
            apply_turn_config(chat_messages, tool_defs, offered, next.config);
        }
        (self.result, self.outcome)
    }

    /// Whether the loop would move. Only the tests ask; the loop calls `apply`.
    #[cfg(test)]
    fn moves(&self) -> bool {
        self.next.is_some()
    }
}

/// Put a re-resolved turn into effect: prompt, definitions and authorisation
/// together.
pub(crate) fn apply_turn_config(
    chat_messages: &mut [ChatMessage],
    tool_defs: &mut Vec<ToolDefinition>,
    offered: &mut HashSet<String>,
    config: TurnConfig,
) {
    replace_system_prompt(chat_messages, &config.system_prompt);
    *tool_defs = config.tool_defs;
    *offered = config.offered;
}

/// Swap the system prompt, and only that.
///
/// The first message, and only if it is a system message — never a search
/// through the transcript. By the time a switch happens the history holds
/// assistant turns, tool results and injected context, and the loop has a second
/// user of the same slot with the opposite intent: steering appends, and must
/// not touch the prefix the prompt cache is keyed on.
pub(crate) fn replace_system_prompt(chat_messages: &mut [ChatMessage], prompt: &str) {
    if let Some(first) = chat_messages.first_mut()
        && first.role == "system"
    {
        first.content = prompt.trim().to_string();
    }
}

/// The user was asked to move into a mode, and answered.
///
/// The mirror of [`exit`], minus the artifact: entering a mode produces nothing
/// to record, it only narrows what the rest of the turn may do. Which is why the
/// decision arrives already made — there is no work on the near side of the
/// question to get the order wrong.
pub(crate) async fn enter(
    pool: &DbPool,
    transitions: &dyn Transitions,
    emit: Option<&dyn Emit>,
    conversation_id: &str,
    target: &'static ModeSpec,
    decision: Option<ApprovalDecision>,
) -> Result<TransitionEffect, String> {
    match decision {
        Some(ApprovalDecision::Approved) => {}
        Some(ApprovalDecision::Denied(Some(reason))) => {
            return Ok(TransitionEffect::said(
                format!("The user would rather not plan first: {reason}\n\nCarry on as you were."),
                "denied",
            ));
        }
        // Nobody answered — the card expired or the turn outlived it. Saying
        // "the user declined" attributes a decision nobody made; the port's
        // contract is that an unanswered question is never a refusal.
        None => {
            return Ok(TransitionEffect::said(
                "No one answered the request to enter plan mode before it expired. \
                 Carry on as you were; you may offer it again later.",
                "denied",
            ));
        }
        _ => {
            return Ok(TransitionEffect::said(
                "The user declined to switch to plan mode. Carry on as you were.",
                "denied",
            ));
        }
    }

    let rebuilt = match store_mode(pool, conversation_id, Some(target.id)).await? {
        Err(e) => Err(e),
        Ok(()) => transitions.rebuild(target).await?,
    };
    Ok(match rebuilt {
        Ok(next) => {
            announce(emit, conversation_id);
            TransitionEffect::switched(
                "The user agreed. You are in plan mode from here: the tools that \
                 change anything are gone for the rest of this conversation until \
                 the plan is approved. Explore and design — do not describe edits \
                 as though you had made them.",
                target,
                next,
            )
        }
        Err(e) => TransitionEffect::said(
            format!(
                "The user agreed, but switching into plan mode failed: {e}. You \
                 are still in the previous mode — tell the user rather than \
                 pretending to plan."
            ),
            "error",
        ),
    })
}

/// Submit the already-saved head. Unlike the former approval waiter this is a
/// bounded durable write: it returns as soon as the review and the turn's
/// `waiting_review` state have committed.
pub(crate) async fn submit(
    transitions: &dyn Transitions,
    arguments: &str,
    request: SubmitPlanRequest,
) -> Result<crate::events::PlanReviewEvent, String> {
    parse_empty_plan_arguments("exit_plan", arguments)?;
    transitions.submit_plan(request).await
}

async fn store_mode(
    pool: &DbPool,
    conversation_id: &str,
    mode: Option<&'static str>,
) -> Result<Result<(), String>, String> {
    let pool = pool.clone();
    let conv_id = conversation_id.to_string();
    tokio::task::spawn_blocking(move || {
        let mut conn = get_conn(&pool)?;
        db::ops::conversation::update_mode(&mut conn, &conv_id, mode, now_ms()).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())
}

/// The toolbar reads the mode off the conversation row, which just changed.
///
/// Ignored if it fails, on both paths and as it always was: the row is already
/// written and this only asks a window to re-read it. It is the one send in the
/// turn that is not part of the answer.
fn announce(emit: Option<&dyn Emit>, conversation_id: &str) {
    if let Some(e) = emit {
        let _ = e.emit_conversation_updated(conversation_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::modes::{PLAN_MODE, WORK_MODE};
    use crate::db::test_db;
    use crate::provider::ChatMessage;
    use std::sync::Mutex;

    fn plan_mode() -> &'static ModeSpec {
        crate::agent::modes::resolve(Some(PLAN_MODE)).unwrap()
    }

    fn work_mode() -> &'static ModeSpec {
        crate::agent::modes::resolve(Some(WORK_MODE)).unwrap()
    }

    fn conversation(pool: &DbPool) {
        let mut conn = pool.get().unwrap();
        db::ops::conversation::create_conversation(&mut conn, "c1", Some("t"), None, None, 1).unwrap();
    }

    fn stored_mode(pool: &DbPool) -> Option<String> {
        let mut conn = pool.get().unwrap();
        db::ops::conversation::get_conversation(&mut conn, "c1").unwrap().mode
    }

    fn def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: String::new(),
            parameters: serde_json::json!({}),
        }
    }

    fn config(prompt: &str, tools: &[&str]) -> TurnConfig {
        TurnConfig {
            tool_defs: tools.iter().map(|t| def(t)).collect(),
            system_prompt: prompt.to_string(),
            offered: tools.iter().map(|t| t.to_string()).collect(),
        }
    }

    /// Hands back a fixed config, or refuses. Records which mode it was asked
    /// for, which is how the switch's destination is checked without a real
    /// resolver.
    struct FakeRebuild {
        answer: Result<Result<TurnConfig, String>, String>,
        asked: Mutex<Vec<&'static str>>,
        submitted: Mutex<Vec<SubmitPlanRequest>>,
    }

    impl FakeRebuild {
        fn giving(prompt: &str, tools: &[&str]) -> Self {
            Self {
                answer: Ok(Ok(config(prompt, tools))),
                asked: Mutex::new(Vec::new()),
                submitted: Mutex::new(Vec::new()),
            }
        }
        fn refusing() -> Self {
            Self {
                answer: Ok(Err("no connection".into())),
                asked: Mutex::new(Vec::new()),
                submitted: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl Transitions for FakeRebuild {
        async fn rebuild(&self, mode: &'static ModeSpec) -> Result<Result<TurnConfig, String>, String> {
            self.asked.lock().unwrap().push(mode.id);
            match &self.answer {
                Ok(Ok(c)) => Ok(Ok(TurnConfig {
                    tool_defs: c.tool_defs.clone(),
                    system_prompt: c.system_prompt.clone(),
                    offered: c.offered.clone(),
                })),
                Ok(Err(e)) => Ok(Err(e.clone())),
                Err(e) => Err(e.clone()),
            }
        }

        async fn submit_plan(&self, request: SubmitPlanRequest) -> Result<crate::events::PlanReviewEvent, String> {
            self.submitted.lock().unwrap().push(request.clone());
            Ok(crate::events::PlanReviewEvent {
                review_id: "review-1".into(),
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

    /// Records every send so a path that emits nothing can be told from one that
    /// emits. Never fails: the desktop ignores this particular send too.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl Emit for Recorder {
        fn emit(&self, channel: &str, _payload: serde_json::Value) -> Result<(), String> {
            self.0.lock().unwrap().push(channel.to_string());
            Ok(())
        }
    }

    struct Loop {
        mode: &'static ModeSpec,
        messages: Vec<ChatMessage>,
        tool_defs: Vec<ToolDefinition>,
        offered: HashSet<String>,
    }

    impl Loop {
        fn in_work() -> Self {
            let mut first = ChatMessage::user("you are helpful");
            first.role = "system".into();
            Self {
                mode: work_mode(),
                messages: vec![first, ChatMessage::user("do the thing")],
                tool_defs: vec![def("write_file")],
                offered: ["write_file".to_string()].into_iter().collect(),
            }
        }
        fn apply(&mut self, effect: TransitionEffect) -> (String, &'static str) {
            effect.apply(
                &mut self.mode,
                &mut self.messages,
                &mut self.tool_defs,
                &mut self.offered,
            )
        }
        fn names(&self) -> Vec<&str> {
            self.tool_defs.iter().map(|d| d.name.as_str()).collect()
        }
    }

    /// The whole reason the effect is one value: all four move, or none do.
    #[tokio::test]
    async fn entering_moves_the_mode_the_tools_the_authorisation_and_the_prompt() {
        let pool = test_db();
        conversation(&pool);
        let rebuild = FakeRebuild::giving("# Plan mode\n\nyou are planning", &["read_file"]);
        let emit = Recorder::default();
        let mut state = Loop::in_work();

        let effect = enter(
            &pool,
            &rebuild,
            Some(&emit),
            "c1",
            plan_mode(),
            Some(ApprovalDecision::Approved),
        )
        .await
        .unwrap();
        let (_, outcome) = state.apply(effect);

        assert_eq!(outcome, "success");
        assert_eq!(state.mode.id, PLAN_MODE);
        assert_eq!(state.names(), ["read_file"]);
        assert!(state.offered.contains("read_file") && !state.offered.contains("write_file"));
        assert_eq!(state.messages[0].content, "# Plan mode\n\nyou are planning");
        assert_eq!(
            state.messages[1].content, "do the thing",
            "and nothing else in the history"
        );
        assert_eq!(stored_mode(&pool).as_deref(), Some(PLAN_MODE));
        assert_eq!(*rebuild.asked.lock().unwrap(), [PLAN_MODE]);
        assert_eq!(*emit.0.lock().unwrap(), ["conversation-updated"]);
    }

    /// The next request is built from `tool_defs` and checked against `offered`,
    /// so "immediately" is exactly this: no second round trip, no re-resolve at
    /// the top of the next iteration.
    #[tokio::test]
    async fn the_next_request_carries_the_new_tools_without_asking_again() {
        let pool = test_db();
        conversation(&pool);
        let rebuild = FakeRebuild::giving("planning", &["read_file", "run_command"]);
        let mut state = Loop::in_work();

        let effect = enter(
            &pool,
            &rebuild,
            None,
            "c1",
            plan_mode(),
            Some(ApprovalDecision::Approved),
        )
        .await
        .unwrap();
        state.apply(effect);

        // What the loop would send and what it would authorise, one iteration
        // later, with nothing in between.
        assert_eq!(state.names(), ["read_file", "run_command"]);
        assert!(
            !state.offered.contains("write_file"),
            "the withheld tool is refused too"
        );
        assert_eq!(rebuild.asked.lock().unwrap().len(), 1);
    }

    /// The row is written and the rebuild is not. The model is told; the tool set
    /// must not pretend otherwise, or the turn burns itself on calls to tools it
    /// does not have.
    #[tokio::test]
    async fn a_switch_that_only_half_happened_does_not_move_the_tool_set() {
        let pool = test_db();
        conversation(&pool);
        let rebuild = FakeRebuild::refusing();
        let emit = Recorder::default();
        let mut state = Loop::in_work();

        let effect = enter(
            &pool,
            &rebuild,
            Some(&emit),
            "c1",
            plan_mode(),
            Some(ApprovalDecision::Approved),
        )
        .await
        .unwrap();
        let (result, outcome) = state.apply(effect);

        assert_eq!(outcome, "error");
        assert!(
            result.contains("no connection"),
            "the model is told what went wrong: {result}"
        );
        assert_eq!(state.mode.id, WORK_MODE, "still where it was");
        assert_eq!(state.names(), ["write_file"]);
        assert!(state.offered.contains("write_file"));
        assert_eq!(state.messages[0].content, "you are helpful");
        assert!(emit.0.lock().unwrap().is_empty(), "nothing to tell the window about");
        // The row did move, which is the whole reason this case exists.
        assert_eq!(stored_mode(&pool).as_deref(), Some(PLAN_MODE));
    }

    #[tokio::test]
    async fn a_refused_entry_carries_the_reason_back_and_changes_nothing() {
        let pool = test_db();
        conversation(&pool);
        let rebuild = FakeRebuild::giving("planning", &["read_file"]);
        let mut state = Loop::in_work();

        let effect = enter(
            &pool,
            &rebuild,
            None,
            "c1",
            plan_mode(),
            Some(ApprovalDecision::Denied(Some("just do it".into()))),
        )
        .await
        .unwrap();
        let (result, outcome) = state.apply(effect);

        assert_eq!(outcome, "denied");
        assert!(result.contains("just do it"));
        assert_eq!(state.mode.id, WORK_MODE);
        assert_eq!(stored_mode(&pool), None, "the row is not touched before the answer");
        assert!(rebuild.asked.lock().unwrap().is_empty());
    }

    /// A card that goes away unanswered reads the same as a refusal, and must
    /// not be read as one that said yes.
    #[tokio::test]
    async fn an_unanswered_entry_is_a_refusal() {
        let pool = test_db();
        conversation(&pool);
        let rebuild = FakeRebuild::giving("planning", &["read_file"]);

        let effect = enter(&pool, &rebuild, None, "c1", plan_mode(), None).await.unwrap();

        assert_eq!(effect.outcome, "denied");
        assert!(!effect.moves());
        assert_eq!(stored_mode(&pool), None);
    }

    #[tokio::test]
    async fn exit_plan_accepts_only_an_empty_object_and_submits_without_rebuilding() {
        let transitions = FakeRebuild::giving("work", &["write_file"]);
        let request = SubmitPlanRequest {
            turn_id: "turn-1".into(),
            assistant_message_id: "message-1".into(),
            provider_call_id: "call-1".into(),
        };
        let event = submit(&transitions, "{}", request).await.unwrap();

        assert_eq!(event.review_id, "review-1");
        assert_eq!(event.status, "pending");
        assert!(
            transitions.asked.lock().unwrap().is_empty(),
            "review is not an approval waiter"
        );
        {
            let submitted = transitions.submitted.lock().unwrap();
            assert_eq!(submitted.len(), 1);
            assert_eq!(submitted[0].provider_call_id, "call-1");
        }

        for arguments in ["not json", r#"{"plan":"old payload"}"#, r#"{"future":true}"#] {
            let request = SubmitPlanRequest {
                turn_id: "turn-2".into(),
                assistant_message_id: "message-2".into(),
                provider_call_id: "call-2".into(),
            };
            let error = submit(&transitions, arguments, request).await.unwrap_err();
            assert!(error.contains("invalid exit_plan arguments"), "{arguments}: {error}");
        }
        assert_eq!(transitions.submitted.lock().unwrap().len(), 1);
    }

    #[test]
    fn update_plan_arguments_are_strict_and_carry_transcript_identity_out_of_band() {
        let hash = "0".repeat(64);
        let request = parse_update_plan_arguments(
            &serde_json::json!({
                "base_generation": 2,
                "base_sha256": hash,
                "patch": "*** Begin Patch\n*** Update File: plan.md\n@@\n-a\n+b\n*** End Patch"
            })
            .to_string(),
            "message-1",
            "call-1",
        )
        .unwrap();
        assert_eq!(request.base_generation, 2);
        assert_eq!(request.source_message_id, "message-1");
        assert_eq!(request.source_call_id, "call-1");

        for arguments in [
            serde_json::json!({"base_generation": -1, "base_sha256": "0".repeat(64), "patch": "x"}),
            serde_json::json!({"base_generation": 0, "base_sha256": "bad", "patch": "x"}),
            serde_json::json!({"base_generation": 0, "base_sha256": "0".repeat(64), "patch": " "}),
            serde_json::json!({"base_generation": 0, "base_sha256": "0".repeat(64), "patch": "x", "future": true}),
        ] {
            assert!(parse_update_plan_arguments(&arguments.to_string(), "m", "c").is_err());
        }
    }

    fn system(content: &str) -> ChatMessage {
        let mut m = ChatMessage::user(content);
        m.role = "system".into();
        m
    }

    /// A second system message is not hypothetical: a compaction summary and an
    /// injected block can both land with that role. Only the first one is the
    /// prompt, and rewriting the others would overwrite them with it.
    #[test]
    fn the_prompt_is_swapped_in_the_first_slot_and_only_there() {
        let mut messages = vec![
            system("old"),
            ChatMessage::user("hello"),
            system("a summary of what came before"),
        ];

        replace_system_prompt(&mut messages, "  new  ");

        assert_eq!(messages[0].content, "new", "trimmed, as the resolver's output is");
        assert_eq!(messages[1].content, "hello");
        assert_eq!(
            messages[2].content, "a summary of what came before",
            "a later system message is not the prompt",
        );
    }

    /// A transcript whose first message is not a system message belongs to a
    /// runner that builds its prompt some other way. Inserting one would move
    /// every other message and invalidate the cached prefix.
    #[test]
    fn a_transcript_with_no_system_message_is_left_alone() {
        let mut messages = vec![ChatMessage::user("hello")];
        replace_system_prompt(&mut messages, "new");
        assert_eq!(messages[0].content, "hello");

        let mut empty: Vec<ChatMessage> = Vec::new();
        replace_system_prompt(&mut empty, "new");
        assert!(empty.is_empty());
    }
}

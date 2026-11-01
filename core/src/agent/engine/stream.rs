//! Reading one model response off the wire.
//!
//! One copy of a state machine that used to exist twice, byte for byte, in
//! `commands/chat.rs` and `onebot/agent.rs` — the only difference being how each
//! of them sent the events out. That difference is now the `Emit` port.

use std::collections::HashSet;

use tokio_util::sync::CancellationToken;

use super::{Commentary, Emit};
use crate::agent::{InlineHiddenTagParser, InlineTagSpec, StreamResult};
use crate::events::ChatStreamEvent;
use crate::provider;

/// How long a stream may say nothing before it is taken as dead.
///
/// Generous because a reasoning model can think for minutes before its first
/// visible token, and the cost of being wrong is a turn killed mid-answer.
pub(crate) const STREAM_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Read a response to the end, emitting as it goes.
///
/// `emit` is `None` when nobody is watching. When it is `Some`, a send that
/// fails ends the read — which of the two runners that actually happens for is
/// the adapter's decision, not this function's.
///
/// `interim` is OneBot's mid-turn mouth. A provider-side tool is not a client
/// `ToolCall`, so the turn loop would otherwise concatenate every visible beat
/// and send it once at the end. The first `ServerToolCall` of a given id is
/// the boundary: whatever has been said so far goes out here, and the round's
/// reply is only the remainder. The desktop passes `None` — it already streamed
/// every character to the window.
pub(crate) async fn consume_stream(
    mut stream: provider::ChatStream,
    cancel: &CancellationToken,
    emit: Option<&dyn Emit>,
    interim: Option<&dyn Commentary>,
    message_id: &str,
    conversation_id: &str,
) -> Result<StreamResult, String> {
    use futures::StreamExt;

    let send = |event: ChatStreamEvent| -> Result<(), String> {
        match emit {
            Some(e) => e.emit_chat(event),
            None => Ok(()),
        }
    };

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut provider_state = provider::state::ProviderStateAccumulator::default();
    let mut tool_acc: Vec<(String, String, String)> = Vec::new();
    let mut usage = None;
    let mut finish_reason = None;
    let mut response_model: Option<String> = None;
    let mut flushed_up_to = 0usize;
    let mut flushed_ids: HashSet<String> = HashSet::new();
    // Some OpenAI-compatible providers inline reasoning as <think> tags in the
    // text stream instead of a separate reasoning field; route it accordingly.
    let mut think_parser = InlineHiddenTagParser::new_streaming(vec![InlineTagSpec {
        tag: (),
        open: "<think>",
        close: "</think>",
    }]);
    // Set by the one exit that means the model reached the end of its answer:
    // the stream running out. Cancellation leaves it false, and so does every
    // error return, none of which get this far.
    let mut ran_to_completion = false;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => { break; }
            chunk = tokio::time::timeout(STREAM_IDLE_TIMEOUT, stream.next()) => {
                match chunk {
                    Err(_) => {
                        return Err("Stream idle timeout".to_string());
                    }
                    Ok(Some(Ok(provider::StreamEvent::Text { content: ref s }))) => {
                        let chunk = think_parser.push_str(s);
                        if !chunk.visible_text.is_empty() {
                            text.push_str(&chunk.visible_text);
                            send(ChatStreamEvent::Text {
                                content: chunk.visible_text.clone(),
                                message_id: message_id.to_string(),
                                conversation_id: conversation_id.to_string(),
                            })?;
                        }
                        for tag in &chunk.extracted {
                            reasoning.push_str(&tag.content);
                            send(ChatStreamEvent::Reasoning {
                                content: tag.content.clone(),
                                message_id: message_id.to_string(),
                                conversation_id: conversation_id.to_string(),
                            })?;
                        }
                    }
                    Ok(Some(Ok(provider::StreamEvent::Reasoning { content: ref s }))) => {
                        reasoning.push_str(s);
                        send(ChatStreamEvent::Reasoning {
                            content: s.clone(),
                            message_id: message_id.to_string(),
                            conversation_id: conversation_id.to_string(),
                        })?;
                    }
                    Ok(Some(Ok(provider::StreamEvent::ProviderStateUpdate { update }))) => {
                        provider_state.apply(update)?;
                    }
                    Ok(Some(Ok(provider::StreamEvent::ToolCallStart { index, ref id, ref name }))) => {
                        // Guard against a malformed/hostile endpoint sending a huge
                        // index that would balloon the Vec allocation.
                        if index >= 256 {
                            return Err(format!("tool call index {index} out of range"));
                        }
                        while tool_acc.len() <= index {
                            tool_acc.push((String::new(), String::new(), String::new()));
                        }
                        tool_acc[index] = (id.clone(), name.clone(), String::new());
                    }
                    Ok(Some(Ok(provider::StreamEvent::ToolCallDelta { index, ref arguments }))) => {
                        if let Some(entry) = tool_acc.get_mut(index) {
                            entry.2.push_str(arguments);
                        }
                    }
                    Ok(Some(Ok(provider::StreamEvent::ToolCallDone { index, ref arguments }))) => {
                        if let Some(entry) = tool_acc.get_mut(index) {
                            entry.2 = arguments.clone();
                        }
                    }
                    Ok(Some(Ok(provider::StreamEvent::ServerToolCall(ref call)))) => {
                        // Announced, never dispatched: the upstream has already
                        // run it. Without this the reader gets a minute of
                        // silence and then an answer out of nowhere.
                        //
                        // Not accumulated into the row either — this is what the
                        // provider did on its own side, and the transcript
                        // records what was said.
                        //
                        // So the card lives as long as the front end's own copy
                        // of the round does: the snapshot taken when the turn
                        // stops rebuilds the row from the database, where this
                        // was never written. The answer and its citations
                        // survive; the searching does not. That is the right way
                        // round, but it is sooner than "on reload" — worth
                        // knowing before wondering where the card went.
                        if let Some(e) = emit {
                            e.emit_chat(ChatStreamEvent::ServerTool {
                                message_id: message_id.to_string(),
                                conversation_id: conversation_id.to_string(),
                                call: call.clone(),
                            })?;
                        }
                        // First announcement of this id is the beat boundary
                        // for a runner that does not stream. The `done`
                        // revision of the same card is skipped even if more
                        // text arrived while the search ran — that text is
                        // still this beat, not a new one.
                        if let Some(interim) = interim
                            && flushed_ids.insert(call.id.clone())
                        {
                            let pending = &text[flushed_up_to..];
                            if !pending.trim().is_empty() {
                                interim.say(pending.to_string()).await;
                                flushed_up_to = text.len();
                            }
                        }
                    }
                    Ok(Some(Ok(provider::StreamEvent::UsageUpdate { usage: ref u }))) => {
                        usage = Some(u.clone());
                    }
                    Ok(Some(Ok(provider::StreamEvent::Stop { ref reason, usage: ref u }))) => {
                        if let Some(u) = u {
                            usage = Some(u.clone());
                        }
                        finish_reason = Some(reason.clone());
                    }
                    Ok(Some(Ok(provider::StreamEvent::Error { ref message }))) => {
                        return Err(message.clone());
                    }
                    Ok(Some(Ok(provider::StreamEvent::ResponseModel { ref model }))) => {
                        if response_model.is_none() {
                            response_model = Some(model.clone());
                        }
                    }
                    Ok(Some(Ok(provider::StreamEvent::CompactionResult { .. }))) => {}
                    Ok(Some(Ok(provider::StreamEvent::MessageStart { .. }))) => {}
                    Ok(Some(Err(e))) => {
                        return Err(e.to_string());
                    }
                    Ok(None) => { ran_to_completion = true; break; }
                }
            }
        }
    }

    // Whatever the tag parser was still holding. Not emitted to a reader who
    // has already stopped watching: the turn was cancelled, and appending to a
    // message they asked to abandon reads as the stop not having worked.
    let tail = think_parser.finish();
    if !cancel.is_cancelled() {
        if !tail.visible_text.is_empty() {
            send(ChatStreamEvent::Text {
                content: tail.visible_text.clone(),
                message_id: message_id.to_string(),
                conversation_id: conversation_id.to_string(),
            })?;
        }
        for tag in &tail.extracted {
            send(ChatStreamEvent::Reasoning {
                content: tag.content.clone(),
                message_id: message_id.to_string(),
                conversation_id: conversation_id.to_string(),
            })?;
        }
    }
    text.push_str(&tail.visible_text);
    for tag in tail.extracted {
        reasoning.push_str(&tag.content);
    }

    // A cancelled turn keeps the text it had — that half answer is worth
    // showing — but not the calls. Running a tool the user just stopped is the
    // one thing the stop button has to prevent.
    let tool_calls: Vec<provider::ToolCall> = if cancel.is_cancelled() {
        vec![]
    } else {
        tool_acc
            .into_iter()
            .filter(|(id, _, _)| !id.is_empty())
            .map(|(id, name, args)| provider::ToolCall {
                id,
                name,
                arguments: args,
            })
            .collect()
    };

    Ok(StreamResult {
        text,
        reasoning,
        provider_state: ran_to_completion.then(|| provider_state.finish()).flatten(),
        tool_calls,
        usage,
        finish_reason,
        response_model,
        ran_to_completion,
        flushed_up_to,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::StreamEvent;
    use std::sync::Mutex;

    fn events(items: Vec<StreamEvent>) -> provider::ChatStream {
        Box::pin(futures::stream::iter(items.into_iter().map(Ok)))
    }

    /// A stream that never yields, so only cancellation ends the read.
    fn never() -> provider::ChatStream {
        Box::pin(futures::stream::pending())
    }

    /// Those events, and then silence. What comes after them can only be
    /// reached by cancelling, which is what makes a mid-read cancellation
    /// deterministic: `select!` picks at random among *ready* branches, so the
    /// stream has to stop being ready for the token to be the only way out.
    fn then_silence(items: Vec<StreamEvent>) -> provider::ChatStream {
        use futures::StreamExt;
        Box::pin(futures::stream::iter(items.into_iter().map(Ok)).chain(futures::stream::pending()))
    }

    /// Cancels the turn the moment the first visible text goes out — a user
    /// pressing Stop on the answer as it appears.
    struct StopOnText(CancellationToken);

    impl Emit for StopOnText {
        fn emit(&self, _channel: &str, payload: serde_json::Value) -> Result<(), String> {
            if payload["type"] == "text" {
                self.0.cancel();
            }
            Ok(())
        }
    }

    /// Records what went out, and can be told to fail — the two runners differ
    /// only in what they then do about it.
    struct Recorder {
        seen: Mutex<Vec<(String, String)>>,
        fail_from: Option<usize>,
        fatal: bool,
    }

    impl Recorder {
        fn watching() -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
                fail_from: None,
                fatal: true,
            }
        }

        /// Fails every send from the nth onwards, and says so — the desktop.
        fn fatal_from(n: usize) -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
                fail_from: Some(n),
                fatal: true,
            }
        }

        /// Fails the same way but never admits it — OneBot.
        fn best_effort_from(n: usize) -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
                fail_from: Some(n),
                fatal: false,
            }
        }

        fn kinds(&self) -> Vec<(String, String)> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Emit for Recorder {
        fn emit(&self, channel: &str, payload: serde_json::Value) -> Result<(), String> {
            assert_eq!(channel, "chat-stream");
            let mut seen = self.seen.lock().unwrap();
            let failing = self.fail_from.is_some_and(|n| seen.len() >= n);
            seen.push((
                payload["type"].as_str().unwrap_or_default().to_string(),
                payload["content"].as_str().unwrap_or_default().to_string(),
            ));
            if failing && self.fatal {
                return Err("the window went away".into());
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_stream_that_ran_out_did_run_to_completion() {
        let cancel = CancellationToken::new();
        let r = consume_stream(
            events(vec![StreamEvent::Text { content: "hi".into() }]),
            &cancel,
            None,
            None,
            "m1",
            "c1",
        )
        .await
        .unwrap();

        assert!(r.ran_to_completion);
        assert_eq!(r.text, "hi");
        // And it says so without a stop event, which plenty of providers never
        // send — so `finish_reason` could not have stood in for this.
        assert!(r.finish_reason.is_none());
    }

    /// `Ok` from a read is not "the model answered". A cancelled read also
    /// returns `Ok`, because the half of the answer that did arrive is worth
    /// keeping — which is exactly why the caller that needs "did it reach the
    /// end" cannot use `Ok` for it.
    #[tokio::test]
    async fn a_cancelled_read_did_not_run_to_completion() {
        let cancel = CancellationToken::new();
        cancel.cancel();

        let r = consume_stream(never(), &cancel, None, None, "m1", "c1").await.unwrap();

        assert!(!r.ran_to_completion, "stopped is not finished");
    }

    /// A provider that answers 200 and then refuses over SSE processed nothing.
    /// That is an error rather than a completion, and the retry and
    /// context-overflow branches upstream depend on it being one.
    #[tokio::test]
    async fn an_error_inside_the_stream_is_not_a_completion() {
        let cancel = CancellationToken::new();
        let r = consume_stream(
            events(vec![StreamEvent::Error {
                message: "context_length_exceeded".into(),
            }]),
            &cancel,
            None,
            None,
            "m1",
            "c1",
        )
        .await;

        assert!(matches!(r, Err(ref e) if e == "context_length_exceeded"));
    }

    /// Stop, pressed while the answer was arriving.
    ///
    /// Two halves, and they pull opposite ways. What was already said stays —
    /// the user watched it appear and deleting it reads as the transcript
    /// lying. What was not yet done does not happen: running a tool the user
    /// just stopped is the one thing the stop button has to prevent, and by
    /// this point the call is fully accumulated and one `break` away from
    /// being returned.
    ///
    /// Cancelled mid-read rather than before it, so both halves are real: a
    /// token cancelled up front proves nothing about text, because there is
    /// none yet.
    #[tokio::test]
    async fn a_cancelled_read_keeps_what_was_said_and_drops_what_was_not_done() {
        let cancel = CancellationToken::new();
        let stopper = StopOnText(cancel.clone());
        // The call is complete before the text arrives, so it is sitting in the
        // accumulator with nothing left to do but be returned.
        let stream = then_silence(vec![
            StreamEvent::ToolCallStart {
                index: 0,
                id: "c1".into(),
                name: "edit_file".into(),
            },
            StreamEvent::ToolCallDone {
                index: 0,
                arguments: "{}".into(),
            },
            StreamEvent::Text {
                content: "on it —".into(),
            },
        ]);

        let r = consume_stream(stream, &cancel, Some(&stopper), None, "m1", "c1")
            .await
            .unwrap();

        assert_eq!(r.text, "on it —", "what the user watched arrive is still there");
        assert!(r.tool_calls.is_empty(), "and the file is not edited");
        assert!(!r.ran_to_completion);
    }

    /// The whole reason `Emit` returns a `Result`. Both runners send the same
    /// events; they disagree only about what a failed send means, and folding
    /// that into `Option<&dyn Emit>` would give one of them the other's
    /// behaviour without anything failing to compile.
    #[tokio::test]
    async fn a_failed_send_is_fatal_for_one_runner_and_nothing_for_the_other() {
        let script = || {
            events(vec![
                StreamEvent::Text { content: "one".into() },
                StreamEvent::Text { content: "two".into() },
                StreamEvent::Text {
                    content: "three".into(),
                },
            ])
        };
        let cancel = CancellationToken::new();

        let desktop = Recorder::fatal_from(1);
        let stopped = consume_stream(script(), &cancel, Some(&desktop), None, "m1", "c1").await;
        assert!(
            stopped.is_err(),
            "the desktop's events are the answer; losing one ends the turn"
        );
        assert_eq!(desktop.kinds().len(), 2, "and it stops at the one that failed");

        let onebot = Recorder::best_effort_from(1);
        let carried_on = consume_stream(script(), &cancel, Some(&onebot), None, "m1", "c1")
            .await
            .expect("a QQ turn's answer does not travel over these events");
        assert_eq!(carried_on.text, "onetwothree");
        assert_eq!(onebot.kinds().len(), 3);
    }

    /// Same script, two adapters that both succeed: what the reader sees must
    /// not depend on which runner is driving.
    #[tokio::test]
    async fn both_runners_produce_the_same_events_when_nothing_fails() {
        let script = || {
            events(vec![
                StreamEvent::Text {
                    content: "vis <think>hidden".into(),
                },
                StreamEvent::Text {
                    content: "</think> more".into(),
                },
                StreamEvent::Reasoning {
                    content: "aside".into(),
                },
            ])
        };
        let cancel = CancellationToken::new();

        let a = Recorder::watching();
        let ra = consume_stream(script(), &cancel, Some(&a), None, "m1", "c1")
            .await
            .unwrap();
        let b = Recorder::best_effort_from(usize::MAX);
        let rb = consume_stream(script(), &cancel, Some(&b), None, "m1", "c1")
            .await
            .unwrap();

        assert_eq!(a.kinds(), b.kinds());
        assert_eq!(ra.text, rb.text);
        assert_eq!(ra.reasoning, rb.reasoning);
        // And the inline <think> really was routed away from the text.
        assert!(!ra.text.contains("hidden"));
        assert!(ra.reasoning.contains("hidden"));
    }

    fn server_search(id: &str, completed: bool) -> StreamEvent {
        StreamEvent::ServerToolCall(provider::ServerToolCall {
            id: id.into(),
            name: "web_search".into(),
            arguments: None,
            sources: Vec::new(),
            completed,
        })
    }

    struct Says(Mutex<Vec<String>>);

    impl Says {
        fn new() -> Self {
            Self(Mutex::new(Vec::new()))
        }

        fn all(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Commentary for Says {
        async fn say(&self, text: String) {
            self.0.lock().unwrap().push(text);
        }
    }

    async fn read_with_says(items: Vec<StreamEvent>, says: &Says) -> StreamResult {
        let cancel = CancellationToken::new();
        consume_stream(events(items), &cancel, None, Some(says), "m1", "c1")
            .await
            .unwrap()
    }

    /// Visible text on either side of two distinct searches is three beats:
    /// the first two leave through Commentary, the last is the round's reply.
    #[tokio::test]
    async fn a_new_server_tool_flushes_the_text_said_so_far() {
        let says = Says::new();
        let r = read_with_says(
            vec![
                StreamEvent::Text {
                    content: "先查房价。".into(),
                },
                server_search("ws_a", false),
                StreamEvent::Text {
                    content: "三套。".into(),
                },
                server_search("ws_b", false),
                StreamEvent::Text {
                    content: "不能换。".into(),
                },
            ],
            &says,
        )
        .await;

        assert_eq!(says.all(), ["先查房价。", "三套。"]);
        assert_eq!(r.text, "先查房价。三套。不能换。");
        assert_eq!(r.unflushed(), "不能换。");
    }

    /// The `done` revision of the same id is not a second boundary, even when
    /// more text arrived while the search ran. Dropping the id set makes this
    /// fire twice.
    #[tokio::test]
    async fn the_done_revision_of_the_same_search_does_not_flush_again() {
        let says = Says::new();
        let r = read_with_says(
            vec![
                StreamEvent::Text {
                    content: "先查。".into(),
                },
                server_search("ws_a", false),
                StreamEvent::Text {
                    content: "搜的时候又说了。".into(),
                },
                server_search("ws_a", true),
            ],
            &says,
        )
        .await;

        assert_eq!(says.all(), ["先查。"]);
        assert_eq!(r.unflushed(), "搜的时候又说了。");
    }

    /// xAI's typical shape: searches first, then one message. Nothing to say
    /// at the boundary, and the whole paragraph is still the round's reply.
    #[tokio::test]
    async fn a_search_with_nothing_said_yet_does_not_flush() {
        let says = Says::new();
        let r = read_with_says(
            vec![
                server_search("ws_a", false),
                server_search("ws_a", true),
                server_search("ws_b", false),
                server_search("ws_b", true),
                StreamEvent::Text {
                    content: "我先查了一下，三套，不能换。".into(),
                },
            ],
            &says,
        )
        .await;

        assert!(says.all().is_empty());
        assert_eq!(r.flushed_up_to, 0);
        assert_eq!(r.unflushed(), "我先查了一下，三套，不能换。");
        assert_eq!(r.text, r.unflushed());
    }

    /// The desktop has no Commentary. The concatenated text is the reply.
    #[tokio::test]
    async fn without_commentary_a_server_tool_does_not_split_the_reply() {
        let cancel = CancellationToken::new();
        let r = consume_stream(
            events(vec![
                StreamEvent::Text {
                    content: "先查房价。".into(),
                },
                server_search("ws_a", false),
                StreamEvent::Text {
                    content: "不能换。".into(),
                },
            ]),
            &cancel,
            None,
            None,
            "m1",
            "c1",
        )
        .await
        .unwrap();

        assert_eq!(r.flushed_up_to, 0);
        assert_eq!(r.text, "先查房价。不能换。");
        assert_eq!(r.unflushed(), r.text);
    }
}

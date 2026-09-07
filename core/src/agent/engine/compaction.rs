//! When a turn runs out of room, and what it does about it.
//!
//! Not a boolean. The two runners differ in three ways at once — whether a
//! preference can switch it off, whether a repeatedly failing summariser is
//! allowed to stop trying, and whether any of it is announced — and a flag per
//! difference would let combinations exist that neither runner has ever been.
//!
//! So it is a policy with two moments. `on_overflow` is recovery: the provider
//! has already refused the request, and something has to come out of the history
//! before it can be sent again. `between_rounds` is prevention: the budget says
//! the next request would be tight, and there is a quiet moment to fix it.
//!
//! Both policies now climb the same ladder in both moments — cheap pass, then a
//! summariser, then a blunt trim if that fails. What is left between them is
//! everything around the ladder rather than the ladder itself: the desktop's is
//! gated on a preference, guarded by a circuit breaker so a summariser that
//! keeps failing stops being asked, and narrated on `compact-start` /
//! `compact-done`. So the variants are really "attended" and "unattended", and
//! are named after their runners for historical reasons only.

use std::sync::Arc;

use crate::agent::{CompactCircuitBreaker, TokenBudget, microcompact, mid_turn_compact, trim_to_context_limit};
use crate::events::{CompactDoneEvent, CompactOutcome, CompactStartEvent, CompactTrigger};
use crate::provider::{ChatMessage, ChatParams, ChatProvider};

use super::Emit;

/// Everything a compaction pass is allowed to touch.
pub(crate) struct Compacting<'a> {
    pub messages: &'a mut Vec<ChatMessage>,
    pub budget: &'a mut TokenBudget,
    /// The turn's own provider. A summary written by a different model than the
    /// one being summarised for would be counted against the wrong tokenizer.
    pub provider: &'a dyn ChatProvider,
    pub params: &'a ChatParams,
    pub keep_recent: usize,
    pub context_limit: usize,
    pub emit: Option<&'a dyn Emit>,
    pub conversation_id: &'a str,
}

impl Compacting<'_> {
    /// The cheap rung: truncate the old tool output and drop the inlined
    /// images, keeping every message. Costs no model call, which is why both
    /// moments reach for it before anything that does.
    ///
    /// Hands back what it freed, because the threshold path puts that number in
    /// front of a person.
    fn cheap_pass(&mut self) -> usize {
        let reclaimed = microcompact(self.messages, self.budget, self.keep_recent);
        self.budget.update_estimate(self.messages);
        reclaimed
    }

    /// The last resort, and the only step that cannot fail. Half the window and
    /// half the tail: deliberately harsher than the threshold path, because by
    /// the time anything calls this the alternative is a request that will be
    /// refused again.
    fn trim(&mut self) {
        trim_to_context_limit(self.messages, self.context_limit / 2, (self.keep_recent / 2).max(2));
        // Re-measured here rather than at each of the four call sites. Trimming
        // is the one step that always changes what the estimate describes, and
        // the caller sizes the retry's output allowance from it — a stale one
        // asks for room this just spent.
        self.budget.update_estimate(self.messages);
    }

    /// The one record that a pass happened at all. `announce` reaches a window
    /// that may not be open, and a successful summary logs nothing on its own,
    /// so without this the only evidence compaction ever ran is the token count
    /// not moving -- which is also what never running looks like. Counts only:
    /// nothing here comes from a message body.
    fn report(&self, trigger: &str, before: usize, rung: &str) {
        tracing::info!(
            conversation_id = %self.conversation_id,
            trigger,
            rung,
            before,
            after = self.budget.current_estimate,
            threshold = self.budget.compact_threshold,
            limit = self.context_limit,
            "compacted mid-turn"
        );
    }

    fn announce_start(&self, trigger: CompactTrigger) {
        let Some(emit) = self.emit else { return };
        let _ = emit.emit_compact_start(CompactStartEvent {
            conversation_id: self.conversation_id.to_string(),
            mid_turn: true,
            trigger,
        });
    }

    fn announce_done(
        &self,
        trigger: CompactTrigger,
        outcome: CompactOutcome,
        tokens_reclaimed: Option<usize>,
        error: Option<String>,
    ) {
        let Some(emit) = self.emit else { return };
        let _ = emit.emit_compact_done(CompactDoneEvent {
            conversation_id: self.conversation_id.to_string(),
            mid_turn: true,
            trigger,
            outcome,
            tokens_reclaimed: tokens_reclaimed.map(|tokens| tokens as u64),
            error,
        });
    }
}

pub enum CompactionPolicy {
    Desktop {
        /// The assistant's `auto_compact_enabled`. Governs the threshold pass
        /// only — recovery from a refused request is not a preference, it is the
        /// alternative to giving up on the turn.
        enabled: bool,
        breaker: Arc<CompactCircuitBreaker>,
    },
    OneBot,
}

impl CompactionPolicy {
    /// The provider refused the request as too large.
    ///
    /// Leaves the estimate describing what is left, on every path: the caller
    /// sizes the retry's output allowance from it, and a stale one would ask for
    /// room the trim just spent.
    pub(crate) async fn on_overflow(&self, mut c: Compacting<'_>) {
        // Whatever the refused request was sized against.
        let before = c.budget.current_estimate;
        match self {
            CompactionPolicy::OneBot => {
                // Ahead of the trim rather than instead of it. Truncating the
                // old tool output can put the history back under the limit on
                // its own, and then the trim discards nothing — which is the
                // whole of what this rung buys here: the same recovery, minus
                // the turns it used to throw away to get there.
                c.cheap_pass();
                c.trim();
                // Still "trim": the rung names the deepest one reached, which is
                // how the other policy reads, and `before`/`after` in the same
                // record already show what the cheap pass achieved.
                c.report("api_error", before, "trim");
            }
            CompactionPolicy::Desktop { breaker, .. } => {
                c.cheap_pass();
                if !(c.budget.needs_compact() && breaker.can_compact()) {
                    c.trim();
                    c.report("api_error", before, "trim");
                    return;
                }
                c.announce_start(CompactTrigger::ApiError);
                let (rung, outcome, error) =
                    match mid_turn_compact(c.messages, c.budget, c.provider, c.params, c.keep_recent).await {
                        Ok(_) => {
                            breaker.record_success();
                            c.budget.update_estimate(c.messages);
                            ("summary", CompactOutcome::Completed, None)
                        }
                        Err(error) => {
                            breaker.record_failure();
                            c.trim();
                            ("trim", CompactOutcome::Fallback, Some(error.to_string()))
                        }
                    };
                c.announce_done(CompactTrigger::ApiError, outcome, None, error);
                c.report("api_error", before, rung);
            }
        }
    }

    /// The tool results are settled and the next request has not been built yet.
    pub(crate) async fn between_rounds(&self, mut c: Compacting<'_>) {
        c.budget.update_estimate(c.messages);
        if !c.budget.needs_compact() {
            return;
        }
        let before = c.budget.current_estimate;
        match self {
            CompactionPolicy::OneBot => {
                c.cheap_pass();
                let mut rung = "microcompact";
                if c.budget.needs_compact() {
                    rung = "summary";
                    if let Err(e) = mid_turn_compact(c.messages, c.budget, c.provider, c.params, c.keep_recent).await {
                        tracing::warn!("OneBot mid-turn compact failed: {e}");
                        rung = "trim";
                        c.trim();
                    }
                    c.budget.update_estimate(c.messages);
                }
                c.report("threshold", before, rung);
            }
            CompactionPolicy::Desktop { enabled, breaker } => {
                if !(*enabled && breaker.can_compact()) {
                    return;
                }
                c.announce_start(CompactTrigger::Threshold);
                let reclaimed = c.cheap_pass();
                if !c.budget.needs_compact() {
                    // Said even when the cheap pass freed nothing, because the
                    // start went out and a window left holding it would show a
                    // compaction that never ends.
                    c.announce_done(
                        CompactTrigger::Threshold,
                        CompactOutcome::Completed,
                        Some(reclaimed),
                        None,
                    );
                    c.report("threshold", before, "microcompact");
                    return;
                }
                match mid_turn_compact(c.messages, c.budget, c.provider, c.params, c.keep_recent).await {
                    Ok(more) => {
                        breaker.record_success();
                        c.budget.update_estimate(c.messages);
                        c.announce_done(
                            CompactTrigger::Threshold,
                            CompactOutcome::Completed,
                            Some(reclaimed + more),
                            None,
                        );
                        c.report("threshold", before, "summary");
                    }
                    Err(e) => {
                        tracing::warn!("Mid-turn compact failed: {e}");
                        breaker.record_failure();
                        c.trim();
                        c.budget.update_estimate(c.messages);
                        c.announce_done(
                            CompactTrigger::Threshold,
                            CompactOutcome::Fallback,
                            Some(reclaimed),
                            Some(e.to_string()),
                        );
                        c.report("threshold", before, "trim");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{AgentResponse, ChatStream, ProviderError, ToolDefinition};

    /// Recovery that reaches for the model has already lost: the provider
    /// refused this very request a moment ago, and the summariser would go to
    /// the same one. Any call at all fails the test.
    struct NeverAsked;

    #[async_trait::async_trait]
    impl ChatProvider for NeverAsked {
        fn adapter_name(&self) -> &'static str {
            "NeverAsked"
        }

        async fn stream_chat_with_tools(
            &self,
            _messages: Vec<ChatMessage>,
            _tools: Vec<ToolDefinition>,
            _params: ChatParams,
        ) -> Result<ChatStream, ProviderError> {
            panic!("recovery asked the model to stream")
        }

        async fn chat(&self, _messages: Vec<ChatMessage>, _params: ChatParams) -> Result<String, ProviderError> {
            panic!("recovery asked the model for a summary")
        }

        async fn chat_with_tools(
            &self,
            _messages: Vec<ChatMessage>,
            _tools: Vec<ToolDefinition>,
            _params: ChatParams,
        ) -> Result<AgentResponse, ProviderError> {
            panic!("recovery asked the model")
        }
    }

    const LIMIT: usize = 32_000;

    fn budget() -> TokenBudget {
        TokenBudget::new("openai", "gpt-4o", LIMIT, 8_000, None)
    }

    fn compacting<'a>(
        messages: &'a mut Vec<ChatMessage>,
        budget: &'a mut TokenBudget,
        provider: &'a dyn ChatProvider,
        params: &'a ChatParams,
    ) -> Compacting<'a> {
        Compacting {
            messages,
            budget,
            provider,
            params,
            keep_recent: 2,
            context_limit: LIMIT,
            emit: None,
            conversation_id: "c1",
        }
    }

    fn system(text: &str) -> ChatMessage {
        ChatMessage {
            role: "system".into(),
            ..ChatMessage::user(text)
        }
    }

    /// A history whose weight is one old tool result, with a short tail behind
    /// it that the cheap pass is not allowed to touch. Over the trim's own
    /// threshold as it stands, and under it once the tool result is truncated —
    /// which is the whole distinction being tested.
    fn one_heavy_tool_result() -> Vec<ChatMessage> {
        vec![
            system("you are helpful"),
            ChatMessage::tool_result("c1", &"word ".repeat(14_000)),
            ChatMessage::user("and then?"),
            ChatMessage::assistant("this"),
            ChatMessage::user("go on"),
            ChatMessage::assistant("that"),
        ]
    }

    /// The thing this policy could not do before: truncating the old output can
    /// put the history back under the limit on its own, and then nothing has to
    /// be thrown away at all.
    #[tokio::test]
    async fn overflow_recovery_truncates_before_it_discards() {
        let (mut messages, mut budget) = (one_heavy_tool_result(), budget());
        let before = messages.len();
        budget.update_estimate(&messages);
        let was = budget.current_estimate;

        CompactionPolicy::OneBot
            .on_overflow(compacting(
                &mut messages,
                &mut budget,
                &NeverAsked,
                &ChatParams::default(),
            ))
            .await;

        assert_eq!(messages.len(), before, "discarded turns it did not have to");
        assert!(
            budget.current_estimate < was,
            "freed nothing: {was} -> {}",
            budget.current_estimate
        );
    }

    /// And it is still only the first rung. A history with nothing truncatable
    /// in it has to lose messages, which is what the trim is for.
    #[tokio::test]
    async fn overflow_recovery_still_discards_when_truncating_is_not_enough() {
        let mut messages = vec![system("you are helpful")];
        for _ in 0..8 {
            messages.push(ChatMessage::user(&"word ".repeat(2_000)));
        }
        let (before, mut budget) = (messages.len(), budget());
        budget.update_estimate(&messages);

        CompactionPolicy::OneBot
            .on_overflow(compacting(
                &mut messages,
                &mut budget,
                &NeverAsked,
                &ChatParams::default(),
            ))
            .await;

        assert!(
            messages.len() < before,
            "nothing came out of a history that had to shrink"
        );
    }

    /// The caller sizes the retry's output allowance from the estimate, so an
    /// estimate describing the history as it was before the pass asks for room
    /// the pass just spent.
    #[tokio::test]
    async fn the_estimate_describes_what_is_left() {
        for messages in [one_heavy_tool_result(), vec![system("s"), ChatMessage::user("hi")]] {
            let (mut messages, mut budget) = (messages, budget());
            budget.update_estimate(&messages);

            CompactionPolicy::OneBot
                .on_overflow(compacting(
                    &mut messages,
                    &mut budget,
                    &NeverAsked,
                    &ChatParams::default(),
                ))
                .await;

            assert_eq!(budget.current_estimate, budget.counter.count_messages(&messages));
        }
    }
}

//! Keeping a prompt cache warm while its turn is still running.
//!
//! Ported from Pi (`cache-warmer.ts`, MIT, Copyright (c) 2025 Mario Zechner);
//! see NOTICE. Rewritten against Meridian's `ChatProvider` and audit ledger.
//!
//! Only the streaming lifecycle is implemented: a timer lives for as long as
//! the turn that started it, and is cancelled when that turn ends. Idle warming
//! — re-sending after the reader has walked away — is deliberately absent until
//! the streaming numbers have been measured on real traffic.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::agent::pricing::Prices;
use crate::db::models::message::MessageUsage;
use crate::db::sea::cap::Db;
use crate::db::sea::ops::audit::{self, SideRequestCost};
use crate::decimal::Decimal;
use crate::provider::{ChatMessage, ChatParams, ChatProvider, ThinkingStyle, ToolDefinition};

/// Below this, a prompt is cheap enough that a refresh costs more thought than
/// it saves. The same floor Pi uses before it tries the arithmetic.
const MIN_WARM_PROMPT_TOKENS: usize = 4_096;
/// A refresh is sent only when it is expected to save at least this much.
const MINIMUM_EXPECTED_SAVINGS: &str = "0.05";
const PER_MILLION: &str = "0.000001";

/// Everything a refresh needs that outlives the request that produced it.
pub struct WarmSeed {
    pub provider: Arc<dyn ChatProvider>,
    pub db: Db,
    pub ttl: Duration,
    pub prices: Option<Prices>,
    pub provider_id: Option<String>,
    pub provider_name: Option<String>,
    pub model_id: Option<String>,
    pub conversation_id: String,
    pub turn_id: String,
}

/// Budget-based thinking derives `budget_tokens` from `max_tokens`. A replay
/// capped at one token would ask a different question and, on Anthropic, key
/// the cache on different bytes — so it is not a refresh at all. Adaptive
/// thinking is replayable; it takes no budget from the cap.
fn replayable(params: &ChatParams) -> bool {
    !(params.thinking_enabled && params.thinking_style == ThinkingStyle::Budget)
}

/// Whether one refresh is expected to be cheaper than the miss it prevents.
///
/// A miss bills the prompt at the input rate; a refresh bills one cache read
/// and the request it saves bills another. Nothing is decided when a price is
/// missing — the arithmetic would be a guess, and guesses here spend money.
fn worthwhile(prices: Option<&Prices>, prompt_tokens: usize) -> bool {
    if prompt_tokens < MIN_WARM_PROMPT_TOKENS {
        return false;
    }
    let Some(prices) = prices else {
        return false;
    };
    let Some(input) = prices.input_price.as_ref() else {
        return false;
    };
    let read = prices.cache_read_price.as_ref().unwrap_or(input);
    let per_token = input.clone() - read.clone() - read.clone();
    if per_token.is_negative() {
        return false;
    }
    let per_million: Decimal = PER_MILLION.parse().expect("constant decimal");
    let floor: Decimal = MINIMUM_EXPECTED_SAVINGS.parse().expect("constant decimal");
    Decimal::from(prompt_tokens as i64) * per_token * per_million >= floor
}

/// One request the turn actually sent.
pub struct WarmSnapshot {
    pub params: ChatParams,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDefinition>,
    /// The prompt this request carried, as the turn already estimated it.
    pub prompt_tokens: usize,
    /// The assistant row the spend is filed against.
    pub message_id: String,
}

/// Owns the timer for one turn. Dropping it is what stops warming, so every
/// early return in the turn loop stops it without having to remember to.
pub struct WarmGuard {
    seed: Arc<WarmSeed>,
    state: Mutex<Option<CancellationToken>>,
}

impl WarmGuard {
    pub fn start(seed: WarmSeed) -> Self {
        Self {
            seed: Arc::new(seed),
            state: Mutex::new(None),
        }
    }

    /// Record the request that was just sent and (re)arm the timer.
    ///
    /// Every real request resets the window: the prefix it wrote is the one
    /// worth keeping, and the one before it is now one round older.
    /// Whether this request is worth a refresh at all.
    ///
    /// Asked before the snapshot is built so an ordinary short turn does not
    /// pay for cloning its history into a refresh that would be refused.
    pub fn should_touch(&self, params: &ChatParams, prompt_tokens: usize) -> bool {
        replayable(params) && self.seed.worthwhile(prompt_tokens)
    }

    pub fn touch(&self, snapshot: WarmSnapshot) {
        if !self.should_touch(&snapshot.params, snapshot.prompt_tokens) {
            return;
        }
        let cancel = CancellationToken::new();
        {
            let Ok(mut state) = self.state.lock() else { return };
            if let Some(previous) = state.take() {
                previous.cancel();
            }
            *state = Some(cancel.clone());
        }
        let seed = Arc::clone(&self.seed);
        // Ninety percent of the way through the entry's life, leaving the
        // dispatch itself inside the window rather than after it.
        let delay = seed.ttl.mul_f64(0.9);
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(delay) => {}
            }
            if let Err(error) = seed.warm_once(snapshot).await {
                // A refresh that failed costs nothing extra and is not worth
                // the user's attention; the next real request will write the
                // cache again.
                tracing::debug!(error = %error, "cache warm request failed");
            }
        });
    }
}

impl Drop for WarmGuard {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock()
            && let Some(cancel) = state.take()
        {
            cancel.cancel();
        }
    }
}

impl WarmSeed {
    fn worthwhile(&self, prompt_tokens: usize) -> bool {
        worthwhile(self.prices.as_ref(), prompt_tokens)
    }

    async fn warm_once(&self, snapshot: WarmSnapshot) -> Result<(), String> {
        let params = ChatParams {
            max_tokens: Some(1),
            ..snapshot.params
        };
        let response = self
            .provider
            .chat_with_tools(snapshot.messages, snapshot.tools, params)
            .await
            .map_err(|error| error.to_string())?;
        let Some(usage) = response.usage else {
            return Ok(());
        };
        let usage = MessageUsage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
            server_tool_calls: usage.billable_tool_calls,
        };
        let cost = SideRequestCost {
            role: audit::CACHE_WARM_ROLE,
            message_id: &snapshot.message_id,
            conversation_id: &self.conversation_id,
            turn_id: Some(&self.turn_id),
            provider_id: self.provider_id.as_deref(),
            provider_name: self.provider_name.as_deref(),
            model_id: self.model_id.as_deref(),
            usage,
            peak_prompt_tokens: snapshot.prompt_tokens.try_into().ok(),
            summary: "cache warm",
        };
        self.db
            .write(async |tx| audit::record_side_request(tx, cost).await)
            .await
            .map_err(|error| error.to_string())
    }
}

/// Whether this provider's prompt cache is worth keeping warm at all, and for
/// how long.
///
/// `None` means the upstream's lifetime is unknown, and an unknown lifetime is
/// not a licence to guess: a refresh sent after the entry expired is a full
/// price write, which is exactly the cost warming exists to avoid.
pub fn ttl_for(provider_type: &str) -> Option<Duration> {
    match provider_type {
        // The documented default entry lifetime for Anthropic's ephemeral
        // breakpoints. xAI and OpenAI document the same order of magnitude;
        // DeepSeek manages its cache itself and accepts the replay safely.
        "anthropic" | "openai" | "xai" | "deepseek" => Some(Duration::from_secs(300)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn priced(input: &str, read: &str) -> Prices {
        Prices {
            input_price: Some(input.parse().unwrap()),
            output_price: Some("4".parse().unwrap()),
            cache_read_price: Some(read.parse().unwrap()),
            cache_write_price: Some("2.5".parse().unwrap()),
            server_tool_price: None,
        }
    }

    #[test]
    fn a_small_prompt_is_not_worth_a_refresh() {
        assert!(!worthwhile(Some(&priced("2", "0.5")), MIN_WARM_PROMPT_TOKENS - 1));
    }

    #[test]
    fn a_cheap_prefix_is_not_worth_a_refresh() {
        // 10k tokens × (2 − 0.5 − 0.5) per million = $0.01, under the $0.05 floor.
        assert!(!worthwhile(Some(&priced("2", "0.5")), 10_000));
        // 100k crosses it.
        assert!(worthwhile(Some(&priced("2", "0.5")), 100_000));
    }

    /// A cache read that costs as much as the input makes the replay a loss.
    #[test]
    fn an_unpriced_premium_is_not_worth_a_refresh() {
        assert!(!worthwhile(Some(&priced("2", "1.1")), 100_000));
    }

    #[test]
    fn a_missing_price_is_not_a_reason_to_spend() {
        assert!(!worthwhile(None, 1_000_000));
    }

    #[test]
    fn budget_thinking_cannot_be_replayed() {
        let mut params = ChatParams {
            thinking_enabled: true,
            thinking_style: ThinkingStyle::Budget,
            ..Default::default()
        };
        assert!(!replayable(&params));
        params.thinking_style = ThinkingStyle::Adaptive;
        assert!(replayable(&params));
    }

    #[test]
    fn an_unknown_provider_gets_no_ttl() {
        assert!(ttl_for("anthropic").is_some());
        assert!(ttl_for("google").is_none());
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::provider::{AgentResponse, ChatStream, ProviderError};

    struct CountingProvider {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ChatProvider for CountingProvider {
        #[cfg(test)]
        fn adapter_name(&self) -> &'static str {
            "counting"
        }

        async fn chat(&self, _messages: Vec<ChatMessage>, _params: ChatParams) -> Result<String, ProviderError> {
            Err(ProviderError::Parse("unused".into()))
        }

        async fn stream_chat_with_tools(
            &self,
            _messages: Vec<ChatMessage>,
            _tools: Vec<ToolDefinition>,
            _params: ChatParams,
        ) -> Result<ChatStream, ProviderError> {
            Err(ProviderError::Parse("unused".into()))
        }

        async fn chat_with_tools(
            &self,
            _messages: Vec<ChatMessage>,
            _tools: Vec<ToolDefinition>,
            _params: ChatParams,
        ) -> Result<AgentResponse, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // No usage, so the refresh never reaches the ledger and the test
            // needs no seeded database beyond what `sea_test_db` builds.
            Ok(AgentResponse {
                text: String::new(),
                reasoning_content: None,
                tool_calls: Vec::new(),
                usage: None,
                provider_state: None,
            })
        }
    }

    fn snapshot(prompt_tokens: usize) -> WarmSnapshot {
        WarmSnapshot {
            params: ChatParams::default(),
            messages: vec![ChatMessage::user("hi")],
            tools: Vec::new(),
            prompt_tokens,
            message_id: "m".into(),
        }
    }

    async fn guard(calls: Arc<AtomicUsize>) -> WarmGuard {
        WarmGuard::start(WarmSeed {
            provider: Arc::new(CountingProvider { calls }),
            db: crate::db::sea::sea_test_db().await,
            ttl: Duration::from_millis(60),
            prices: Some(priced("2", "0.5")),
            provider_id: None,
            provider_name: None,
            model_id: None,
            conversation_id: "c".into(),
            turn_id: "t".into(),
        })
    }

    #[tokio::test]
    async fn a_refresh_fires_before_the_entry_expires() {
        let calls = Arc::new(AtomicUsize::new(0));
        let guard = guard(Arc::clone(&calls)).await;
        guard.touch(snapshot(100_000));
        tokio::time::sleep(Duration::from_millis(90)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// The whole streaming lifecycle: dropping the guard is what stops it, so
    /// every early return in the turn loop stops warming without remembering.
    #[tokio::test]
    async fn dropping_the_guard_cancels_the_refresh() {
        let calls = Arc::new(AtomicUsize::new(0));
        let guard = guard(Arc::clone(&calls)).await;
        guard.touch(snapshot(100_000));
        drop(guard);
        tokio::time::sleep(Duration::from_millis(90)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    /// A second real request replaces the timer rather than leaving two
    /// refreshes pointed at different prefixes.
    #[tokio::test]
    async fn a_second_request_replaces_the_timer() {
        let calls = Arc::new(AtomicUsize::new(0));
        let guard = guard(Arc::clone(&calls)).await;
        guard.touch(snapshot(100_000));
        tokio::time::sleep(Duration::from_millis(30)).await;
        guard.touch(snapshot(100_000));
        tokio::time::sleep(Duration::from_millis(90)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

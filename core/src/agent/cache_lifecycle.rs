//! Conversation-level idle prompt cache lifecycle management.
//!
//! While [`super::cache_warmer::WarmGuard`] keeps a prefix warm *during* a
//! single long-running turn (and cancels when the turn ends), this manager
//! handles what happens *between* turns — critical for OneBot/QQ bots and
//! interactive desktop chats where pauses regularly cross a 5-minute or 1-hour
//! cache TTL.
//!
//! Four per-assistant policies are supported:
//! - [`CacheExpiryPolicy::Off`]: no idle timer.
//! - [`CacheExpiryPolicy::Warm`]: proactively refresh up to `max_warm_count`
//!   consecutive times before stopping.
//! - [`CacheExpiryPolicy::Compact`]: right before the warm cache expires, run
//!   prefix-preserving forked compaction using the conversation's own model if
//!   prompt tokens meet `compact_min_tokens`.
//! - [`CacheExpiryPolicy::WarmThenCompact`]: refresh up to `max_warm_count`
//!   times, then run forked compaction before the final refreshed window ends.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::agent::cache_warmer::{CacheExpiryPolicy, WarmSeed, WarmSnapshot, replayable, worthwhile_for_ttl};
use crate::agent::compact::{WarmCacheCompactRequest, do_compact_with_warm_cache};
use crate::agent::pricing::Prices;
use crate::provider::{ChatMessage, ChatParams, ChatProvider, ToolDefinition};
use crate::services::Services;

/// Data captured from the final API round of a completed turn so the idle
/// manager can either replay the exact cached prefix (warm) or extend it with a
/// compaction instruction (forked compact).
#[derive(Clone)]
pub struct CompletedRoundSnapshot {
    pub params: ChatParams,
    pub warm_messages: Vec<ChatMessage>,
    pub last_assistant: Option<ChatMessage>,
    pub tools: Vec<ToolDefinition>,
    pub prompt_tokens: usize,
    pub message_id: String,
}

/// Everything the idle cache manager needs once a turn completes.
pub struct IdleCacheSeed {
    pub conversation_id: String,
    pub turn_id: String,
    pub provider: Arc<dyn ChatProvider>,
    pub provider_id: Option<String>,
    pub provider_name: Option<String>,
    pub model_id: String,
    pub prices: Option<Prices>,
    pub ttl: Duration,
    pub policy: CacheExpiryPolicy,
    pub max_warm_count: u32,
    pub compact_min_tokens: usize,
    pub keep_recent: usize,
    pub context_limit: usize,
    pub snapshot: CompletedRoundSnapshot,
}

impl IdleCacheSeed {
    /// Resolve policy and thresholds from the assistant & model once a turn
    /// finishes. Shared by Desktop and OneBot so both runners obey one rule.
    #[allow(clippy::too_many_arguments)]
    pub fn from_turn(
        assistant: Option<&crate::db::entity::assistant::Model>,
        compact_threshold: Option<usize>,
        conversation_id: String,
        turn_id: String,
        provider: Arc<dyn ChatProvider>,
        provider_id: Option<String>,
        provider_name: Option<String>,
        model_id: String,
        prices: Option<Prices>,
        ttl: Duration,
        keep_recent: usize,
        context_limit: usize,
        snapshot: CompletedRoundSnapshot,
    ) -> Self {
        let policy = assistant
            .map(|a| a.cache_expiry_policy)
            .unwrap_or(CacheExpiryPolicy::Compact);
        let max_warm_count = assistant
            .map(|a| a.cache_max_warm_count.max(0) as u32)
            .unwrap_or(crate::agent::cache_warmer::DEFAULT_CACHE_MAX_WARM_COUNT as u32);
        let configured_min = assistant
            .map(|a| a.cache_compact_min_tokens)
            // domain-default: fallback idle-compaction token floor when a conversation has no assistant row
            .unwrap_or(crate::agent::cache_warmer::DEFAULT_CACHE_COMPACT_MIN_TOKENS);
        let compact_min_tokens = if configured_min > 0 {
            configured_min as usize
        } else {
            compact_threshold.unwrap_or(context_limit / 10 * 7)
        };
        Self {
            conversation_id,
            turn_id,
            provider,
            provider_id,
            provider_name,
            model_id,
            prices,
            ttl,
            policy,
            max_warm_count,
            compact_min_tokens,
            keep_recent,
            context_limit,
            snapshot,
        }
    }
}

struct ArmedEntry {
    generation: u64,
    cancel: CancellationToken,
}

/// Process-wide table of armed idle cache timers, keyed by `conversation_id`.
///
/// Lives on [`crate::services::ServicesInner`] alongside `TurnCoordinator` so
/// Desktop and OneBot share one lifecycle table and OneBot restarts do not drop
/// or duplicate pending timers.
#[derive(Default)]
pub struct CacheLifecycleManager {
    inner: Mutex<ManagerState>,
}

#[derive(Default)]
struct ManagerState {
    next_gen: u64,
    entries: HashMap<String, ArmedEntry>,
}

/// Compute when to fire before a cache entry of lifetime `ttl` expires.
///
/// For real-world TTLs (>= 60s), leaves a bounded 15s–60s lead margin (10% of
/// TTL clamped to `[15s, 60s]`) so 5m triggers at 4m30s and 1h triggers at
/// 59m00s rather than wasting 6 minutes of a 1-hour entry. For tiny test TTLs,
/// fires at 85% of `ttl`.
pub fn trigger_delay(ttl: Duration) -> Duration {
    let secs = ttl.as_secs();
    if secs >= 60 {
        let lead = (secs / 10).clamp(15, 60);
        Duration::from_secs(secs.saturating_sub(lead))
    } else {
        ttl.mul_f64(0.85)
    }
}

impl CacheLifecycleManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancel any pending idle timer for `conversation_id`.
    ///
    /// Called whenever a new turn starts, a manual compaction runs, a branch
    /// switches, or a conversation is reset/deleted.
    pub fn cancel(&self, conversation_id: &str) {
        let Ok(mut state) = self.inner.lock() else { return };
        if let Some(entry) = state.entries.remove(conversation_id) {
            entry.cancel.cancel();
        }
    }

    #[cfg(test)]
    pub fn is_armed(&self, conversation_id: &str) -> bool {
        self.inner
            .lock()
            .map(|s| s.entries.contains_key(conversation_id))
            .unwrap_or(false)
    }

    /// Arm (or replace) the idle cache timer for a conversation whose turn just
    /// finished cleanly.
    pub fn arm(&self, services: Services, seed: IdleCacheSeed) {
        self.cancel(&seed.conversation_id);
        if seed.policy == CacheExpiryPolicy::Off || seed.ttl.is_zero() {
            return;
        }

        let cancel = CancellationToken::new();
        let conversation_id = seed.conversation_id.clone();
        let generation = {
            let Ok(mut state) = self.inner.lock() else { return };
            state.next_gen = state.next_gen.wrapping_add(1);
            let generation = state.next_gen;
            state.entries.insert(
                conversation_id.clone(),
                ArmedEntry {
                    generation,
                    cancel: cancel.clone(),
                },
            );
            generation
        };

        tokio::spawn(async move {
            run_idle_lifecycle(services.clone(), seed, cancel).await;
            let Ok(mut state) = services.cache_lifecycle.inner.lock() else {
                return;
            };
            if state
                .entries
                .get(&conversation_id)
                .is_some_and(|e| e.generation == generation)
            {
                state.entries.remove(&conversation_id);
            }
        });
    }
}

async fn run_idle_lifecycle(services: Services, seed: IdleCacheSeed, cancel: CancellationToken) {
    let delay = trigger_delay(seed.ttl);
    let mut warmed_count: u32 = 0;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(delay) => {}
        }

        if cancel.is_cancelled() {
            return;
        }

        let want_warm = match seed.policy {
            CacheExpiryPolicy::Off => return,
            CacheExpiryPolicy::Warm | CacheExpiryPolicy::WarmThenCompact => warmed_count < seed.max_warm_count,
            CacheExpiryPolicy::Compact => false,
        };

        if want_warm {
            let eligible = replayable(&seed.snapshot.params)
                && worthwhile_for_ttl(
                    seed.prices.as_ref(),
                    seed.snapshot.prompt_tokens,
                    seed.snapshot.params.cache_ttl,
                );
            if eligible {
                // Skip if another writer currently holds the conversation.
                let lease = match services
                    .turns
                    .try_acquire_mutation(&seed.conversation_id, "idle_cache_warm")
                {
                    Ok(lease) => lease,
                    Err(_) => return,
                };
                let warm_seed = WarmSeed {
                    provider: Arc::clone(&seed.provider),
                    db: services.db.clone(),
                    ttl: seed.ttl,
                    prices: seed.prices.clone(),
                    provider_id: seed.provider_id.clone(),
                    provider_name: seed.provider_name.clone(),
                    model_id: Some(seed.model_id.clone()),
                    conversation_id: seed.conversation_id.clone(),
                    turn_id: seed.turn_id.clone(),
                };
                let warm_snapshot = WarmSnapshot {
                    params: seed.snapshot.params.clone(),
                    messages: seed.snapshot.warm_messages.clone(),
                    tools: seed.snapshot.tools.clone(),
                    prompt_tokens: seed.snapshot.prompt_tokens,
                    message_id: seed.snapshot.message_id.clone(),
                };
                let res = warm_seed.warm_once(warm_snapshot).await;
                drop(lease);
                match res {
                    Ok(()) => {
                        warmed_count += 1;
                        tracing::info!(
                            conversation_id = %seed.conversation_id,
                            warmed_count,
                            max_warm_count = seed.max_warm_count,
                            "idle prompt cache warmed"
                        );
                        continue;
                    }
                    Err(error) => {
                        tracing::debug!(
                            conversation_id = %seed.conversation_id,
                            error = %error,
                            "idle cache warm request failed"
                        );
                        return;
                    }
                }
            } else if seed.policy == CacheExpiryPolicy::Warm {
                tracing::debug!(
                    conversation_id = %seed.conversation_id,
                    prompt_tokens = seed.snapshot.prompt_tokens,
                    "idle cache warm skipped (ineligible or below savings floor)"
                );
                return;
            }
            // For `WarmThenCompact`, if warming is ineligible (e.g. budget
            // thinking or below warm savings floor), fall through to try
            // expiry compaction once.
        }

        if matches!(
            seed.policy,
            CacheExpiryPolicy::Compact | CacheExpiryPolicy::WarmThenCompact
        ) {
            let req = WarmCacheCompactRequest {
                conversation_id: seed.conversation_id.clone(),
                expected_head_id: seed.snapshot.message_id.clone(),
                provider: Arc::clone(&seed.provider),
                provider_id: seed.provider_id.clone(),
                provider_name: seed.provider_name.clone(),
                model_id: seed.model_id.clone(),
                params: seed.snapshot.params.clone(),
                warm_messages: seed.snapshot.warm_messages.clone(),
                last_assistant: seed.snapshot.last_assistant.clone(),
                tools: seed.snapshot.tools.clone(),
                prompt_tokens: seed.snapshot.prompt_tokens,
                keep_recent: seed.keep_recent,
                context_limit: seed.context_limit,
                min_tokens: seed.compact_min_tokens,
            };
            match do_compact_with_warm_cache(&services, req).await {
                Ok(Some(anchor_id)) => {
                    tracing::info!(
                        conversation_id = %seed.conversation_id,
                        anchor_id = %anchor_id,
                        "idle cache-expiry forked compaction completed"
                    );
                }
                Ok(None) => {
                    tracing::debug!(
                        conversation_id = %seed.conversation_id,
                        "idle cache-expiry forked compaction skipped"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        conversation_id = %seed.conversation_id,
                        error = %error,
                        "idle cache-expiry forked compaction failed"
                    );
                }
            }
        }
        return;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::db::sea::ops::{conversation as conv_ops, message as msg_ops};
    use crate::provider::{AgentResponse, ChatStream, ProviderError, TokenUsage};
    use crate::util::now_ms;

    struct RecordingProvider {
        calls: Arc<AtomicUsize>,
        saw_compact_instruction: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ChatProvider for RecordingProvider {
        fn adapter_name(&self) -> &'static str {
            "recording"
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
            messages: Vec<ChatMessage>,
            _tools: Vec<ToolDefinition>,
            params: ChatParams,
        ) -> Result<AgentResponse, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let is_compact = params.cache_read_only_tail
                && messages
                    .last()
                    .is_some_and(|m| m.origin == crate::provider::MessageOrigin::CompactInstruction);
            if is_compact {
                self.saw_compact_instruction.fetch_add(1, Ordering::SeqCst);
            }
            Ok(AgentResponse {
                text: if is_compact {
                    "Structured summary of earlier turns.".into()
                } else {
                    "ok".into()
                },
                reasoning_content: None,
                tool_calls: Vec::new(),
                usage: Some(TokenUsage {
                    prompt_tokens: Some(60_000),
                    completion_tokens: Some(100),
                    total_tokens: Some(60_100),
                    cache_read_tokens: Some(59_000),
                    cache_write_tokens: Some(0),
                    cache_write_1h_tokens: None,
                    billable_tool_calls: None,
                }),
                provider_state: None,
            })
        }
    }

    fn priced() -> Prices {
        Prices {
            input_price: Some("3".parse().unwrap()),
            output_price: Some("15".parse().unwrap()),
            cache_read_price: Some("0.3".parse().unwrap()),
            cache_write_price: Some("3.75".parse().unwrap()),
            cache_write_1h_price: Some("6".parse().unwrap()),
            server_tool_price: None,
        }
    }

    async fn seed_conversation(services: &Services, conv_id: &str) -> String {
        let now = now_ms();
        services
            .db
            .write(async |tx| {
                conv_ops::create(tx, conv_id, Some("test"), None, None, now).await?;
                let mut parent: Option<String> = None;
                for i in 0..6 {
                    let role = if i % 2 == 0 { "user" } else { "assistant" };
                    let id = format!("m-{i}");
                    let content = format!("turn {i} {}", "x".repeat(400));
                    let row = msg_ops::new_row(&id, conv_id, role, &content, now + i as i64);
                    msg_ops::append(tx, row, parent.as_deref()).await?;
                    parent = Some(id);
                }
                Ok::<_, crate::db::sea::DbErr>(parent.unwrap())
            })
            .await
            .unwrap()
    }

    fn make_seed(
        conv_id: &str,
        head_id: &str,
        provider: Arc<dyn ChatProvider>,
        policy: CacheExpiryPolicy,
        max_warm_count: u32,
    ) -> IdleCacheSeed {
        IdleCacheSeed {
            conversation_id: conv_id.into(),
            turn_id: "t-1".into(),
            provider,
            provider_id: None,
            provider_name: None,
            model_id: "claude-sonnet-4-6".into(),
            prices: Some(priced()),
            ttl: Duration::from_millis(40),
            policy,
            max_warm_count,
            compact_min_tokens: 40_000,
            keep_recent: 100,
            context_limit: 200_000,
            snapshot: CompletedRoundSnapshot {
                params: ChatParams {
                    model: "claude-sonnet-4-6".into(),
                    max_tokens: Some(8_192),
                    cache_ttl: Some(crate::provider::PromptCacheTtl::OneHour),
                    ..Default::default()
                },
                warm_messages: vec![
                    ChatMessage {
                        role: "system".into(),
                        content: "sys".into(),
                        reasoning_content: None,
                        tool_calls: None,
                        tool_call_id: None,
                        tool_error: false,
                        provider_state: None,
                        origin: crate::provider::MessageOrigin::Assistant,
                        sent_at: None,
                    },
                    ChatMessage::persisted_user("hello", 1_700_000_000_000, None),
                ],
                last_assistant: Some(ChatMessage::assistant("hi there")),
                tools: Vec::new(),
                prompt_tokens: 60_000,
                message_id: head_id.into(),
            },
        }
    }

    #[test]
    fn trigger_delay_scales_cleanly_from_5m_to_1h_to_24h() {
        assert_eq!(trigger_delay(Duration::from_secs(300)), Duration::from_secs(270));
        assert_eq!(trigger_delay(Duration::from_secs(3_600)), Duration::from_secs(3_540));
        assert_eq!(trigger_delay(Duration::from_secs(86_400)), Duration::from_secs(86_340));
    }

    #[tokio::test]
    async fn warm_then_compact_refreshes_n_times_then_runs_forked_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let services = crate::services::bare_services(dir.path()).await;
        let head = seed_conversation(&services, "c-wtc").await;
        let calls = Arc::new(AtomicUsize::new(0));
        let compacts = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(RecordingProvider {
            calls: Arc::clone(&calls),
            saw_compact_instruction: Arc::clone(&compacts),
        });

        services.cache_lifecycle.arm(
            services.clone(),
            make_seed("c-wtc", &head, provider, CacheExpiryPolicy::WarmThenCompact, 2),
        );

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut compacted_persisted = false;
        while tokio::time::Instant::now() < deadline {
            if calls.load(Ordering::SeqCst) >= 3 && compacts.load(Ordering::SeqCst) >= 1 {
                let history = msg_ops::list(&services.db, "c-wtc").await.unwrap();
                if history.iter().any(|m| m.is_compact_summary.get()) {
                    compacted_persisted = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3, "2 warms + 1 forked compact");
        assert_eq!(compacts.load(Ordering::SeqCst), 1);
        assert!(
            compacted_persisted,
            "summary row must have been persisted on the conversation"
        );
    }

    #[tokio::test]
    async fn cancel_stops_pending_idle_action() {
        let dir = tempfile::tempdir().unwrap();
        let services = crate::services::bare_services(dir.path()).await;
        let head = seed_conversation(&services, "c-cancel").await;
        let calls = Arc::new(AtomicUsize::new(0));
        let compacts = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(RecordingProvider {
            calls: Arc::clone(&calls),
            saw_compact_instruction: Arc::clone(&compacts),
        });

        services.cache_lifecycle.arm(
            services.clone(),
            make_seed("c-cancel", &head, provider, CacheExpiryPolicy::Compact, 3),
        );
        assert!(services.cache_lifecycle.is_armed("c-cancel"));
        services.cache_lifecycle.cancel("c-cancel");
        assert!(!services.cache_lifecycle.is_armed("c-cancel"));

        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use tiktoken::CoreBpe;

use crate::provider::ChatMessage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenizerKind {
    Cl100kBase,
    O200kBase,
}

struct Tokenizers {
    cl100k: &'static CoreBpe,
    o200k: &'static CoreBpe,
}

static TOKENIZERS: OnceLock<Tokenizers> = OnceLock::new();

fn tokenizers() -> &'static Tokenizers {
    TOKENIZERS.get_or_init(|| Tokenizers {
        cl100k: tiktoken::get_encoding("cl100k_base").expect("cl100k_base encoding"),
        o200k: tiktoken::get_encoding("o200k_base").expect("o200k_base encoding"),
    })
}

pub fn tokenizer_for_model(provider_type: &str, model: &str) -> TokenizerKind {
    let m = model.to_lowercase();
    let _ = provider_type;
    if m.starts_with("gpt-4.1")
        || m.starts_with("gpt-4.5")
        || m.starts_with("o3")
        || m.starts_with("o4")
        || m.starts_with("o1")
    {
        TokenizerKind::O200kBase
    } else {
        TokenizerKind::Cl100kBase
    }
}

const MESSAGE_OVERHEAD: usize = 4;
const MULTIMODAL_PART_OVERHEAD: usize = 4;
const IMAGE_PART_TOKENS: usize = 4_096;
const FILE_PART_TOKENS: usize = 16_384;

#[derive(Clone)]
pub struct TokenCounter {
    kind: TokenizerKind,
    correction_factor: Arc<AtomicU32>,
}

impl TokenCounter {
    pub fn new(kind: TokenizerKind) -> Self {
        Self {
            kind,
            correction_factor: Arc::new(AtomicU32::new(f32::to_bits(1.0))),
        }
    }

    pub fn for_model(provider_type: &str, model: &str) -> Self {
        Self::new(tokenizer_for_model(provider_type, model))
    }

    fn factor(&self) -> f32 {
        f32::from_bits(self.correction_factor.load(Ordering::Relaxed))
    }

    pub fn count(&self, text: &str) -> usize {
        let tok = tokenizers();
        let enc = match self.kind {
            TokenizerKind::Cl100kBase => &tok.cl100k,
            TokenizerKind::O200kBase => &tok.o200k,
        };
        let raw = enc.count(text);
        let factor = self.factor();
        if (factor - 1.0).abs() < 0.001 {
            raw
        } else {
            (raw as f32 * factor).round() as usize
        }
    }

    /// Count a stored message body without treating inline attachment bytes as
    /// prose. Multimodal bodies are OpenAI-style parts arrays across every
    /// adapter; providers charge images and files by their media rules, not by
    /// tokenising the base64 transport encoding.
    pub fn count_content(&self, content: &str) -> usize {
        if !content.starts_with('[') {
            return self.count(content);
        }
        let Ok(parts) = serde_json::from_str::<Vec<serde_json::Value>>(content) else {
            return self.count(content);
        };
        if parts.is_empty()
            || !parts
                .iter()
                .all(|part| match part.get("type").and_then(|value| value.as_str()) {
                    Some("text") => part.get("text").is_some_and(serde_json::Value::is_string),
                    Some("image_url") => part.pointer("/image_url/url").is_some_and(serde_json::Value::is_string),
                    Some("file") => part.pointer("/file/url").is_some_and(serde_json::Value::is_string),
                    _ => false,
                })
        {
            return self.count(content);
        }

        parts
            .iter()
            .map(|part| {
                MULTIMODAL_PART_OVERHEAD
                    + match part.get("type").and_then(|value| value.as_str()) {
                        Some("text") => self.count(part.get("text").and_then(|value| value.as_str()).unwrap_or("")),
                        Some("image_url") => IMAGE_PART_TOKENS,
                        Some("file") => FILE_PART_TOKENS,
                        _ => unreachable!("part types were validated above"),
                    }
            })
            .sum()
    }

    pub fn count_message(&self, msg: &ChatMessage) -> usize {
        let mut tokens = MESSAGE_OVERHEAD;
        tokens += self.count_content(&msg.content);
        if let Some(ref reasoning) = msg.reasoning_content {
            tokens += self.count(reasoning);
        }
        if let Some(ref tcs) = msg.tool_calls {
            for tc in tcs {
                tokens += self.count(&tc.name) + self.count(&tc.arguments) + 4;
            }
        }
        if let Some(ref _id) = msg.tool_call_id {
            tokens += 2;
        }
        if let Some(ref state) = msg.provider_state
            && let Ok(wire_state) = state.to_storage_json()
        {
            // Opaque continuation state is replayed and billed like every other
            // byte in the prompt. The storage DTO is a close conservative proxy
            // for its provider-specific wire framing.
            tokens += self.count(&wire_state);
        }
        tokens
    }

    pub fn count_messages(&self, messages: &[ChatMessage]) -> usize {
        messages.iter().map(|m| self.count_message(m)).sum::<usize>() + 3
    }

    pub fn calibrate(&self, estimated: usize, actual: usize) {
        if estimated == 0 || actual == 0 {
            return;
        }
        let new_ratio = actual as f32 / estimated as f32;
        let old = self.factor();
        let blended = old * 0.7 + new_ratio * 0.3;
        let clamped = blended.clamp(0.5, 2.0);
        self.correction_factor.store(f32::to_bits(clamped), Ordering::Relaxed);
    }
}

/// The most of the window a single reply is allowed to be reserved.
///
/// A model's advertised maximum is a ceiling, not a forecast: a 256k model that
/// *can* emit 128k tokens will almost never be asked to, and reserving all of it
/// spends half the window on a reply nobody wanted. Bounding it is what lets a
/// large window actually be used.
const OUTPUT_RESERVE_CAP: usize = 32_000;

/// Room for what neither side counted: the tool definitions, the wire framing,
/// and the gap between our tokenizer and the provider's.
const HEADROOM_CAP: usize = 8_000;

/// The latest a turn can start compacting and still have somewhere to put the
/// answer.
///
/// Not a percentage of the window. A flat 90% is what Codex uses and it works
/// there because nothing in that path claims output space; we send `max_tokens`
/// on every request, and most providers count it against the same window — so
/// the same 90% leaves a request the provider has to refuse.
fn safe_threshold(context_limit: usize, max_output: usize) -> usize {
    let reserve = max_output.min(OUTPUT_RESERVE_CAP);
    let headroom = (context_limit / 20).min(HEADROOM_CAP);
    // Never below half the window. A configuration that would put it there is
    // one no amount of compacting can rescue — the reply simply does not fit —
    // and compacting on every single turn would hide that rather than fix it.
    context_limit.saturating_sub(reserve + headroom).max(context_limit / 2)
}

/// The smallest reply worth making a request for.
///
/// Under this the model has nowhere to put an answer: it is refused outright,
/// or it returns a sentence cut in half. The prompt is paid for either way, so
/// the honest move is to say the window is full before sending.
pub const MIN_REPLY_TOKENS: usize = 256;

pub struct TokenBudget {
    pub context_limit: usize,
    pub compact_threshold: usize,
    pub current_estimate: usize,
    pub counter: TokenCounter,
}

impl TokenBudget {
    pub fn new(
        provider_type: &str,
        model: &str,
        context_limit: usize,
        max_output: usize,
        compact_threshold_override: Option<usize>,
    ) -> Self {
        let safe = safe_threshold(context_limit, max_output);
        // A stored threshold may only ever bring compaction *forward*. It
        // arrives from the model's configuration while `context_limit` may come
        // from the assistant's, so the two are not guaranteed to be about the
        // same number — and a stored value that outran the window is exactly the
        // shape that stopped compaction firing at all.
        let compact_threshold = compact_threshold_override.map_or(safe, |t| t.min(safe));
        Self {
            context_limit,
            compact_threshold,
            current_estimate: 0,
            counter: TokenCounter::for_model(provider_type, model),
        }
    }

    /// What is left of the window with this prompt in it.
    ///
    /// Most providers count the prompt and `max_tokens` against one budget, so
    /// this is the most an answer may be allowed to reach — and only ever a
    /// ceiling, never a floor under one. Zero means the prompt already fills the
    /// window: no ceiling makes that request servable, and picking a small one
    /// anyway just moves the refusal.
    pub fn room_for_reply(&self) -> usize {
        self.context_limit.saturating_sub(self.current_estimate)
    }

    /// What to ask for as this request's output ceiling.
    ///
    /// The configured maximum is what the model *can* write, not what it will,
    /// and asking for all of it on top of a long prompt is a request most
    /// providers have to refuse. So it is trimmed to what is actually left.
    ///
    /// `None` where nothing is configured, and it stays `None`: leaving the
    /// field off is what lets the provider fit the answer to the room it has,
    /// and a number invented here would be a cap the user never asked for.
    pub fn reply_ceiling(&self, configured: Option<usize>) -> Option<usize> {
        configured.map(|c| c.min(self.room_for_reply()))
    }

    pub fn update_estimate(&mut self, messages: &[ChatMessage]) {
        self.current_estimate = self.counter.count_messages(messages);
    }

    pub fn calibrate_from_usage(&mut self, usage: &crate::provider::TokenUsage) {
        if let Some(prompt) = usage.prompt_tokens
            && prompt > 0
            && self.current_estimate > 0
        {
            self.counter.calibrate(self.current_estimate, prompt as usize);
        }
    }

    pub fn needs_compact(&self) -> bool {
        self.current_estimate > self.compact_threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokenizer_for_model_o200k() {
        assert_eq!(tokenizer_for_model("openai", "gpt-4.1-mini"), TokenizerKind::O200kBase);
        assert_eq!(tokenizer_for_model("openai", "o3-mini"), TokenizerKind::O200kBase);
        assert_eq!(tokenizer_for_model("openai", "o4-mini"), TokenizerKind::O200kBase);
    }

    #[test]
    fn test_tokenizer_for_model_cl100k() {
        assert_eq!(tokenizer_for_model("openai", "gpt-4o"), TokenizerKind::Cl100kBase);
        assert_eq!(
            tokenizer_for_model("anthropic", "claude-sonnet-4-20250514"),
            TokenizerKind::Cl100kBase
        );
        assert_eq!(
            tokenizer_for_model("deepseek", "deepseek-chat"),
            TokenizerKind::Cl100kBase
        );
    }

    #[test]
    fn test_count_known_string() {
        let counter = TokenCounter::new(TokenizerKind::Cl100kBase);
        let tokens = counter.count("Hello, world!");
        assert!(tokens > 0 && tokens < 10);
    }

    #[test]
    fn test_count_messages_overhead() {
        let counter = TokenCounter::new(TokenizerKind::Cl100kBase);
        let msgs = vec![ChatMessage::user("hi")];
        let total = counter.count_messages(&msgs);
        let content_only = counter.count("hi");
        assert!(total > content_only);
    }

    fn image_content(payload: &str, caption: &str, image_count: usize) -> String {
        let mut parts = vec![serde_json::json!({ "type": "text", "text": caption })];
        parts.extend((0..image_count).map(|_| {
            serde_json::json!({
                "type": "image_url",
                "image_url": { "url": format!("data:image/jpeg;base64,{payload}") }
            })
        }));
        serde_json::Value::Array(parts).to_string()
    }

    #[test]
    fn inline_image_budget_does_not_scale_with_base64_length() {
        let counter = TokenCounter::new(TokenizerKind::Cl100kBase);
        let small = image_content("QUJD", "look", 1);
        let large = image_content(&"A".repeat(1_000_000), "look", 1);

        assert_eq!(counter.count_content(&small), counter.count_content(&large));
        assert!(counter.count_content(&large) < 10_000);
        assert!(
            counter.count(&large) > 100_000,
            "the regression sample must be large as ordinary text"
        );
    }

    #[test]
    fn multimodal_budget_counts_captions_and_each_image() {
        let counter = TokenCounter::new(TokenizerKind::Cl100kBase);
        let one = counter.count_content(&image_content("QUJD", "short", 1));
        let two = counter.count_content(&image_content("QUJD", "short", 2));
        let long_caption = counter.count_content(&image_content("QUJD", &"caption ".repeat(200), 1));

        assert_eq!(two - one, IMAGE_PART_TOKENS + MULTIMODAL_PART_OVERHEAD);
        assert!(long_caption > one);
    }

    #[test]
    fn non_multimodal_json_and_malformed_arrays_stay_plain_text() {
        let counter = TokenCounter::new(TokenizerKind::Cl100kBase);
        for content in [
            r#"[{"type":"custom","payload":"abc"}]"#,
            r#"[{"type":"image_url","payload":"not an image part"}]"#,
            "[not valid json",
        ] {
            assert_eq!(counter.count_content(content), counter.count(content));
        }
    }

    #[test]
    fn test_calibrate_adjusts_factor() {
        let counter = TokenCounter::new(TokenizerKind::Cl100kBase);
        counter.calibrate(100, 150);
        let f = counter.factor();
        assert!(f > 1.0, "factor should increase: {f}");
    }

    #[test]
    fn test_calibrate_clamps() {
        let counter = TokenCounter::new(TokenizerKind::Cl100kBase);
        counter.calibrate(100, 1000);
        let f = counter.factor();
        assert!(f <= 2.0, "factor should be clamped: {f}");
    }

    #[test]
    fn test_budget_thresholds() {
        // Reserve is the model's own maximum here, being under the cap.
        let budget = TokenBudget::new("openai", "gpt-4o", 128_000, 16_384, None);
        assert_eq!(budget.compact_threshold, 128_000 - 16_384 - 6_400);
    }

    /// The case that made a large window unusable: reserving all of a model's
    /// advertised output spent half the context on a reply nobody asked for, so
    /// a 256k window started compacting at 128k.
    #[test]
    fn a_huge_advertised_output_does_not_eat_the_window() {
        let budget = TokenBudget::new("openai", "gpt-4o", 256_000, 128_000, None);
        assert_eq!(budget.compact_threshold, 256_000 - 32_000 - 8_000);
        assert!(
            budget.compact_threshold > 256_000 / 2,
            "a big window has to stay usable: {}",
            budget.compact_threshold,
        );
    }

    /// A reply still has to fit at the moment compaction starts, which is the
    /// property the old formula lost and the stored override never had.
    ///
    /// Stated at the threshold rather than at rest: the ceiling is whatever is
    /// left, so it only says anything once the prompt has grown to the point the
    /// question is being asked.
    #[test]
    fn a_reply_still_fits_at_the_moment_compaction_fires() {
        for (limit, max_out) in [
            (128_000, 16_384),
            (256_000, 128_000),
            (200_000, 64_000),
            (32_000, 8_000),
            (1_000_000, 128_000),
            (8_000, 8_000),
        ] {
            let mut b = TokenBudget::new("openai", "gpt-4o", limit, max_out, None);
            b.current_estimate = b.compact_threshold;
            let reply = b.reply_ceiling(Some(max_out)).unwrap();
            assert!(
                b.compact_threshold + reply <= limit,
                "{limit}/{max_out}: threshold {} + reply {reply} overruns the window",
                b.compact_threshold,
            );
        }
    }

    /// The shape that stopped compaction firing at all: a stored threshold, from
    /// the model's configuration, larger than the window the assistant's
    /// configuration actually imposes.
    #[test]
    fn a_stored_threshold_can_only_bring_compaction_forward() {
        let safe = TokenBudget::new("openai", "gpt-4o", 256_000, 128_000, None).compact_threshold;

        let over = TokenBudget::new("openai", "gpt-4o", 256_000, 128_000, Some(244_800));
        assert_eq!(over.compact_threshold, safe, "an override cannot postpone it");

        let under = TokenBudget::new("openai", "gpt-4o", 256_000, 128_000, Some(100_000));
        assert_eq!(under.compact_threshold, 100_000, "but it can bring it forward");
    }

    /// A tiny window cannot be rescued by compacting sooner, so it is not asked
    /// to compact on every turn either.
    #[test]
    fn a_window_too_small_for_its_own_output_still_gets_a_usable_threshold() {
        let budget = TokenBudget::new("openai", "gpt-4o", 8_000, 8_000, None);
        assert_eq!(budget.compact_threshold, 4_000);
    }

    #[test]
    fn the_reply_ceiling_is_what_is_left_rather_than_what_was_asked_for() {
        let mut budget = TokenBudget::new("openai", "gpt-4o", 256_000, 128_000, None);

        budget.current_estimate = 10_000;
        assert_eq!(
            budget.reply_ceiling(Some(128_000)),
            Some(128_000),
            "plenty of room, ask for it all"
        );

        budget.current_estimate = 200_000;
        assert_eq!(
            budget.reply_ceiling(Some(128_000)),
            Some(56_000),
            "trimmed to the room left"
        );
    }

    /// The ceiling is never a floor. A small remainder is a reason to stop, not
    /// a reason to round up -- rounding up is how the request that could not be
    /// served got built in the first place, one order of magnitude smaller.
    #[test]
    fn a_ceiling_never_exceeds_what_is_left() {
        let mut budget = TokenBudget::new("openai", "gpt-4o", 256_000, 128_000, None);

        for estimate in [255_500, 255_999, 256_000, 300_000] {
            budget.current_estimate = estimate;
            let asked = budget.reply_ceiling(Some(128_000)).unwrap();
            assert!(
                estimate + asked <= 256_000.max(estimate),
                "{estimate}: asked for {asked} with {} left",
                budget.room_for_reply(),
            );
            assert!(
                asked <= budget.room_for_reply(),
                "{estimate}: asked for more than is left"
            );
        }
    }

    /// Nothing configured means nothing sent, rather than something invented
    /// here and applied to every reply in the app.
    ///
    /// Not reachable through `resolve_turn_params` today: it backfills
    /// `max_tokens` with the model's advertised output, so a turn always
    /// carries one. This is about which of the two decides -- whether the
    /// absence of a setting can be manufactured into a cap by a floor down
    /// here. Whether the backfill itself should exist is a separate question
    /// and not this function's to answer: Anthropic requires the field and
    /// falls back to 4096 without it.
    #[test]
    fn an_unset_ceiling_stays_unset() {
        let mut budget = TokenBudget::new("openai", "gpt-4o", 256_000, 128_000, None);
        budget.current_estimate = 10_000;
        assert_eq!(budget.reply_ceiling(None), None);
        budget.current_estimate = 255_900;
        assert_eq!(budget.reply_ceiling(None), None);
    }

    /// What the caller checks before building a request at all.
    #[test]
    fn a_full_window_reports_no_room() {
        let mut budget = TokenBudget::new("openai", "gpt-4o", 256_000, 128_000, None);
        budget.current_estimate = 255_900;
        assert!(budget.room_for_reply() < MIN_REPLY_TOKENS);
        budget.current_estimate = 300_000;
        assert_eq!(budget.room_for_reply(), 0);
        budget.current_estimate = 200_000;
        assert!(budget.room_for_reply() >= MIN_REPLY_TOKENS);
    }

    #[test]
    fn test_budget_needs_compact() {
        let mut budget = TokenBudget::new("openai", "gpt-4o", 1000, 200, None);
        budget.current_estimate = 900;
        assert!(budget.needs_compact());
        budget.current_estimate = 100;
        assert!(!budget.needs_compact());
    }
}

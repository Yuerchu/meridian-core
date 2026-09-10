//! Tool result pruning — trim old, large tool outputs before sending.
//!
//! Inspired by grok-build's `PruningConfig`. When the conversation grows long,
//! old tool results that nobody is looking at any more dominate the token count.
//! Compaction rewrites the whole history; this is cheaper and earlier — it trims
//! the content in place and lets compaction run less often.
//!
//! Three rules, applied from gentlest to harshest:
//!
//! 1. **Protected zone**: the most recent `keep_last_n` assistant rounds and
//!    their tool results are never touched.
//! 2. **Soft trim**: a tool result older than the protected zone and longer than
//!    `soft_threshold` characters keeps its first `head` and last `tail`
//!    characters, with a `[…trimmed…]` marker in between.
//! 3. **Hard clear**: a tool result older than `hard_age` assistant rounds is
//!    replaced entirely with a short marker.

use crate::provider::ChatMessage;

pub(crate) struct PruningConfig {
    pub keep_last_n: usize,
    pub soft_threshold: usize,
    pub soft_head: usize,
    pub soft_tail: usize,
    pub hard_age: usize,
}

impl Default for PruningConfig {
    fn default() -> Self {
        Self {
            keep_last_n: 3,
            soft_threshold: 4000,
            soft_head: 1500,
            soft_tail: 1500,
            hard_age: 10,
        }
    }
}

const SOFT_MARKER: &str = "\n[…trimmed — tool output abbreviated…]\n";
const HARD_MARKER: &str = "[Tool result omitted — too old]";

/// Count assistant rounds from the end and prune old tool results in place.
///
/// A "round" is one assistant message (which may have tool calls after it).
/// Tool results belong to the round of the assistant message that issued them.
pub(crate) fn prune_tool_results(messages: &mut [ChatMessage], config: &PruningConfig) {
    // Count total assistant rounds, then assign each message an age
    // measured from the most recent round (age 0 = newest).
    let total_rounds = messages.iter().filter(|m| m.role == "assistant").count();
    if total_rounds == 0 {
        return;
    }

    // Forward pass: assign each message the 0-based round index of its
    // most recent preceding assistant. A tool result belongs to the
    // assistant that issued it (the one directly before it).
    let mut ages: Vec<usize> = vec![0; messages.len()];
    let mut current_round = 0;
    let mut in_round = false;
    for i in 0..messages.len() {
        if messages[i].role == "assistant" {
            if in_round {
                current_round += 1;
            }
            in_round = true;
        }
        // Convert forward index to age: age = (total_rounds - 1) - current_round
        ages[i] = if in_round {
            (total_rounds - 1).saturating_sub(current_round)
        } else {
            total_rounds // older than any round
        };
    }

    for (i, msg) in messages.iter_mut().enumerate() {
        if msg.role != "tool" {
            continue;
        }

        let age = ages[i];

        if age < config.keep_last_n {
            continue;
        }

        if age >= config.hard_age {
            msg.content = HARD_MARKER.to_string();
            continue;
        }

        if msg.content.len() >= config.soft_threshold {
            let chars: Vec<char> = msg.content.chars().collect();
            if chars.len() >= config.soft_threshold {
                let head: String = chars[..config.soft_head.min(chars.len())].iter().collect();
                let tail_start = chars.len().saturating_sub(config.soft_tail);
                let tail: String = chars[tail_start..].iter().collect();
                msg.content = format!("{head}{SOFT_MARKER}{tail}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(text: &str) -> ChatMessage {
        ChatMessage {
            role: "assistant".into(),
            content: text.into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            tool_error: false,
            provider_state: None,
            origin: Default::default(),
        }
    }

    fn tool(id: &str, content: &str) -> ChatMessage {
        ChatMessage::tool_result(id, content)
    }

    fn user(text: &str) -> ChatMessage {
        ChatMessage::user(text)
    }

    fn big_content(n: usize) -> String {
        "x".repeat(n)
    }

    #[test]
    fn recent_tool_results_are_untouched() {
        let big = big_content(5000);
        let mut msgs = vec![
            user("hi"),
            assistant("let me check"),
            tool("c1", &big),
            assistant("here you go"),
        ];
        let config = PruningConfig {
            keep_last_n: 3,
            ..Default::default()
        };
        prune_tool_results(&mut msgs, &config);
        assert_eq!(msgs[2].content.len(), 5000);
    }

    #[test]
    fn old_big_result_is_soft_trimmed() {
        let big = big_content(5000);
        let mut msgs = vec![
            user("hi"),
            assistant("round 0"),
            tool("c1", &big),
            user("ok"),
            assistant("round 1"),
            user("ok"),
            assistant("round 2"),
            user("ok"),
            assistant("round 3 (most recent)"),
        ];
        let config = PruningConfig {
            keep_last_n: 3,
            soft_threshold: 4000,
            soft_head: 100,
            soft_tail: 100,
            hard_age: 10,
        };
        prune_tool_results(&mut msgs, &config);
        assert!(msgs[2].content.contains("trimmed"));
        assert!(msgs[2].content.len() < 5000);
    }

    #[test]
    fn old_small_result_not_trimmed() {
        let mut msgs = vec![
            user("hi"),
            assistant("round 0"),
            tool("c1", "short result"),
            user("ok"),
            assistant("round 1"),
            user("ok"),
            assistant("round 2"),
            user("ok"),
            assistant("round 3"),
        ];
        let config = PruningConfig::default();
        prune_tool_results(&mut msgs, &config);
        assert_eq!(msgs[2].content, "short result");
    }

    #[test]
    fn very_old_result_is_hard_cleared() {
        let mut msgs = vec![user("hi"), assistant("old"), tool("c1", "some result")];
        // Add 10 more rounds to push the first tool result past hard_age.
        for i in 0..10 {
            msgs.push(user("ok"));
            msgs.push(assistant(&format!("round {}", i + 1)));
        }
        let config = PruningConfig {
            keep_last_n: 3,
            hard_age: 10,
            ..Default::default()
        };
        prune_tool_results(&mut msgs, &config);
        assert_eq!(msgs[2].content, HARD_MARKER);
    }

    #[test]
    fn empty_messages_no_panic() {
        let mut msgs: Vec<ChatMessage> = vec![];
        prune_tool_results(&mut msgs, &PruningConfig::default());
    }

    #[test]
    fn non_tool_messages_untouched() {
        let big = big_content(5000);
        let mut msgs = vec![
            user(&big),
            assistant("old round"),
            user("ok"),
            assistant("round 1"),
            user("ok"),
            assistant("round 2"),
            user("ok"),
            assistant("round 3"),
        ];
        let config = PruningConfig::default();
        prune_tool_results(&mut msgs, &config);
        assert_eq!(msgs[0].content.len(), 5000);
    }
}

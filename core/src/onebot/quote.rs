//! What a message quoted, and what is inside a merged-forward bubble.
//!
//! Both are the same shape of problem: content that is *referred to* rather than
//! carried, and that costs an API call to see. A quote used to be flattened to a
//! line of text here, which is how a quoted sticker reached the model as the
//! five literal characters `[动画表情]` — the sticker was in the reply, the
//! reply was fetched, and everything but its prose was thrown away on arrival.
//!
//! On a phone that is not an edge case: QQ gives no way to @ the bot and attach
//! a sticker in one message, so quoting the sticker *is* how a group member
//! shows the bot one. What comes back now is a `ParsedMessage` like any other,
//! and the caller merges its media into the turn.

use std::sync::Arc;

use super::format::{self, FORWARD_SENTINEL, ForwardRef, ParsedMessage};
use super::protocol::OneBotAction;
use super::{SharedState, call_api};

/// How many levels of forward-inside-a-forward are followed. Two covers the
/// real case — someone forwards a conversation that itself contains a forwarded
/// screenshot thread — and stops there because each level is an API call per
/// bubble and the reader's attention runs out long before the recursion does.
const MAX_FORWARD_DEPTH: usize = 2;

/// A forwarded conversation can be hundreds of messages. This is what gets
/// read; the rest is reported as a count so the model knows it is seeing a
/// prefix rather than the whole thing.
const MAX_FORWARD_MESSAGES: usize = 20;

/// Per-line cap inside a forward. A single forwarded message can be an essay,
/// and twenty of those is a context window.
const FORWARD_LINE_CHARS: usize = 200;

/// A message that another message replied to.
pub struct Quoted {
    pub sender: String,
    /// Who wrote it, when the adapter says. Needed to tell the bot's own
    /// messages apart from everyone else's — quoting the bot is common, and its
    /// own stickers must not be captured back into the pool as if a group
    /// member had used them.
    pub sender_id: Option<i64>,
    /// The quoted message's own id, which is what its voice has to be
    /// transcribed against. The turn's `event.message_id` is the *reply*, and
    /// asking the adapter to transcribe that yields nothing.
    pub message_id: i64,
    pub parsed: ParsedMessage,
}

/// Fetch the message `message_id` and parse it whole — media included.
///
/// `None` when the adapter cannot produce it (an old message it no longer
/// caches, most often) or when what came back has neither text nor media.
pub async fn fetch(state: &Arc<SharedState>, message_id: i64) -> Option<Quoted> {
    let echo = uuid::Uuid::new_v4().to_string();
    let data = call_api(state, OneBotAction::get_msg(message_id, echo)).await.ok()?;

    let sender_obj = data.get("sender");
    let sender = sender_obj
        .and_then(|s| {
            s.get("card")
                .and_then(|v| v.as_str())
                .filter(|c| !c.is_empty())
                .or_else(|| s.get("nickname").and_then(|v| v.as_str()))
        })
        .unwrap_or("Unknown")
        .to_string();
    let sender_id = sender_obj.and_then(|s| s.get("user_id")).and_then(|v| v.as_i64());

    // `None` for self_id on purpose: an @mention inside the quoted message is
    // part of what that person wrote, not this turn's way of addressing the bot.
    let mut parsed = format::parse_segments(data.get("message")?, None);
    expand_forwards(state, &mut parsed).await;

    if parsed.text.is_empty() && !parsed.has_media() {
        return None;
    }
    Some(Quoted {
        sender,
        sender_id,
        message_id,
        parsed,
    })
}

/// Replace every `FORWARD_SENTINEL` in `parsed.text` with the conversation it
/// stands for. Does nothing when there are none, which is the common case.
pub async fn expand_forwards(state: &Arc<SharedState>, parsed: &mut ParsedMessage) {
    if parsed.forwards.is_empty() {
        return;
    }
    let mut rendered = Vec::with_capacity(parsed.forwards.len());
    for forward in &parsed.forwards {
        rendered.push(render_forward(state, forward, 1).await);
    }
    parsed.text = substitute(&parsed.text, FORWARD_SENTINEL, &rendered);
}

/// Replace the i-th occurrence of `sentinel` with `values[i]`, matching by
/// position rather than by first occurrence. Anything past the end of `values`
/// falls back to the readable placeholder.
fn substitute(text: &str, sentinel: char, values: &[String]) -> String {
    let mut parts = text.split(sentinel);
    let mut out = String::with_capacity(text.len());
    out.push_str(parts.next().unwrap_or(""));
    for (i, part) in parts.enumerate() {
        match values.get(i) {
            Some(value) => out.push_str(value),
            None => out.push_str("[聊天记录]"),
        }
        out.push_str(part);
    }
    out
}

async fn render_forward(state: &Arc<SharedState>, forward: &ForwardRef, depth: usize) -> String {
    let messages = match forward.inline.clone() {
        Some(inline) => Some(inline),
        None => match forward.id.as_deref() {
            Some(id) => {
                let echo = uuid::Uuid::new_v4().to_string();
                match call_api(state, OneBotAction::get_forward_msg(id, echo)).await {
                    Ok(data) => data
                        .get("messages")
                        .or_else(|| data.get("message"))
                        .cloned()
                        .or(Some(data)),
                    Err(e) => {
                        tracing::debug!(error = %e, "get_forward_msg failed");
                        None
                    }
                }
            }
            None => None,
        },
    };

    let Some(messages) = messages.as_ref().and_then(|m| m.as_array()) else {
        return "[聊天记录]".to_string();
    };
    if messages.is_empty() {
        return "[聊天记录: 空]".to_string();
    }

    let mut lines = Vec::new();
    for node in messages.iter().take(MAX_FORWARD_MESSAGES) {
        // OB11 nests the real message under `data` for `node` segments; adapters
        // returning a plain list put it at the top level.
        let body = node.get("data").filter(|d| d.get("content").is_some()).unwrap_or(node);
        let name = body
            .get("sender")
            .and_then(|s| {
                s.get("card")
                    .and_then(|v| v.as_str())
                    .filter(|c| !c.is_empty())
                    .or_else(|| s.get("nickname").and_then(|v| v.as_str()))
            })
            .or_else(|| body.get("nickname").and_then(|v| v.as_str()))
            .unwrap_or("某人");
        let Some(content) = body.get("message").or_else(|| body.get("content")) else {
            continue;
        };

        let mut inner = format::parse_segments(content, None);
        // A forward inside a forward: follow it while there is budget, and
        // otherwise leave the placeholder — `substitute` is not called, so the
        // sentinel falls through to `restore_sentinels` below as [聊天记录].
        if !inner.forwards.is_empty() && depth < MAX_FORWARD_DEPTH {
            let mut nested = Vec::with_capacity(inner.forwards.len());
            for forward in &inner.forwards {
                nested.push(Box::pin(render_forward(state, forward, depth + 1)).await);
            }
            inner.text = substitute(&inner.text, FORWARD_SENTINEL, &nested);
        }

        // Media inside a forward stays a placeholder. Downloading it would mean
        // an unbounded number of images from one bubble, and the sentinels here
        // have no turn to be aligned against — unlike the quoted message itself,
        // whose media the caller merges in.
        let line = format::restore_sentinels(&inner.text);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        lines.push(format!("  {name}: {}", truncate(line, FORWARD_LINE_CHARS)));
    }

    if lines.is_empty() {
        return "[聊天记录]".to_string();
    }
    let omitted = messages.len().saturating_sub(MAX_FORWARD_MESSAGES);
    if omitted > 0 {
        lines.push(format!("  (还有 {omitted} 条未显示)"));
    }
    format!("[聊天记录:\n{}\n]", lines.join("\n"))
}

fn truncate(value: &str, limit: usize) -> String {
    let mut out: String = value.chars().take(limit).collect();
    if out.chars().count() < value.chars().count() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitution_matches_by_position_not_first_occurrence() {
        let text = format!("看{FORWARD_SENTINEL}还有{FORWARD_SENTINEL}");
        let out = substitute(&text, FORWARD_SENTINEL, &["[A]".into(), "[B]".into()]);
        assert_eq!(out, "看[A]还有[B]");
    }

    #[test]
    fn a_sentinel_with_no_value_stays_readable() {
        let text = format!("{FORWARD_SENTINEL}{FORWARD_SENTINEL}");
        let out = substitute(&text, FORWARD_SENTINEL, &["[A]".into()]);
        assert_eq!(out, "[A][聊天记录]");
        assert_eq!(substitute("无", FORWARD_SENTINEL, &[]), "无");
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        assert_eq!(truncate("你好世界", 2), "你好…");
        assert_eq!(truncate("你好", 5), "你好");
    }
}

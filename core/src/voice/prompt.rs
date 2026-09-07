//! System-prompt context block for conversations that contain voice input.
//!
//! ASR-side hotword biasing is not available on this model (its sentencepiece
//! tokens are incompatible with sherpa's hotword encoding), so proper-noun
//! correction happens here instead: the chat model is told which messages were
//! dictated and asked to resolve homophones against project memory.

use crate::db::models::message::MessageRow;

/// Returns the voice-input guidance block, or `None` for conversations that
/// have never seen voice input — typing users pay zero prompt tokens.
///
/// Both the chat loop and the token estimator call this with the same active
/// path so their system prompts stay identical. The chat loop additionally
/// passes `current_turn_is_voice` because the incoming message is not on the
/// path yet at resolve time; the estimator catches up one call later, a
/// one-block transient the estimate can tolerate.
pub fn voice_context_block(path: &[MessageRow], current_turn_is_voice: bool) -> Option<String> {
    let has_voice = current_turn_is_voice || path.iter().any(|m| m.source.as_deref() == Some("voice"));
    if !has_voice {
        return None;
    }
    // Deliberately short, and deliberately free of worked examples. An earlier
    // version told the model to read transcripts "by sound rather than
    // literally" and illustrated it with a spelled-out name; the model then
    // spent its whole reasoning budget trying to reverse-engineer what the user
    // had actually said, and matched their words against the example instead of
    // reading them. Recovering the original wording from a bad transcript is not
    // something it can do, so the instruction now gives it an exit instead.
    Some(
        "<voice_input>\n\
         Messages prefixed [voice] were dictated through speech-to-text and may \
         contain homophone substitutions or dropped words, most often in names.\n\
         Read them for intent and move on. If project memory holds a name that \
         matches one by sound, use the remembered spelling. If a message is too \
         garbled to act on, say plainly which part you did not catch and ask — do \
         not reason about what the user might have said, and do not treat the \
         transcript as a puzzle to solve. A newly explained name is worth saving \
         with save_memory.\n\
         </voice_input>"
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(source: Option<&str>) -> MessageRow {
        MessageRow {
            id: "m".into(),
            conversation_id: "c".into(),
            role: "user".into(),
            content: String::new(),
            provider_id: None,
            model_id: None,
            input_tokens: None,
            output_tokens: None,
            tool_calls: None,
            tool_call_id: None,
            sort_order: 0,
            created_at: 0,
            reasoning_content: None,
            rating: None,
            schema_version: 2,
            is_compact_summary: 0,
            sender_id: None,
            parent_id: None,
            compact_anchor_id: None,
            source: source.map(String::from),
            turn_id: None,
            tool_outcome: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            server_tool_calls: None,
            provider_name: None,
            provider_state: None,
            auto_review: None,
        }
    }

    #[test]
    fn absent_without_voice_messages() {
        assert!(voice_context_block(&[msg(None)], false).is_none());
        assert!(voice_context_block(&[], false).is_none());
    }

    #[test]
    fn present_for_voice_history_or_current_turn() {
        assert!(voice_context_block(&[msg(Some("voice"))], false).is_some());
        assert!(voice_context_block(&[], true).is_some());
    }
}

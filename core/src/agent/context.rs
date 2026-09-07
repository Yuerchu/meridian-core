use std::collections::HashMap;

use crate::db::models::message::MessageRow;
use crate::db::models::message_context_item::MessageContextItemRow;
use crate::db::ops::message::ActiveContext;
use crate::provider::{self, ChatMessage, SenderRef};

use super::tokenizer::{TokenBudget, TokenCounter, TokenizerKind};
use super::tool_calls::parse_stored_tool_calls;

/// Last known nickname per platform user id. Nicknames are not stored on the
/// message row (they change), so multi-speaker surfaces pass a lookup built
/// from the subject table.
pub(crate) type SenderNames = HashMap<i64, String>;

/// Single-speaker shorthand. Production callers all attribute senders now, so
/// only tests still take this path.
#[cfg(any(test, feature = "test-support"))]
pub fn build_messages(
    system_prompt: &str,
    context: &ActiveContext,
    user_message: &str,
) -> Result<Vec<ChatMessage>, String> {
    build_messages_with_senders(
        system_prompt,
        context,
        vec![ChatMessage::user(user_message)],
        &SenderNames::new(),
    )
}

/// `trailing` carries this turn's new messages, each already attributed by the
/// caller. History rows are attributed from their stored `sender_id`; rows
/// written before that column existed stay `LegacyUser` — someone said them, but
/// who is not recoverable, and reading it back out of the text prefix would let
/// a user forge it.
///
/// Takes the whole `ActiveContext` rather than a message list plus a cursor:
/// the two have to describe the same path, and passing them separately meant
/// every caller had to remember to pair them.
pub fn build_messages_with_senders(
    system_prompt: &str,
    context: &ActiveContext,
    trailing: Vec<ChatMessage>,
    sender_names: &SenderNames,
) -> Result<Vec<ChatMessage>, String> {
    build_messages_with_context_items(system_prompt, context, trailing, sender_names, &HashMap::new())
}

/// Build provider history while replaying the frozen context items attached to
/// each user row. Keeping the map separate from `MessageRow` means raw snapshots
/// never cross the transcript DTO or audit boundary.
pub fn build_messages_with_context_items(
    system_prompt: &str,
    context: &ActiveContext,
    trailing: Vec<ChatMessage>,
    sender_names: &SenderNames,
    context_items: &HashMap<String, Vec<MessageContextItemRow>>,
) -> Result<Vec<ChatMessage>, String> {
    let mut msgs = Vec::new();
    if !system_prompt.is_empty() {
        msgs.push(ChatMessage {
            role: "system".into(),
            content: system_prompt.into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            tool_error: false,
            provider_state: None,
            origin: provider::MessageOrigin::Assistant,
        });
    }
    if let Some(summary) = context.summary.as_ref() {
        // Deliberately not SystemContext: a summary stands in for the
        // history it replaced and must stay compactable and stay put.
        // SystemContext is reserved for background we regenerate each turn,
        // which `take_injected_context` lifts out and re-appends at the tail.
        msgs.push(ChatMessage::user(&summary.content));
    }
    for m in context.live() {
        push_history_message(&mut msgs, m, sender_names, context_items.get(&m.id).map(Vec::as_slice))?;
    }
    msgs.extend(trailing);
    // Unconditional, so every caller gets a payload the provider will accept.
    // History can hold a tool row whose assistant row was deleted (or never
    // written), and a `tool` message with no matching `tool_calls` is rejected
    // outright. The two turn drivers used to each call this themselves while the
    // three token-estimation callers did not, so the estimate counted rows that
    // never went out.
    remove_orphan_tool_messages(&mut msgs);
    attach_sender_note(&mut msgs);
    Ok(msgs)
}

/// Explain the `<sender>` marker once, in the system prompt, whenever anyone in
/// this payload is attributed.
///
/// Lives here rather than in each adapter because the marker is no longer a
/// fallback for formats lacking a `name` field — it is how every format carries
/// a speaker — so the explanation is not a per-adapter concern either.
fn attach_sender_note(msgs: &mut Vec<ChatMessage>) {
    if !provider::needs_sender_note(msgs) {
        return;
    }
    if let Some(system) = msgs.first_mut().filter(|m| m.role == "system") {
        system.content.push_str("\n\n");
        system.content.push_str(provider::SENDER_PREFIX_NOTE);
        return;
    }
    msgs.insert(
        0,
        ChatMessage {
            role: "system".into(),
            content: provider::SENDER_PREFIX_NOTE.into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            tool_error: false,
            provider_state: None,
            origin: provider::MessageOrigin::Assistant,
        },
    );
}

fn sender_ref(user_id: i64, names: &SenderNames) -> SenderRef {
    SenderRef {
        user_id,
        nickname: names.get(&user_id).cloned(),
    }
}

fn push_history_message(
    msgs: &mut Vec<ChatMessage>,
    m: &MessageRow,
    names: &SenderNames,
    context_items: Option<&[MessageContextItemRow]>,
) -> Result<(), String> {
    use crate::db::models::message::MessageRole;

    let role = MessageRole::parse(&m.role).map_err(|error| format!("message {}: {error}", m.id))?;
    match role {
        MessageRole::User => {
            // Dictated messages carry a marker the voice_input prompt block
            // explains. Applied to the payload only — the stored row and the
            // UI keep the clean transcript.
            let content = if m.source.as_deref() == Some("voice") {
                std::borrow::Cow::Owned(format!("[voice] {}", m.content))
            } else {
                std::borrow::Cow::Borrowed(m.content.as_str())
            };
            match m.sender_id {
                Some(uid) => msgs.push(ChatMessage::user_from(&content, sender_ref(uid, names))),
                None => msgs.push(ChatMessage::user(&content)),
            }
        }
        MessageRole::Assistant => {
            let provider_state = m
                .provider_state
                .as_deref()
                .map(provider::state::ProviderState::from_storage_json)
                .transpose()
                .map_err(|error| format!("message {} has invalid persisted provider_state: {error}", m.id))?;
            let tool_calls = parse_stored_tool_calls(m.schema_version, m.tool_calls.as_deref())
                .map_err(|error| format!("message {} has invalid persisted tool_calls: {error}", m.id))?;
            // Providers require arguments to be a valid JSON object. A stream
            // cut mid-argument persists a fragment; sending that verbatim makes
            // the whole request fail. Normalise here — the only place the value
            // leaves this process — rather than in the storage reader.
            let tool_calls: Vec<_> = tool_calls
                .into_iter()
                .map(|tc| {
                    let valid = serde_json::from_str::<serde_json::Value>(&tc.arguments)
                        .ok()
                        .filter(|v| v.is_object())
                        .is_some();
                    if valid {
                        tc
                    } else {
                        crate::provider::ToolCall {
                            arguments: "{}".to_string(),
                            ..tc
                        }
                    }
                })
                .collect();
            let reasoning = m.reasoning_content.clone();
            if !tool_calls.is_empty() {
                let mut message = ChatMessage::assistant_with_tools(&m.content, reasoning, tool_calls);
                message.provider_state = provider_state;
                msgs.push(message);
            } else {
                msgs.push(ChatMessage {
                    role: "assistant".into(),
                    content: m.content.clone(),
                    reasoning_content: reasoning,
                    tool_calls: None,
                    tool_call_id: None,
                    tool_error: false,
                    provider_state,
                    origin: provider::MessageOrigin::Assistant,
                });
            }
        }
        MessageRole::Tool => {
            let call_id = m
                .tool_call_id
                .as_deref()
                .ok_or_else(|| format!("tool message {} is missing tool_call_id", m.id))?;
            let failed = match m.tool_outcome.as_deref() {
                None => false,
                Some(value) => match crate::events::ToolOutcome::parse(value)
                    .map_err(|error| format!("tool message {}: {error}", m.id))?
                {
                    crate::events::ToolOutcome::Success => false,
                    crate::events::ToolOutcome::Denied | crate::events::ToolOutcome::Error => true,
                },
            };
            if failed {
                msgs.push(ChatMessage::tool_error(call_id, &m.content));
            } else {
                msgs.push(ChatMessage::tool_result(call_id, &m.content));
            }
        }
        // Background we injected on an earlier turn and then froze into the
        // history. The wire role is `user` either way — see
        // `ChatMessage::system_context` — and going back through it here is what
        // makes the bytes identical to the turn that first sent it. They have to
        // be: this row exists so that the prefix in front of it stays cached, and
        // a single character of drift undoes exactly that.
        //
        // Not `MessageOrigin::LegacyUser` with the wrapper baked into `content`,
        // which would render the same but would put the row outside everything
        // that treats injected context as different from conversation —
        // `take_injected_context` above all.
        MessageRole::Context => msgs.push(ChatMessage::system_context(&m.content)),
    }
    if role == MessageRole::User {
        push_message_context(msgs, context_items.unwrap_or_default())?;
    }
    Ok(())
}

fn push_message_context(msgs: &mut Vec<ChatMessage>, items: &[MessageContextItemRow]) -> Result<(), String> {
    for rendered in render_message_context_items(items)? {
        msgs.push(ChatMessage::user_provided_context(&rendered));
    }
    Ok(())
}

/// Render the frozen context attached to one user message exactly as native
/// history replay does. Compaction uses the same projection so a summary does
/// not silently replace an `@` marker or `!` command with none of the evidence
/// the original turn received.
pub(super) fn render_message_context_items(items: &[MessageContextItemRow]) -> Result<Vec<String>, String> {
    for item in items {
        crate::workspace::reference::MessageContextKind::parse(&item.kind)?;
    }
    // A shell retry stores every attempt for diagnosis, but only the final one
    // is evidence for the next model turn. File and directory references all
    // remain in request order.
    let final_shell = items
        .iter()
        .filter(|item| item.kind == "shell_output")
        .max_by_key(|item| item.position)
        .map(|item| item.id.as_str());
    items
        .iter()
        .filter(|item| item.kind != "shell_output" || final_shell == Some(item.id.as_str()))
        .map(|item| {
            Ok(crate::workspace::reference::render_context_item(
                crate::workspace::reference::MessageContextKind::parse(&item.kind)?,
                item.display_path.as_deref(),
                item.line_start,
                item.line_end,
                &item.content,
                item.truncated != 0,
            ))
        })
        .collect()
}

fn has_stored_user_content(message: &ChatMessage) -> bool {
    message.role == "user"
        && matches!(
            message.origin,
            provider::MessageOrigin::User(_) | provider::MessageOrigin::LegacyUser
        )
}

pub fn resolve_file_uris_in_messages(
    messages: &mut [ChatMessage],
    files_root: Option<&std::path::Path>,
) -> Result<(), String> {
    for msg in messages.iter_mut() {
        // Only user-authored attachments may be inlined: assistant/tool content
        // is model-influenced and must never trigger local file reads.
        if !has_stored_user_content(msg) {
            continue;
        }
        let Some(mut parts) = provider::decode_message_parts(&msg.content)? else {
            continue;
        };
        let Some(files_root) = files_root else { continue };
        let mut changed = false;
        for part in parts.iter_mut() {
            let url = match part {
                provider::MessageContentPart::ImageUrl { image_url } => Some(image_url.url.clone()),
                provider::MessageContentPart::File { file } => Some(file.url.clone()),
                _ => None,
            };
            if let Some(ref uri) = url
                && let Some(path) = crate::files::resolve_attachment_uri(uri, files_root)
            {
                let mime = mime_guess::from_path(&path).first_or_octet_stream().to_string();
                if let Ok(data_uri) = crate::files::file_to_base64_data_uri(&path, &mime) {
                    match part {
                        provider::MessageContentPart::ImageUrl { image_url } => image_url.url = data_uri,
                        provider::MessageContentPart::File { file } => file.url = data_uri,
                        _ => unreachable!("the URL came from an attachment part"),
                    }
                    changed = true;
                }
            }
        }
        if changed {
            msg.content = provider::encode_message_parts(&parts)?;
        }
    }
    Ok(())
}

/// Converts Meridian's transcript-only sticker part into provider-supported
/// text/image parts. Confirmed stickers are semantic text. An unlabelled sticker
/// is shown only on the current turn; old unknown stickers stay a placeholder so
/// history does not repeatedly pay for the same pixels.
pub fn resolve_sticker_parts_in_messages(
    messages: &mut [ChatMessage],
    pool: &crate::db::DbPool,
    data_dir: Option<&std::path::Path>,
    include_current_visual: bool,
) -> Result<(), String> {
    let decoded = messages
        .iter()
        .map(|message| {
            if has_stored_user_content(message) {
                provider::decode_message_parts(&message.content)
            } else {
                Ok(None)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let current_user = decoded.iter().rposition(Option::is_some);
    let Ok(mut conn) = pool.get() else { return Ok(()) };

    for (message_index, (message, parts)) in messages.iter_mut().zip(decoded).enumerate() {
        let Some(parts) = parts else { continue };
        let mut changed = false;
        let mut provider_parts = Vec::with_capacity(parts.len() + 1);
        for part in parts {
            let provider::MessageContentPart::Sticker { sticker_id, .. } = part else {
                provider_parts.push(part);
                continue;
            };
            changed = true;
            let Ok(sticker) = crate::db::ops::emoji::get_emoji(&mut conn, &sticker_id) else {
                provider_parts.push(provider::MessageContentPart::Text {
                    text: "[unavailable sticker]".into(),
                });
                continue;
            };
            if sticker.semantic_status == "confirmed" {
                let tags = sticker.tags.as_deref().filter(|tags| !tags.trim().is_empty());
                let description = match tags {
                    Some(tags) => format!("[sticker: {}; tags: {}]", sticker.name, tags),
                    None => format!("[sticker: {}]", sticker.name),
                };
                provider_parts.push(provider::MessageContentPart::Text { text: description });
                continue;
            }

            provider_parts.push(provider::MessageContentPart::Text {
                text: if include_current_visual && current_user == Some(message_index) && !sticker.file_name.is_empty()
                {
                    "[unlabelled sticker attached; infer its visible reaction cautiously]"
                } else {
                    "[unlabelled sticker]"
                }
                .into(),
            });
            if !include_current_visual || current_user != Some(message_index) || sticker.file_name.is_empty() {
                continue;
            }
            let Some(data_dir) = data_dir else { continue };
            let path = crate::emoji::emoji_path(data_dir, &sticker.pack_id, &sticker.file_name);
            if let Ok(data_uri) = crate::emoji::vision_preview_data_uri(&path) {
                provider_parts.push(provider::MessageContentPart::ImageUrl {
                    image_url: provider::MessageContentUrl { url: data_uri },
                });
            }
        }
        if changed {
            message.content = provider::encode_message_parts(&provider_parts)?;
        }
    }
    Ok(())
}

static DEFAULT_COUNTER: std::sync::OnceLock<TokenCounter> = std::sync::OnceLock::new();

fn default_counter() -> &'static TokenCounter {
    DEFAULT_COUNTER.get_or_init(|| TokenCounter::new(TokenizerKind::Cl100kBase))
}

pub(crate) fn estimate_tokens(content: &str) -> usize {
    default_counter().count_content(content) + 4
}

/// Lift out the background we injected ourselves (the memory block).
///
/// It is not conversation, so it must not be summarised or dropped along with
/// old turns: doing so loses everyone's memories mid-turn while the model keeps
/// acting as if it still has them, and — worse — feeds `<owner_notes>` through a
/// summariser that strips the never-quote wrapper protecting them. Callers put
/// it back at the tail, where it stays inside every subsequent keep-recent
/// window.
pub(crate) fn take_injected_context(messages: &mut Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut taken = Vec::new();
    messages.retain(|m| {
        if m.origin.is_system_context() {
            taken.push(m.clone());
            false
        } else {
            true
        }
    });
    taken
}

const MAX_MODEL_USER_CONTEXT_TOKENS: usize = 25_000;
const USER_CONTEXT_BUDGET_MARKER: &str =
    "\n[additional user-provided context omitted by Meridian to fit the model context window]";

fn truncate_user_context_to_tokens(content: &str, max_tokens: usize) -> Option<String> {
    let counter = default_counter();
    if counter.count(USER_CONTEXT_BUDGET_MARKER) > max_tokens {
        return None;
    }
    let boundaries = content
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(content.len()))
        .collect::<Vec<_>>();
    let mut low = 0usize;
    let mut high = boundaries.len() - 1;
    while low < high {
        let mid = (low + high).div_ceil(2);
        let candidate = format!("{}{USER_CONTEXT_BUDGET_MARKER}", &content[..boundaries[mid]]);
        if counter.count(&candidate) <= max_tokens {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    let candidate = format!("{}{USER_CONTEXT_BUDGET_MARKER}", &content[..boundaries[low]]);
    (counter.count(&candidate) <= max_tokens).then_some(candidate)
}

/// User-provided snapshots are ordinary compactable history, but a large one
/// can still land inside the recent tail that trimming deliberately preserves.
/// Reserve at most one quarter of the model window for all such messages,
/// keeping the newest evidence first and making any partial copy explicit.
pub(crate) fn cap_user_provided_context(messages: &mut Vec<ChatMessage>, context_limit: usize) {
    let mut keep = vec![true; messages.len()];
    let mut remaining = (context_limit / 4).min(MAX_MODEL_USER_CONTEXT_TOKENS);
    for index in (0..messages.len()).rev() {
        if messages[index].origin != provider::MessageOrigin::UserProvidedContext {
            continue;
        }
        let cost = estimate_tokens(&messages[index].content);
        if cost <= remaining {
            remaining -= cost;
            continue;
        }
        let content_tokens = remaining.saturating_sub(4);
        if let Some(truncated) = truncate_user_context_to_tokens(&messages[index].content, content_tokens) {
            let cost = estimate_tokens(&truncated);
            messages[index].content = truncated;
            remaining = remaining.saturating_sub(cost);
        } else {
            keep[index] = false;
        }
    }
    let mut index = 0usize;
    messages.retain(|_| {
        let retain = keep[index];
        index += 1;
        retain
    });
}

pub fn trim_to_context_limit(messages: &mut Vec<ChatMessage>, context_limit: usize, keep_recent: usize) {
    let safe_limit = context_limit * 4 / 5;
    cap_user_provided_context(messages, context_limit);
    let total_tokens: usize = messages.iter().map(|m| estimate_tokens(&m.content)).sum();
    if total_tokens <= safe_limit {
        return;
    }
    // Held aside so a long tool loop cannot push the memory block out of the
    // window; it is re-appended before the kept tail.
    let injected = take_injected_context(messages);
    let has_system = messages.first().is_some_and(|m| m.role == "system");
    let system_offset = if has_system { 1 } else { 0 };
    let keep = (keep_recent * 2).min(messages.len().saturating_sub(system_offset));
    let start = messages.len() - keep;
    let mut trimmed = Vec::new();
    if has_system {
        trimmed.push(messages[0].clone());
    }
    trimmed.extend(injected);
    trimmed.extend_from_slice(&messages[start..]);
    remove_orphan_tool_messages(&mut trimmed);
    *messages = trimmed;
}

pub(crate) fn remove_orphan_tool_messages(messages: &mut Vec<ChatMessage>) {
    let mut valid_call_ids = std::collections::HashSet::new();
    for m in messages.iter() {
        if let Some(ref tcs) = m.tool_calls {
            for tc in tcs {
                valid_call_ids.insert(tc.id.clone());
            }
        }
    }
    messages.retain(|m| {
        if m.role == "tool"
            && let Some(ref id) = m.tool_call_id
        {
            return valid_call_ids.contains(id);
        }
        true
    });
    // Also remove assistant tool_calls whose results were dropped
    let mut valid_result_ids = std::collections::HashSet::new();
    for m in messages.iter() {
        if m.role == "tool"
            && let Some(ref id) = m.tool_call_id
        {
            valid_result_ids.insert(id.clone());
        }
    }
    for m in messages.iter_mut() {
        if let Some(ref mut tcs) = m.tool_calls {
            tcs.retain(|tc| valid_result_ids.contains(&tc.id));
            if tcs.is_empty() {
                m.tool_calls = None;
            }
        }
    }
}

static DATA_URI_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();

pub(crate) fn data_uri_re() -> &'static regex::Regex {
    DATA_URI_RE.get_or_init(|| regex::Regex::new(r"data:(image/[^;]+);base64,[A-Za-z0-9+/=]+").unwrap())
}

pub fn microcompact(messages: &mut [ChatMessage], budget: &TokenBudget, keep_recent_turns: usize) -> usize {
    let before = budget.counter.count_messages(messages);

    let has_system = messages.first().is_some_and(|m| m.role == "system");
    let system_offset = if has_system { 1 } else { 0 };
    let keep_msgs = keep_recent_turns * 2;
    // Clamp to system_offset: for short histories boundary can fall below it,
    // which would invert the slice below and panic.
    let boundary = messages.len().saturating_sub(keep_msgs).max(system_offset);

    for msg in messages[system_offset..boundary].iter_mut() {
        if msg.role == "tool" {
            let tokens = budget.counter.count(&msg.content);
            if tokens > 2000 {
                let chars: Vec<char> = msg.content.chars().collect();
                let head_end = char_index_for_tokens(&budget.counter, &chars, 200);
                let tail_start = chars
                    .len()
                    .saturating_sub(char_index_for_tokens_rev(&budget.counter, &chars, 100));
                if head_end < tail_start {
                    let head: String = chars[..head_end].iter().collect();
                    let tail: String = chars[tail_start..].iter().collect();
                    msg.content = format!("{head}\n[... truncated, was {tokens} tokens ...]\n{tail}");
                }
            }
        }

        msg.content = data_uri_re()
            .replace_all(&msg.content, |caps: &regex::Captures| {
                let mime = caps.get(1).map(|m| m.as_str()).unwrap_or("image/unknown");
                format!("[image: {mime}]")
            })
            .into_owned();

        if msg.role == "assistant" {
            msg.reasoning_content = None;
        }
    }

    let after = budget.counter.count_messages(messages);
    before.saturating_sub(after)
}

fn char_index_for_tokens(counter: &TokenCounter, chars: &[char], target_tokens: usize) -> usize {
    let mut idx = (target_tokens * 4).min(chars.len());
    loop {
        let s: String = chars[..idx].iter().collect();
        if counter.count(&s) >= target_tokens || idx >= chars.len() {
            break idx;
        }
        idx = (idx + 50).min(chars.len());
    }
}

fn char_index_for_tokens_rev(counter: &TokenCounter, chars: &[char], target_tokens: usize) -> usize {
    let mut count = (target_tokens * 4).min(chars.len());
    loop {
        // Recompute start each round: the counted suffix must grow with count,
        // otherwise a low-token-density tail loops forever.
        let start = chars.len().saturating_sub(count);
        let s: String = chars[start..].iter().collect();
        if counter.count(&s) >= target_tokens || start == 0 {
            break count;
        }
        count = (count + 50).min(chars.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use provider::ToolCall;

    fn msg(id: &str, role: &str, content: &str) -> MessageRow {
        MessageRow {
            id: id.into(),
            conversation_id: "c".into(),
            role: role.into(),
            content: content.into(),
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
            source: None,
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

    fn chat_msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: content.into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            tool_error: false,
            provider_state: None,
            origin: provider::MessageOrigin::LegacyUser,
        }
    }

    /// A linear conversation with nothing compacted — what these tests are about.
    fn ctx(history: &[MessageRow]) -> ActiveContext {
        ActiveContext {
            path: history.iter().filter(|m| m.is_compact_summary == 0).cloned().collect(),
            summary: None,
            anchor_index: None,
            head_id: history.last().map(|m| m.id.clone()),
        }
    }

    #[test]
    fn test_build_messages_with_system() {
        let history = vec![msg("1", "user", "hi")];
        let msgs = build_messages("You are a helper", &ctx(&history), "new question").unwrap();
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content, "You are a helper");
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[1].content, "hi");
        assert_eq!(msgs[2].role, "user");
        assert_eq!(msgs[2].content, "new question");
    }

    #[test]
    fn test_build_messages_empty_system() {
        let msgs = build_messages("", &ctx(&[]), "hello").unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
    }

    #[test]
    fn provider_state_survives_database_context_rebuild() {
        use provider::state::{
            GoogleSignatureLocation, GoogleThoughtSignature, ProviderState, ProviderStatePayload, ProviderStateProducer,
        };

        let state = ProviderState {
            version: 1,
            producer: ProviderStateProducer {
                vendor: "google".into(),
                protocol: "openai_chat_completions".into(),
                model: "gemini-3.7-flash".into(),
            },
            payload: ProviderStatePayload::GoogleThoughtSignatures {
                signatures: vec![GoogleThoughtSignature {
                    location: GoogleSignatureLocation::Message,
                    signature: "durable-signature".into(),
                }],
            },
        };
        let mut row = msg("1", "assistant", "answer");
        row.provider_state = Some(state.to_storage_json().unwrap());
        let messages = build_messages("", &ctx(&[row]), "continue").unwrap();
        assert_eq!(messages[0].provider_state.as_ref(), Some(&state));
    }

    #[test]
    fn corrupt_persisted_provider_state_aborts_context_rebuild() {
        let mut row = msg("broken-state", "assistant", "answer");
        row.provider_state = Some(r#"{"version":1,"future":true}"#.into());

        let error = build_messages("", &ctx(&[row]), "continue")
            .expect_err("corrupt provider state must not be flattened into None");
        assert!(error.contains("broken-state"), "{error}");
        assert!(error.contains("invalid persisted provider_state"), "{error}");
    }

    #[test]
    fn test_build_messages_filters_roles() {
        let mut orphaned_tool_output = msg("2", "tool", "result");
        orphaned_tool_output.tool_call_id = Some("call-1".into());
        let history = vec![msg("1", "user", "q"), orphaned_tool_output, msg("3", "assistant", "a")];
        let msgs = build_messages("sys", &ctx(&history), "new").unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[1].content, "q");
        assert_eq!(msgs[2].role, "assistant");
        assert_eq!(msgs[2].content, "a");
        assert_eq!(msgs[3].role, "user");
        assert_eq!(msgs[3].content, "new");
    }

    #[test]
    fn test_build_messages_strips_tool_calls_with_no_result() {
        // A tool row can go missing: its insert is fire-and-forget, and deleting
        // an assistant message leaves its tool rows unreferenced. Either way a
        // `tool_calls` nothing answers is rejected by the provider, so the pairing
        // has to be repaired here rather than at each call site.
        let mut assistant = msg("2", "assistant", "calling a tool");
        assistant.tool_calls =
            Some(r#"[{"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{}"}}]"#.into());
        let history = vec![msg("1", "user", "q"), assistant];
        let msgs = build_messages("sys", &ctx(&history), "next").unwrap();
        let a = msgs.iter().find(|m| m.role == "assistant").unwrap();
        assert!(a.tool_calls.is_none(), "unanswered tool_calls should be stripped");
    }

    #[test]
    /// A stream cut mid-argument leaves a fragment on the row. The row still
    /// reads (the transcript has to open), but what reaches the provider is a
    /// well-formed empty object, since a fragment fails the whole request.
    fn a_truncated_argument_fragment_is_sent_as_an_empty_object() {
        let mut assistant = msg("cut-short", "assistant", "");
        assistant.tool_calls = Some(
            r#"[{"id":"c1","type":"function","function":{"name":"run_command","arguments":"{\"command\":\"echo half"}}]"#
                .into(),
        );
        // Answered, because an unanswered call is stripped from the context on
        // its own — the model rejected the fragment as invalid arguments.
        let mut result = msg("cut-short-result", "tool", "invalid arguments");
        result.tool_call_id = Some("c1".into());
        let built =
            build_messages("", &ctx(&[assistant, result]), "next").expect("a fragment must not abort the rebuild");
        let call = built
            .iter()
            .find_map(|m| m.tool_calls.as_ref())
            .and_then(|calls| calls.first())
            .expect("the call is still on the row");
        assert_eq!(call.arguments, "{}");
    }

    #[test]
    fn corrupt_persisted_tool_calls_abort_context_rebuild() {
        let mut assistant = msg("broken-message", "assistant", "");
        assistant.tool_calls = Some("not-json".into());
        let error = build_messages("", &ctx(&[assistant]), "next")
            .expect_err("corrupt history must not be flattened into a plain assistant row");
        assert!(error.contains("broken-message"), "{error}");
        assert!(error.contains("invalid persisted tool_calls"), "{error}");
    }

    #[test]
    fn test_build_messages_drops_orphan_tool_row() {
        let mut tool = msg("2", "tool", "result");
        tool.tool_call_id = Some("call_1".into());
        let history = vec![msg("1", "user", "q"), tool];
        let msgs = build_messages("sys", &ctx(&history), "next").unwrap();
        assert!(
            msgs.iter().all(|m| m.role != "tool"),
            "orphan tool row should be dropped"
        );
    }

    #[test]
    fn test_resolve_file_uris_only_user_and_contained() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("files");
        std::fs::create_dir_all(root.join("c1")).unwrap();
        let inside = root.join("c1").join("img.png");
        std::fs::write(&inside, b"\x89PNG").unwrap();
        let uri = format!("file:///{}", inside.to_string_lossy().replace('\\', "/"));
        let content = format!(r#"[{{"type":"image_url","image_url":{{"url":"{uri}"}}}}]"#);

        let mut msgs = vec![chat_msg("assistant", &content), chat_msg("user", &content)];
        resolve_file_uris_in_messages(&mut msgs, Some(&root)).unwrap();
        assert!(
            msgs[0].content.contains("file:///"),
            "assistant content must never be inlined"
        );
        assert!(
            msgs[1].content.contains("data:"),
            "user attachment inside the root should inline"
        );

        let outside = dir.path().join("evil.txt");
        std::fs::write(&outside, b"x").unwrap();
        let uri2 = format!("file:///{}", outside.to_string_lossy().replace('\\', "/"));
        let content2 = format!(r#"[{{"type":"image_url","image_url":{{"url":"{uri2}"}}}}]"#);
        let mut msgs2 = vec![chat_msg("user", &content2)];
        resolve_file_uris_in_messages(&mut msgs2, Some(&root)).unwrap();
        assert!(
            msgs2[0].content.contains("file:///"),
            "paths outside the root must not inline"
        );

        // No root configured → nothing is inlined at all.
        let mut msgs3 = vec![chat_msg("user", &content)];
        resolve_file_uris_in_messages(&mut msgs3, None).unwrap();
        assert!(msgs3[0].content.contains("file:///"));
    }

    #[test]
    fn malformed_content_parts_abort_attachment_resolution() {
        let mut messages = vec![ChatMessage::user("[{not-json")];
        let error = resolve_file_uris_in_messages(&mut messages, None).unwrap_err();
        assert!(error.contains("invalid persisted message content parts"), "{error}");
    }

    #[test]
    fn sticker_parts_become_semantics_and_old_unknowns_do_not_resend_pixels() {
        let pool = crate::db::test_db();
        let mut conn = pool.get().unwrap();
        crate::db::ops::emoji_pack::create_pack(
            &mut conn,
            &crate::db::models::emoji_pack::EmojiPackInsert {
                id: "p1",
                name: "pack",
                description: None,
                cover_image: None,
                is_builtin: 0,
                sort_order: 0,
                created_at: 1,
                updated_at: 1,
                kind: "manual",
                source_account_id: None,
            },
        )
        .unwrap();
        let make = |id, name, status| crate::db::models::emoji::EmojiInsert {
            id,
            pack_id: "p1",
            name,
            tags: Some("reaction"),
            file_name: "",
            file_format: "",
            sort_order: 0,
            created_at: 1,
            source: "local",
            source_key: None,
            native_payload: None,
            semantic_status: status,
            suggested_name: None,
            suggested_tags: None,
            file_size: 0,
            seen_count: 1,
            last_seen_at: Some(1),
        };
        crate::db::ops::emoji::create_emoji(&mut conn, &make("known", "wave", "confirmed")).unwrap();
        crate::db::ops::emoji::create_emoji(&mut conn, &make("unknown", "pending-x", "pending")).unwrap();
        drop(conn);

        let mut messages = vec![
            ChatMessage::user(r#"[{"type":"sticker","sticker_id":"unknown"}]"#),
            ChatMessage::assistant("ok"),
            ChatMessage::user(r#"[{"type":"sticker","sticker_id":"known"},{"type":"sticker","sticker_id":"unknown"}]"#),
        ];
        resolve_sticker_parts_in_messages(&mut messages, &pool, None, true).unwrap();
        assert!(messages[0].content.contains("[unlabelled sticker]"));
        assert!(!messages[0].content.contains("image_url"));
        assert!(messages[2].content.contains("[sticker: wave; tags: reaction]"));
        assert!(messages[2].content.contains("[unlabelled sticker]"));
        assert!(!messages[2].content.contains("\"type\":\"sticker\""));
    }

    #[test]
    fn test_trim_no_trim_needed() {
        let mut msgs = vec![chat_msg("system", "sys"), chat_msg("user", "hi")];
        trim_to_context_limit(&mut msgs, 100_000, 5);
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn recent_inline_image_does_not_fill_the_window_by_base64_length() {
        let image = serde_json::json!([
            { "type": "text", "text": "what is in this image?" },
            {
                "type": "image_url",
                "image_url": { "url": format!("data:image/jpeg;base64,{}", "A".repeat(1_000_000)) }
            }
        ])
        .to_string();
        let mut messages = vec![chat_msg("system", "sys"), chat_msg("user", &image)];

        trim_to_context_limit(&mut messages, 20_000, 20);

        assert_eq!(
            messages.len(),
            2,
            "the current image must remain available to the model"
        );
        assert!(messages[1].content.contains("data:image/jpeg;base64,"));
        assert!(
            messages
                .iter()
                .map(|message| estimate_tokens(&message.content))
                .sum::<usize>()
                < 10_000,
            "base64 transport bytes must not be treated as text tokens"
        );
    }

    #[test]
    fn microcompact_still_replaces_old_inline_images() {
        let image = serde_json::json!([
            { "type": "image_url", "image_url": { "url": "data:image/png;base64,QUJD" } }
        ])
        .to_string();
        let budget = TokenBudget::new("openai", "gpt-4o", 128_000, 16_384, None);
        let mut messages = vec![
            chat_msg("system", "sys"),
            chat_msg("user", &image),
            chat_msg("assistant", "old answer"),
            chat_msg("user", "new question"),
            chat_msg("assistant", "new answer"),
        ];

        microcompact(&mut messages, &budget, 1);

        assert!(!messages[1].content.contains("base64"));
        assert!(messages[1].content.contains("[image: image/png]"));
    }

    #[test]
    fn test_trim_preserves_system() {
        let mut msgs = vec![chat_msg("system", &"s".repeat(1000))];
        for i in 0..20 {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            msgs.push(chat_msg(role, &"x".repeat(200)));
        }
        trim_to_context_limit(&mut msgs, 500, 2);
        assert_eq!(msgs[0].role, "system");
        assert!(msgs.len() < 21);
    }

    #[test]
    fn test_trim_keeps_recent() {
        let mut msgs = Vec::new();
        for i in 0..10 {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            msgs.push(chat_msg(role, &format!("msg-{i}")));
        }
        trim_to_context_limit(&mut msgs, 10, 2);
        let last = msgs.last().unwrap();
        assert_eq!(last.content, "msg-9");
    }

    #[test]
    fn test_trim_removes_orphan_tool_results() {
        let mut msgs = vec![
            chat_msg("system", "sys"),
            ChatMessage::assistant_with_tools(
                "I'll call a tool",
                None,
                vec![ToolCall {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    arguments: "{}".into(),
                }],
            ),
            ChatMessage::tool_result("call_1", "file content"),
            chat_msg("user", &"x".repeat(500)),
            chat_msg("assistant", &"y".repeat(500)),
        ];
        trim_to_context_limit(&mut msgs, 100, 2);
        for m in &msgs {
            if m.role == "tool" {
                let id = m.tool_call_id.as_deref().unwrap();
                let has_call = msgs.iter().any(|am| {
                    am.tool_calls
                        .as_ref()
                        .is_some_and(|tcs| tcs.iter().any(|tc| tc.id == id))
                });
                assert!(
                    has_call,
                    "orphan tool result with call_id={id} should have been removed"
                );
            }
        }
    }

    #[test]
    fn test_microcompact_short_history_with_system_no_panic() {
        let budget = TokenBudget::new("openai", "gpt-4o", 128_000, 16_384, None);
        let mut msgs = vec![
            chat_msg("system", "You are a helpful assistant."),
            chat_msg("user", "hello"),
            chat_msg("assistant", "hi there"),
        ];
        // boundary < system_offset here; must not panic.
        let _ = microcompact(&mut msgs, &budget, 10);
        assert_eq!(msgs.len(), 3);
    }

    #[test]
    fn test_char_index_for_tokens_rev_terminates_on_low_density() {
        let counter = TokenCounter::new(TokenizerKind::Cl100kBase);
        // Low token-density tail: a fixed-start loop would spin forever.
        let chars: Vec<char> = "=".repeat(4000).chars().collect();
        let idx = char_index_for_tokens_rev(&counter, &chars, 100);
        assert!(idx <= chars.len());
    }
    /// The wire flag comes off the row's `tool_outcome`, so an error the
    /// model was told about in one turn is still marked one when the history
    /// is replayed. A row from before the column, and a success, stay plain.
    #[test]
    fn a_failed_tool_row_is_replayed_as_a_tool_error() {
        let replay = |outcome: Option<&str>| {
            let mut row = msg("t1", "tool", "boom");
            row.tool_call_id = Some("call_1".into());
            row.tool_outcome = outcome.map(str::to_owned);
            let mut msgs = Vec::new();
            push_history_message(&mut msgs, &row, &SenderNames::new(), None).unwrap();
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0].role, "tool");
            assert_eq!(msgs[0].tool_call_id.as_deref(), Some("call_1"));
            msgs[0].tool_error
        };
        assert!(replay(Some("error")));
        assert!(replay(Some("denied")));
        assert!(!replay(Some("success")));
        assert!(!replay(None));
    }
}

#[cfg(test)]
mod injected_context_tests {
    use super::*;
    use crate::provider::MessageOrigin;

    fn long_history(n: usize) -> Vec<ChatMessage> {
        (0..n)
            .map(|i| ChatMessage::user(&"word ".repeat(200).repeat(i % 2 + 1)))
            .collect()
    }

    /// A stored row carrying an injection frozen by an earlier turn.
    fn frozen_row(content: &str, source: &str) -> MessageRow {
        MessageRow {
            id: "m1".into(),
            conversation_id: "c".into(),
            role: "context".into(),
            content: content.into(),
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
            source: Some(source.into()),
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

    /// The memory block must survive a trim that drops old turns. Before this
    /// guard it sat in the middle of history and was dropped with everything
    /// else, so the model silently lost every memory mid-turn.
    #[test]
    fn trimming_keeps_the_injected_memory_block() {
        let mut msgs = vec![ChatMessage {
            role: "system".into(),
            content: "sys".into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            tool_error: false,
            provider_state: None,
            origin: MessageOrigin::Assistant,
        }];
        msgs.push(ChatMessage::system_context("<bot_memories>\n- x\n</bot_memories>"));
        msgs.extend(long_history(40));

        trim_to_context_limit(&mut msgs, 1_000, 2);

        assert!(
            msgs.iter().any(|m| m.origin.is_system_context()),
            "the memory block must not be trimmed away with old turns"
        );
        // And it stays ahead of the recent tail rather than at the very end.
        let idx = msgs.iter().position(|m| m.origin.is_system_context()).unwrap();
        assert!(idx < msgs.len() - 1);
    }

    /// A frozen memory row has to come back off the history as the exact bytes
    /// the turn that wrote it put on the wire.
    ///
    /// This is what the whole design rests on. The row exists so the prefix in
    /// front of it stays in the provider's cache; one character of drift and the
    /// payloads diverge at that point, which costs more than never having frozen
    /// it. Nothing else in the test suite would notice — the conversation still
    /// reads correctly either way, it just stops being cheap.
    #[test]
    fn a_frozen_memory_row_is_replayed_byte_for_byte() {
        let block = "<bot_memories>\n- [general] k: v\n</bot_memories>";

        // What the turn that first assembled it sent.
        let fresh = ChatMessage::system_context(block);
        // What the next turn reads back off the history.
        let mut replayed = Vec::new();
        let row = frozen_row(block, "memory|full|100.abc|-|onebot:user:1");
        push_history_message(&mut replayed, &row, &SenderNames::new(), None).unwrap();

        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].content, fresh.content);
        assert_eq!(replayed[0].role, fresh.role);
        assert!(matches!(replayed[0].origin, MessageOrigin::SystemContext));

        // And on the wire, which is where it actually matters.
        for rendering in [provider::SenderRendering::NameField, provider::SenderRendering::Prefix] {
            assert_eq!(
                provider::render_message(&replayed[0], rendering).unwrap().content,
                provider::render_message(&fresh, rendering).unwrap().content,
            );
        }
    }

    /// It also has to keep the protection injected context gets: a frozen block
    /// carries `<owner_notes>`, and a summariser paraphrasing those out of the
    /// wrapper that forbids quoting them is the one outcome worth designing
    /// against.
    #[test]
    fn a_frozen_memory_row_is_still_injected_context() {
        let row = frozen_row(
            "<owner_notes>\n- [general] k: v\n</owner_notes>",
            "memory|full|100.abc|-|",
        );
        let mut msgs = Vec::new();
        push_history_message(&mut msgs, &row, &SenderNames::new(), None).unwrap();
        msgs.push(ChatMessage::user("hi"));

        let taken = take_injected_context(&mut msgs);
        assert_eq!(taken.len(), 1, "a frozen block must be liftable like a fresh one");
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn take_injected_context_removes_only_injected_rows() {
        let mut msgs = vec![
            ChatMessage::user("hi"),
            ChatMessage::user_provided_context("file snapshot"),
            ChatMessage::system_context("memories"),
            ChatMessage::assistant("hello"),
        ];
        let taken = take_injected_context(&mut msgs);
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].content, "memories");
        assert_eq!(msgs.len(), 3);
        assert!(
            msgs.iter()
                .any(|m| matches!(m.origin, MessageOrigin::UserProvidedContext)),
            "user-provided context remains ordinary compactable history"
        );
        assert!(!msgs.iter().any(|m| m.origin.is_system_context()));
    }

    #[test]
    fn only_the_final_shell_attempt_enters_native_history() {
        let item = |id: &str, position: i32, content: &str| MessageContextItemRow {
            id: id.into(),
            message_id: "m".into(),
            position,
            kind: "shell_output".into(),
            content: content.into(),
            display_path: None,
            line_start: None,
            line_end: None,
            content_hash: "hash".into(),
            byte_count: content.len() as i32,
            line_count: 1,
            token_count: 1,
            truncated: 0,
            metadata: None,
            created_at: 1,
        };
        let mut messages = Vec::new();
        push_message_context(
            &mut messages,
            &[
                item("in-doubt", 0, "result may be in doubt"),
                item("final", 1, "final result"),
            ],
        )
        .unwrap();

        assert_eq!(messages.len(), 1);
        assert!(messages[0].content.contains("final result"));
        assert!(!messages[0].content.contains("result may be in doubt"));
    }

    #[test]
    fn a_recent_user_context_cannot_bypass_the_native_prompt_budget() {
        let mut messages = vec![
            ChatMessage::user("question"),
            ChatMessage::user_provided_context(&"界".repeat(20_000)),
            ChatMessage::assistant("previous answer"),
        ];

        trim_to_context_limit(&mut messages, 1_000, 20);

        let contexts = messages
            .iter()
            .filter(|message| message.origin == MessageOrigin::UserProvidedContext)
            .collect::<Vec<_>>();
        assert_eq!(
            contexts.len(),
            1,
            "the newest context is reduced rather than silently reclassified"
        );
        assert!(contexts[0].content.contains(USER_CONTEXT_BUDGET_MARKER));
        assert!(
            contexts
                .iter()
                .map(|message| estimate_tokens(&message.content))
                .sum::<usize>()
                <= 250,
            "all native user-provided context stays inside one quarter of the model window",
        );
    }

    #[test]
    fn native_user_context_has_an_aggregate_cap_across_messages() {
        let mut messages = vec![
            ChatMessage::user_provided_context(&"old ".repeat(4_000)),
            ChatMessage::user("question"),
            ChatMessage::user_provided_context(&"new ".repeat(4_000)),
        ];

        trim_to_context_limit(&mut messages, 2_000, 20);

        let context_tokens = messages
            .iter()
            .filter(|message| message.origin == MessageOrigin::UserProvidedContext)
            .map(|message| estimate_tokens(&message.content))
            .sum::<usize>();
        assert!(context_tokens <= 500);
        assert!(
            messages
                .iter()
                .filter(|message| message.origin == MessageOrigin::UserProvidedContext)
                .any(|message| message.content.contains("new ")),
            "the newest pending evidence receives the bounded allocation first",
        );
    }

    /// The interrupted-turn block is the one piece of context whose whole point
    /// is to be read *this* turn, and a long tool loop is exactly the shape of
    /// turn that would otherwise push it out of the window. It rides the same
    /// injected-context channel as memory, so trimming lifts it out and puts it
    /// back rather than dropping it.
    #[test]
    fn an_interrupted_turn_survives_trimming() {
        let mut msgs = vec![ChatMessage::system_context(
            "<interrupted_turn>\nedit_file may have taken effect.\n</interrupted_turn>",
        )];
        for i in 0..80 {
            msgs.push(ChatMessage::user(&format!("question {i} {}", "x".repeat(400))));
            msgs.push(ChatMessage::assistant(&format!("answer {i} {}", "y".repeat(400))));
        }

        trim_to_context_limit(&mut msgs, 2_000, 4);

        let block = msgs.iter().find(|m| m.origin.is_system_context());
        assert!(
            block.is_some_and(|m| m.content.starts_with("<interrupted_turn>")),
            "the block a turn exists to read must not be the thing trimming drops",
        );
    }

    /// A compaction summary replaces history, so it must remain compactable —
    /// tagging it as injected background would make it immortal and it would
    /// accumulate one copy per compaction.
    #[test]
    fn compaction_summaries_are_not_treated_as_injected() {
        let history = [crate::db::models::message::MessageRow {
            id: "s".into(),
            conversation_id: "c".into(),
            role: "user".into(),
            content: "summary text".into(),
            provider_id: None,
            model_id: None,
            input_tokens: None,
            output_tokens: None,
            tool_calls: None,
            tool_call_id: None,
            sort_order: -1,
            created_at: 1,
            reasoning_content: None,
            rating: None,
            schema_version: 2,
            is_compact_summary: 1,
            sender_id: None,
            parent_id: None,
            compact_anchor_id: None,
            source: None,
            turn_id: None,
            tool_outcome: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            server_tool_calls: None,
            provider_name: None,
            provider_state: None,
            auto_review: None,
        }];
        let context = crate::db::ops::message::ActiveContext {
            path: Vec::new(),
            summary: Some(history[0].clone()),
            anchor_index: None,
            head_id: None,
        };
        let msgs = build_messages("", &context, "now").unwrap();
        assert!(
            !msgs.iter().any(|m| m.origin.is_system_context()),
            "a summary is history's stand-in, not regenerated background"
        );
    }
}

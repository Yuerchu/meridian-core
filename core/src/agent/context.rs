use std::collections::HashMap;

use crate::db::entity::message as message_entity;
use crate::db::models::message_context_item::MessageContextItemRow;
use crate::db::ops::message::ActiveContext;
use crate::provider::{self, ChatMessage, SenderRef};

use super::tokenizer::{TokenBudget, TokenCounter, TokenizerKind};
use super::tool_calls::parse_stored_tool_calls;

/// Last known nickname per platform user id. Nicknames are not stored on the
/// message row (they change), so multi-speaker surfaces pass a lookup built
/// from the subject table.
pub type SenderNames = HashMap<i64, String>;

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
/// each user row. Keeping the map separate from `message_entity::Model` means raw snapshots
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
            sent_at: None,
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
    attach_marker_note(&mut msgs);
    Ok(msgs)
}

/// Explain the `<sent_at>` and `<sender>` markers once, in the system prompt,
/// whenever any message in this payload carries one.
///
/// Lives here rather than in each adapter because the markers are not a
/// fallback for formats lacking a `name` field — they are how every format
/// carries a speaker and a send time — so the explanation is not a per-adapter
/// concern either.
fn attach_marker_note(msgs: &mut Vec<ChatMessage>) {
    if !provider::needs_marker_note(msgs) {
        return;
    }
    if let Some(system) = msgs.first_mut().filter(|m| m.role == "system") {
        system.content.push_str("\n\n");
        system.content.push_str(provider::MESSAGE_MARKER_NOTE);
        return;
    }
    msgs.insert(
        0,
        ChatMessage {
            role: "system".into(),
            content: provider::MESSAGE_MARKER_NOTE.into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            tool_error: false,
            provider_state: None,
            origin: provider::MessageOrigin::Assistant,
            sent_at: None,
        },
    );
}

pub(crate) fn sender_ref(user_id: i64, names: &SenderNames) -> SenderRef {
    SenderRef {
        user_id,
        nickname: names.get(&user_id).cloned(),
    }
}

/// Last known nickname per platform user, read off the subject table. Both the
/// history replay and the live turn resolve a speaker through this one map, so
/// the same person renders the same way on both sides of a turn boundary.
pub fn load_sender_names(conn: &mut diesel::SqliteConnection) -> Result<SenderNames, String> {
    let subjects = crate::db::ops::memory::list_subjects(conn).map_err(|e| e.to_string())?;
    Ok(subjects
        .into_iter()
        .filter_map(|s| {
            let uid = s.user_id()?;
            Some((uid, s.display_name?))
        })
        .collect())
}

/// The payload message for something a person said, built the same way whether
/// it is being sent for the first time or replayed from its row.
///
/// This is the one place a persisted user message becomes a `ChatMessage`. The
/// live turn calls it with the instant it is about to write as `created_at`;
/// `push_history_message` calls it with the instant the row carries. Both must
/// produce identical bytes, because the first is what the provider cached and
/// the second is what it is asked to match — so the two are not allowed to be
/// two code paths.
///
/// Dictated messages carry a marker the voice_input prompt block explains.
/// Applied to the payload only — the stored row and the UI keep the clean
/// transcript.
pub fn persisted_user_message(
    content: &str,
    source: Option<&str>,
    sender: Option<SenderRef>,
    created_at: i64,
) -> ChatMessage {
    if source == Some("voice") {
        ChatMessage::persisted_user(&format!("[voice] {content}"), created_at, sender)
    } else {
        ChatMessage::persisted_user(content, created_at, sender)
    }
}

fn push_history_message(
    msgs: &mut Vec<ChatMessage>,
    m: &message_entity::Model,
    names: &SenderNames,
    context_items: Option<&[MessageContextItemRow]>,
) -> Result<(), String> {
    use crate::db::models::message::MessageRole;

    let role = MessageRole::parse(&m.role).map_err(|error| format!("message {}: {error}", m.id))?;
    match role {
        MessageRole::User => msgs.push(persisted_user_message(
            &m.content,
            m.source.as_deref(),
            m.sender_id.map(|uid| sender_ref(uid, names)),
            m.created_at,
        )),
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
                    sent_at: None,
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

/// Records on every sticker part what the sticker was known as right now, so
/// that every later request renders this message the same way (CLAUDE.md,
/// "What was sent is never dropped or rewritten"). Called once, where a user
/// message enters the backend — the desktop's send, OneBot's inbound handler —
/// before it is either rendered or stored, so the live message and its row
/// carry the same bytes. A client-supplied `seen_as` is overwritten: it is the
/// backend's record, not something a sender gets to claim.
///
/// Content without a sticker comes back as the very string it was.
///
/// A sticker labelled later does not reach messages already sent. If the model
/// should hear about it, that is a notice appended at the end, the way memory
/// and the checklist report a change. Anthropic has a native form for a notice
/// that only matters for one turn — a `role: "system"` entry in `messages`
/// with `clear_at: "next_user_message"` (beta header
/// `mid-conversation-system-clear-at-2026-08-21`), which stays in place, is
/// cleared after the next user message and keeps later thinking valid. Not
/// used: it is a beta that may still change, and only one provider has it.
pub async fn freeze_sticker_parts(db: &crate::db::sea::cap::Db, content: &str) -> Result<String, String> {
    let Some(mut parts) = provider::decode_message_parts(content)? else {
        return Ok(content.to_string());
    };
    if !parts
        .iter()
        .any(|part| matches!(part, provider::MessageContentPart::Sticker { .. }))
    {
        return Ok(content.to_string());
    }
    for part in parts.iter_mut() {
        let provider::MessageContentPart::Sticker {
            sticker_id, seen_as, ..
        } = part
        else {
            continue;
        };
        let sticker = crate::db::sea::ops::emoji::get_emoji(db, sticker_id)
            .await
            .map_err(|e| format!("could not read sticker `{sticker_id}`: {e}"))?
            .ok_or_else(|| format!("sticker `{sticker_id}` does not exist"))?;
        *seen_as = Some(
            if sticker.semantic_status == crate::db::entity::emoji::EmojiSemanticStatus::Confirmed {
                let tags = sticker.tags.as_deref().filter(|tags| !tags.trim().is_empty());
                provider::StickerSeenAs::Described {
                    text: match tags {
                        Some(tags) => format!("[sticker: {}; tags: {}]", sticker.name, tags),
                        None => format!("[sticker: {}]", sticker.name),
                    },
                }
            } else {
                provider::StickerSeenAs::Unlabelled
            },
        );
    }
    provider::encode_message_parts(&parts)
}

/// Converts Meridian's transcript-only sticker part into provider-supported
/// text/image parts, from what `freeze_sticker_parts` recorded and nothing
/// else. A confirmed sticker is its frozen text. An unlabelled one is shown as
/// its picture — on every request, not only while it is the newest message:
/// the history has to replay byte for byte, so the pixels the model saw once
/// stay in front of it (they are cached after the first time).
///
/// The output depends on `supports_images`, which is fixed for a model; a
/// preview that cannot be produced is an error, since leaving it out would
/// silently change what an earlier request said.
///
/// `db` is `None` for a runner with no services (tests); sticker parts are
/// then left as they are, and an adapter refuses them.
pub async fn resolve_sticker_parts_in_messages(
    messages: &mut [ChatMessage],
    db: Option<&crate::db::sea::cap::Db>,
    data_dir: Option<&std::path::Path>,
    supports_images: bool,
) -> Result<(), String> {
    let Some(db) = db else { return Ok(()) };
    for message in messages.iter_mut() {
        if !has_stored_user_content(message) {
            continue;
        }
        let Some(parts) = provider::decode_message_parts(&message.content)? else {
            continue;
        };
        let mut changed = false;
        let mut provider_parts = Vec::with_capacity(parts.len() + 1);
        for part in parts {
            let provider::MessageContentPart::Sticker {
                sticker_id, seen_as, ..
            } = part
            else {
                provider_parts.push(part);
                continue;
            };
            changed = true;
            match seen_as {
                None => return Err(format!("sticker part `{sticker_id}` was never frozen")),
                Some(provider::StickerSeenAs::Described { text }) => {
                    provider_parts.push(provider::MessageContentPart::Text { text });
                }
                Some(provider::StickerSeenAs::Unlabelled) => {
                    provider_parts.extend(unlabelled_sticker_parts(db, data_dir, supports_images, &sticker_id).await?);
                }
            }
        }
        if changed {
            message.content = provider::encode_message_parts(&provider_parts)?;
        }
    }
    Ok(())
}

async fn unlabelled_sticker_parts(
    db: &crate::db::sea::cap::Db,
    data_dir: Option<&std::path::Path>,
    supports_images: bool,
    sticker_id: &str,
) -> Result<Vec<provider::MessageContentPart>, String> {
    let placeholder = || {
        vec![provider::MessageContentPart::Text {
            text: "[unlabelled sticker]".into(),
        }]
    };
    let Some(data_dir) = data_dir.filter(|_| supports_images) else {
        return Ok(placeholder());
    };
    // `message_stickers` holds the row with `RESTRICT`, so a sticker a message
    // showed is still there.
    let sticker = crate::db::sea::ops::emoji::get_emoji(db, sticker_id)
        .await
        .map_err(|e| format!("could not read sticker `{sticker_id}`: {e}"))?
        .ok_or_else(|| format!("sticker `{sticker_id}` does not exist"))?;
    if sticker.file_name.is_empty() {
        return Ok(placeholder());
    }
    let path = crate::emoji::emoji_path(data_dir, &sticker.pack_id, &sticker.file_name);
    let data_uri = crate::emoji::vision_preview_data_uri(&path)
        .map_err(|e| format!("could not render sticker `{sticker_id}` for the model: {e}"))?;
    Ok(vec![
        provider::MessageContentPart::Text {
            text: "[unlabelled sticker attached; infer its visible reaction cautiously]".into(),
        },
        provider::MessageContentPart::ImageUrl {
            image_url: provider::MessageContentUrl { url: data_uri },
        },
    ])
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

    fn msg(id: &str, role: &str, content: &str) -> message_entity::Model {
        message_entity::Model {
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
            is_compact_summary: crate::db::types::SqlBool::FALSE,
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
            tool_diffs: None,
            response_model_id: None,
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
            sent_at: None,
        }
    }

    /// A linear conversation with nothing compacted — what these tests are about.
    fn ctx(history: &[message_entity::Model]) -> ActiveContext {
        ActiveContext {
            path: history
                .iter()
                .filter(|m| !m.is_compact_summary.get())
                .cloned()
                .collect(),
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
        // A history row carries a send time, so the note explaining the marker
        // rides along on the system prompt.
        assert!(
            msgs[0].content.starts_with("You are a helper\n\n"),
            "{}",
            msgs[0].content
        );
        assert!(msgs[0].content.ends_with(provider::MESSAGE_MARKER_NOTE));
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

    /// A user row carries its send time back out exactly as the live message
    /// carried it in, and renders to the same bytes. This is the property the
    /// `<sent_at>` marker rests on: the live message is what the provider
    /// cached, and the replay is what it is asked to match.
    #[test]
    fn a_user_row_is_replayed_byte_for_byte() {
        use provider::{SenderRendering, render_message};
        const T: i64 = 1_600_000_000_000;
        let names: SenderNames = [(7, "Seven".to_string())].into_iter().collect();

        for (sender_id, source, text) in [
            (None, None, "hello"),
            (Some(7), None, "hello"),
            (None, Some("voice"), "dictated"),
        ] {
            // What the turn that first sent it built.
            let fresh = persisted_user_message(text, source, sender_id.map(|uid| sender_ref(uid, &names)), T);
            // What the next turn reads back off the row.
            let mut row = msg("r", "user", text);
            row.created_at = T;
            row.sender_id = sender_id;
            row.source = source.map(str::to_owned);
            let mut replayed = Vec::new();
            push_history_message(&mut replayed, &row, &names, None).unwrap();

            assert_eq!(replayed.len(), 1);
            assert_eq!(replayed[0].sent_at, Some(T));
            for rendering in [SenderRendering::NameField, SenderRendering::Prefix] {
                assert_eq!(
                    render_message(&replayed[0], rendering).unwrap().content,
                    render_message(&fresh, rendering).unwrap().content,
                    "{sender_id:?} {source:?}",
                );
            }
        }
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

    async fn sticker_db() -> (crate::db::sea::cap::Db, tempfile::TempDir) {
        use crate::db::entity::emoji::EmojiSemanticStatus;
        use crate::db::entity::emoji_pack::EmojiPackKind;
        use crate::db::sea::ops::emoji::tests::{insert, sticker};
        use crate::db::sea::ops::emoji_pack::tests::{insert as insert_pack, pack};

        let db = crate::db::sea::sea_test_db().await;
        insert_pack(&db, pack("p1", EmojiPackKind::Manual, None, 0)).await;
        let make = |id: &str, name: &str, status| {
            let mut row = sticker(id, "p1", status);
            row.name = name.into();
            row.tags = Some("reaction".into());
            row.file_name = format!("{id}.jpg");
            row
        };
        insert(&db, make("known", "wave", EmojiSemanticStatus::Confirmed)).await;
        insert(&db, make("unknown", "pending-x", EmojiSemanticStatus::Pending)).await;
        let data_dir = tempfile::tempdir().unwrap();
        for id in ["known", "unknown"] {
            let path = crate::emoji::emoji_path(data_dir.path(), "p1", &format!("{id}.jpg"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"pixels").unwrap();
        }
        (db, data_dir)
    }

    /// The sticker message as the model is given it, with the rest of the
    /// conversation around it.
    async fn rendered_first(
        db: &crate::db::sea::cap::Db,
        data_dir: &std::path::Path,
        conversation: &[&str],
    ) -> Result<String, String> {
        let mut messages: Vec<ChatMessage> = conversation.iter().map(|c| ChatMessage::user(c)).collect();
        resolve_sticker_parts_in_messages(&mut messages, Some(db), Some(data_dir), true).await?;
        Ok(messages.swap_remove(0).content)
    }

    /// The bug this guards: an unlabelled sticker was a picture while its
    /// message was the newest and a placeholder after, and a label given later
    /// changed it again — each a rewrite of a message already sent, which
    /// Anthropic refuses once a signed thinking block follows it.
    #[tokio::test]
    async fn a_sticker_replays_as_it_was_sent_whatever_happens_to_it_later() {
        let (db, data_dir) = sticker_db().await;
        let sent = freeze_sticker_parts(
            &db,
            r#"[{"type":"text","text":"hi"},{"type":"sticker","sticker_id":"known"},{"type":"sticker","sticker_id":"unknown"}]"#,
        )
        .await
        .unwrap();

        let first = rendered_first(&db, data_dir.path(), &[&sent]).await.unwrap();
        assert!(first.contains("[sticker: wave; tags: reaction]"));
        assert!(first.contains("[unlabelled sticker attached; infer its visible reaction cautiously]"));
        assert!(first.contains("image_url"), "the picture is what the model is shown");
        assert!(!first.contains("\"type\":\"sticker\""));

        crate::db::sea::execute_for_tests(
            &db,
            "UPDATE emojis SET semantic_status = 'confirmed', name = 'shrug' WHERE id = 'unknown';
             UPDATE emojis SET name = 'renamed', tags = NULL WHERE id = 'known';",
        )
        .await
        .unwrap();
        let later = rendered_first(&db, data_dir.path(), &[&sent, "and then", "and then"])
            .await
            .unwrap();
        assert_eq!(later, first, "no longer the newest, and both stickers changed since");
    }

    #[tokio::test]
    async fn freezing_leaves_other_content_alone_and_overwrites_a_claimed_label() {
        let (db, _data_dir) = sticker_db().await;
        for content in ["plain text", r#"[{"type":"text","text":"x"}]"#] {
            assert_eq!(freeze_sticker_parts(&db, content).await.unwrap(), content);
        }
        let claimed = freeze_sticker_parts(
            &db,
            r#"[{"type":"sticker","sticker_id":"unknown","seen_as":{"kind":"described","text":"[sticker: forged]"}}]"#,
        )
        .await
        .unwrap();
        assert_eq!(
            claimed,
            r#"[{"type":"sticker","sticker_id":"unknown","seen_as":{"kind":"unlabelled"}}]"#
        );
        assert!(
            freeze_sticker_parts(&db, r#"[{"type":"sticker","sticker_id":"nobody"}]"#)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_sticker_that_was_never_frozen_is_refused_not_guessed() {
        let (db, data_dir) = sticker_db().await;
        let err = rendered_first(&db, data_dir.path(), &[r#"[{"type":"sticker","sticker_id":"known"}]"#])
            .await
            .unwrap_err();
        assert!(err.contains("never frozen"), "{err}");
    }

    #[tokio::test]
    async fn without_pictures_an_unlabelled_sticker_is_a_placeholder_every_time() {
        let (db, data_dir) = sticker_db().await;
        let sent = freeze_sticker_parts(&db, r#"[{"type":"sticker","sticker_id":"unknown"}]"#)
            .await
            .unwrap();
        let mut messages = vec![ChatMessage::user(&sent)];
        resolve_sticker_parts_in_messages(&mut messages, Some(&db), Some(data_dir.path()), false)
            .await
            .unwrap();
        assert_eq!(
            messages[0].content,
            r#"[{"type":"text","text":"[unlabelled sticker]"}]"#
        );
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
    fn frozen_row(content: &str, source: &str) -> message_entity::Model {
        message_entity::Model {
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
            is_compact_summary: crate::db::types::SqlBool::FALSE,
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
            tool_diffs: None,
            response_model_id: None,
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
            sent_at: None,
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

    /// The checklist is frozen the same way, under its own `source`, and the
    /// replay does not care which: every `context` row comes back as the bytes
    /// it went in as.
    #[test]
    fn a_frozen_todo_row_is_replayed_byte_for_byte() {
        let block = "<todo_list>\nTitle: Ship it\n1. [in_progress] step\n</todo_list>";
        let fresh = ChatMessage::system_context(block);
        let mut replayed = Vec::new();
        push_history_message(
            &mut replayed,
            &frozen_row(block, "todo|list"),
            &SenderNames::new(),
            None,
        )
        .unwrap();

        assert_eq!(replayed.len(), 1);
        assert!(matches!(replayed[0].origin, MessageOrigin::SystemContext));
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
        let history = [crate::db::entity::message::Model {
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
            is_compact_summary: crate::db::types::SqlBool::TRUE,
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
            tool_diffs: None,
            response_model_id: None,
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

//! OneBot notice event handling: pokes trigger an AI turn, recalls and group
//! membership changes become inbox notes the model sees on its next turn
//! (mid-turn injected when one is running) — existing history is never mutated.

use std::sync::Arc;

use super::format;
use super::protocol::{OneBotAction, OneBotEvent};
use super::session::{SessionKey, SessionKind};
use super::{SharedState, call_api, handler, push_notice_note, was_seen_message};
use crate::util::now_ms;

const POKE_COOLDOWN_MS: i64 = 30_000;
const RECALL_SUMMARY_CHARS: usize = 50;

pub async fn handle_notice(event: &OneBotEvent, state: &Arc<SharedState>, conn_id: u64) -> Vec<OneBotAction> {
    match event.notice_type.as_deref() {
        Some("notify") if event.sub_type.as_deref() == Some("poke") => handle_poke(event, state, conn_id).await,
        Some("group_recall") | Some("friend_recall") => {
            handle_recall(event, state).await;
            vec![]
        }
        Some("group_increase") | Some("group_decrease") => {
            handle_membership(event, state).await;
            vec![]
        }
        other => {
            tracing::debug!("Unhandled OneBot notice type: {other:?}");
            vec![]
        }
    }
}

async fn handle_poke(event: &OneBotEvent, state: &Arc<SharedState>, conn_id: u64) -> Vec<OneBotAction> {
    let self_id = event.self_id.unwrap_or(0);
    let user_id = event.user_id.unwrap_or(0);
    // Only react when the bot itself got poked by someone else.
    if event.target_id != Some(self_id) || user_id == self_id || user_id == 0 {
        return vec![];
    }

    let session_key = match event.group_id {
        Some(gid) => SessionKey::group(gid),
        None => SessionKey::private(user_id),
    };

    // Cooldown is checked and refreshed under the lock so concurrent pokes
    // can't both pass.
    {
        let now = now_ms();
        let mut states = state.session_states.lock();
        let s = states.entry(session_key.to_string()).or_default();
        if now - s.last_poke_reply_ms < POKE_COOLDOWN_MS {
            return vec![];
        }
        s.last_poke_reply_ms = now;
    }

    let nickname = lookup_nickname(state, &session_key, user_id).await;
    let display = match nickname {
        Some(ref n) => format!("{n}({user_id})"),
        None => user_id.to_string(),
    };
    let title = match session_key.kind {
        SessionKind::Private => format!("[QQ] {}", nickname.as_deref().unwrap_or("Unknown")),
        SessionKind::Group => format!("[QQ] 群{}", session_key.id),
    };
    let content = format!("[系统提示] {display} 戳了戳你");

    // A poke is an interaction by a real person, so it carries their identity
    // and refreshes their memory clock like any other message.
    let sender = super::SenderContext {
        user_id,
        nickname: nickname.clone(),
        // A poke event carries no sender object, so neither is known here.
        role: None,
        title: None,
        is_admin: state.config.admin_users.contains(&user_id),
        is_group: session_key.kind == SessionKind::Group,
    };

    handler::run_agent_turn(
        state,
        conn_id,
        event.self_id,
        &session_key,
        &title,
        sender,
        content,
        None,
    )
    .await
}

async fn handle_recall(event: &OneBotEvent, state: &Arc<SharedState>) {
    let self_id = event.self_id.unwrap_or(0);
    let user_id = event.user_id.unwrap_or(0);
    let Some(message_id) = event.message_id else { return };
    // The bot recalling its own message needs no note.
    if user_id == self_id {
        return;
    }

    let session_key = match event.notice_type.as_deref() {
        Some("group_recall") => match event.group_id {
            Some(gid) => SessionKey::group(gid),
            None => return,
        },
        _ => SessionKey::private(user_id),
    };

    // Only report recalls of messages the model actually saw; anything else
    // would be noise (and would hand the model content its author retracted).
    if !was_seen_message(&state.session_states, &session_key, message_id) {
        return;
    }

    let (sender_name, summary) = fetch_recalled_summary(state, message_id).await;
    let who = match sender_name {
        Some(n) => format!("{n}({user_id})"),
        None => user_id.to_string(),
    };
    let by_operator = event
        .operator_id
        .filter(|op| *op != user_id && *op != 0)
        .map(|op| format!("(由 {op} 撤回)"))
        .unwrap_or_default();
    let text = match summary {
        Some(s) => format!("[系统提示] {who} 撤回了消息{by_operator}:\"{s}\""),
        None => format!("[系统提示] {who} 撤回了一条消息{by_operator}"),
    };
    push_notice_note(&state.session_states, &session_key, text);
}

async fn handle_membership(event: &OneBotEvent, state: &Arc<SharedState>) {
    let self_id = event.self_id.unwrap_or(0);
    let user_id = event.user_id.unwrap_or(0);
    let Some(group_id) = event.group_id else { return };
    // Bot joining/leaving: the session context doesn't need a note about itself.
    if user_id == self_id || user_id == 0 {
        return;
    }

    let text = match event.notice_type.as_deref() {
        Some("group_increase") => format!("[系统提示] {user_id} 加入了群聊"),
        Some("group_decrease") => match event.sub_type.as_deref() {
            Some("kick") => format!("[系统提示] {user_id} 被移出了群聊"),
            _ => format!("[系统提示] {user_id} 退出了群聊"),
        },
        _ => return,
    };
    push_notice_note(&state.session_states, &SessionKey::group(group_id), text);
}

/// Best-effort nickname lookup; falls back to `None` (caller shows the QQ id).
async fn lookup_nickname(state: &Arc<SharedState>, session: &SessionKey, user_id: i64) -> Option<String> {
    let echo = uuid::Uuid::new_v4().to_string();
    let action = match session.kind {
        SessionKind::Group => OneBotAction::get_group_member_info(session.id, user_id, echo),
        SessionKind::Private => OneBotAction::get_stranger_info(user_id, echo),
    };
    let data = call_api(state, action).await.ok()?;
    data.get("card")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| data.get("nickname").and_then(|v| v.as_str()).filter(|s| !s.is_empty()))
        .map(String::from)
}

/// Recover sender name + a short content summary of a recalled message via
/// `get_msg` (llbot serves recalled messages from its local cache).
async fn fetch_recalled_summary(state: &Arc<SharedState>, message_id: i64) -> (Option<String>, Option<String>) {
    let echo = uuid::Uuid::new_v4().to_string();
    let Ok(data) = call_api(state, OneBotAction::get_msg(message_id, echo)).await else {
        return (None, None);
    };
    let sender = data.get("sender").and_then(|s| {
        s.get("card")
            .and_then(|v| v.as_str())
            .filter(|c| !c.is_empty())
            .or_else(|| s.get("nickname").and_then(|v| v.as_str()))
            .map(String::from)
    });
    let summary = data
        .get("message")
        .map(|m| format::segments_to_text(m, None))
        .filter(|s| !s.is_empty())
        .map(|s| {
            let truncated: String = s.chars().take(RECALL_SUMMARY_CHARS).collect();
            if truncated.chars().count() < s.chars().count() {
                format!("{truncated}…")
            } else {
                truncated
            }
        });
    (sender, summary)
}

//! The checklist, frozen into the history the way memory is.
//!
//! `<todo_list>` used to sit at the end of the system prompt. `update_todos`
//! rewrites it several times a turn, and the system prompt is the front of what
//! a provider caches — so every turn after a change missed the cache from the
//! system block on, which is to say entirely. The block is a `role="context"`
//! row now: written ahead of the user message when, and only when, it differs
//! from the most recent frozen one on the live path, and replayed from there
//! byte for byte like the memory row beside it (`push_history_message`).
//!
//! One rule covers compaction and trimming: **no frozen row on the live path
//! means write the full block.** Compaction's `live()` starts at the anchor and
//! a row before it is simply absent; trimming lifts context rows out and puts
//! them back, so the latest one is still found. Within a turn the model has the
//! results of its own `update_todos` calls and needs no new row; the next turn
//! reconciles.
//!
//! A conversation that never had a checklist writes nothing — a QQ group, a
//! sub-agent. One that had a list and has since finished it writes the cleared
//! marker once, so the model stops acting on a list the last frozen row still
//! shows as open.

use crate::db::entity::message as message_entity;
use crate::db::sea::DbErr;
use crate::db::sea::cap::Db;

const SOURCE_TAG: &str = "todo";

/// What a frozen checklist row says: a list, or that there is none any more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoKind {
    List,
    None,
}

/// Sent once when a frozen non-empty list has since been emptied. Never empty
/// (Kimi refuses empty text) and never changing (it is replayed).
pub const TODO_CLEARED_MARKER: &str = "<todo_list>\nNo checklist is in progress.\n</todo_list>";

/// A checklist block to send this turn and freeze into the history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoInjection {
    pub text: String,
    pub kind: TodoKind,
}

impl TodoInjection {
    /// `todo|list` or `todo|none`: what the row's `source` says it is.
    pub fn source(&self) -> String {
        let kind = match self.kind {
            TodoKind::List => "list",
            TodoKind::None => "none",
        };
        format!("{SOURCE_TAG}|{kind}")
    }
}

/// `Ok(None)` for a source that is not ours — memory rows live beside these and
/// have their own parser — and an error for one that claims to be ours and is
/// malformed. A row this module cannot read is a row it must not silently skip:
/// skipping it would re-send a block the model already has.
fn parse_source(source: &str) -> Result<Option<TodoKind>, String> {
    let mut parts = source.split('|');
    if parts.next() != Some(SOURCE_TAG) {
        return Ok(None);
    }
    match (parts.next(), parts.next()) {
        (Some("list"), None) => Ok(Some(TodoKind::List)),
        (Some("none"), None) => Ok(Some(TodoKind::None)),
        _ => Err(format!("unrecognised checklist context source '{source}'")),
    }
}

/// The most recent frozen checklist on the live path, if there is one.
fn prior(live: &[message_entity::Model]) -> Result<Option<(TodoKind, &str)>, String> {
    for row in live.iter().rev() {
        if row.role != "context" {
            continue;
        }
        let Some(source) = row.source.as_deref() else {
            continue;
        };
        if let Some(kind) = parse_source(source)? {
            return Ok(Some((kind, row.content.as_str())));
        }
    }
    Ok(None)
}

/// What to freeze, given the live path and the checklist as it renders now.
fn decide(
    conversation_id: &str,
    live: &[message_entity::Model],
    rendered: Result<Option<String>, String>,
) -> Result<Option<TodoInjection>, String> {
    let rendered = match rendered {
        Ok(rendered) => rendered,
        // Not "empty": an empty answer would write the cleared marker and tell
        // the model the checklist is gone when only the read failed.
        Err(e) => {
            tracing::warn!(
                conversation_id = %conversation_id, block = "todo", error = %e,
                "could not read the todo list; it will not be refreshed this turn"
            );
            return Ok(None);
        }
    };
    Ok(match (prior(live)?, rendered) {
        (None, Some(text)) => Some(TodoInjection {
            text,
            kind: TodoKind::List,
        }),
        (None, None) => None,
        (Some((_, before)), Some(text)) => (before != text).then_some(TodoInjection {
            text,
            kind: TodoKind::List,
        }),
        (Some((TodoKind::None, _)), None) => None,
        (Some((TodoKind::List, _)), None) => Some(TodoInjection {
            text: TODO_CLEARED_MARKER.to_string(),
            kind: TodoKind::None,
        }),
    })
}

/// Decide what, if anything, to send and freeze this turn. Reads only: the
/// list and its items in one snapshot.
///
/// The block is compared against the frozen one as *bytes*, trimmed the way
/// it is sent, so "unchanged" is exactly "the model already has this".
pub async fn plan_todo_injection(
    db: &Db,
    conversation_id: &str,
    live: &[message_entity::Model],
) -> Result<Option<TodoInjection>, String> {
    let rendered = db
        .read(async |tx| crate::db::sea::ops::todo::get_active_view(tx, conversation_id).await)
        .await
        .map(|view| {
            view.as_ref()
                .and_then(crate::db::sea::ops::todo::format_todo_block)
                .map(|block| block.trim_start().to_string())
        })
        .map_err(|e: DbErr| e.to_string());
    decide(conversation_id, live, rendered)
}

/// Freeze this turn's checklist block into the history, after the memory row and
/// ahead of the user message, and answer with the row the next write should
/// hang off. Same writer, same failure policy as the memory row.
pub async fn persist_todo_injection(
    db: &Db,
    injection: &TodoInjection,
    conversation_id: &str,
    turn_id: &str,
    parent: Option<String>,
    now: i64,
) -> Option<String> {
    super::memory_context::persist_context_row(
        db,
        injection.text.clone(),
        injection.source(),
        "checklist",
        conversation_id,
        turn_id,
        parent,
        now,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::todo::ItemStatus;
    use crate::db::sea::ops::todo::TodoItemSpec;

    async fn seeded() -> Db {
        let db = crate::db::sea::sea_test_db().await;
        db.write(async |tx| {
            crate::db::sea::ops::conversation::create_conversation(tx, "c1", Some("t"), None, None, 1).await?;
            crate::db::sea::ops::turn::begin(tx, "t1", "c1", crate::turn::TurnOrigin::Desktop, None, 1000).await
        })
        .await
        .unwrap();
        db
    }

    async fn replace_active_list(db: &Db, title: &str, items: &[TodoItemSpec], now: i64) {
        db.write(async |tx| crate::db::sea::ops::todo::replace_active_list(tx, "c1", title, items, now).await)
            .await
            .unwrap();
    }

    fn step(content: &str, status: ItemStatus) -> TodoItemSpec {
        TodoItemSpec {
            content: content.into(),
            active_form: format!("{content}ing"),
            status,
        }
    }

    /// A frozen checklist row, as `persist_todo_injection` would have left it.
    fn frozen(injection: &TodoInjection) -> message_entity::Model {
        message_entity::Model {
            id: uuid::Uuid::new_v4().to_string(),
            conversation_id: "c1".into(),
            role: "context".into(),
            content: injection.text.clone(),
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
            source: Some(injection.source()),
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

    /// Run a turn start the way a surface would: plan against the rows frozen
    /// so far, then append this turn's row to them.
    async fn round(db: &Db, history: &mut Vec<message_entity::Model>) -> Option<TodoInjection> {
        let injection = plan_todo_injection(db, "c1", history).await.unwrap();
        if let Some(injection) = &injection {
            history.push(frozen(injection));
        }
        injection
    }

    /// The QQ group and the sub-agent: no list, nothing frozen, nothing to say.
    #[tokio::test]
    async fn a_conversation_without_a_checklist_writes_nothing() {
        let db = seeded().await;
        assert_eq!(round(&db, &mut Vec::new()).await, None);
    }

    #[tokio::test]
    async fn a_new_checklist_is_frozen_in_full() {
        let db = seeded().await;
        replace_active_list(&db, "Ship it", &[step("step", ItemStatus::Pending)], 10).await;

        let first = round(&db, &mut Vec::new())
            .await
            .expect("a list nobody has seen is sent");
        assert_eq!(first.kind, TodoKind::List);
        assert!(
            first.text.starts_with("<todo_list>\nTitle: Ship it\n"),
            "{}",
            first.text
        );
        assert!(!first.text.starts_with('\n'), "stored trimmed, the way it is sent");
    }

    /// Nothing changed, so nothing is sent — the row from the earlier turn is
    /// still in the history and the model can still read it.
    #[tokio::test]
    async fn an_unchanged_checklist_injects_nothing() {
        let db = seeded().await;
        replace_active_list(&db, "Ship it", &[step("step", ItemStatus::Pending)], 10).await;
        let mut history = Vec::new();

        assert!(round(&db, &mut history).await.is_some());
        assert_eq!(round(&db, &mut history).await, None);
        assert_eq!(round(&db, &mut history).await, None);
    }

    /// A tick between turns writes one new row, hung where the message will hang
    /// off it; and once it is on the path, the next turn has nothing to add.
    #[tokio::test]
    async fn a_changed_checklist_writes_exactly_one_row_before_the_message() {
        use crate::db::sea::ops::{conversation as conversation_ops, message as message_ops, todo as todo_ops};

        let db = crate::db::sea::sea_test_db().await;
        db.write(async |tx| {
            conversation_ops::create_conversation(tx, "c1", Some("t"), None, None, 1).await?;
            crate::db::sea::ops::turn::begin(tx, "t1", "c1", crate::turn::TurnOrigin::Desktop, None, 1000).await
        })
        .await
        .unwrap();
        async fn live(db: &Db) -> Vec<message_entity::Model> {
            let conv = conversation_ops::get_conversation(db, "c1").await.unwrap().unwrap();
            let history = message_ops::list_messages(db, "c1").await.unwrap();
            message_ops::active_context(&history, conv.head_message_id.as_deref())
                .live()
                .to_vec()
        }
        async fn user_row(db: &Db, id: &str, parent: Option<&str>, now: i64) {
            let row = message_entity::Model {
                turn_id: Some("t1".into()),
                ..message_ops::new_row(id, "c1", "user", "go on", now)
            };
            db.write(async |tx| message_ops::append_message(tx, row, parent).await)
                .await
                .unwrap();
        }
        async fn tick(db: &Db, status: ItemStatus, now: i64) {
            let items = [todo_ops::TodoItemSpec {
                content: "step".into(),
                active_form: "stepping".into(),
                status,
            }];
            db.write(async |tx| todo_ops::replace_active_list(tx, "c1", "Ship it", &items, now).await)
                .await
                .unwrap();
        }

        tick(&db, ItemStatus::Pending, 10).await;
        let first = plan_todo_injection(&db, "c1", &live(&db).await).await.unwrap().unwrap();
        let first_row = persist_todo_injection(&db, &first, "c1", "t1", None, 100)
            .await
            .expect("the row is written and its id handed back");
        user_row(&db, "u1", Some(&first_row), 101).await;

        tick(&db, ItemStatus::InProgress, 200).await;
        let second = plan_todo_injection(&db, "c1", &live(&db).await)
            .await
            .unwrap()
            .expect("a ticked step is a changed list");
        assert_ne!(second.text, first.text);
        let second_row = persist_todo_injection(&db, &second, "c1", "t1", Some("u1".into()), 300)
            .await
            .unwrap();
        user_row(&db, "u2", Some(&second_row), 301).await;

        let path = live(&db).await;
        let todo_rows: Vec<&message_entity::Model> = path
            .iter()
            .filter(|r| r.role == "context" && r.source.as_deref().is_some_and(|s| s.starts_with("todo|")))
            .collect();
        assert_eq!(todo_rows.len(), 2, "one row per change, no more");
        assert_eq!(todo_rows[1].id, second_row);
        let u2 = path.iter().find(|r| r.id == "u2").unwrap();
        assert_eq!(
            u2.parent_id.as_deref(),
            Some(second_row.as_str()),
            "the message hangs off the new row"
        );

        assert_eq!(plan_todo_injection(&db, "c1", &path).await.unwrap(), None);
    }

    /// Finishing the last step archives the list. The model's newest frozen row
    /// still shows it open, so it is told once that nothing is in progress — and
    /// only once.
    #[tokio::test]
    async fn an_emptied_checklist_writes_the_cleared_marker_once() {
        let db = seeded().await;
        replace_active_list(&db, "Ship it", &[step("step", ItemStatus::InProgress)], 10).await;
        let mut history = Vec::new();
        assert_eq!(round(&db, &mut history).await.map(|i| i.kind), Some(TodoKind::List));

        replace_active_list(&db, "Ship it", &[step("step", ItemStatus::Completed)], 20).await;
        assert!(
            db.read(async |tx| crate::db::sea::ops::todo::get_active_view(tx, "c1").await)
                .await
                .unwrap()
                .is_none(),
            "a finished list is archived"
        );
        let cleared = round(&db, &mut history)
            .await
            .expect("the model is told the list is gone");
        assert_eq!(cleared.kind, TodoKind::None);
        assert_eq!(cleared.text, TODO_CLEARED_MARKER);
        assert_eq!(cleared.source(), "todo|none");

        assert_eq!(round(&db, &mut history).await, None, "and told once");

        // A new list after that is a list nobody has seen.
        replace_active_list(&db, "Again", &[step("more", ItemStatus::Pending)], 30).await;
        assert_eq!(round(&db, &mut history).await.map(|i| i.kind), Some(TodoKind::List));
    }

    /// Compaction leaves `live()` starting at its anchor; a frozen row before the
    /// anchor is simply not there, and the rule says: write it all again.
    #[tokio::test]
    async fn a_history_cut_before_the_todo_row_starts_over() {
        let db = seeded().await;
        replace_active_list(&db, "Ship it", &[step("step", ItemStatus::Pending)], 10).await;
        let mut history = Vec::new();
        let first = round(&db, &mut history).await.unwrap();

        assert_eq!(plan_todo_injection(&db, "c1", &history).await.unwrap(), None);
        let after_cut = plan_todo_injection(&db, "c1", &history[1..]).await.unwrap();
        assert_eq!(after_cut, Some(first), "the same full block, as if never sent");
    }

    #[test]
    fn todo_source_contract_is_exact() {
        assert_eq!(parse_source("todo|list").unwrap(), Some(TodoKind::List));
        assert_eq!(parse_source("todo|none").unwrap(), Some(TodoKind::None));
        assert_eq!(
            parse_source("memory|full|-|-|").unwrap(),
            None,
            "memory rows are not ours"
        );
        assert!(parse_source("todo").is_err());
        assert!(parse_source("todo|later").is_err());
        assert!(parse_source("todo|list|extra").is_err());
    }
}

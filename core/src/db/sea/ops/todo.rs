//! Reading and writing `todo_lists` and `todo_items`.
//!
//! No function here opens a transaction of its own: a write takes the caller's
//! `WriteTx`, and the caller's `Db::write` is the `BEGIN IMMEDIATE`.

use sea_orm::ActiveValue::{NotSet, Set, Unchanged};
use sea_orm::{ActiveModelTrait, ColumnTrait, DbErr, EntityTrait, QueryFilter, QueryOrder};

use crate::db::entity::todo_item::ItemStatus;
use crate::db::entity::todo_list::ListStatus;
use crate::db::entity::{todo_item, todo_list};
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, Snapshot, WriteTx};
use crate::db::sea::ops::plan;
use crate::db::types::EpochMs;

/// One step as the model supplied it, before it gets an id and a position.
pub struct TodoItemSpec {
    pub content: String,
    pub active_form: String,
    pub status: ItemStatus,
}

/// A checklist and its steps, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoListView {
    pub list: todo_list::Model,
    pub items: Vec<todo_item::Model>,
}

pub async fn get_active_list(db: &impl Read, conversation_id: &str) -> Result<Option<todo_list::Model>, DbErr> {
    todo_list::Entity::find()
        .filter(todo_list::Column::ConversationId.eq(conversation_id))
        .filter(todo_list::Column::Status.eq(ListStatus::InProgress))
        .one(db.conn()?)
        .await
}

pub async fn list_items(db: &impl Read, list_id: &str) -> Result<Vec<todo_item::Model>, DbErr> {
    todo_item::Entity::find()
        .filter(todo_item::Column::ListId.eq(list_id))
        .order_by_asc(todo_item::Column::SortOrder)
        .all(db.conn()?)
        .await
}

/// The active list plus its items, or `None` once every step is done and the
/// list has been archived. Two statements, so it takes a snapshot.
pub async fn get_active_view(db: &impl Snapshot, conversation_id: &str) -> Result<Option<TodoListView>, DbErr> {
    let Some(list) = get_active_list(db, conversation_id).await? else {
        return Ok(None);
    };
    let items = list_items(db, &list.id).await?;
    Ok(Some(TodoListView { list, items }))
}

/// Every list of a conversation, oldest first.
#[cfg(test)]
pub async fn list_lists(db: &impl Read, conversation_id: &str) -> Result<Vec<todo_list::Model>, DbErr> {
    todo_list::Entity::find()
        .filter(todo_list::Column::ConversationId.eq(conversation_id))
        .order_by_asc(todo_list::Column::CreatedAt)
        .all(db.conn()?)
        .await
}

/// Write the checklist the model just sent.
///
/// Items are replaced wholesale rather than diffed: the tool contract is
/// "here is the list as it now stands", so reconciling identities would invent
/// a distinction the model never made. A different `title` retires the running
/// list and opens a new one, which is how a conversation ends up holding
/// several. A list whose steps are all done retires itself, so the next call
/// starts fresh.
pub async fn replace_active_list(
    tx: &WriteTx,
    conversation_id: &str,
    title: &str,
    items: &[TodoItemSpec],
    now: EpochMs,
) -> Result<TodoListView, DbErr> {
    replace_active_list_with_plan_completion(tx, conversation_id, title, items, now)
        .await
        .map(|(view, _)| view)
}

/// The same checklist write plus how many plans it retired. Both happen in
/// the caller's write, so a crash after the last step is archived cannot
/// leave the just-finished plan in force.
pub async fn replace_active_list_with_plan_completion(
    tx: &WriteTx,
    conversation_id: &str,
    title: &str,
    items: &[TodoItemSpec],
    now: EpochMs,
) -> Result<(TodoListView, u64), DbErr> {
    let list_id = match get_active_list(tx, conversation_id).await? {
        Some(list) if list.title == title => {
            todo_list::ActiveModel {
                id: Unchanged(list.id.clone()),
                updated_at: Set(now),
                ..Default::default()
            }
            .update(tx.conn()?)
            .await?;
            list.id
        }
        other => {
            if let Some(stale) = other {
                archive(tx, &stale.id, now).await?;
            }
            let id = uuid::Uuid::new_v4().to_string();
            todo_list::Entity::insert(todo_list::ActiveModel {
                id: Set(id.clone()),
                conversation_id: Set(conversation_id.to_owned()),
                title: Set(title.to_owned()),
                status: Set(ListStatus::InProgress),
                created_at: Set(now),
                updated_at: Set(now),
            })
            .exec_without_returning(tx.conn()?)
            .await?;
            id
        }
    };

    todo_item::Entity::delete_many()
        .filter(todo_item::Column::ListId.eq(&list_id))
        .exec(tx.conn()?)
        .await?;
    if !items.is_empty() {
        todo_item::Entity::insert_many(items.iter().enumerate().map(|(idx, item)| todo_item::ActiveModel {
            id: Set(uuid::Uuid::new_v4().to_string()),
            list_id: Set(list_id.clone()),
            content: Set(item.content.clone()),
            active_form: Set(item.active_form.clone()),
            status: Set(item.status),
            sort_order: Set(idx as i32),
            created_at: Set(now),
        }))
        .exec_without_returning(tx.conn()?)
        .await?;
    }

    let retired_plans = if items.iter().all(|i| i.status == ItemStatus::Completed) {
        archive(tx, &list_id, now).await?;
        plan::complete_active(tx, conversation_id, now).await?
    } else {
        0
    };

    let list = todo_list::Entity::find_by_id(&list_id)
        .one(tx.conn()?)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound(format!("todo list `{list_id}`")))?;
    let items = list_items(tx, &list_id).await?;
    Ok((TodoListView { list, items }, retired_plans))
}

async fn archive(tx: &WriteTx, list_id: &str, now: EpochMs) -> Result<(), DbErr> {
    todo_list::ActiveModel {
        id: Unchanged(list_id.to_owned()),
        status: Set(ListStatus::Completed),
        updated_at: Set(now),
        conversation_id: NotSet,
        title: NotSet,
        created_at: NotSet,
    }
    .update(tx.conn()?)
    .await?;
    Ok(())
}

/// Render a checklist the way `agent::todo_context` freezes it into the
/// history: `None` for no steps, a leading blank line built in. The bytes are
/// cached by the provider, so the Diesel `format_todo_block` renders through
/// this one too.
pub fn render_todo_block<'a>(title: &str, steps: impl IntoIterator<Item = (&'a str, &'a str)>) -> Option<String> {
    let mut steps = steps.into_iter().peekable();
    steps.peek()?;
    let mut block = String::from("\n\n<todo_list>\n");
    block.push_str(&format!("Title: {title}\n"));
    for (idx, (status, content)) in steps.enumerate() {
        block.push_str(&format!("{}. [{status}] {content}\n", idx + 1));
    }
    block.push_str("</todo_list>");
    Some(block)
}

pub fn format_todo_block(view: &TodoListView) -> Option<String> {
    render_todo_block(
        &view.list.title,
        view.items
            .iter()
            .map(|item| (item.status.as_str(), item.content.as_str())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::entity::plan_document;
    use crate::db::entity::plan_document::PlanDocumentState;
    use crate::db::sea::cap::Db;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    async fn conversation() -> Db {
        let db = sea_test_db().await;
        execute_for_tests(
            &db,
            "INSERT INTO conversations (id, created_at, updated_at) VALUES ('c1', 1, 1)",
        )
        .await
        .unwrap();
        db
    }

    fn input(content: &str, status: ItemStatus) -> TodoItemSpec {
        TodoItemSpec {
            content: content.to_string(),
            active_form: format!("Doing {content}"),
            status,
        }
    }

    async fn replace(db: &Db, title: &str, items: Vec<TodoItemSpec>, now: EpochMs) -> TodoListView {
        db.write(async |tx| replace_active_list(tx, "c1", title, &items, now).await)
            .await
            .unwrap()
    }

    async fn active(db: &Db) -> Option<TodoListView> {
        db.read(async |tx| get_active_view(tx, "c1").await).await.unwrap()
    }

    #[tokio::test]
    async fn a_fresh_conversation_has_no_list() {
        let db = conversation().await;
        assert_eq!(active(&db).await, None);
        assert!(list_lists(&db, "c1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn first_call_opens_a_list() {
        let db = conversation().await;
        let view = replace(
            &db,
            "Refactor auth",
            vec![
                input("Extract token check", ItemStatus::InProgress),
                input("Add tests", ItemStatus::Pending),
            ],
            10,
        )
        .await;

        assert_eq!(view.list.title, "Refactor auth");
        assert_eq!(view.list.status, ListStatus::InProgress);
        assert_eq!(view.items.len(), 2);
        assert_eq!((view.items[0].sort_order, view.items[1].sort_order), (0, 1));
        assert_eq!(view.items[0].active_form, "Doing Extract token check");
        assert_eq!(active(&db).await, Some(view));
    }

    #[tokio::test]
    async fn same_title_replaces_items_in_place() {
        let db = conversation().await;
        let first = replace(
            &db,
            "Refactor auth",
            vec![input("a", ItemStatus::InProgress), input("b", ItemStatus::Pending)],
            10,
        )
        .await;
        let second = replace(
            &db,
            "Refactor auth",
            vec![
                input("a", ItemStatus::Completed),
                input("b", ItemStatus::InProgress),
                input("c", ItemStatus::Pending),
            ],
            20,
        )
        .await;

        assert_eq!(first.list.id, second.list.id);
        assert_eq!(second.list.updated_at, 20);
        assert_eq!(second.items.len(), 3);
        assert_eq!(list_lists(&db, "c1").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn new_title_archives_the_previous_list() {
        let db = conversation().await;
        let first = replace(&db, "Phase one", vec![input("a", ItemStatus::InProgress)], 10).await;
        let second = replace(&db, "Phase two", vec![input("b", ItemStatus::InProgress)], 20).await;

        assert_ne!(first.list.id, second.list.id);
        let lists = list_lists(&db, "c1").await.unwrap();
        let statuses: Vec<_> = lists.iter().map(|l| l.status).collect();
        assert_eq!(statuses, [ListStatus::Completed, ListStatus::InProgress]);
        // The archived list keeps its own items rather than losing them to the successor.
        assert_eq!(list_items(&db, &first.list.id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn finishing_every_step_archives_the_list() {
        let db = conversation().await;
        let view = replace(
            &db,
            "Refactor auth",
            vec![input("a", ItemStatus::Completed), input("b", ItemStatus::Completed)],
            10,
        )
        .await;

        assert_eq!(view.list.status, ListStatus::Completed);
        assert_eq!(active(&db).await, None);
    }

    /// Finishing a checklist retires an approved plan — the versioned document
    /// and the legacy artifact both — and leaves a document still in review
    /// alone.
    #[tokio::test]
    async fn checklist_completion_retires_only_an_approved_plan() {
        let db = conversation().await;
        execute_for_tests(
            &db,
            "INSERT INTO plan_documents (id, conversation_id, state, file_rel_path, created_at, updated_at)
                 VALUES ('d1', 'c1', 'reviewing', 'plan.md', 1, 1);
             INSERT INTO mode_artifacts (id, conversation_id, kind, content, status, created_at, updated_at)
                 VALUES ('legacy', 'c1', 'plan', '# Legacy', 'approved', 1, 1)",
        )
        .await
        .unwrap();
        let document = async || {
            plan_document::Entity::find_by_id("d1")
                .one(db.conn().unwrap())
                .await
                .unwrap()
                .unwrap()
        };

        let finish = |title: &'static str, now| {
            let db = db.clone();
            async move {
                db.write(async |tx| {
                    replace_active_list_with_plan_completion(tx, "c1", title, &[input("a", ItemStatus::Completed)], now)
                        .await
                })
                .await
                .unwrap()
                .1
            }
        };
        assert_eq!(
            finish("Review is still pending", 2).await,
            1,
            "the legacy artifact only"
        );
        assert_eq!(
            document().await.state,
            PlanDocumentState::Reviewing,
            "finishing an unrelated checklist must not retire a pending review"
        );
        assert_eq!(plan::get_active(&db, "c1").await.unwrap(), None);

        execute_for_tests(&db, "UPDATE plan_documents SET state = 'approved' WHERE id = 'd1'")
            .await
            .unwrap();
        let before = document().await.lock_version;
        assert_eq!(finish("Implement approved plan", 3).await, 1);
        let after = document().await;
        assert_eq!(
            (after.state, after.lock_version, after.updated_at),
            (PlanDocumentState::Done, before + 1, 3)
        );
    }

    #[tokio::test]
    async fn only_one_list_can_be_in_progress() {
        let db = conversation().await;
        replace(&db, "Phase one", vec![input("a", ItemStatus::Pending)], 10).await;

        // Bypassing replace_active_list is the only way to attempt this; the
        // partial unique index is what stops it, not the code above.
        let forced = execute_for_tests(
            &db,
            "INSERT INTO todo_lists (id, conversation_id, title, status, created_at, updated_at)
                 VALUES ('forced', 'c1', 'Sneaky', 'in_progress', 20, 20)",
        )
        .await;
        assert!(forced.is_err());
    }

    #[tokio::test]
    async fn deleting_a_conversation_cascades_to_lists_and_items() {
        let db = conversation().await;
        let view = replace(&db, "Refactor auth", vec![input("a", ItemStatus::Pending)], 10).await;

        execute_for_tests(&db, "DELETE FROM conversations WHERE id = 'c1'")
            .await
            .unwrap();
        assert!(list_lists(&db, "c1").await.unwrap().is_empty());
        assert!(list_items(&db, &view.list.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_block_lists_every_step_and_an_empty_list_renders_none() {
        let db = conversation().await;
        assert_eq!(format_todo_block(&replace(&db, "Empty", vec![], 10).await), None);

        let view = replace(
            &db,
            "Refactor auth",
            vec![input("a", ItemStatus::Completed), input("b", ItemStatus::InProgress)],
            11,
        )
        .await;
        assert_eq!(
            format_todo_block(&view).unwrap(),
            "\n\n<todo_list>\nTitle: Refactor auth\n1. [completed] a\n2. [in_progress] b\n</todo_list>"
        );
    }
}

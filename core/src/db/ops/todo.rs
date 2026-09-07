use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::todo::{
    ItemStatus, ListStatus, TodoItemInsert, TodoItemRow, TodoListInsert, TodoListRow, TodoListView,
};
use crate::db::schema::{todo_items, todo_lists};

/// One step as the model supplied it, before it gets an id and a position.
pub struct TodoItemSpec {
    pub content: String,
    pub active_form: String,
    pub status: ItemStatus,
}

pub fn get_active_list(conn: &mut SqliteConnection, conversation_id: &str) -> QueryResult<Option<TodoListRow>> {
    todo_lists::table
        .filter(todo_lists::conversation_id.eq(conversation_id))
        .filter(todo_lists::status.eq(ListStatus::InProgress.as_str()))
        .first::<TodoListRow>(conn)
        .optional()
}

pub fn list_items(conn: &mut SqliteConnection, list_id: &str) -> QueryResult<Vec<TodoItemRow>> {
    todo_items::table
        .filter(todo_items::list_id.eq(list_id))
        .order(todo_items::sort_order.asc())
        .load::<TodoItemRow>(conn)
}

/// The active list plus its items, or `None` once every step is done and the
/// list has been archived.
pub fn get_active_view(conn: &mut SqliteConnection, conversation_id: &str) -> QueryResult<Option<TodoListView>> {
    let Some(list) = get_active_list(conn, conversation_id)? else {
        return Ok(None);
    };
    let items = list_items(conn, &list.id)?;
    Ok(Some(TodoListView { list, items }))
}

#[cfg(test)]
pub fn list_lists(conn: &mut SqliteConnection, conversation_id: &str) -> QueryResult<Vec<TodoListRow>> {
    todo_lists::table
        .filter(todo_lists::conversation_id.eq(conversation_id))
        .order(todo_lists::created_at.asc())
        .load::<TodoListRow>(conn)
}

/// Write the checklist the model just sent.
///
/// Items are replaced wholesale rather than diffed: the tool contract is
/// "here is the list as it now stands", so reconciling identities would invent
/// a distinction the model never made. A different `title` retires the running
/// list and opens a new one, which is how a conversation ends up holding
/// several. A list whose steps are all done retires itself, so the next call
/// starts fresh.
pub fn replace_active_list(
    conn: &mut SqliteConnection,
    conversation_id: &str,
    title: &str,
    items: &[TodoItemSpec],
    now: i64,
) -> QueryResult<TodoListView> {
    replace_active_list_with_plan_completion(conn, conversation_id, title, items, now).map(|(view, _)| view)
}

/// The same checklist write plus whether it retired an approved plan. Keeping
/// both state changes in one transaction prevents a crash after the last todo
/// is archived from leaving the just-finished plan permanently active.
pub fn replace_active_list_with_plan_completion(
    conn: &mut SqliteConnection,
    conversation_id: &str,
    title: &str,
    items: &[TodoItemSpec],
    now: i64,
) -> QueryResult<(TodoListView, usize)> {
    conn.transaction(|conn| {
        let active = get_active_list(conn, conversation_id)?;

        let list_id = match active {
            Some(list) if list.title == title => {
                diesel::update(todo_lists::table.find(&list.id))
                    .set(todo_lists::updated_at.eq(now))
                    .execute(conn)?;
                list.id
            }
            other => {
                if let Some(stale) = other {
                    archive(conn, &stale.id, now)?;
                }
                let id = uuid::Uuid::new_v4().to_string();
                diesel::insert_into(todo_lists::table)
                    .values(&TodoListInsert {
                        id: &id,
                        conversation_id,
                        title,
                        status: ListStatus::InProgress.as_str(),
                        created_at: now,
                        updated_at: now,
                    })
                    .execute(conn)?;
                id
            }
        };

        diesel::delete(todo_items::table.filter(todo_items::list_id.eq(&list_id))).execute(conn)?;

        let ids: Vec<String> = (0..items.len()).map(|_| uuid::Uuid::new_v4().to_string()).collect();
        let rows: Vec<TodoItemInsert> = items
            .iter()
            .zip(&ids)
            .enumerate()
            .map(|(idx, (item, id))| TodoItemInsert {
                id,
                list_id: &list_id,
                content: &item.content,
                active_form: &item.active_form,
                status: item.status.as_str(),
                sort_order: idx as i32,
                created_at: now,
            })
            .collect();
        if !rows.is_empty() {
            diesel::insert_into(todo_items::table).values(&rows).execute(conn)?;
        }

        let retired_plans = if items.iter().all(|i| i.status == ItemStatus::Completed) {
            archive(conn, &list_id, now)?;
            crate::db::ops::plan::complete_active(conn, conversation_id, now)?
        } else {
            0
        };

        let list = todo_lists::table.find(&list_id).first::<TodoListRow>(conn)?;
        let items = list_items(conn, &list_id)?;
        Ok((TodoListView { list, items }, retired_plans))
    })
}

fn archive(conn: &mut SqliteConnection, list_id: &str, now: i64) -> QueryResult<()> {
    diesel::update(todo_lists::table.find(list_id))
        .set((
            todo_lists::status.eq(ListStatus::Completed.as_str()),
            todo_lists::updated_at.eq(now),
        ))
        .execute(conn)?;
    Ok(())
}

/// Render the running checklist for the system prompt. Empty lists yield
/// `None` so an idle conversation does not carry a hollow tag around, and the
/// leading blank lines match the other prompt blocks' spacing contract.
pub fn format_todo_block(view: &TodoListView) -> Option<String> {
    if view.items.is_empty() {
        return None;
    }
    let mut block = String::from("\n\n<todo_list>\n");
    block.push_str(&format!("Title: {}\n", view.list.title));
    for (idx, item) in view.items.iter().enumerate() {
        block.push_str(&format!("{}. [{}] {}\n", idx + 1, item.status, item.content));
    }
    block.push_str("</todo_list>");
    Some(block)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    /// Lists hang off a real conversation row: foreign keys are enforced on
    /// every pooled connection, which is what makes the cascade below real.
    fn seed_conversation(conn: &mut SqliteConnection, id: &str) {
        use crate::db::schema::conversations;
        diesel::insert_into(conversations::table)
            .values((
                conversations::id.eq(id),
                conversations::created_at.eq(1),
                conversations::updated_at.eq(1),
            ))
            .execute(conn)
            .unwrap();
    }

    fn input(content: &str, status: ItemStatus) -> TodoItemSpec {
        TodoItemSpec {
            content: content.to_string(),
            active_form: format!("Doing {content}"),
            status,
        }
    }

    /// Migrations are plain SQL and Diesel does not check them at compile time,
    /// so this is the only place a broken CREATE TABLE surfaces before runtime.
    #[test]
    fn migrations_apply_and_tables_are_queryable() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_conversation(&mut conn, "c1");
        assert!(get_active_view(&mut conn, "c1").unwrap().is_none());
        assert!(list_lists(&mut conn, "c1").unwrap().is_empty());
    }

    #[test]
    fn first_call_opens_a_list() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_conversation(&mut conn, "c1");

        let view = replace_active_list(
            &mut conn,
            "c1",
            "Refactor auth",
            &[
                input("Extract token check", ItemStatus::InProgress),
                input("Add tests", ItemStatus::Pending),
            ],
            10,
        )
        .unwrap();

        assert_eq!(view.list.title, "Refactor auth");
        assert_eq!(view.list.status, "in_progress");
        assert_eq!(view.items.len(), 2);
        assert_eq!(view.items[0].sort_order, 0);
        assert_eq!(view.items[1].sort_order, 1);
        assert_eq!(view.items[0].active_form, "Doing Extract token check");
    }

    #[test]
    fn same_title_replaces_items_in_place() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_conversation(&mut conn, "c1");

        let first = replace_active_list(
            &mut conn,
            "c1",
            "Refactor auth",
            &[input("a", ItemStatus::InProgress), input("b", ItemStatus::Pending)],
            10,
        )
        .unwrap();

        let second = replace_active_list(
            &mut conn,
            "c1",
            "Refactor auth",
            &[
                input("a", ItemStatus::Completed),
                input("b", ItemStatus::InProgress),
                input("c", ItemStatus::Pending),
            ],
            20,
        )
        .unwrap();

        assert_eq!(first.list.id, second.list.id);
        assert_eq!(second.items.len(), 3);
        assert_eq!(list_lists(&mut conn, "c1").unwrap().len(), 1);
    }

    #[test]
    fn new_title_archives_the_previous_list() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_conversation(&mut conn, "c1");

        let first =
            replace_active_list(&mut conn, "c1", "Phase one", &[input("a", ItemStatus::InProgress)], 10).unwrap();
        let second =
            replace_active_list(&mut conn, "c1", "Phase two", &[input("b", ItemStatus::InProgress)], 20).unwrap();

        assert_ne!(first.list.id, second.list.id);
        let lists = list_lists(&mut conn, "c1").unwrap();
        assert_eq!(lists.len(), 2);
        assert_eq!(lists[0].status, "completed");
        assert_eq!(lists[1].status, "in_progress");
        // The archived list keeps its own items rather than losing them to the successor.
        assert_eq!(list_items(&mut conn, &first.list.id).unwrap().len(), 1);
    }

    #[test]
    fn finishing_every_step_archives_the_list() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_conversation(&mut conn, "c1");

        let view = replace_active_list(
            &mut conn,
            "c1",
            "Refactor auth",
            &[input("a", ItemStatus::Completed), input("b", ItemStatus::Completed)],
            10,
        )
        .unwrap();

        assert_eq!(view.list.status, "completed");
        assert!(get_active_view(&mut conn, "c1").unwrap().is_none());
    }

    #[test]
    fn checklist_completion_retires_only_an_approved_versioned_plan() {
        use crate::db::models::plan_review::PlanDocumentState;
        use crate::db::schema::plan_documents;

        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_conversation(&mut conn, "c1");
        let document = crate::db::ops::plan_review::create_or_resume_document(&mut conn, "c1", 1).unwrap();
        diesel::update(plan_documents::table.find(&document.id))
            .set(plan_documents::state.eq(PlanDocumentState::Reviewing.as_str()))
            .execute(&mut conn)
            .unwrap();

        replace_active_list(
            &mut conn,
            "c1",
            "Review is still pending",
            &[input("a", ItemStatus::Completed)],
            2,
        )
        .unwrap();
        assert_eq!(
            crate::db::ops::plan_review::get_document(&mut conn, &document.id)
                .unwrap()
                .state()
                .unwrap(),
            PlanDocumentState::Reviewing,
            "finishing an unrelated checklist must not retire a pending review"
        );

        diesel::update(plan_documents::table.find(&document.id))
            .set(plan_documents::state.eq(PlanDocumentState::Approved.as_str()))
            .execute(&mut conn)
            .unwrap();
        replace_active_list(
            &mut conn,
            "c1",
            "Implement approved plan",
            &[input("a", ItemStatus::Completed)],
            3,
        )
        .unwrap();
        assert_eq!(
            crate::db::ops::plan_review::get_document(&mut conn, &document.id)
                .unwrap()
                .state()
                .unwrap(),
            PlanDocumentState::Done
        );
    }

    #[test]
    fn only_one_list_can_be_in_progress() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_conversation(&mut conn, "c1");
        replace_active_list(&mut conn, "c1", "Phase one", &[input("a", ItemStatus::Pending)], 10).unwrap();

        // Bypassing replace_active_list is the only way to attempt this; the
        // partial unique index is what stops it, not the code above.
        let result = diesel::insert_into(todo_lists::table)
            .values(&TodoListInsert {
                id: "forced",
                conversation_id: "c1",
                title: "Sneaky",
                status: ListStatus::InProgress.as_str(),
                created_at: 20,
                updated_at: 20,
            })
            .execute(&mut conn);

        assert!(result.is_err());
    }

    #[test]
    fn deleting_a_conversation_cascades_to_lists_and_items() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_conversation(&mut conn, "c1");
        let view =
            replace_active_list(&mut conn, "c1", "Refactor auth", &[input("a", ItemStatus::Pending)], 10).unwrap();

        crate::db::ops::conversation::delete_conversation(&mut conn, "c1").unwrap();

        assert!(list_lists(&mut conn, "c1").unwrap().is_empty());
        assert!(list_items(&mut conn, &view.list.id).unwrap().is_empty());
    }

    #[test]
    fn empty_list_renders_no_block() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_conversation(&mut conn, "c1");
        let view = replace_active_list(&mut conn, "c1", "Empty", &[], 10).unwrap();

        assert!(format_todo_block(&view).is_none());
    }

    #[test]
    fn block_lists_every_step_with_its_state() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_conversation(&mut conn, "c1");
        let view = replace_active_list(
            &mut conn,
            "c1",
            "Refactor auth",
            &[input("a", ItemStatus::Completed), input("b", ItemStatus::InProgress)],
            10,
        )
        .unwrap();

        let block = format_todo_block(&view).unwrap();
        assert!(block.starts_with("\n\n<todo_list>"));
        assert!(block.ends_with("</todo_list>"));
        assert!(block.contains("Title: Refactor auth"));
        assert!(block.contains("1. [completed] a"));
        assert!(block.contains("2. [in_progress] b"));
    }
}

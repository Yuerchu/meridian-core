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

fn get_active_list(conn: &mut SqliteConnection, conversation_id: &str) -> QueryResult<Option<TodoListRow>> {
    todo_lists::table
        .filter(todo_lists::conversation_id.eq(conversation_id))
        .filter(todo_lists::status.eq(ListStatus::InProgress.as_str()))
        .first::<TodoListRow>(conn)
        .optional()
}

fn list_items(conn: &mut SqliteConnection, list_id: &str) -> QueryResult<Vec<TodoItemRow>> {
    todo_items::table
        .filter(todo_items::list_id.eq(list_id))
        .order(todo_items::sort_order.asc())
        .load::<TodoItemRow>(conn)
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
fn replace_active_list_with_plan_completion(
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

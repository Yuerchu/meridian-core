use diesel::prelude::*;
use serde::Serialize;

use crate::db::schema::{todo_items, todo_lists};

pub use crate::db::entity::todo_item::ItemStatus;
pub use crate::db::entity::todo_list::ListStatus;

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = todo_lists)]
pub struct TodoListRow {
    pub id: String,
    pub conversation_id: String,
    pub title: String,
    pub status: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = todo_lists)]
pub struct TodoListInsert<'a> {
    pub id: &'a str,
    pub conversation_id: &'a str,
    pub title: &'a str,
    pub status: &'a str,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = todo_items)]
pub struct TodoItemRow {
    pub id: String,
    pub list_id: String,
    pub content: String,
    pub active_form: String,
    pub status: String,
    pub sort_order: i32,
    pub created_at: i64,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = todo_items)]
pub struct TodoItemInsert<'a> {
    pub id: &'a str,
    pub list_id: &'a str,
    pub content: &'a str,
    pub active_form: &'a str,
    pub status: &'a str,
    pub sort_order: i32,
    pub created_at: i64,
}

/// A list plus its items, which is the only shape either the model or the UI
/// ever wants — neither half is useful alone.
#[derive(Debug, Clone, Serialize)]
pub struct TodoListView {
    pub list: TodoListRow,
    pub items: Vec<TodoItemRow>,
}

//! `todo_items`: one step of a checklist.

use sea_orm::entity::prelude::*;
use serde::Serialize;

use crate::db::types::{EpochMs, text_enum_column};

/// Per-item state. `InProgress` marks the single step being worked on right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, strum::IntoStaticStr, strum::EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ItemStatus {
    Pending,
    InProgress,
    Completed,
}

impl ItemStatus {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value
            .parse()
            .map_err(|_| format!("unknown todo status '{value}'; expected pending, in_progress or completed"))
    }
}

text_enum_column!(ItemStatus);

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "todo_items")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub list_id: String,
    pub content: String,
    pub active_form: String,
    pub status: ItemStatus,
    pub sort_order: i32,
    pub created_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::todo_list::Entity",
        from = "Column::ListId",
        to = "super::todo_list::Column::Id",
        on_delete = "Cascade"
    )]
    TodoList,
}

impl Related<super::todo_list::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::TodoList.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

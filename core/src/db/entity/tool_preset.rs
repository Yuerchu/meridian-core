//! `tool_presets`: named tool allow-lists an assistant can point at.
//!
//! `tool_names` decodes at the read: a preset whose list will not parse fails
//! the query rather than becoming an empty allow-list, which would quietly
//! change what every assistant using it may do.

use sea_orm::entity::prelude::*;

use crate::db::types::{EpochMs, Json, SqlBool};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "tool_presets")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub tool_names: Json<Vec<String>>,
    pub is_builtin: SqlBool,
    pub sort_order: i32,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// A partial update: a field left `None` is not written. The nullable columns
/// are `Option<Option<_>>`, so `Some(None)` clears one.
#[derive(Debug, Default, DeriveIntoActiveModel)]
pub struct ToolPresetChangeset {
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub icon: Option<Option<String>>,
    pub tool_names: Option<Json<Vec<String>>>,
    pub sort_order: Option<i32>,
    pub updated_at: Option<EpochMs>,
}

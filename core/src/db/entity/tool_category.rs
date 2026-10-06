//! `tool_categories`: the groups the tool settings page sorts custom tools into.

use sea_orm::entity::prelude::*;

use crate::db::types::EpochMs;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "tool_categories")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub sort_order: i32,
    pub created_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::custom_tool::Entity")]
    CustomTool,
}

impl Related<super::custom_tool::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::CustomTool.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

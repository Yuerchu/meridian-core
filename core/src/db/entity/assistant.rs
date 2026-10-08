//! `assistants`: a persona, its default model and the knobs a turn reads.
//!
//! `enabled_tools` is a JSON list and decodes at the read; a preset, when one
//! is set, wins over it (see `agent::turn_config`).
//!
//! The entity exists ahead of the assistant ops, which still run on Diesel
//! inside the plan-review barrier transactions: projects and the skill and
//! emoji bindings reference this table, and the drift test checks a reference
//! only when both ends have an entity.

use sea_orm::entity::prelude::*;

use crate::db::types::{EpochMs, Json, SqlBool};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "assistants")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub avatar: Option<String>,
    pub system_prompt: String,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_tokens: Option<i32>,
    pub is_default: SqlBool,
    pub sort_order: i32,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
    pub context_limit: i32,
    pub compact_keep_recent: i32,
    pub enabled_tools: Option<Json<Vec<String>>>,
    pub thinking_enabled: SqlBool,
    pub thinking_budget: Option<i32>,
    pub tool_preset_id: Option<String>,
    pub auto_compact_enabled: SqlBool,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::provider::Entity",
        from = "Column::ProviderId",
        to = "super::provider::Column::Id",
        on_delete = "SetNull"
    )]
    Provider,
    #[sea_orm(
        belongs_to = "super::tool_preset::Entity",
        from = "Column::ToolPresetId",
        to = "super::tool_preset::Column::Id",
        on_delete = "SetNull"
    )]
    ToolPreset,
}

impl Related<super::provider::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Provider.def()
    }
}

impl Related<super::tool_preset::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::ToolPreset.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

/// A partial update: a field left `None` is not written. The nullable
/// columns are `Option<Option<_>>`, so `Some(None)` clears one.
#[derive(Debug, Default, DeriveIntoActiveModel)]
pub struct AssistantChangeset {
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub avatar: Option<Option<String>>,
    pub system_prompt: Option<String>,
    pub provider_id: Option<Option<String>>,
    pub model_id: Option<Option<String>>,
    pub temperature: Option<Option<f32>>,
    pub top_p: Option<Option<f32>>,
    pub max_tokens: Option<Option<i32>>,
    pub is_default: Option<SqlBool>,
    pub context_limit: Option<i32>,
    pub compact_keep_recent: Option<i32>,
    pub enabled_tools: Option<Option<Json<Vec<String>>>>,
    pub thinking_enabled: Option<SqlBool>,
    pub thinking_budget: Option<Option<i32>>,
    pub tool_preset_id: Option<Option<String>>,
    pub auto_compact_enabled: Option<SqlBool>,
    pub updated_at: Option<EpochMs>,
}

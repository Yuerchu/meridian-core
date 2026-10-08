//! `acp_context_deliveries`: which context items a hosted agent has already
//! been sent, so a resumed session is not sent them twice.

use sea_orm::entity::prelude::*;

use crate::db::types::EpochMs;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "acp_context_deliveries")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub context_item_id: String,
    pub delivered_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::message_context_item::Entity",
        from = "Column::ContextItemId",
        to = "super::message_context_item::Column::Id",
        on_delete = "Cascade"
    )]
    MessageContextItem,
}

impl Related<super::message_context_item::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::MessageContextItem.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

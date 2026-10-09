//! `mode_artifacts`: what a mode produced for the user to approve. Plans are
//! the only kind today (`kind` = "plan").

use sea_orm::entity::prelude::*;
use serde::Serialize;

use crate::db::types::{EpochMs, text_enum_column};

/// Where an artifact stands with the user.
///
/// `Superseded` and `Done` both retire an approved artifact without deleting
/// it: the first when a newer one replaces it, the second when the work it
/// described is finished. Retiring rather than deleting keeps the history of
/// what was proposed readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, strum::IntoStaticStr, strum::EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    Approved,
    Rejected,
    Superseded,
    Done,
}

impl PlanStatus {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value.parse().map_err(|_| format!("unknown artifact status '{value}'"))
    }
}

text_enum_column!(PlanStatus);

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "mode_artifacts")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub conversation_id: String,
    pub kind: String,
    pub content: String,
    pub status: PlanStatus,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::conversation::Entity",
        from = "Column::ConversationId",
        to = "super::conversation::Column::Id",
        on_delete = "Cascade"
    )]
    Conversation,
}

impl Related<super::conversation::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Conversation.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

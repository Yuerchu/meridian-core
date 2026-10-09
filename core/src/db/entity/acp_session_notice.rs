//! `acp_session_notices`: what a hosted agent told us about its session —
//! a lost connection, a login, a limit — one row per notice id, revised in
//! place.

use sea_orm::entity::prelude::*;

use crate::db::types::{EpochMs, Json, text_enum_column};
use crate::events::{AcpNoticeAction, AcpNoticeCategory, AcpNoticeSeverity};

text_enum_column!(AcpNoticeCategory);
text_enum_column!(AcpNoticeSeverity);

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "acp_session_notices")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub conversation_id: String,
    pub turn_id: Option<String>,
    pub notice_id: String,
    pub revision: i32,
    pub category: AcpNoticeCategory,
    pub severity: AcpNoticeSeverity,
    pub title: String,
    pub details: Option<String>,
    pub reason: Option<String>,
    pub actions: Json<Vec<AcpNoticeAction>>,
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

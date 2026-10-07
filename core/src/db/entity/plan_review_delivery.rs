//! `plan_review_deliveries`: getting a review's decision back to the agent
//! that asked, one row per target. `payload_json` stays text: its shape
//! depends on the target, and it is decoded by the dispatcher that sends it.

use sea_orm::entity::prelude::*;

use crate::db::types::{EpochMs, checked_text_enum};

checked_text_enum!(PlanDeliveryTarget {
    Native = "native",
    Acp = "acp",
});

checked_text_enum!(PlanDeliveryState {
    Queued = "queued",
    Dispatched = "dispatched",
    Acknowledged = "acknowledged",
    Held = "held",
    InDoubt = "in_doubt",
});

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "plan_review_deliveries")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub review_id: String,
    pub target: PlanDeliveryTarget,
    pub state: PlanDeliveryState,
    pub payload_json: String,
    pub attempt_token: Option<String>,
    pub target_session_id: Option<String>,
    pub target_turn_id: Option<String>,
    pub error: Option<String>,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
    pub dispatched_at: Option<EpochMs>,
    pub acknowledged_at: Option<EpochMs>,
    pub held_at: Option<EpochMs>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::plan_review_session::Entity",
        from = "Column::ReviewId",
        to = "super::plan_review_session::Column::Id",
        on_delete = "Cascade"
    )]
    Review,
}

impl Related<super::plan_review_session::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Review.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

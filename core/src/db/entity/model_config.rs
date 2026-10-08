//! `model_configs`: one provider's door to a model — the wire id it answers
//! to there, the profile describing it, and what (if anything) is different
//! about reaching it this way: its own rates when it overrides them, and the
//! provider-side tools.
//!
//! Read the prices only through `agent::model_config::effective`, the one
//! place that knows `overrides_pricing` decides between these and the
//! profile's. `pricing_tiers` and `server_tools` stay JSON text for the reason
//! given on `model_profile`.

use sea_orm::entity::prelude::*;

use crate::db::types::{EpochMs, SqlBool};
use crate::decimal::Decimal;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "model_configs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub provider_id: String,
    pub model_id: String,
    pub profile_id: String,
    pub overrides_pricing: SqlBool,
    pub input_price: Option<Decimal>,
    pub output_price: Option<Decimal>,
    pub cache_read_price: Option<Decimal>,
    pub cache_write_price: Option<Decimal>,
    pub pricing_tiers: Option<String>,
    pub server_tools: Option<String>,
    /// Per **thousand** provider-side tool calls. Independent of
    /// `overrides_pricing`: there is no profile-level rate to fall back to.
    pub server_tool_price: Option<Decimal>,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::provider::Entity",
        from = "Column::ProviderId",
        to = "super::provider::Column::Id",
        on_delete = "Cascade"
    )]
    Provider,
    /// No `ON DELETE`: a config without a profile has no window to run a turn
    /// with, so the database refuses. `sea::ops::model_profile::
    /// delete_if_unreferenced` collects a profile once nothing points at it.
    #[sea_orm(
        belongs_to = "super::model_profile::Entity",
        from = "Column::ProfileId",
        to = "super::model_profile::Column::Id"
    )]
    Profile,
}

impl Related<super::provider::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Provider.def()
    }
}

impl Related<super::model_profile::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Profile.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

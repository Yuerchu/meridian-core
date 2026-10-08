//! `model_profiles`: what a model *is*, independent of who serves it — its
//! window, its capability patch and, usually, its prices. Every
//! `model_configs` row that reaches the model points at one.
//!
//! `pricing_tiers` and `capability_overrides` stay JSON text here: the turn
//! loop, the price snapshot and the usage report read them through
//! `agent::model_config::effective`, which still hands them on as text to
//! `agent::pricing::parse_tiers` and the strict capability decoder. They take a
//! type when those readers move off Diesel.

use sea_orm::ActiveValue::Set;
use sea_orm::entity::prelude::*;

use crate::db::types::EpochMs;
use crate::decimal::Decimal;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "model_profiles")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub name: String,
    pub context_window: i32,
    pub compact_threshold: i32,
    pub max_output_tokens: Option<i32>,
    /// `None` is an unconfigured rate; `Some(0)` is an explicit free one.
    pub input_price: Option<Decimal>,
    pub output_price: Option<Decimal>,
    pub cache_read_price: Option<Decimal>,
    pub cache_write_price: Option<Decimal>,
    pub pricing_tiers: Option<String>,
    pub capability_overrides: Option<String>,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// A whole profile as the editor submits it: every field is written, `None`
/// included, so an absent price means "cleared" rather than "unchanged".
///
/// Written out by hand rather than derived. `DeriveIntoActiveModel` reads a
/// plain `Option` field's `None` as "leave this column alone", which here would
/// keep a price the person just cleared.
#[derive(Debug, Clone)]
pub struct ModelProfileChangeset {
    pub name: String,
    pub context_window: i32,
    pub compact_threshold: i32,
    pub max_output_tokens: Option<i32>,
    pub input_price: Option<Decimal>,
    pub output_price: Option<Decimal>,
    pub cache_read_price: Option<Decimal>,
    pub cache_write_price: Option<Decimal>,
    pub pricing_tiers: Option<String>,
    pub capability_overrides: Option<String>,
    pub updated_at: EpochMs,
}

impl ModelProfileChangeset {
    /// Every column but the key and `created_at`, set.
    pub fn into_active_model(self) -> ActiveModel {
        ActiveModel {
            name: Set(self.name),
            context_window: Set(self.context_window),
            compact_threshold: Set(self.compact_threshold),
            max_output_tokens: Set(self.max_output_tokens),
            input_price: Set(self.input_price),
            output_price: Set(self.output_price),
            cache_read_price: Set(self.cache_read_price),
            cache_write_price: Set(self.cache_write_price),
            pricing_tiers: Set(self.pricing_tiers),
            capability_overrides: Set(self.capability_overrides),
            updated_at: Set(self.updated_at),
            ..Default::default()
        }
    }
}

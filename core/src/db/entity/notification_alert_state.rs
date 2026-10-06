//! `notification_alert_state`: what has already been said.
//!
//! One row per condition, keyed on what the alert is *about*. Raising an alert
//! and reporting it are two separate writes (`sea::ops::notification`), which
//! is why `last_notified_at` is its own column rather than a flag folded into
//! `last_raised_at`.

use sea_orm::entity::prelude::*;

use crate::db::types::EpochMs;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "notification_alert_state")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub alert_key: String,
    pub first_raised_at: EpochMs,
    pub last_raised_at: EpochMs,
    /// NULL until a delivery was actually accepted. See the migration: writing
    /// this when the alert is raised is what makes an unsent alert look sent.
    pub last_notified_at: Option<EpochMs>,
    pub fingerprint: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

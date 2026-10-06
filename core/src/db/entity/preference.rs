//! `preferences`: one string per key.
//!
//! The keys are closed sets owned by the modules that read them — hooks,
//! notify, onebot, the shell's `PreferenceKey` — and so is the meaning of each
//! value; this is only the row. Callers that want a parsed value parse it at
//! the read and treat anything they do not recognise as an error, never as a
//! default (see `hooks::load_config` and its tests for the shape of that).

use sea_orm::entity::prelude::*;

use crate::db::types::EpochMs;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "preferences")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub key: String,
    pub value: String,
    pub updated_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

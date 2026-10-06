//! `journal_blobs`: the row for one content-addressed snapshot.
//!
//! The bytes live under the journal root (`journal::blobs`), written before
//! the row — "bytes before rows" — so a row here always names a file that
//! exists, and a blob no version references any more is deletable rows first,
//! file after.

use sea_orm::entity::prelude::*;

use crate::db::types::EpochMs;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "journal_blobs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub sha256: String,
    pub byte_len: i64,
    pub line_count: i32,
    pub created_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

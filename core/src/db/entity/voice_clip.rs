//! `voice_clips`: one occurrence of a recording, with its own sender.
//!
//! The account and session columns are copied from the blob row at write time
//! (`db::sea::ops::voice_corpus::record_clip` reads them off the blob rather
//! than taking them as arguments), so the two cannot disagree. `ON DELETE
//! CASCADE` from the blob is what lets a tombstone collector delete a blob
//! that acquired a clip between its SELECT and its UPDATE without stranding
//! the clip.

use sea_orm::entity::prelude::*;

use super::voice_blob::VoiceCorpusSourceType;
use crate::db::types::EpochMs;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "voice_clips")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub blob_id: String,
    pub bot_self_id: i64,
    pub source_type: VoiceCorpusSourceType,
    pub source_id: String,
    /// The message's **sender**, not the acoustic speaker: the two differ when
    /// someone forwards another person's voice.
    pub sender_id: String,
    pub platform_message_id: Option<i64>,
    pub segment_index: i32,
    /// `None` = the transcript was not obtained, or the message carried several
    /// record segments — a per-message transcript cannot be assigned to one.
    pub transcript: Option<String>,
    pub transcript_source: Option<String>,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::voice_blob::Entity",
        from = "Column::BlobId",
        to = "super::voice_blob::Column::Id",
        on_delete = "Cascade"
    )]
    Blob,
}

impl Related<super::voice_blob::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Blob.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

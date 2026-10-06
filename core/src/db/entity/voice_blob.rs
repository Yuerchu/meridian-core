//! `voice_blobs`: one stored recording per `(account, session, format, sha)`.
//!
//! Two tables rather than one, because the same audio sent by two people is
//! two captures: this row is the bytes, `voice_clips` is each occurrence with
//! its own sender. Why the account is a dimension of the dedupe key, and why
//! ownership is a token plus a monotonic epoch rather than a process id, is
//! argued in migration 39 (now part of the baseline).
//!
//! `status` is text rather than an integer because it appears in logs and
//! exports, and a person reading `damaged` knows what happened while `2` says
//! nothing. The stored spelling and the schema's `CHECK` are held together by
//! a test below. The composite `CHECK` — `pending` if and only if an owner
//! token and a lease are present — stays the database's job.

use sea_orm::entity::prelude::*;
use serde::Serialize;

use crate::db::types::EpochMs;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "voice_blobs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub bot_self_id: i64,
    pub source_type: VoiceCorpusSourceType,
    pub source_id: String,
    pub sha256: String,
    pub file_format: String,
    pub file_name: String,
    pub file_size: i64,
    pub status: VoiceBlobStatus,
    /// Unique per claim. A process id is not enough: two tasks *inside one
    /// process* running `CAS WHERE owner = <old>` both write back the same
    /// value and both believe they won.
    pub owner_token: Option<String>,
    /// Monotonic, so takeovers order: an old owner waking up carries the old
    /// epoch and cannot write.
    pub fence_epoch: i64,
    pub lease_expires_at: Option<EpochMs>,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// Which kind of OneBot conversation a sample came from.
///
/// The column has no `CHECK`; the stored spelling is pinned against the wire
/// spelling (serde) and strum below, and an unknown stored value fails the
/// read rather than arriving as a default. `EnumIter` is SeaORM's re-export,
/// which `ActiveEnum` requires, not the crate's own strum.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    EnumIter,
    Serialize,
    strum::IntoStaticStr,
    strum::EnumString,
    DeriveActiveEnum,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum VoiceCorpusSourceType {
    #[sea_orm(string_value = "onebot_group")]
    OnebotGroup,
    #[sea_orm(string_value = "onebot_private")]
    OnebotPrivate,
}

impl VoiceCorpusSourceType {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value
            .parse()
            .map_err(|_| format!("unknown voice corpus source_type '{value}'"))
    }
}

/// Publication state of a blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, strum::IntoStaticStr, strum::EnumString, DeriveActiveEnum)]
#[strum(serialize_all = "snake_case")]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum VoiceBlobStatus {
    /// An owner is publishing it, or that owner died (lease + fencing tell
    /// the two apart).
    #[sea_orm(string_value = "pending")]
    Pending,
    /// Listable, exportable.
    #[sea_orm(string_value = "ready")]
    Ready,
    /// The file is missing or fails the size/sha check. Not exported, and
    /// not pretended to be fine.
    #[sea_orm(string_value = "damaged")]
    Damaged,
    /// Tombstone: the row goes only after the file is gone.
    #[sea_orm(string_value = "deleting")]
    Deleting,
}

impl VoiceBlobStatus {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use sea_orm::Iterable;

    use super::*;

    /// The names `CHECK (status IN (…))` on `voice_blobs` allows, read out of
    /// the snapshot. The table's DDL spans two lines there (the composite
    /// CHECK wraps), so the statement is cut out of the whole text.
    fn allowed_by_status_check() -> BTreeSet<String> {
        let snapshot = include_str!("../../../schema.snapshot.sql");
        let start = snapshot
            .find("CREATE TABLE \"voice_blobs\"")
            .expect("the snapshot builds voice_blobs");
        let table = &snapshot[start..];
        let table = &table[..table[1..].find("CREATE TABLE").map(|i| i + 1).unwrap_or(table.len())];
        let prefix = "CHECK (status IN (";
        let start = table.find(prefix).expect("a CHECK on status") + prefix.len();
        let end = start + table[start..].find("))").expect("the CHECK closes");
        table[start..end]
            .split(',')
            .map(|name| name.trim().trim_matches('\'').to_owned())
            .collect()
    }

    /// The stored spelling of `status` and the schema's `CHECK` are one list,
    /// read out of the snapshot rather than restated here, so the enum and the
    /// live constraint cannot drift apart silently.
    #[test]
    fn the_stored_status_spelling_is_what_the_check_constraint_names() {
        let mut stored = BTreeSet::new();
        for status in VoiceBlobStatus::iter() {
            let db = status.to_value();
            assert_eq!(status.as_str(), db, "{status:?}: strum and the stored value disagree");
            assert_eq!(VoiceBlobStatus::try_from_value(&db).unwrap(), status);
            stored.insert(db);
        }
        assert_eq!(
            stored,
            allowed_by_status_check(),
            "VoiceBlobStatus and the schema's CHECK name different states"
        );
        assert!(VoiceBlobStatus::try_from_value(&"archived".to_owned()).is_err());
    }

    /// `source_type` has no `CHECK` in the schema, so the only lists to hold
    /// together are the stored spelling, the wire spelling and strum's — and
    /// an unknown value has to be refused at the read, since nothing in the
    /// database refuses it at the write.
    #[test]
    fn the_stored_source_type_spelling_is_the_wire_spelling() {
        let mut stored = BTreeSet::new();
        for kind in VoiceCorpusSourceType::iter() {
            let db = kind.to_value();
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::Value::String(db.clone()),
                "{kind:?} is spelt one way on the wire and another in the database"
            );
            assert_eq!(kind.as_str(), db, "{kind:?}: strum and the stored value disagree");
            assert_eq!(VoiceCorpusSourceType::try_from_value(&db).unwrap(), kind);
            assert_eq!(VoiceCorpusSourceType::parse(&db).unwrap(), kind);
            stored.insert(db);
        }
        assert_eq!(
            stored,
            BTreeSet::from(["onebot_group".to_owned(), "onebot_private".to_owned()])
        );
        assert!(VoiceCorpusSourceType::try_from_value(&"onebot_channel".to_owned()).is_err());
        let error = VoiceCorpusSourceType::parse("onebot_channel").unwrap_err();
        assert!(error.contains("unknown voice corpus source_type"), "{error}");
    }
}

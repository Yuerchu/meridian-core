//! `journal_versions`: one observed transition on a file's chain.
//!
//! The chain invariant — for `seq > 1`, `observed_old_sha` equals the previous
//! version's `new_sha` — is the writer's job (`db::sea::ops::journal`), not the
//! schema's; what the schema does pin is that an `external` row names no
//! conversation (`CHECK (source <> 'external' OR conversation_id IS NULL)`).
//!
//! `op` and `source` are strings in the database rather than integers for the
//! voice-corpus reason: they appear in exports and logs, and `external` reads
//! while `6` does not. The stored spelling and the schema's `CHECK` lists are
//! held together by a test below.

use sea_orm::entity::prelude::*;

use crate::db::types::EpochMs;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "journal_versions")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub file_id: String,
    pub seq: i64,
    pub op: VersionOp,
    /// `None` = the file did not exist when the writer looked.
    pub observed_old_sha: Option<String>,
    /// `None` = the write was a deletion.
    pub new_sha: Option<String>,
    pub source: VersionSource,
    pub conversation_id: Option<String>,
    pub turn_id: Option<String>,
    /// Which project the file belonged to at write time — a snapshot, so a
    /// conversation moving projects or being deleted cannot rewrite history.
    pub project_id: Option<String>,
    pub origin: Option<String>,
    pub model_id: Option<String>,
    pub tool_name: Option<String>,
    /// For `rename_to`: the exact `rename_from` *version* the content came
    /// from — a version rather than a file, because the old path can be
    /// recreated later and a chain-level pointer would let blame wander into
    /// an unrelated incarnation.
    pub moved_from_version_id: Option<String>,
    pub created_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::journal_file::Entity",
        from = "Column::FileId",
        to = "super::journal_file::Column::Id",
        on_delete = "Cascade"
    )]
    File,
    #[sea_orm(
        belongs_to = "super::journal_blob::Entity",
        from = "Column::NewSha",
        to = "super::journal_blob::Column::Sha256"
    )]
    NewBlob,
    #[sea_orm(
        belongs_to = "super::journal_blob::Entity",
        from = "Column::ObservedOldSha",
        to = "super::journal_blob::Column::Sha256"
    )]
    ObservedOldBlob,
}

impl Related<super::journal_file::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::File.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

/// What a version row says happened.
///
/// `EnumIter` is SeaORM's re-export, which `ActiveEnum` requires, not the
/// crate's own strum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, strum::IntoStaticStr, strum::EnumString, DeriveActiveEnum)]
#[strum(serialize_all = "snake_case")]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum VersionOp {
    #[sea_orm(string_value = "write")]
    Write,
    #[sea_orm(string_value = "edit")]
    Edit,
    #[sea_orm(string_value = "patch")]
    Patch,
    #[sea_orm(string_value = "delete")]
    Delete,
    #[sea_orm(string_value = "rename_from")]
    RenameFrom,
    #[sea_orm(string_value = "rename_to")]
    RenameTo,
    /// Observed across a `run_command` bracket rather than performed by a
    /// file tool; attributed to the turn, drawn as "inferred".
    #[sea_orm(string_value = "command_observed")]
    CommandObserved,
    /// The chain head did not match what was observed: somebody else changed
    /// the file. Carries no conversation by construction (CHECK in the DDL).
    #[sea_orm(string_value = "external")]
    External,
    #[sea_orm(string_value = "rewind")]
    Rewind,
}

impl VersionOp {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

/// Which capture path wrote the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, strum::IntoStaticStr, strum::EnumString, DeriveActiveEnum)]
#[strum(serialize_all = "snake_case")]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum VersionSource {
    #[sea_orm(string_value = "native")]
    Native,
    #[sea_orm(string_value = "hosted")]
    Hosted,
    #[sea_orm(string_value = "inferred")]
    Inferred,
    #[sea_orm(string_value = "external")]
    External,
    #[sea_orm(string_value = "rewind")]
    Rewind,
}

impl VersionSource {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use sea_orm::Iterable;

    use super::*;

    /// The names a `CHECK (<column> IN (…))` on `journal_versions` allows,
    /// read out of the snapshot. The table's DDL spans several lines there,
    /// so the statement is cut out of the whole text rather than found as one
    /// line.
    fn allowed_by_check(column: &str) -> BTreeSet<String> {
        let snapshot = include_str!("../../../schema.snapshot.sql");
        let start = snapshot
            .find("CREATE TABLE \"journal_versions\"")
            .expect("the snapshot builds journal_versions");
        let table = &snapshot[start..];
        let table = &table[..table[1..].find("CREATE TABLE").map(|i| i + 1).unwrap_or(table.len())];
        let prefix = format!("CHECK ({column} IN (");
        let start = table.find(&prefix).unwrap_or_else(|| panic!("a CHECK on {column}")) + prefix.len();
        let end = start + table[start..].find("))").expect("the CHECK closes");
        table[start..end]
            .split(',')
            .map(|name| name.trim().trim_matches('\'').to_owned())
            .collect()
    }

    /// The stored spelling and the schema's `CHECK` are one list, for both
    /// enums. Read out of the snapshot rather than restated here, so the enum
    /// and the live constraint cannot drift apart silently.
    #[test]
    fn the_stored_spelling_is_what_the_check_constraints_name() {
        let mut stored = BTreeSet::new();
        for op in VersionOp::iter() {
            let db = op.to_value();
            assert_eq!(op.as_str(), db, "{op:?}: strum and the stored value disagree");
            assert_eq!(VersionOp::try_from_value(&db).unwrap(), op);
            stored.insert(db);
        }
        assert_eq!(
            stored,
            allowed_by_check("op"),
            "VersionOp and the schema's CHECK name different operations"
        );
        assert!(VersionOp::try_from_value(&"move".to_owned()).is_err());

        let mut stored = BTreeSet::new();
        for source in VersionSource::iter() {
            let db = source.to_value();
            assert_eq!(source.as_str(), db, "{source:?}: strum and the stored value disagree");
            assert_eq!(VersionSource::try_from_value(&db).unwrap(), source);
            stored.insert(db);
        }
        assert_eq!(
            stored,
            allowed_by_check("source"),
            "VersionSource and the schema's CHECK name different sources"
        );
        assert!(VersionSource::try_from_value(&"manual".to_owned()).is_err());
    }
}

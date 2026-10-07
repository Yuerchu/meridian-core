//! `memories`: what the assistant remembers, in four layers.
//!
//! The scope, origin, visibility, type and deletion-actor columns have no
//! `CHECK`, so the enums here are what hold each list closed: an unknown value
//! fails the read rather than being shown or injected as something it is not.
//! The constants and scope-id helpers live here too, beside the rows they
//! bound.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

use crate::db::types::{EpochMs, text_enum_column};

/// Global memories all share one row anchor within their scope type. A literal
/// `'_'` rather than an empty string so logs and debugging can tell "the global
/// scope" apart from "scope_id was never set".
pub const GLOBAL_SCOPE_ID: &str = "_";

/// Longest a single memory may be, enforced in ops so every writer (IPC, slash
/// command, extraction pass) shares the limit. Prompt budget is enforced
/// separately at injection time; this cap only stops one row from eating it all.
pub const MAX_MEMORY_CONTENT_LEN: usize = 500;

pub const MAX_MEMORIES_PER_PROJECT: usize = 100;
pub const MAX_ONEBOT_GLOBAL_MEMORIES: usize = 50;
/// The client-side counterpart of the OneBot global layer. Same order of
/// magnitude: it is injected into every desktop turn, so it competes with the
/// project layer for the same budget.
pub const MAX_CLIENT_GLOBAL_MEMORIES: usize = 50;
/// People holding at least one live `normal` memory. Owner-only rows do not
/// count: eviction never deletes them, so a person left with nothing but the
/// operator's notes would hold a slot forever and evicting them would free
/// nothing.
pub const MAX_REMEMBERED_SUBJECTS: usize = 200;
pub const MAX_MEMORIES_PER_SUBJECT: usize = 20;
/// Rows tracked only for their last-seen clock, with no memories at all. Every
/// speaker gets touched, so without this the table grows with every passer-by.
pub const MAX_TRACKED_SUBJECTS: usize = 2_000;
/// Manually pinned subjects are exempt from eviction, so they need their own
/// ceiling or pinning becomes a way around MAX_REMEMBERED_SUBJECTS.
pub const MAX_PINNED_SUBJECTS: usize = 50;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "memories")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub scope_type: MemoryScope,
    pub scope_id: String,
    pub key: String,
    pub content: String,
    pub memory_type: MemoryType,
    pub subject_scope_id: Option<String>,
    pub origin: Origin,
    pub visibility: Visibility,
    pub source_session_id: Option<String>,
    pub deleted_at: Option<EpochMs>,
    pub deleted_by: Option<DeletedBy>,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// A partial update: a field left `None` is not written.
#[derive(Debug, Default, DeriveIntoActiveModel)]
pub struct MemoryChangeset {
    pub content: Option<String>,
    pub memory_type: Option<MemoryType>,
    pub visibility: Option<Visibility>,
    pub updated_at: Option<EpochMs>,
}

impl Model {
    pub fn is_owner_only(&self) -> bool {
        self.visibility == Visibility::OwnerOnly
    }
}

/// Which anchor a memory hangs off. Separate from `Origin` and `Visibility`:
/// those answer "where was it learned" and "who may see it".
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    EnumIter,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
    DeriveActiveEnum,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum MemoryScope {
    #[sea_orm(string_value = "project")]
    Project,
    /// Everything the user says directly in Meridian, outside any project.
    /// Deliberately separate from `OnebotGlobal`: the two are sibling roots, and
    /// neither is injected into the other's conversations.
    #[sea_orm(string_value = "client_global")]
    ClientGlobal,
    #[sea_orm(string_value = "onebot_global")]
    OnebotGlobal,
    #[sea_orm(string_value = "onebot_user")]
    OnebotUser,
}

impl MemoryScope {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value.parse().map_err(|_| format!("unknown memory scope `{value}`"))
    }

    pub fn all() -> Vec<&'static str> {
        <Self as sea_orm::Iterable>::iter().map(|v| v.as_str()).collect()
    }
}

/// The surface a memory was learned on. Gates injection: what was learned in a
/// private chat must never surface in a group.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    EnumIter,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
    DeriveActiveEnum,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum Origin {
    #[sea_orm(string_value = "private")]
    Private,
    #[sea_orm(string_value = "group")]
    Group,
    #[sea_orm(string_value = "admin")]
    Admin,
    #[sea_orm(string_value = "desktop")]
    Desktop,
    /// Migrated from the pre-layering schema. It was a user's words, but no
    /// trustworthy sender survives, so it must not pass as evidence produced by
    /// the identity pipeline. Injected as if it were `Group`.
    #[sea_orm(string_value = "legacy")]
    Legacy,
}

impl Origin {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    pub fn all() -> Vec<&'static str> {
        <Self as sea_orm::Iterable>::iter().map(|v| v.as_str()).collect()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value.parse().map_err(|_| format!("unknown memory origin `{value}`"))
    }

    /// Origins that may be injected into a group conversation. Deliberately
    /// excludes `Private`: this one predicate is the privacy boundary.
    pub fn group_visible() -> &'static [Origin] {
        &[Origin::Group, Origin::Admin, Origin::Legacy]
    }
}

/// Who may see a memory. Kept apart from `Origin` because the operator's private
/// annotation about a person and the operator's approved bot-wide rule share an
/// origin but must not share visibility: the model has to be able to quote the
/// latter, and must never quote the former.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    EnumIter,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
    DeriveActiveEnum,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum Visibility {
    #[sea_orm(string_value = "normal")]
    Normal,
    #[sea_orm(string_value = "owner_only")]
    OwnerOnly,
}

impl Visibility {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value
            .parse()
            .map_err(|_| format!("unknown memory visibility `{value}`"))
    }

    pub fn all() -> Vec<&'static str> {
        <Self as sea_orm::Iterable>::iter().map(|v| v.as_str()).collect()
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    EnumIter,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
    DeriveActiveEnum,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum MemoryType {
    #[sea_orm(string_value = "general")]
    General,
    #[sea_orm(string_value = "preference")]
    Preference,
    #[sea_orm(string_value = "fact")]
    Fact,
    #[sea_orm(string_value = "instruction")]
    Instruction,
    #[sea_orm(string_value = "relationship")]
    Relationship,
}

impl MemoryType {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    pub fn all() -> Vec<&'static str> {
        <Self as sea_orm::Iterable>::iter().map(|v| v.as_str()).collect()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value.parse().map_err(|_| format!("unknown memory type `{value}`"))
    }
}

/// Why a row was soft-deleted. Surfaced in the trash view so the operator can
/// tell "the bot forgot this person" apart from "someone deleted it".
///
/// Not a `DeriveActiveEnum`: its stored value `self` would become a variant
/// named `Self` in what that derive generates. `text_enum_column!` builds the
/// column from `as_str` and `parse` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::IntoStaticStr, strum::EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum DeletedBy {
    /// The subject removed their own memory.
    #[strum(serialize = "self")]
    #[serde(rename = "self")]
    SelfRemoved,
    Admin,
    Lru,
}

impl DeletedBy {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value
            .parse()
            .map_err(|_| format!("unknown memory deletion actor `{value}`"))
    }
}

text_enum_column!(DeletedBy);

/// Build the scope id for a OneBot user. The `onebot:` prefix is not decoration:
/// bare numeric ids would collide across platforms, and a collision would
/// silently inject one person's memories into a same-numbered stranger on
/// another platform.
pub fn onebot_user_scope_id(user_id: i64) -> String {
    format!("onebot:{user_id}")
}

pub fn parse_onebot_user_scope_id(scope_id: &str) -> Option<i64> {
    scope_id.strip_prefix("onebot:")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use sea_orm::{ActiveEnum, Iterable};

    use super::*;

    /// One spelling per value: the stored string, the IPC string and the
    /// `as_str` the rest of the crate compares with are the same list.
    #[test]
    fn stored_and_wire_spellings_are_one_list() {
        fn check<E>(name: &str)
        where
            E: ActiveEnum<Value = String> + Iterable + Serialize + Copy + std::fmt::Debug,
            &'static str: From<E>,
        {
            for value in E::iter() {
                let stored = value.to_value();
                let wire = serde_json::to_value(value).unwrap();
                assert_eq!(wire.as_str(), Some(stored.as_str()), "{name}: {value:?}");
                assert_eq!(<&'static str>::from(value), stored, "{name}: {value:?}");
            }
        }
        check::<MemoryScope>("scope");
        check::<Origin>("origin");
        check::<Visibility>("visibility");
        check::<MemoryType>("type");
        for by in [DeletedBy::SelfRemoved, DeletedBy::Admin, DeletedBy::Lru] {
            assert_eq!(serde_json::to_value(by).unwrap().as_str(), Some(by.as_str()));
            assert_eq!(DeletedBy::parse(by.as_str()).unwrap(), by);
        }
        assert_eq!(DeletedBy::SelfRemoved.as_str(), "self");
        assert!(MemoryScope::parse("workspace").is_err());
        assert!(Visibility::parse("public").is_err());
    }

    #[test]
    fn admin_origin_is_group_visible() {
        assert!(Origin::group_visible().contains(&Origin::Admin));
        assert!(!Origin::group_visible().contains(&Origin::Desktop));
        assert!(!Origin::group_visible().contains(&Origin::Private));
    }
}

use diesel::prelude::*;
use serde::{Deserialize, Serialize};
use strum::{EnumIter, IntoEnumIterator};

use crate::db::schema::{memories, memory_proposals, memory_subjects};

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

/// Which anchor a memory hangs off. Separate from `Origin` and `Visibility`:
/// those answer "where was it learned" and "who may see it".
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, EnumIter, Serialize, Deserialize, strum::IntoStaticStr, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum MemoryScope {
    Project,
    /// Everything the user says directly in Meridian, outside any project.
    /// Deliberately separate from `OnebotGlobal`: the two are sibling roots, and
    /// neither is injected into the other's conversations.
    ClientGlobal,
    OnebotGlobal,
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
        Self::iter().map(|v| v.as_str()).collect()
    }
}

/// The surface a memory was learned on. Gates injection: what was learned in a
/// private chat must never surface in a group.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, EnumIter, Serialize, Deserialize, strum::IntoStaticStr, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Origin {
    Private,
    Group,
    Admin,
    Desktop,
    /// Migrated from the pre-layering schema. It was a user's words, but no
    /// trustworthy sender survives, so it must not pass as evidence produced by
    /// the identity pipeline. Injected as if it were `Group`.
    Legacy,
}

impl Origin {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    pub fn all() -> Vec<&'static str> {
        Self::iter().map(|v| v.as_str()).collect()
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
    Debug, Clone, Copy, PartialEq, Eq, EnumIter, Serialize, Deserialize, strum::IntoStaticStr, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Visibility {
    Normal,
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
        Self::iter().map(|v| v.as_str()).collect()
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, EnumIter, Serialize, Deserialize, strum::IntoStaticStr, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum MemoryType {
    General,
    Preference,
    Fact,
    Instruction,
    Relationship,
}

impl MemoryType {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    pub fn all() -> Vec<&'static str> {
        Self::iter().map(|v| v.as_str()).collect()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value.parse().map_err(|_| format!("unknown memory type `{value}`"))
    }
}

/// Why a row was soft-deleted. Surfaced in the trash view so the operator can
/// tell "the bot forgot this person" apart from "someone deleted it".
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, EnumIter, Serialize, Deserialize, strum::IntoStaticStr, strum::EnumString,
)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, Serialize, Deserialize, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ProposalStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
}

impl ProposalStatus {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

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

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = memories)]
pub struct MemoryRow {
    pub id: String,
    pub scope_type: String,
    pub scope_id: String,
    pub key: String,
    pub content: String,
    pub memory_type: String,
    pub subject_scope_id: Option<String>,
    pub origin: String,
    pub visibility: String,
    pub source_session_id: Option<String>,
    pub deleted_at: Option<i64>,
    pub deleted_by: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl MemoryRow {
    pub fn visibility(&self) -> Result<Visibility, String> {
        Visibility::parse(&self.visibility)
    }

    pub fn is_owner_only(&self) -> Result<bool, String> {
        self.visibility().map(|visibility| visibility == Visibility::OwnerOnly)
    }
}

#[derive(Debug, Insertable)]
#[diesel(table_name = memories)]
pub struct MemoryInsert<'a> {
    pub id: &'a str,
    pub scope_type: &'a str,
    pub scope_id: &'a str,
    pub key: &'a str,
    pub content: &'a str,
    pub memory_type: &'a str,
    pub subject_scope_id: Option<&'a str>,
    pub origin: &'a str,
    pub visibility: &'a str,
    pub source_session_id: Option<&'a str>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Default, AsChangeset)]
#[diesel(table_name = memories)]
pub struct MemoryChangeset {
    pub content: Option<String>,
    pub memory_type: Option<String>,
    pub visibility: Option<String>,
    pub updated_at: Option<i64>,
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = memory_subjects)]
pub struct MemorySubjectRow {
    pub scope_id: String,
    pub display_name: Option<String>,
    pub last_seen_at: i64,
    pub created_at: i64,
    pub is_protected: i32,
    pub is_pinned: i32,
    pub opted_out: i32,
}

impl MemorySubjectRow {
    pub fn user_id(&self) -> Option<i64> {
        parse_onebot_user_scope_id(&self.scope_id)
    }

    pub fn is_opted_out(&self) -> bool {
        self.opted_out != 0
    }
}

#[derive(Debug, Insertable)]
#[diesel(table_name = memory_subjects)]
pub struct MemorySubjectInsert<'a> {
    pub scope_id: &'a str,
    pub display_name: Option<&'a str>,
    pub last_seen_at: i64,
    pub created_at: i64,
    pub is_protected: i32,
    pub is_pinned: i32,
    pub opted_out: i32,
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = memory_proposals)]
pub struct MemoryProposalRow {
    pub id: i32,
    pub key: String,
    pub content: String,
    pub memory_type: String,
    pub origin_session: Option<String>,
    pub proposer_id: Option<i64>,
    pub status: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub resolved_at: Option<i64>,
    pub resolved_by: Option<i64>,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = memory_proposals)]
pub struct MemoryProposalInsert<'a> {
    pub key: &'a str,
    pub content: &'a str,
    pub memory_type: &'a str,
    pub origin_session: Option<&'a str>,
    pub proposer_id: Option<i64>,
    pub status: &'a str,
    pub created_at: i64,
    pub expires_at: i64,
}

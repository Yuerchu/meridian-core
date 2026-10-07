//! The subject row as Diesel reads it, for the one Diesel caller left
//! (`db::ops::memory::list_subjects`). The memory types, constants and
//! scope-id helpers are in `db::entity::memory`.

use diesel::prelude::*;

use crate::db::entity::memory::parse_onebot_user_scope_id;
use crate::db::schema::memory_subjects;

#[derive(Debug, Clone, Queryable, Selectable)]
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
}

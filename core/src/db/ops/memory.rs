//! What is left of the Diesel memory ops: the two that still run on a Diesel
//! connection beside other Diesel work. `list_subjects` names speakers for
//! compaction, which reads the transcript on the same connection;
//! `delete_project_memories` runs inside `project::delete_project`'s
//! transaction. Everything else is `db::sea::ops::memory`.

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::entity::memory::MemoryScope;
use crate::db::models::memory::MemorySubjectRow;
use crate::db::schema::{memories, memory_subjects};

/// Hard delete, not soft: with the project gone a tombstone's scope_id points
/// nowhere, so it could be neither restored nor shown in the trash.
pub fn delete_project_memories(conn: &mut SqliteConnection, project_id: &str) -> QueryResult<usize> {
    diesel::delete(
        memories::table
            .filter(memories::scope_type.eq(MemoryScope::Project.as_str()))
            .filter(memories::scope_id.eq(project_id.to_string())),
    )
    .execute(conn)
}

pub fn list_subjects(conn: &mut SqliteConnection) -> QueryResult<Vec<MemorySubjectRow>> {
    memory_subjects::table
        .order(memory_subjects::last_seen_at.desc())
        .load::<MemorySubjectRow>(conn)
}

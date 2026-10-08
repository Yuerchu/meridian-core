//! What is left of the Diesel memory ops: `list_subjects`, which names
//! speakers for compaction, which reads the transcript on the same
//! connection. Everything else is `db::sea::ops::memory`.

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::memory::MemorySubjectRow;
use crate::db::schema::memory_subjects;

pub fn list_subjects(conn: &mut SqliteConnection) -> QueryResult<Vec<MemorySubjectRow>> {
    memory_subjects::table
        .order(memory_subjects::last_seen_at.desc())
        .load::<MemorySubjectRow>(conn)
}

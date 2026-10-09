use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::project::ProjectRow;
#[allow(unused_imports)]
use crate::db::schema::projects;
use crate::db::sea::ops::project::normalize_path;

pub fn get_project(conn: &mut SqliteConnection, id: &str) -> QueryResult<ProjectRow> {
    projects::table.find(id).first::<ProjectRow>(conn)
}

/// The project whose working directory is `path`, if there is one.
///
/// Compared in Rust over the whole (small) table rather than in SQL, because
/// the two sides come from different places and rarely agree character for
/// character: one was typed into a directory picker, the other arrives from
/// another program's idea of its own working directory. Separator, trailing
/// slash and — on Windows — case all have to stop mattering, and none of that
/// survives a `WHERE path = ?`.
pub fn find_project_by_path(conn: &mut SqliteConnection, path: &str) -> QueryResult<Option<ProjectRow>> {
    let wanted = normalize_path(path);
    if wanted.is_empty() {
        return Ok(None);
    }
    // Most recently touched first, so of two spellings of one directory the
    // live project wins, as it did when this read the listing.
    Ok(projects::table
        .order(projects::updated_at.desc())
        .load::<ProjectRow>(conn)?
        .into_iter()
        .find(|p| p.path.as_deref().map(normalize_path).as_deref() == Some(wanted.as_str())))
}

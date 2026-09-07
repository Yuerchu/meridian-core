use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::project::{ProjectChangeset, ProjectInsert, ProjectRow};
#[allow(unused_imports)]
use crate::db::schema::projects;

pub fn list_projects(conn: &mut SqliteConnection) -> QueryResult<Vec<ProjectRow>> {
    projects::table
        .order(projects::updated_at.desc())
        .load::<ProjectRow>(conn)
}

pub fn get_project(conn: &mut SqliteConnection, id: &str) -> QueryResult<ProjectRow> {
    projects::table.find(id).first::<ProjectRow>(conn)
}

pub fn find_project_by_source(
    conn: &mut SqliteConnection,
    source_type: &str,
    source_id: &str,
) -> QueryResult<Option<ProjectRow>> {
    projects::table
        .filter(projects::source_type.eq(source_type))
        .filter(projects::source_id.eq(source_id))
        .first::<ProjectRow>(conn)
        .optional()
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
    Ok(list_projects(conn)?
        .into_iter()
        .find(|p| p.path.as_deref().map(normalize_path).as_deref() == Some(wanted.as_str())))
}

/// Enough normalisation to compare two paths that name the same directory.
///
/// Deliberately textual: `canonicalize` would be stricter but touches the disk
/// and fails outright on a directory that has been moved or unmounted, which
/// would turn "cannot check right now" into "not this project".
///
/// Not the journal's file key. Journal identity lives in
/// `journal::normalize_file_key`: a trailing space on Unix is a different
/// file, and folding it here is what a directory picker needs and what a
/// chain key must not do.
///
/// The backslash is a separator only on Windows. On Unix it is an ordinary
/// filename character, and folding it into `/` there would make `a\b` and a
/// real `a/b` the same directory.
pub(crate) fn normalize_path(path: &str) -> String {
    let trimmed = path.trim();
    if cfg!(windows) {
        trimmed.replace('\\', "/").trim_end_matches('/').to_lowercase()
    } else {
        trimmed.trim_end_matches('/').to_string()
    }
}

pub fn create_project(conn: &mut SqliteConnection, new: &ProjectInsert) -> QueryResult<ProjectRow> {
    diesel::insert_into(projects::table).values(new).execute(conn)?;
    projects::table.find(new.id).first::<ProjectRow>(conn)
}

pub fn update_project(conn: &mut SqliteConnection, id: &str, changeset: &ProjectChangeset) -> QueryResult<ProjectRow> {
    diesel::update(projects::table.find(id)).set(changeset).execute(conn)?;
    projects::table.find(id).first::<ProjectRow>(conn)
}

/// Memories are not reachable by foreign key any more (scope_id is polymorphic),
/// so the cascade happens here — in ops rather than in the command layer, so
/// every caller is covered.
pub fn delete_project(conn: &mut SqliteConnection, id: &str) -> QueryResult<()> {
    conn.transaction(|conn| {
        crate::db::ops::memory::delete_project_memories(conn, id)?;
        diesel::delete(projects::table.find(id)).execute(conn)?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    fn project(conn: &mut SqliteConnection, id: &str, path: Option<&str>) {
        create_project(
            conn,
            &ProjectInsert {
                id,
                name: "p",
                path,
                source_type: "local",
                source_id: None,
                assistant_id: None,
                description: None,
                created_at: 1,
                updated_at: 1,
            },
        )
        .unwrap();
    }

    /// The two sides are typed by different programs, so they agree on the
    /// directory without agreeing on the string.
    ///
    /// The separator and case halves of that only exist on Windows — see
    /// `normalize_path` on why a backslash is left alone elsewhere — so the
    /// spellings tried are the platform's own.
    #[test]
    fn a_path_matches_despite_separators_and_trailing_slash() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let (stored, asked): (&str, Vec<&str>) = if cfg!(windows) {
            (
                r"C:\Users\me\Code\repo",
                vec![
                    r"C:\Users\me\Code\repo",
                    "C:/Users/me/Code/repo",
                    "C:/Users/me/Code/repo/",
                    "  C:/Users/me/Code/repo  ",
                    "c:/users/me/code/repo",
                ],
            )
        } else {
            (
                "/home/me/Code/repo",
                vec!["/home/me/Code/repo", "/home/me/Code/repo/", "  /home/me/Code/repo  "],
            )
        };
        project(&mut conn, "p1", Some(stored));

        for asked in asked {
            let found = find_project_by_path(&mut conn, asked).unwrap();
            assert_eq!(found.map(|p| p.id), Some("p1".into()), "asked `{asked}`");
        }
    }

    #[test]
    fn a_different_directory_is_not_a_match() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        project(&mut conn, "p1", Some("C:/Code/repo"));

        assert!(find_project_by_path(&mut conn, "C:/Code/other").unwrap().is_none());
        // A prefix is a different directory, not the same one.
        assert!(find_project_by_path(&mut conn, "C:/Code").unwrap().is_none());
    }

    #[test]
    fn a_project_without_a_path_never_matches() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        project(&mut conn, "p1", None);

        assert!(find_project_by_path(&mut conn, "C:/Code/repo").unwrap().is_none());
        assert!(find_project_by_path(&mut conn, "").unwrap().is_none());
    }

    /// Project comparison still trims: a picker or another program's cwd
    /// grows spaces. The journal's file key deliberately does not — see
    /// `journal::normalize_file_key`.
    #[test]
    fn project_comparison_still_trims_surrounding_whitespace() {
        assert_eq!(normalize_path("/tmp/file"), normalize_path(" /tmp/file "));
    }
}

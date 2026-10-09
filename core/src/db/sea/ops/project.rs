//! Reading and writing `projects`.
//!
//! No function here opens a transaction of its own: a write takes the caller's
//! `WriteTx`, and the caller's `Db::write` is the `BEGIN IMMEDIATE`.

use sea_orm::ActiveValue::Unchanged;
use sea_orm::{ActiveModelTrait, ColumnTrait, DbErr, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder};

use crate::db::entity::project;
use crate::db::entity::project::ProjectChangeset;
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::sea::ops::memory;

/// Most recently touched first.
pub async fn list_projects(db: &impl Read) -> Result<Vec<project::Model>, DbErr> {
    project::Entity::find()
        .order_by_desc(project::Column::UpdatedAt)
        .all(db.conn()?)
        .await
}

/// `None` for an id with no row. The Diesel version answers `NotFound`
/// instead; here the absence is in the type.
pub async fn get_project(db: &impl Read, id: &str) -> Result<Option<project::Model>, DbErr> {
    project::Entity::find_by_id(id).one(db.conn()?).await
}

fn not_found(id: &str) -> DbErr {
    DbErr::RecordNotFound(format!("project `{id}`"))
}

/// Inserts the row the caller built and reads it back.
pub async fn create_project(tx: &WriteTx, model: project::Model) -> Result<project::Model, DbErr> {
    let id = model.id.clone();
    project::Entity::insert(model.into_active_model())
        .exec_without_returning(tx.conn()?)
        .await?;
    get_project(tx, &id).await?.ok_or_else(|| not_found(&id))
}

/// `RecordNotFound` when there is no such row, before anything is written.
pub async fn update_project(tx: &WriteTx, id: &str, changeset: ProjectChangeset) -> Result<project::Model, DbErr> {
    let existing = get_project(tx, id).await?.ok_or_else(|| not_found(id))?;
    let mut row = changeset.into_active_model();
    row.id = Unchanged(existing.id);
    row.update(tx.conn()?).await
}

/// Deletes the project and every memory filed under it, in the caller's
/// write: memories reach a project by a polymorphic scope id, not a foreign
/// key, so nothing would cascade on its own. Conversations in it keep their
/// rows with no project. How many projects went: 0 for an id already gone.
/// The project a OneBot session lives in, by where its messages come from.
pub async fn find_project_by_source(
    db: &impl Read,
    source_type: project::ProjectSource,
    source_id: &str,
) -> Result<Option<project::Model>, DbErr> {
    project::Entity::find()
        .filter(project::Column::SourceType.eq(source_type))
        .filter(project::Column::SourceId.eq(source_id))
        .one(db.conn()?)
        .await
}

/// The project whose directory this is, however the two sides spelled it
/// (`normalize_path`). Most recently touched first, so of
/// two spellings of one directory the live project wins.
pub async fn find_project_by_path(db: &impl Read, path: &str) -> Result<Option<project::Model>, DbErr> {
    let wanted = normalize_path(path);
    if wanted.is_empty() {
        return Ok(None);
    }
    Ok(project::Entity::find()
        .order_by_desc(project::Column::UpdatedAt)
        .all(db.conn()?)
        .await?
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

pub async fn delete_project(tx: &WriteTx, id: &str) -> Result<u64, DbErr> {
    memory::delete_project_memories(tx, id).await?;
    Ok(project::Entity::delete_by_id(id).exec(tx.conn()?).await?.rows_affected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::entity::project::ProjectSource;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    fn project_row(id: &str, updated_at: i64) -> project::Model {
        project::Model {
            id: id.into(),
            name: id.to_uppercase(),
            path: Some(format!("/work/{id}")),
            source_type: ProjectSource::Local,
            source_id: None,
            assistant_id: None,
            description: Some("notes".into()),
            created_at: 1,
            updated_at,
        }
    }

    async fn with_path(db: &crate::db::sea::cap::Db, id: &str, path: Option<&str>) {
        let row = project::Model {
            path: path.map(str::to_owned),
            ..project_row(id, 1)
        };
        db.write(async |tx| create_project(tx, row).await).await.unwrap();
    }

    /// The two sides are typed by different programs, so they agree on the
    /// directory without agreeing on the string. The separator and case halves
    /// only exist on Windows, so the spellings tried are the platform's own.
    #[tokio::test]
    async fn a_path_matches_despite_separators_and_trailing_slash() {
        let db = sea_test_db().await;
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
        with_path(&db, "p1", Some(stored)).await;

        for asked in asked {
            let found = find_project_by_path(&db, asked).await.unwrap();
            assert_eq!(found.map(|p| p.id), Some("p1".into()), "asked `{asked}`");
        }
    }

    /// A different directory, a prefix of one, and a project with no path at
    /// all are none of them a match.
    #[tokio::test]
    async fn only_the_same_directory_matches() {
        let db = sea_test_db().await;
        with_path(&db, "p1", Some("C:/Code/repo")).await;
        with_path(&db, "p2", None).await;

        assert!(find_project_by_path(&db, "C:/Code/other").await.unwrap().is_none());
        assert!(find_project_by_path(&db, "C:/Code").await.unwrap().is_none());
        assert!(find_project_by_path(&db, "").await.unwrap().is_none());
    }

    /// Project comparison still trims: a picker or another program's cwd
    /// grows spaces. The journal's file key deliberately does not — see
    /// `journal::normalize_file_key`.
    #[test]
    fn project_comparison_still_trims_surrounding_whitespace() {
        assert_eq!(normalize_path("/tmp/file"), normalize_path(" /tmp/file "));
    }

    /// Listed newest first; an update writes only what it names and clears
    /// with `Some(None)`; a missing id is an error before anything is written.
    #[tokio::test]
    async fn projects_list_newest_first_and_update_in_part() {
        let db = sea_test_db().await;
        for (id, at) in [("old", 1), ("new", 5)] {
            let row = project_row(id, at);
            assert_eq!(
                db.write(async |tx| create_project(tx, row.clone()).await)
                    .await
                    .unwrap(),
                row
            );
        }
        let ids: Vec<_> = list_projects(&db).await.unwrap().into_iter().map(|p| p.id).collect();
        assert_eq!(ids, ["new", "old"]);

        let updated = db
            .write(async |tx| {
                let changes = ProjectChangeset {
                    path: Some(Some("/elsewhere".into())),
                    description: Some(None),
                    updated_at: Some(9),
                    ..Default::default()
                };
                update_project(tx, "old", changes).await
            })
            .await
            .unwrap();
        assert_eq!(
            updated,
            project::Model {
                path: Some("/elsewhere".into()),
                description: None,
                updated_at: 9,
                ..project_row("old", 1)
            }
        );
        let missing = db
            .write(async |tx| update_project(tx, "nope", ProjectChangeset::default()).await)
            .await;
        assert!(matches!(missing, Err(DbErr::RecordNotFound(_))));
    }

    /// Deleting a project takes its memories with it and leaves every other
    /// scope's alone.
    #[tokio::test]
    async fn deleting_a_project_deletes_its_memories() {
        let db = sea_test_db().await;
        for id in ["gone", "kept"] {
            db.write(async |tx| create_project(tx, project_row(id, 1)).await)
                .await
                .unwrap();
        }
        execute_for_tests(
            &db,
            "INSERT INTO memories (id, scope_type, scope_id, key, content, created_at, updated_at)
                 VALUES ('m1', 'project', 'gone', 'k', 'v', 1, 1),
                        ('m2', 'project', 'kept', 'k', 'v', 1, 1),
                        ('m3', 'client_global', 'gone', 'k', 'v', 1, 1)",
        )
        .await
        .unwrap();

        assert_eq!(db.write(async |tx| delete_project(tx, "gone").await).await.unwrap(), 1);
        assert_eq!(get_project(&db, "gone").await.unwrap(), None);
        let left: Vec<_> = memory::list_all(&db).await.unwrap().into_iter().map(|m| m.id).collect();
        assert_eq!(left.len(), 2);
        assert!(!left.contains(&"m1".to_string()), "{left:?}");
        assert_eq!(db.write(async |tx| delete_project(tx, "gone").await).await.unwrap(), 0);
    }
}

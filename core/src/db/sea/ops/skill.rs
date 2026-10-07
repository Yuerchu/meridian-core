//! Reading and writing `skills`, the index of skill directories.
//!
//! No function here opens a transaction of its own: a write takes the caller's
//! `WriteTx`, and the caller's `Db::write` is the `BEGIN IMMEDIATE`.

use sea_orm::ActiveValue::Unchanged;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ActiveModelTrait, ColumnTrait, DbErr, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder};

use crate::db::entity::skill;
use crate::db::entity::skill::SkillChangeset;
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};

pub async fn list_skills(db: &impl Read) -> Result<Vec<skill::Model>, DbErr> {
    skill::Entity::find()
        .order_by_asc(skill::Column::DirName)
        .all(db.conn()?)
        .await
}

pub async fn get_skill(db: &impl Read, dir_name: &str) -> Result<Option<skill::Model>, DbErr> {
    skill::Entity::find_by_id(dir_name).one(db.conn()?).await
}

fn not_found(dir_name: &str) -> DbErr {
    DbErr::RecordNotFound(format!("skill `{dir_name}`"))
}

/// Insert the row, or refresh what a rescan owns on the existing one: the
/// LLM-facing fields, the display fields the caller carried over, the source
/// and the hash. `is_enabled` and `created_at` are the user's and the first
/// sighting's, and an upsert leaves them alone.
pub async fn upsert_skill(tx: &WriteTx, model: skill::Model) -> Result<skill::Model, DbErr> {
    let dir_name = model.dir_name.clone();
    skill::Entity::insert(model.into_active_model())
        .on_conflict(
            OnConflict::column(skill::Column::DirName)
                .update_columns([
                    skill::Column::LlmName,
                    skill::Column::LlmDescription,
                    skill::Column::DisplayName,
                    skill::Column::DisplayDescription,
                    skill::Column::Source,
                    skill::Column::IsBuiltin,
                    skill::Column::MtimeHash,
                    skill::Column::UpdatedAt,
                ])
                .to_owned(),
        )
        .exec_without_returning(tx.conn()?)
        .await?;
    get_skill(tx, &dir_name).await?.ok_or_else(|| not_found(&dir_name))
}

/// `RecordNotFound` when there is no such row, before anything is written.
pub async fn update_skill(tx: &WriteTx, dir_name: &str, changeset: SkillChangeset) -> Result<skill::Model, DbErr> {
    let existing = get_skill(tx, dir_name).await?.ok_or_else(|| not_found(dir_name))?;
    let mut row = changeset.into_active_model();
    row.dir_name = Unchanged(existing.dir_name);
    row.update(tx.conn()?).await
}

/// How many rows went: 0 for a skill that was already gone.
pub async fn delete_skill(tx: &WriteTx, dir_name: &str) -> Result<u64, DbErr> {
    Ok(skill::Entity::delete_by_id(dir_name)
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

/// Drop every row whose directory is not in `present`; their bindings go with
/// them by cascade.
pub async fn delete_missing(tx: &WriteTx, present: &[String]) -> Result<u64, DbErr> {
    Ok(skill::Entity::delete_many()
        .filter(skill::Column::DirName.is_not_in(present.iter().map(String::as_str)))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::entity::skill::SkillSource;
    use crate::db::sea::cap::Db;
    use crate::db::sea::sea_test_db;
    use crate::db::types::SqlBool;

    pub(crate) fn make_skill(dir_name: &str, llm_name: &str) -> skill::Model {
        skill::Model {
            dir_name: dir_name.into(),
            llm_name: llm_name.into(),
            llm_description: "Does a thing".into(),
            display_name: "A Skill".into(),
            display_description: None,
            source: SkillSource::User,
            is_enabled: SqlBool::TRUE,
            is_builtin: SqlBool::FALSE,
            mtime_hash: Some("abc".into()),
            created_at: 1000,
            updated_at: 1000,
        }
    }

    async fn upsert(db: &Db, model: skill::Model) -> skill::Model {
        db.write(async |tx| upsert_skill(tx, model).await).await.unwrap()
    }

    #[tokio::test]
    async fn upsert_inserts_then_updates() {
        let db = sea_test_db().await;
        let created = upsert(&db, make_skill("my-skill", "my-skill")).await;
        assert_eq!(created.llm_description, "Does a thing");

        let mut changed = make_skill("my-skill", "my-skill");
        changed.llm_description = "Does another thing".into();
        changed.created_at = 2000;
        changed.updated_at = 2000;
        let updated = upsert(&db, changed).await;

        assert_eq!(updated.llm_description, "Does another thing");
        assert_eq!(updated.updated_at, 2000);
        assert_eq!(updated.created_at, 1000, "the first sighting stays the creation time");
        assert_eq!(list_skills(&db).await.unwrap().len(), 1, "upsert must not duplicate");
    }

    #[tokio::test]
    async fn upsert_preserves_user_toggled_enabled_state() {
        let db = sea_test_db().await;
        upsert(&db, make_skill("s", "s")).await;
        db.write(async |tx| {
            update_skill(
                tx,
                "s",
                SkillChangeset {
                    is_enabled: Some(SqlBool::FALSE),
                    ..Default::default()
                },
            )
            .await
        })
        .await
        .unwrap();

        upsert(&db, make_skill("s", "s")).await;

        assert!(!get_skill(&db, "s").await.unwrap().unwrap().is_enabled.get());
    }

    #[tokio::test]
    async fn list_is_sorted_and_delete_missing_removes_only_absent_dirs() {
        let db = sea_test_db().await;
        for dir in ["zebra", "alpha", "middle"] {
            upsert(&db, make_skill(dir, dir)).await;
        }
        let names: Vec<_> = list_skills(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.dir_name)
            .collect();
        assert_eq!(names, ["alpha", "middle", "zebra"]);

        let present = vec!["middle".to_string()];
        assert_eq!(
            db.write(async |tx| delete_missing(tx, &present).await).await.unwrap(),
            2
        );
        let names: Vec<_> = list_skills(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.dir_name)
            .collect();
        assert_eq!(names, ["middle"]);
        assert_eq!(db.write(async |tx| delete_skill(tx, "middle").await).await.unwrap(), 1);
    }
}

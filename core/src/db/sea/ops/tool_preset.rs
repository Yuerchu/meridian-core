//! Reading and writing `tool_presets`.
//!
//! No function here opens a transaction of its own: a write takes the caller's
//! `WriteTx`, and the caller's `Db::write` is the `BEGIN IMMEDIATE`.

use sea_orm::ActiveValue::Unchanged;
use sea_orm::{ActiveModelTrait, DbErr, EntityTrait, IntoActiveModel, QueryOrder};

use crate::db::entity::tool_preset;
use crate::db::entity::tool_preset::ToolPresetChangeset;
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};

pub async fn list_presets(db: &impl Read) -> Result<Vec<tool_preset::Model>, DbErr> {
    tool_preset::Entity::find()
        .order_by_asc(tool_preset::Column::SortOrder)
        .order_by_asc(tool_preset::Column::Id)
        .all(db.conn()?)
        .await
}

pub async fn get_preset(db: &impl Read, id: &str) -> Result<Option<tool_preset::Model>, DbErr> {
    tool_preset::Entity::find_by_id(id).one(db.conn()?).await
}

fn not_found(id: &str) -> DbErr {
    DbErr::RecordNotFound(format!("tool preset `{id}`"))
}

/// Inserts the row the caller built and reads it back.
pub async fn create_preset(tx: &WriteTx, model: tool_preset::Model) -> Result<tool_preset::Model, DbErr> {
    let id = model.id.clone();
    tool_preset::Entity::insert(model.into_active_model())
        .exec_without_returning(tx.conn()?)
        .await?;
    get_preset(tx, &id).await?.ok_or_else(|| not_found(&id))
}

/// `RecordNotFound` when there is no such row, before anything is written.
pub async fn update_preset(
    tx: &WriteTx,
    id: &str,
    changeset: ToolPresetChangeset,
) -> Result<tool_preset::Model, DbErr> {
    let existing = get_preset(tx, id).await?.ok_or_else(|| not_found(id))?;
    let mut row = changeset.into_active_model();
    row.id = Unchanged(existing.id);
    row.update(tx.conn()?).await
}

/// How many rows went: 0 for an id that was already gone.
pub async fn delete_preset(tx: &WriteTx, id: &str) -> Result<u64, DbErr> {
    Ok(tool_preset::Entity::delete_by_id(id)
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::cap::Db;
    use crate::db::sea::{execute_for_tests, sea_test_db};
    use crate::db::types::{Json, SqlBool};

    fn preset(id: &str, sort_order: i32) -> tool_preset::Model {
        tool_preset::Model {
            id: id.to_owned(),
            name: id.to_owned(),
            description: Some("d".into()),
            icon: None,
            tool_names: Json(vec!["read_file".into(), "glob".into()]),
            is_builtin: SqlBool::FALSE,
            sort_order,
            created_at: 1,
            updated_at: 1,
        }
    }

    async fn insert(db: &Db, model: tool_preset::Model) -> tool_preset::Model {
        db.write(async |tx| create_preset(tx, model).await).await.unwrap()
    }

    #[tokio::test]
    async fn a_created_row_reads_back_and_lists_in_order() {
        let db = sea_test_db().await;
        let written = preset("b", 1);
        assert_eq!(insert(&db, written.clone()).await, written);
        insert(&db, preset("a", 0)).await;
        let ids: Vec<_> = list_presets(&db).await.unwrap().into_iter().map(|p| p.id).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(get_preset(&db, "missing").await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_update_writes_only_what_it_names() {
        let db = sea_test_db().await;
        insert(&db, preset("p", 0)).await;
        let updated = db
            .write(async |tx| {
                update_preset(
                    tx,
                    "p",
                    ToolPresetChangeset {
                        description: Some(None),
                        tool_names: Some(Json(vec!["glob".into()])),
                        ..Default::default()
                    },
                )
                .await
            })
            .await
            .unwrap();
        assert_eq!(updated.description, None);
        assert_eq!(*updated.tool_names, ["glob"]);
        assert_eq!(updated.name, "p");

        assert_eq!(db.write(async |tx| delete_preset(tx, "p").await).await.unwrap(), 1);
        let missing = db
            .write(async |tx| update_preset(tx, "p", ToolPresetChangeset::default()).await)
            .await;
        assert!(matches!(missing, Err(DbErr::RecordNotFound(_))), "{missing:?}");
    }

    /// An allow-list that does not parse is not an empty allow-list.
    #[tokio::test]
    async fn a_malformed_list_fails_the_read() {
        let db = sea_test_db().await;
        insert(&db, preset("p", 0)).await;
        execute_for_tests(&db, "UPDATE tool_presets SET tool_names = 'read_file'")
            .await
            .unwrap();
        assert!(get_preset(&db, "p").await.is_err());
        assert!(list_presets(&db).await.is_err());
    }
}

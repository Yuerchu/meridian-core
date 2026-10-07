//! Reading and writing `custom_tools`.
//!
//! No function here opens a transaction of its own: a write takes the caller's
//! `WriteTx`, and the caller's `Db::write` is the `BEGIN IMMEDIATE`.

use sea_orm::ActiveValue::Unchanged;
use sea_orm::{ActiveModelTrait, ColumnTrait, DbErr, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder};

use crate::db::entity::custom_tool;
use crate::db::entity::custom_tool::CustomToolChangeset;
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::types::SqlBool;

pub async fn list_tools(db: &impl Read) -> Result<Vec<custom_tool::Model>, DbErr> {
    custom_tool::Entity::find()
        .order_by_asc(custom_tool::Column::SortOrder)
        .order_by_asc(custom_tool::Column::Id)
        .all(db.conn()?)
        .await
}

pub async fn list_enabled_tools(db: &impl Read) -> Result<Vec<custom_tool::Model>, DbErr> {
    custom_tool::Entity::find()
        .filter(custom_tool::Column::IsEnabled.eq(SqlBool::TRUE))
        .order_by_asc(custom_tool::Column::SortOrder)
        .order_by_asc(custom_tool::Column::Id)
        .all(db.conn()?)
        .await
}

async fn get_tool(db: &impl Read, id: &str) -> Result<custom_tool::Model, DbErr> {
    custom_tool::Entity::find_by_id(id)
        .one(db.conn()?)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound(format!("custom tool `{id}`")))
}

/// Inserts the row the caller built and reads it back.
pub async fn create_tool(tx: &WriteTx, model: custom_tool::Model) -> Result<custom_tool::Model, DbErr> {
    let id = model.id.clone();
    custom_tool::Entity::insert(model.into_active_model())
        .exec_without_returning(tx.conn()?)
        .await?;
    get_tool(tx, &id).await
}

/// `RecordNotFound` when there is no such row, before anything is written.
pub async fn update_tool(tx: &WriteTx, id: &str, changeset: CustomToolChangeset) -> Result<custom_tool::Model, DbErr> {
    let existing = get_tool(tx, id).await?;
    let mut row = changeset.into_active_model();
    row.id = Unchanged(existing.id);
    row.update(tx.conn()?).await
}

/// How many rows went: 0 for an id that was already gone.
pub async fn delete_tool(tx: &WriteTx, id: &str) -> Result<u64, DbErr> {
    Ok(custom_tool::Entity::delete_by_id(id)
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::entity::tool_category;
    use crate::db::sea::cap::Db;
    use crate::db::sea::ops::tool_category::create_category;
    use crate::db::sea::{execute_for_tests, sea_test_db};
    use crate::db::types::Json;
    use crate::tools::Permission;

    fn tool(id: &str, sort_order: i32, enabled: bool, category: Option<&str>) -> custom_tool::Model {
        custom_tool::Model {
            id: id.to_owned(),
            name: id.to_owned(),
            description: "d".into(),
            category_id: category.map(str::to_owned),
            parameters_schema: Json(serde_json::json!({"type": "object"}).as_object().unwrap().clone()),
            command: "echo".into(),
            args_template: Some("{{x}}".into()),
            working_directory: None,
            timeout_ms: Some(1000),
            permission: Permission::Never,
            is_enabled: enabled.into(),
            sort_order,
            created_at: 1,
            updated_at: 1,
        }
    }

    async fn insert(db: &Db, model: custom_tool::Model) -> custom_tool::Model {
        db.write(async |tx| create_tool(tx, model).await).await.unwrap()
    }

    #[tokio::test]
    async fn a_created_row_reads_back_and_lists_filter_and_sort() {
        let db = sea_test_db().await;
        let written = tool("b", 1, true, None);
        assert_eq!(insert(&db, written.clone()).await, written);
        insert(&db, tool("a", 2, false, None)).await;
        insert(&db, tool("c", 0, true, None)).await;

        let all: Vec<_> = list_tools(&db).await.unwrap().into_iter().map(|t| t.id).collect();
        assert_eq!(all, ["c", "b", "a"]);
        let enabled: Vec<_> = list_enabled_tools(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(enabled, ["c", "b"]);
    }

    #[tokio::test]
    async fn an_update_writes_only_what_it_names() {
        let db = sea_test_db().await;
        insert(&db, tool("t", 0, true, None)).await;
        let updated = db
            .write(async |tx| {
                update_tool(
                    tx,
                    "t",
                    CustomToolChangeset {
                        args_template: Some(None),
                        permission: Some(Permission::Ask),
                        ..Default::default()
                    },
                )
                .await
            })
            .await
            .unwrap();
        assert_eq!(updated.args_template, None);
        assert_eq!(updated.permission, Permission::Ask);
        assert_eq!(updated.timeout_ms, Some(1000));
    }

    /// The foreign key is `ON DELETE SET NULL`: losing a category leaves its
    /// tools in place, uncategorised.
    #[tokio::test]
    async fn deleting_a_category_uncategorises_its_tools() {
        let db = sea_test_db().await;
        db.write(async |tx| {
            create_category(
                tx,
                tool_category::Model {
                    id: "cat".into(),
                    name: "Cat".into(),
                    description: None,
                    icon: None,
                    sort_order: 0,
                    created_at: 1,
                },
            )
            .await
        })
        .await
        .unwrap();
        insert(&db, tool("t", 0, true, Some("cat"))).await;
        execute_for_tests(&db, "DELETE FROM tool_categories").await.unwrap();
        assert_eq!(list_tools(&db).await.unwrap()[0].category_id, None);
    }

    #[tokio::test]
    async fn a_malformed_row_fails_the_read() {
        for (column, value) in [("parameters_schema", "'[]'"), ("permission", "'sometimes'")] {
            let db = sea_test_db().await;
            insert(&db, tool("t", 0, true, None)).await;
            execute_for_tests(&db, &format!("UPDATE custom_tools SET {column} = {value}"))
                .await
                .unwrap();
            assert!(list_enabled_tools(&db).await.is_err(), "{column} = {value} decoded");
        }
    }
}

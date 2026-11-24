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

pub async fn list(db: &impl Read) -> Result<Vec<custom_tool::Model>, DbErr> {
    custom_tool::Entity::find()
        .order_by_asc(custom_tool::Column::SortOrder)
        .order_by_asc(custom_tool::Column::Id)
        .all(db.conn()?)
        .await
}

pub async fn list_enabled(db: &impl Read) -> Result<Vec<custom_tool::Model>, DbErr> {
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
pub async fn create(tx: &WriteTx, model: custom_tool::Model) -> Result<custom_tool::Model, DbErr> {
    validate_execution(model.execution.as_ref(), model.is_enabled, model.timeout_ms)?;
    let id = model.id.clone();
    custom_tool::Entity::insert(model.into_active_model())
        .exec_without_returning(tx.conn()?)
        .await?;
    get_tool(tx, &id).await
}

/// `RecordNotFound` when there is no such row, before anything is written.
pub async fn update(tx: &WriteTx, id: &str, changeset: CustomToolChangeset) -> Result<custom_tool::Model, DbErr> {
    let existing = get_tool(tx, id).await?;
    validate_execution(
        changeset.execution.as_ref().unwrap_or(&existing.execution).as_ref(),
        changeset.is_enabled.unwrap_or(existing.is_enabled),
        changeset.timeout_ms.unwrap_or(existing.timeout_ms),
    )?;
    let mut row = changeset.into_active_model();
    row.id = Unchanged(existing.id);
    row.update(tx.conn()?).await
}

fn validate_execution(
    execution: Option<&crate::db::types::Json<crate::tools::custom::CustomToolExecution>>,
    enabled: SqlBool,
    timeout_ms: Option<i32>,
) -> Result<(), DbErr> {
    if let Some(execution) = execution {
        execution.validate().map_err(DbErr::Custom)?;
        if timeout_ms.is_some_and(|timeout| timeout <= 0) {
            return Err(DbErr::Custom("Custom tool timeout must be positive".into()));
        }
    } else if enabled.get() {
        return Err(DbErr::Custom(
            "Legacy custom tools cannot be enabled without a safe execution definition".into(),
        ));
    }
    Ok(())
}

/// How many rows went: 0 for an id that was already gone.
pub async fn delete(tx: &WriteTx, id: &str) -> Result<u64, DbErr> {
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
    use crate::db::sea::ops::tool_category as tool_category_ops;
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
            execution: Some(Json(crate::tools::custom::CustomToolExecution::Executable {
                program: "echo".into(),
                args: vec![],
            })),
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
        db.write(async |tx| create(tx, model).await).await.unwrap()
    }

    #[tokio::test]
    async fn legacy_drafts_remain_editable_but_cannot_be_enabled() {
        let db = sea_test_db().await;
        let mut legacy = tool("legacy", 0, false, None);
        legacy.execution = None;
        insert(&db, legacy).await;
        let changed = db
            .write(async |tx| {
                update(
                    tx,
                    "legacy",
                    CustomToolChangeset {
                        command: Some("printf '%s'".into()),
                        args_template: Some(Some("\"{{x}}\"".into())),
                        ..Default::default()
                    },
                )
                .await
            })
            .await
            .unwrap();
        assert_eq!(changed.command, "printf '%s'");
        assert!(!changed.is_enabled.get());
        assert!(
            db.write(async |tx| update(
                tx,
                "legacy",
                CustomToolChangeset {
                    is_enabled: Some(SqlBool::TRUE),
                    ..Default::default()
                }
            )
            .await)
                .await
                .is_err()
        );
        let converted = db
            .write(async |tx| {
                update(
                    tx,
                    "legacy",
                    CustomToolChangeset {
                        execution: Some(tool("safe", 0, true, None).execution),
                        is_enabled: Some(SqlBool::TRUE),
                        ..Default::default()
                    },
                )
                .await
            })
            .await
            .unwrap();
        assert!(converted.is_enabled.get());
        assert!(
            db.write(async |tx| update(
                tx,
                "legacy",
                CustomToolChangeset {
                    execution: Some(None),
                    ..Default::default()
                }
            )
            .await)
                .await
                .is_err()
        );
        assert!(list_enabled(&db).await.unwrap()[0].execution.is_some());
    }

    #[tokio::test]
    async fn a_created_row_reads_back_and_lists_filter_and_sort() {
        let db = sea_test_db().await;
        let written = tool("b", 1, true, None);
        assert_eq!(insert(&db, written.clone()).await, written);
        insert(&db, tool("a", 2, false, None)).await;
        insert(&db, tool("c", 0, true, None)).await;

        let all: Vec<_> = list(&db).await.unwrap().into_iter().map(|t| t.id).collect();
        assert_eq!(all, ["c", "b", "a"]);
        let enabled: Vec<_> = list_enabled(&db).await.unwrap().into_iter().map(|t| t.id).collect();
        assert_eq!(enabled, ["c", "b"]);
    }

    #[tokio::test]
    async fn an_update_writes_only_what_it_names() {
        let db = sea_test_db().await;
        insert(&db, tool("t", 0, true, None)).await;
        let updated = db
            .write(async |tx| {
                update(
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
            tool_category_ops::create(
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
        assert_eq!(list(&db).await.unwrap()[0].category_id, None);
    }

    #[tokio::test]
    async fn a_malformed_row_fails_the_read() {
        for (column, value) in [("parameters_schema", "'[]'"), ("permission", "'sometimes'")] {
            let db = sea_test_db().await;
            insert(&db, tool("t", 0, true, None)).await;
            execute_for_tests(&db, &format!("UPDATE custom_tools SET {column} = {value}"))
                .await
                .unwrap();
            assert!(list_enabled(&db).await.is_err(), "{column} = {value} decoded");
        }
    }
}

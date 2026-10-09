//! Reading and writing `assistants`.
//!
//! The shell's assistant commands run on these; the assistant reads inside
//! core's Diesel work (turn configuration, OneBot, the hook reviewer, ACP,
//! first-run seeding) still use `db::ops::assistant`, and the pairs are listed
//! in `docs/dual-impl.md`.
//!
//! No function here opens a transaction of its own: a write takes the caller's
//! `WriteTx`, and the caller's `Db::write` is the `BEGIN IMMEDIATE`.

use sea_orm::ActiveValue::Unchanged;
use sea_orm::{ActiveModelTrait, ColumnTrait, DbErr, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder};

use crate::db::entity::assistant;
use crate::db::entity::assistant::AssistantChangeset;
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};

pub async fn list_assistants(db: &impl Read) -> Result<Vec<assistant::Model>, DbErr> {
    assistant::Entity::find()
        .order_by_asc(assistant::Column::SortOrder)
        .all(db.conn()?)
        .await
}

pub async fn get_assistant(db: &impl Read, id: &str) -> Result<Option<assistant::Model>, DbErr> {
    assistant::Entity::find_by_id(id).one(db.conn()?).await
}

/// The assistant marked default, if any. More than one is not prevented by
/// the schema; the first the table yields wins, as it always has.
pub async fn get_default_assistant(db: &impl Read) -> Result<Option<assistant::Model>, DbErr> {
    assistant::Entity::find()
        .filter(assistant::Column::IsDefault.eq(crate::db::types::SqlBool::TRUE))
        .one(db.conn()?)
        .await
}

fn not_found(id: &str) -> DbErr {
    DbErr::RecordNotFound(format!("assistant `{id}`"))
}

/// Inserts the row the caller built and reads it back.
pub async fn create_assistant(tx: &WriteTx, model: assistant::Model) -> Result<assistant::Model, DbErr> {
    let id = model.id.clone();
    assistant::Entity::insert(model.into_active_model())
        .exec_without_returning(tx.conn()?)
        .await?;
    get_assistant(tx, &id).await?.ok_or_else(|| not_found(&id))
}

/// `RecordNotFound` when there is no such row, before anything is written.
pub async fn update_assistant(
    tx: &WriteTx,
    id: &str,
    changeset: AssistantChangeset,
) -> Result<assistant::Model, DbErr> {
    let existing = get_assistant(tx, id).await?.ok_or_else(|| not_found(id))?;
    let mut row = changeset.into_active_model();
    row.id = Unchanged(existing.id);
    row.update(tx.conn()?).await
}

/// How many rows went: 0 for an id that was already gone. Conversations and
/// projects that named it are set to no assistant; its skill and sticker-pack
/// bindings go with it.
pub async fn delete_assistant(tx: &WriteTx, id: &str) -> Result<u64, DbErr> {
    Ok(assistant::Entity::delete_by_id(id)
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::sea_test_db;
    use crate::db::types::{Json, SqlBool};

    fn assistant_row(id: &str, sort_order: i32) -> assistant::Model {
        assistant::Model {
            id: id.into(),
            name: id.to_uppercase(),
            description: None,
            avatar: None,
            system_prompt: "be brief".into(),
            provider_id: None,
            model_id: Some("m".into()),
            temperature: Some(0.5),
            top_p: None,
            max_tokens: None,
            is_default: SqlBool::FALSE,
            sort_order,
            created_at: 1,
            updated_at: 1,
            context_limit: 0,
            compact_keep_recent: 10,
            enabled_tools: Some(Json(vec!["read_file".into()])),
            thinking_enabled: SqlBool::FALSE,
            thinking_budget: None,
            tool_preset_id: None,
            auto_compact_enabled: SqlBool::FALSE,
        }
    }

    #[tokio::test]
    async fn the_default_assistant_is_the_one_marked_default() {
        let db = sea_test_db().await;
        assert!(get_default_assistant(&db).await.unwrap().is_none());
        let mut marked = assistant_row("b", 1);
        marked.is_default = SqlBool::TRUE;
        db.write(async |tx| {
            create_assistant(tx, assistant_row("a", 0)).await?;
            create_assistant(tx, marked).await
        })
        .await
        .unwrap();
        assert_eq!(
            get_default_assistant(&db).await.unwrap().map(|a| a.id).as_deref(),
            Some("b")
        );
    }

    #[tokio::test]
    async fn assistants_list_in_order_update_in_part_and_delete() {
        let db = sea_test_db().await;
        let written = assistant_row("b", 1);
        assert_eq!(
            db.write(async |tx| create_assistant(tx, written.clone()).await)
                .await
                .unwrap(),
            written
        );
        db.write(async |tx| create_assistant(tx, assistant_row("a", 0)).await)
            .await
            .unwrap();
        let ids: Vec<_> = list_assistants(&db).await.unwrap().into_iter().map(|a| a.id).collect();
        assert_eq!(ids, ["a", "b"]);

        let updated = db
            .write(async |tx| {
                update_assistant(
                    tx,
                    "b",
                    AssistantChangeset {
                        temperature: Some(None),
                        enabled_tools: Some(None),
                        thinking_enabled: Some(SqlBool::TRUE),
                        updated_at: Some(9),
                        ..Default::default()
                    },
                )
                .await
            })
            .await
            .unwrap();
        assert_eq!(
            (
                updated.temperature,
                updated.enabled_tools,
                updated.thinking_enabled.get(),
                updated.updated_at
            ),
            (None, None, true, 9)
        );
        assert_eq!(updated.system_prompt, "be brief", "a field left out is not written");

        let missing = db
            .write(async |tx| update_assistant(tx, "nope", AssistantChangeset::default()).await)
            .await;
        assert!(matches!(missing, Err(DbErr::RecordNotFound(_))));
        assert_eq!(db.write(async |tx| delete_assistant(tx, "b").await).await.unwrap(), 1);
        assert_eq!(get_assistant(&db, "b").await.unwrap(), None);
    }
}

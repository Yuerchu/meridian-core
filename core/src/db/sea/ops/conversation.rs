//! Reading and writing `conversations`.
//!
//! Most conversation ops still run on Diesel, inside the queue, turn and
//! plan-review transactions. A function appears here once a SeaORM root needs
//! it (the setters below are the shell's guarded conversation commands), and
//! stays a dual implementation (`docs/dual-impl.md`) until the last Diesel
//! caller has moved.

use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

use crate::db::entity::conversation;
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::types::{EpochMs, SqlBool};

/// Every conversation's id, in no particular order.
pub async fn all_ids(db: &impl Read) -> Result<Vec<String>, DbErr> {
    conversation::Entity::find()
        .select_only()
        .column(conversation::Column::Id)
        .into_tuple()
        .all(db.conn()?)
        .await
}

/// Every conversation whose working directory comes from this project, by
/// id. Archived and delegated rows count: changing or deleting the project
/// changes their next turn's directory too.
pub async fn ids_by_project(db: &impl Read, project_id: &str) -> Result<Vec<String>, DbErr> {
    conversation::Entity::find()
        .filter(conversation::Column::ProjectId.eq(project_id))
        .select_only()
        .column(conversation::Column::Id)
        .order_by_asc(conversation::Column::Id)
        .into_tuple()
        .all(db.conn()?)
        .await
}

/// `None` for an id with no row. The Diesel version answers `NotFound`
/// instead, which its callers match on; here the absence is in the type.
pub async fn get_conversation(db: &impl Read, id: &str) -> Result<Option<conversation::Model>, DbErr> {
    conversation::Entity::find_by_id(id).one(db.conn()?).await
}

/// Writes `row`'s set columns to the conversation `id`; how many rows that
/// touched, 0 for an id with no row (as the Diesel setters, which say nothing).
async fn update_columns(tx: &WriteTx, id: &str, row: conversation::ActiveModel) -> Result<u64, DbErr> {
    Ok(conversation::Entity::update_many()
        .set(row)
        .filter(conversation::Column::Id.eq(id))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

pub async fn update_assistant(
    tx: &WriteTx,
    id: &str,
    assistant_id: Option<String>,
    now: EpochMs,
) -> Result<u64, DbErr> {
    let row = conversation::ActiveModel {
        assistant_id: Set(assistant_id),
        updated_at: Set(now),
        ..Default::default()
    };
    update_columns(tx, id, row).await
}

/// The thinking level is stored as written by `StoredThinkingLevel::as_str`.
pub async fn update_reasoning_prefs(
    tx: &WriteTx,
    id: &str,
    thinking_level: Option<String>,
    fast_mode: bool,
    now: EpochMs,
) -> Result<u64, DbErr> {
    let row = conversation::ActiveModel {
        thinking_level: Set(thinking_level),
        fast_mode: Set(SqlBool::from(fast_mode)),
        updated_at: Set(now),
        ..Default::default()
    };
    update_columns(tx, id, row).await
}

/// The standing approval for ordinary edits. Its own setter, apart from the
/// mode: a mode narrows what the assistant can do, this widens what it can do
/// without asking.
pub async fn update_accept_edits(tx: &WriteTx, id: &str, accept_edits: bool, now: EpochMs) -> Result<u64, DbErr> {
    let row = conversation::ActiveModel {
        accept_edits: Set(SqlBool::from(accept_edits)),
        updated_at: Set(now),
        ..Default::default()
    };
    update_columns(tx, id, row).await
}

/// Refile the conversation under another project, or under none. For a native
/// conversation this moves what the next turn resolves its working directory
/// and file access against.
pub async fn update_project(tx: &WriteTx, id: &str, project_id: Option<String>, now: EpochMs) -> Result<u64, DbErr> {
    let row = conversation::ActiveModel {
        project_id: Set(project_id),
        updated_at: Set(now),
        ..Default::default()
    };
    update_columns(tx, id, row).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    #[tokio::test]
    async fn a_conversation_reads_back_and_a_missing_one_is_none() {
        let db = sea_test_db().await;
        execute_for_tests(
            &db,
            "INSERT INTO conversations (id, title, is_pinned, created_at, updated_at) VALUES ('c1', 'Hi', 1, 5, 6)",
        )
        .await
        .unwrap();
        let row = get_conversation(&db, "c1").await.unwrap().unwrap();
        assert_eq!(
            (row.title.as_deref(), row.is_pinned.get(), row.updated_at),
            (Some("Hi"), true, 6)
        );
        assert_eq!(get_conversation(&db, "c2").await.unwrap(), None);
        assert_eq!(all_ids(&db).await.unwrap(), ["c1"]);
    }

    /// Each setter writes its own columns and the clock, and nothing else; a
    /// missing id touches nothing.
    #[tokio::test]
    async fn each_setter_writes_only_its_columns() {
        let db = sea_test_db().await;
        execute_for_tests(
            &db,
            "INSERT INTO assistants (id, name, created_at, updated_at) VALUES ('a1', 'A', 1, 1);
             INSERT INTO projects (id, name, created_at, updated_at) VALUES ('p1', 'P', 1, 1);
             INSERT INTO conversations (id, title, assistant_id, project_id, thinking_level, fast_mode,
                     accept_edits, created_at, updated_at)
                 VALUES ('c1', 'Hi', 'a1', 'p1', 'high', 1, 0, 1, 1)",
        )
        .await
        .unwrap();
        let before = get_conversation(&db, "c1").await.unwrap().unwrap();

        db.write(async |tx| {
            update_assistant(tx, "c1", None, 2).await?;
            update_reasoning_prefs(tx, "c1", None, false, 3).await?;
            update_accept_edits(tx, "c1", true, 4).await?;
            update_project(tx, "c1", None, 5).await
        })
        .await
        .unwrap();
        let after = get_conversation(&db, "c1").await.unwrap().unwrap();
        assert_eq!(
            after,
            conversation::Model {
                assistant_id: None,
                thinking_level: None,
                fast_mode: SqlBool::FALSE,
                accept_edits: SqlBool::TRUE,
                project_id: None,
                updated_at: 5,
                ..before
            }
        );

        let touched = db
            .write(async |tx| update_project(tx, "nope", Some("p1".into()), 6).await)
            .await
            .unwrap();
        assert_eq!(touched, 0);
    }
}

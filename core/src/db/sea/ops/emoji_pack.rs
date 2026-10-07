//! Reading and writing `emoji_packs` and `assistant_emoji_packs`.
//!
//! No function here opens a transaction of its own: a write takes the caller's
//! `WriteTx`, and the caller's `Db::write` is the `BEGIN IMMEDIATE`.

use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ColumnTrait, DbErr, EntityTrait, IntoActiveModel, JoinType, QueryFilter, QueryOrder, QuerySelect, RelationTrait,
    Set,
};

use crate::db::entity::emoji_pack::EmojiPackKind;
use crate::db::entity::{assistant_emoji_pack, emoji_pack};
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::types::EpochMs;

pub async fn list_packs(db: &impl Read) -> Result<Vec<emoji_pack::Model>, DbErr> {
    emoji_pack::Entity::find()
        .order_by_asc(emoji_pack::Column::SortOrder)
        .order_by_asc(emoji_pack::Column::Id)
        .all(db.conn()?)
        .await
}

pub async fn get_pack(db: &impl Read, id: &str) -> Result<Option<emoji_pack::Model>, DbErr> {
    emoji_pack::Entity::find_by_id(id).one(db.conn()?).await
}

/// Inserts the row the caller built and reads it back.
pub async fn create_pack(tx: &WriteTx, model: emoji_pack::Model) -> Result<emoji_pack::Model, DbErr> {
    let id = model.id.clone();
    emoji_pack::Entity::insert(model.into_active_model())
        .exec_without_returning(tx.conn()?)
        .await?;
    get_pack(tx, &id)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound(format!("emoji pack `{id}`")))
}

/// How many rows went: 0 for a pack that was already gone. Its stickers and
/// assignments go with it by cascade.
pub async fn delete_pack(tx: &WriteTx, id: &str) -> Result<u64, DbErr> {
    Ok(emoji_pack::Entity::delete_by_id(id)
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

/// The packs assigned to an assistant, in display order.
pub async fn list_packs_for_assistant(db: &impl Read, assistant_id: &str) -> Result<Vec<emoji_pack::Model>, DbErr> {
    emoji_pack::Entity::find()
        .join(JoinType::InnerJoin, emoji_pack::Relation::AssistantEmojiPack.def())
        .filter(assistant_emoji_pack::Column::AssistantId.eq(assistant_id))
        .order_by_asc(emoji_pack::Column::SortOrder)
        .order_by_asc(emoji_pack::Column::Id)
        .all(db.conn()?)
        .await
}

/// Assign a pack to an assistant; assigning it twice is not an error.
pub async fn assign_pack(tx: &WriteTx, assistant_id: &str, pack_id: &str, now: EpochMs) -> Result<(), DbErr> {
    assistant_emoji_pack::Entity::insert(assistant_emoji_pack::ActiveModel {
        assistant_id: Set(assistant_id.to_owned()),
        pack_id: Set(pack_id.to_owned()),
        created_at: Set(now),
    })
    .on_conflict(
        OnConflict::columns([
            assistant_emoji_pack::Column::AssistantId,
            assistant_emoji_pack::Column::PackId,
        ])
        .do_nothing()
        .to_owned(),
    )
    .exec_without_returning(tx.conn()?)
    .await?;
    Ok(())
}

/// Remove an assignment; removing one that is not there is not an error.
pub async fn unassign_pack(tx: &WriteTx, assistant_id: &str, pack_id: &str) -> Result<(), DbErr> {
    assistant_emoji_pack::Entity::delete_by_id((assistant_id.to_owned(), pack_id.to_owned()))
        .exec(tx.conn()?)
        .await?;
    Ok(())
}

pub async fn list_assigned_pack_ids(db: &impl Read, assistant_id: &str) -> Result<Vec<String>, DbErr> {
    assistant_emoji_pack::Entity::find()
        .select_only()
        .column(assistant_emoji_pack::Column::PackId)
        .filter(assistant_emoji_pack::Column::AssistantId.eq(assistant_id))
        .order_by_asc(assistant_emoji_pack::Column::PackId)
        .into_tuple()
        .all(db.conn()?)
        .await
}

/// The OneBot pool of one account, if it has been opened.
pub async fn get_by_source_account(db: &impl Read, account_id: &str) -> Result<Option<emoji_pack::Model>, DbErr> {
    emoji_pack::Entity::find()
        .filter(emoji_pack::Column::Kind.eq(EmojiPackKind::Onebot))
        .filter(emoji_pack::Column::SourceAccountId.eq(account_id))
        .one(db.conn()?)
        .await
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::sea::cap::Db;
    use crate::db::sea::{execute_for_tests, sea_test_db};
    use crate::db::types::SqlBool;

    pub(crate) fn pack(id: &str, kind: EmojiPackKind, account: Option<&str>, sort_order: i32) -> emoji_pack::Model {
        emoji_pack::Model {
            id: id.into(),
            name: id.into(),
            description: None,
            cover_image: None,
            is_builtin: SqlBool::FALSE,
            sort_order,
            created_at: 1,
            updated_at: 1,
            kind,
            source_account_id: account.map(str::to_owned),
        }
    }

    pub(crate) async fn insert(db: &Db, model: emoji_pack::Model) -> emoji_pack::Model {
        db.write(async |tx| create_pack(tx, model).await).await.unwrap()
    }

    #[tokio::test]
    async fn packs_list_in_order_and_assignments_are_idempotent() {
        let db = sea_test_db().await;
        execute_for_tests(
            &db,
            "INSERT INTO assistants (id, name, created_at, updated_at) VALUES ('a1', 'A', 1, 1)",
        )
        .await
        .unwrap();
        let written = pack("b", EmojiPackKind::Manual, None, 1);
        assert_eq!(insert(&db, written.clone()).await, written);
        insert(&db, pack("a", EmojiPackKind::Manual, None, 0)).await;
        let ids: Vec<_> = list_packs(&db).await.unwrap().into_iter().map(|p| p.id).collect();
        assert_eq!(ids, ["a", "b"]);

        for _ in 0..2 {
            db.write(async |tx| assign_pack(tx, "a1", "b", 5).await).await.unwrap();
        }
        assert_eq!(list_assigned_pack_ids(&db, "a1").await.unwrap(), ["b"]);
        let assigned: Vec<_> = list_packs_for_assistant(&db, "a1")
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(assigned, ["b"]);
        db.write(async |tx| unassign_pack(tx, "a1", "b").await).await.unwrap();
        assert!(list_assigned_pack_ids(&db, "a1").await.unwrap().is_empty());
    }

    /// One pool per account: the lookup finds it by kind and account, and the
    /// partial unique index refuses a second.
    #[tokio::test]
    async fn an_account_has_one_onebot_pool() {
        let db = sea_test_db().await;
        insert(&db, pack("pool", EmojiPackKind::Onebot, Some("42"), 0)).await;
        insert(&db, pack("manual", EmojiPackKind::Manual, None, 0)).await;
        assert_eq!(
            get_by_source_account(&db, "42").await.unwrap().map(|p| p.id),
            Some("pool".into())
        );
        assert_eq!(get_by_source_account(&db, "43").await.unwrap(), None);
        let second = db
            .write(async |tx| create_pack(tx, pack("again", EmojiPackKind::Onebot, Some("42"), 0)).await)
            .await;
        assert!(second.is_err());
        assert_eq!(db.write(async |tx| delete_pack(tx, "pool").await).await.unwrap(), 1);
    }
}

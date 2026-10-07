//! Reading and writing `conversations`.
//!
//! Most conversation ops still run on Diesel, inside the queue, turn and
//! plan-review transactions; a function appears here once a SeaORM root needs
//! it, and stays a dual implementation (`docs/dual-impl.md`) until the last
//! Diesel caller has moved.

use sea_orm::{DbErr, EntityTrait, QuerySelect};

use crate::db::entity::conversation;
use crate::db::sea::cap::Read;

/// Every conversation's id, in no particular order.
pub async fn all_ids(db: &impl Read) -> Result<Vec<String>, DbErr> {
    conversation::Entity::find()
        .select_only()
        .column(conversation::Column::Id)
        .into_tuple()
        .all(db.conn()?)
        .await
}

/// `None` for an id with no row. The Diesel version answers `NotFound`
/// instead, which its callers match on; here the absence is in the type.
pub async fn get_conversation(db: &impl Read, id: &str) -> Result<Option<conversation::Model>, DbErr> {
    conversation::Entity::find_by_id(id).one(db.conn()?).await
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
}

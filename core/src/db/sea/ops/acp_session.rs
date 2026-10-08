//! Which agent session a hosted conversation is, and the directory it runs in.

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter, QuerySelect};

use crate::db::entity::acp_session;
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::types::EpochMs;

/// What is on record for a conversation; `None` for one that is not hosted or
/// whose row was never written.
pub async fn get(db: &impl Read, conversation_id: &str) -> Result<Option<acp_session::Model>, DbErr> {
    acp_session::Entity::find_by_id(conversation_id).one(db.conn()?).await
}

/// Which conversation owns each agent session this app knows about, as
/// (session id, conversation id). Rows with no session id own nothing.
pub async fn owners(db: &impl Read) -> Result<Vec<(String, String)>, DbErr> {
    acp_session::Entity::find()
        .filter(acp_session::Column::AcpSessionId.is_not_null())
        .select_only()
        .column(acp_session::Column::AcpSessionId)
        .column(acp_session::Column::ConversationId)
        .into_tuple()
        .all(db.conn()?)
        .await
}

/// Write what a session opened as, the first time or the tenth. On a
/// conflict `created_at` stays, so it keeps saying when the conversation
/// first had a session; the session id written is the one the agent answered
/// with, which on a resume need not be the one asked for.
pub async fn upsert(
    tx: &WriteTx,
    conversation_id: &str,
    acp_session_id: Option<&str>,
    cwd: &str,
    now: EpochMs,
) -> Result<(), DbErr> {
    acp_session::Entity::insert(acp_session::ActiveModel {
        conversation_id: Set(conversation_id.to_owned()),
        acp_session_id: Set(acp_session_id.map(str::to_owned)),
        cwd: Set(cwd.to_owned()),
        created_at: Set(now),
        updated_at: Set(now),
    })
    .on_conflict(
        OnConflict::column(acp_session::Column::ConversationId)
            .update_columns([
                acp_session::Column::AcpSessionId,
                acp_session::Column::Cwd,
                acp_session::Column::UpdatedAt,
            ])
            .to_owned(),
    )
    .exec_without_returning(tx.conn()?)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    /// The first write creates, a later one moves the session and the
    /// directory but keeps when it was first opened; only rows with a
    /// session id own one.
    #[tokio::test]
    async fn a_session_is_written_once_and_moved_after() {
        let db = sea_test_db().await;
        execute_for_tests(
            &db,
            "INSERT INTO conversations (id, created_at, updated_at) VALUES ('c1', 1, 1), ('c2', 1, 1)",
        )
        .await
        .unwrap();
        db.write(async |tx| {
            upsert(tx, "c1", Some("s1"), "/a", 10).await?;
            upsert(tx, "c2", None, "/b", 10).await?;
            upsert(tx, "c1", Some("s2"), "/c", 20).await
        })
        .await
        .unwrap();
        let c1 = get(&db, "c1").await.unwrap().unwrap();
        assert_eq!(
            (
                c1.acp_session_id.as_deref(),
                c1.cwd.as_str(),
                c1.created_at,
                c1.updated_at
            ),
            (Some("s2"), "/c", 10, 20)
        );
        assert_eq!(owners(&db).await.unwrap(), [("s2".to_string(), "c1".to_string())]);
        assert_eq!(get(&db, "nope").await.unwrap(), None);
    }
}

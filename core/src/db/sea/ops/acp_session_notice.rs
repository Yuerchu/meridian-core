//! The incidents a hosted Claude Code session reported about itself, read
//! back with the conversation snapshot so a failure survives a reload.

use sea_orm::ActiveValue::{Set, Unchanged};
use sea_orm::{ActiveModelTrait, ColumnTrait, DbErr, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder};

use crate::db::entity::acp_session_notice;
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};

/// Record a revision of an incident, keeping only the newest: a first
/// sighting inserts, a higher revision updates in place (keeping the row id
/// the frontend already holds), anything else is a no-op and answers `None`
/// — the caller's cue to announce nothing. The read and the write share the
/// caller's `BEGIN IMMEDIATE`, since two revisions can arrive back to back.
pub async fn upsert_if_newer(
    tx: &WriteTx,
    notice: acp_session_notice::Model,
) -> Result<Option<acp_session_notice::Model>, DbErr> {
    let existing = acp_session_notice::Entity::find()
        .filter(acp_session_notice::Column::ConversationId.eq(&notice.conversation_id))
        .filter(acp_session_notice::Column::NoticeId.eq(&notice.notice_id))
        .one(tx.conn()?)
        .await?;
    match existing {
        None => {
            let id = notice.id.clone();
            acp_session_notice::Entity::insert(notice.into_active_model())
                .exec_without_returning(tx.conn()?)
                .await?;
            acp_session_notice::Entity::find_by_id(id).one(tx.conn()?).await
        }
        Some(row) if notice.revision > row.revision => {
            let updated = acp_session_notice::ActiveModel {
                id: Unchanged(row.id.clone()),
                // A revision may attach a session-scoped incident to the turn
                // it ended, but never detach one.
                turn_id: Set(notice.turn_id.or(row.turn_id)),
                revision: Set(notice.revision),
                category: Set(notice.category),
                severity: Set(notice.severity),
                title: Set(notice.title),
                details: Set(notice.details),
                reason: Set(notice.reason),
                actions: Set(notice.actions),
                updated_at: Set(notice.updated_at),
                ..Default::default()
            }
            .update(tx.conn()?)
            .await?;
            Ok(Some(updated))
        }
        Some(_) => Ok(None),
    }
}

/// Every incident on record for a conversation, oldest first.
pub async fn list_for_conversation(
    db: &impl Read,
    conversation_id: &str,
) -> Result<Vec<acp_session_notice::Model>, DbErr> {
    acp_session_notice::Entity::find()
        .filter(acp_session_notice::Column::ConversationId.eq(conversation_id))
        .order_by_asc(acp_session_notice::Column::CreatedAt)
        .order_by_asc(acp_session_notice::Column::Id)
        .all(db.conn()?)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::cap::Db;
    use crate::db::sea::{execute_for_tests, sea_test_db};
    use crate::db::types::Json;
    use crate::events::{AcpNoticeAction, AcpNoticeCategory, AcpNoticeSeverity};

    async fn with_conversations(ids: &[&str]) -> Db {
        let db = sea_test_db().await;
        for id in ids {
            execute_for_tests(
                &db,
                &format!(
                    "INSERT INTO conversations (id, title, created_at, updated_at) VALUES ('{id}', 'hosted', 0, 0)"
                ),
            )
            .await
            .unwrap();
        }
        db
    }

    fn notice(
        id: &str,
        conversation_id: &str,
        notice_id: &str,
        revision: i32,
        title: &str,
        now: i64,
    ) -> acp_session_notice::Model {
        acp_session_notice::Model {
            id: id.into(),
            conversation_id: conversation_id.into(),
            turn_id: None,
            notice_id: notice_id.into(),
            revision,
            category: AcpNoticeCategory::Limit,
            severity: AcpNoticeSeverity::Warning,
            title: title.into(),
            details: None,
            reason: None,
            actions: Json(Vec::new()),
            created_at: now,
            updated_at: now,
        }
    }

    async fn record(db: &Db, row: acp_session_notice::Model) -> Option<acp_session_notice::Model> {
        db.write(async |tx| upsert_if_newer(tx, row).await).await.unwrap()
    }

    /// A warning, then the same incident at a higher revision saying it became
    /// the failure: one row throughout, keeping its first id and first sighting.
    #[tokio::test]
    async fn a_higher_revision_updates_the_incident_in_place() {
        let db = with_conversations(&["c1"]).await;
        let first = record(
            &db,
            notice("n1", "c1", "turn-1:error", 1, "Retrying, attempt 1 of 5.", 100),
        )
        .await
        .expect("a first sighting is written");
        assert_eq!(first.revision, 1);

        let second = acp_session_notice::Model {
            severity: AcpNoticeSeverity::Error,
            actions: Json(vec![AcpNoticeAction::Retry]),
            turn_id: Some("turn-1".into()),
            ..notice("n2", "c1", "turn-1:error", 2, "Rate limit reached.", 200)
        };
        let updated = record(&db, second).await.expect("a higher revision is written");
        assert_eq!(updated.id, "n1", "the row keeps the id the first sighting minted");
        assert_eq!(
            (updated.revision, updated.title.as_str(), updated.severity),
            (2, "Rate limit reached.", AcpNoticeSeverity::Error)
        );
        assert_eq!(updated.actions, Json(vec![AcpNoticeAction::Retry]));
        assert_eq!(updated.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(
            (updated.created_at, updated.updated_at),
            (100, 200),
            "first seen stays first seen"
        );

        // A later revision that names no turn does not detach it.
        let third = record(&db, notice("n3", "c1", "turn-1:error", 3, "Still limited.", 300))
            .await
            .unwrap();
        assert_eq!(third.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(list_for_conversation(&db, "c1").await.unwrap().len(), 1);
    }

    /// The same revision again, or an older one arriving late, changes
    /// nothing and announces nothing.
    #[tokio::test]
    async fn an_equal_or_lower_revision_is_ignored() {
        let db = with_conversations(&["c1"]).await;
        record(&db, notice("n1", "c1", "s1:notice:1", 2, "Model fallback.", 100)).await;
        assert_eq!(
            record(&db, notice("n2", "c1", "s1:notice:1", 2, "Again.", 200)).await,
            None
        );
        assert_eq!(
            record(&db, notice("n3", "c1", "s1:notice:1", 1, "Older.", 300)).await,
            None
        );
        let rows = list_for_conversation(&db, "c1").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].title.as_str(), rows[0].updated_at), ("Model fallback.", 100));
    }

    /// The adapter's ids are scoped to its session: the same one in two
    /// conversations is two incidents, and incidents go with their
    /// conversation.
    #[tokio::test]
    async fn incidents_belong_to_their_conversation() {
        let db = with_conversations(&["c1", "c2"]).await;
        record(&db, notice("n1", "c1", "shared", 1, "one", 100)).await;
        record(&db, notice("n2", "c2", "shared", 1, "two", 100)).await;
        assert_eq!(list_for_conversation(&db, "c1").await.unwrap().len(), 1);
        assert_eq!(list_for_conversation(&db, "c2").await.unwrap().len(), 1);

        execute_for_tests(&db, "DELETE FROM conversations WHERE id = 'c1'")
            .await
            .unwrap();
        assert!(list_for_conversation(&db, "c1").await.unwrap().is_empty());
    }
}

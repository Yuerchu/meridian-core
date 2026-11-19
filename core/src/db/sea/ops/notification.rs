//! Reading and writing the two notification tables.
//!
//! Nothing here decides *whether* to notify — that is `notify::alert`, which
//! owns the cooldown and the fingerprint comparison. This layer only records
//! what happened, and its one rule is that raising an alert and reporting it
//! are two separate writes.
//!
//! No function here opens a transaction of its own: a write takes the caller's
//! `WriteTx`, and the caller's `Db::write` is the `BEGIN IMMEDIATE`. That is
//! what lets a command count and insert under one lock.

use sea_orm::ActiveValue::{Set, Unchanged};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DbErr, EntityTrait, IntoActiveModel, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect,
};

use crate::db::entity::notification_webhook::{NotificationWebhookChangeset, NotificationWebhookHealthChangeset};
use crate::db::entity::{notification_alert_state, notification_webhook};
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::types::{EpochMs, SqlBool};

const DELIVERY_ERROR_CLASSES: &[&str] = &[
    "http",
    "network",
    "timeout",
    "limit",
    "retry_limit",
    "build",
    "configuration",
    "vendor_rejected",
    "failed",
];

/// Data-only startup hygiene, deliberately not a numbered schema migration.
/// Leave all health counters, timestamps, configuration, NULLs and known classes
/// untouched. Running this again affects zero already-scrubbed rows.
pub async fn scrub_legacy_webhook_errors(tx: &WriteTx) -> Result<u64, DbErr> {
    Ok(notification_webhook::Entity::update_many()
        .set(notification_webhook::ActiveModel {
            last_error: Set(Some("failed".to_string())),
            ..Default::default()
        })
        .filter(notification_webhook::Column::LastError.is_not_null())
        .filter(notification_webhook::Column::LastError.is_not_in(DELIVERY_ERROR_CLASSES.iter().copied()))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

fn safe_health(mut row: notification_webhook::Model) -> notification_webhook::Model {
    if let Some(error) = &mut row.last_error {
        *error = delivery_error_class(error).to_string();
    }
    row
}

pub async fn list_webhooks(db: &impl Read) -> Result<Vec<notification_webhook::Model>, DbErr> {
    notification_webhook::Entity::find()
        .order_by_asc(notification_webhook::Column::CreatedAt)
        .order_by_asc(notification_webhook::Column::Id)
        .all(db.conn()?)
        .await
        .map(|rows| rows.into_iter().map(safe_health).collect())
}

pub async fn list_enabled_webhooks(db: &impl Read) -> Result<Vec<notification_webhook::Model>, DbErr> {
    notification_webhook::Entity::find()
        .filter(notification_webhook::Column::IsEnabled.eq(SqlBool::TRUE))
        .order_by_asc(notification_webhook::Column::CreatedAt)
        .order_by_asc(notification_webhook::Column::Id)
        .all(db.conn()?)
        .await
        .map(|rows| rows.into_iter().map(safe_health).collect())
}

pub async fn get_webhook(db: &impl Read, id: &str) -> Result<Option<notification_webhook::Model>, DbErr> {
    notification_webhook::Entity::find_by_id(id)
        .one(db.conn()?)
        .await
        .map(|row| row.map(safe_health))
}

pub async fn count_webhooks(db: &impl Read) -> Result<u64, DbErr> {
    notification_webhook::Entity::find().count(db.conn()?).await
}

fn webhook_not_found(id: &str) -> DbErr {
    DbErr::RecordNotFound(format!("notification webhook `{id}`"))
}

async fn webhook_or_not_found(tx: &WriteTx, id: &str) -> Result<notification_webhook::Model, DbErr> {
    get_webhook(tx, id).await?.ok_or_else(|| webhook_not_found(id))
}

/// Inserts the row the caller built and reads it back.
pub async fn create_webhook(
    tx: &WriteTx,
    model: notification_webhook::Model,
) -> Result<notification_webhook::Model, DbErr> {
    let id = model.id.clone();
    notification_webhook::Entity::insert(safe_health(model).into_active_model())
        .exec_without_returning(tx.conn()?)
        .await?;
    webhook_or_not_found(tx, &id).await
}

/// `RecordNotFound` when there is no such row, before anything is written.
pub async fn update_webhook(
    tx: &WriteTx,
    id: &str,
    changeset: NotificationWebhookChangeset,
) -> Result<notification_webhook::Model, DbErr> {
    let existing = webhook_or_not_found(tx, id).await?;
    let mut row = changeset.into_active_model();
    row.id = Unchanged(existing.id);
    row.update(tx.conn()?).await.map(safe_health)
}

/// The number of rows removed: zero for an id nothing had.
pub async fn delete_webhook(tx: &WriteTx, id: &str) -> Result<u64, DbErr> {
    Ok(notification_webhook::Entity::delete_by_id(id)
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

/// Record one delivery attempt against the endpoint that made it.
///
/// Success clears the error and the counter; failure keeps the last successful
/// timestamp, because "it worked at 09:00 and has failed since" is the sentence
/// somebody needs and clearing it would leave only "it is failing".
pub async fn record_delivery_attempt(tx: &WriteTx, id: &str, now: EpochMs, error: Option<&str>) -> Result<(), DbErr> {
    // Only the counter, not the row: a delivery is recorded against an
    // endpoint whatever state its other columns are in.
    let previous: i32 = notification_webhook::Entity::find_by_id(id)
        .select_only()
        .column(notification_webhook::Column::ConsecutiveFailures)
        .into_tuple()
        .one(tx.conn()?)
        .await?
        .ok_or_else(|| webhook_not_found(id))?;
    let changeset = match error {
        None => NotificationWebhookHealthChangeset {
            last_attempt_at: Some(now),
            last_success_at: Some(Some(now)),
            last_error: Some(None),
            consecutive_failures: Some(0),
            updated_at: Some(now),
        },
        Some(message) => NotificationWebhookHealthChangeset {
            last_attempt_at: Some(now),
            last_success_at: None,
            last_error: Some(Some(delivery_error_class(message).to_owned())),
            consecutive_failures: Some(previous.saturating_add(1)),
            updated_at: Some(now),
        },
    };
    let mut row = changeset.into_active_model();
    row.id = Unchanged(id.to_owned());
    notification_webhook::Entity::update(row)
        .exec_without_returning(tx.conn()?)
        .await?;
    Ok(())
}

pub fn delivery_error_class(message: &str) -> &'static str {
    DELIVERY_ERROR_CLASSES
        .iter()
        .copied()
        .find(|class| *class == message)
        .unwrap_or("failed")
}

pub async fn get_alert_state(
    db: &impl Read,
    alert_key: &str,
) -> Result<Option<notification_alert_state::Model>, DbErr> {
    notification_alert_state::Entity::find_by_id(alert_key)
        .one(db.conn()?)
        .await
}

/// Note that the condition is still true, without claiming anybody was told.
///
/// `last_notified_at` is carried across untouched — that separation is the
/// whole point of having two writes. A changed fingerprint is stored so the
/// policy can see that this is a different (usually worse) situation than the
/// one already reported.
pub async fn record_raised(
    tx: &WriteTx,
    alert_key: &str,
    fingerprint: &str,
    now: EpochMs,
) -> Result<notification_alert_state::Model, DbErr> {
    match get_alert_state(tx, alert_key).await? {
        Some(existing) => {
            let row = notification_alert_state::ActiveModel {
                alert_key: Unchanged(existing.alert_key),
                last_raised_at: Set(now),
                fingerprint: Set(fingerprint.to_owned()),
                ..Default::default()
            };
            notification_alert_state::Entity::update(row)
                .exec_without_returning(tx.conn()?)
                .await?;
        }
        None => {
            let row = notification_alert_state::ActiveModel {
                alert_key: Set(alert_key.to_owned()),
                first_raised_at: Set(now),
                last_raised_at: Set(now),
                last_notified_at: Set(None),
                fingerprint: Set(fingerprint.to_owned()),
            };
            notification_alert_state::Entity::insert(row)
                .exec_without_returning(tx.conn()?)
                .await?;
        }
    }
    get_alert_state(tx, alert_key)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound(format!("notification alert `{alert_key}`")))
}

/// Somebody was actually told.
///
/// Called only after at least one enabled endpoint accepted the delivery. The
/// watcher this replaces marked an alert as announced before the send and had
/// to guard the "nothing is connected" case by hand; keeping the write here
/// means the guard is the call site not being reached.
pub async fn record_notified(tx: &WriteTx, alert_key: &str, now: EpochMs) -> Result<u64, DbErr> {
    let row = notification_alert_state::ActiveModel {
        last_notified_at: Set(Some(now)),
        ..Default::default()
    };
    Ok(notification_alert_state::Entity::update_many()
        .set(row)
        .filter(notification_alert_state::Column::AlertKey.eq(alert_key))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

/// The condition cleared, so the next occurrence is a new alert.
///
/// Deleting rather than flagging: an absent row and a resolved row would be two
/// spellings of one state, and the policy would have to handle both.
pub async fn clear_alert(tx: &WriteTx, alert_key: &str) -> Result<u64, DbErr> {
    Ok(notification_alert_state::Entity::delete_by_id(alert_key)
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn startup_scrubs_legacy_webhook_errors_on_disk() {
        use crate::db::entity::notification_webhook;
        use crate::db::sea::cap::sealed::Access;
        use crate::keyring::SuppliedPassphraseStore;
        use crate::secrets::SecretsManager;
        use sea_orm::EntityTrait;
        use std::sync::Arc;

        crate::logging::init_early();
        let dir = tempfile::tempdir().unwrap();
        crate::db::sea::bridge::previous_release_file_with(
            &dir.path().join("meridian.db"),
            crate::db::sea::legacy::LEGACY.len(),
            "INSERT INTO notification_webhooks
                (id, name, url, format, events, last_error, consecutive_failures, created_at, updated_at)
             VALUES ('legacy-hook', 'kept name', 'https://example.invalid/kept-config', 'generic', '[]',
                     'http 500: legacy-secret-body', 3, 10, 20)",
        )
        .await
        .unwrap();
        for _ in 0..2 {
            let store = SuppliedPassphraseStore::new("a passphrase for the test only", "test").unwrap();
            let secrets = Arc::new(SecretsManager::new_with_keyring_store(
                dir.path().to_path_buf(),
                Arc::new(store),
            ));
            let services = crate::bootstrap::bootstrap_with_secrets(
                dir.path().to_path_buf(),
                crate::events::EventBus::new(),
                secrets,
            )
            .await
            .unwrap();
            // Read raw storage: a sanitized getter would hide a missing startup hook.
            let row = services
                .db
                .read(async |tx| {
                    notification_webhook::Entity::find_by_id("legacy-hook")
                        .one(tx.conn()?)
                        .await
                })
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.last_error.as_deref(), Some("failed"));
            assert_eq!(row.name, "kept name");
            assert_eq!(row.url, "https://example.invalid/kept-config");
            assert_eq!(row.consecutive_failures, 3);
            assert_eq!(row.updated_at, 20);
        }
    }

    use super::*;
    use crate::db::entity::notification_webhook::{
        BodyTemplate, NotificationEventKind, NotificationEvents, NotificationFormat,
    };
    use crate::db::sea::cap::Db;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    fn hook(id: &str, format: NotificationFormat, template: Option<serde_json::Value>) -> notification_webhook::Model {
        notification_webhook::Model {
            id: id.to_owned(),
            name: "test".into(),
            url: "https://example.invalid/hook".into(),
            format,
            events: NotificationEvents::from(vec![NotificationEventKind::BalanceLow]),
            is_enabled: SqlBool::TRUE,
            body_template: template.map(BodyTemplate::from),
            last_attempt_at: None,
            last_success_at: None,
            last_error: None,
            consecutive_failures: 0,
            created_at: 1,
            updated_at: 1,
        }
    }

    async fn insert(db: &Db, model: notification_webhook::Model) -> notification_webhook::Model {
        db.write(async |tx| create_webhook(tx, model).await).await.unwrap()
    }

    async fn attempt(db: &Db, id: &str, now: EpochMs, error: Option<&str>) {
        db.write(async |tx| record_delivery_attempt(tx, id, now, error).await)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_created_row_reads_back_as_it_was_written() {
        let db = sea_test_db().await;
        let template = serde_json::json!({"title": "{{title}}"});
        let written = insert(&db, hook("hook-1", NotificationFormat::Custom, Some(template.clone()))).await;
        assert_eq!(written, hook("hook-1", NotificationFormat::Custom, Some(template)));
        assert_eq!(get_webhook(&db, "hook-1").await.unwrap(), Some(written));
        assert_eq!(get_webhook(&db, "hook-9").await.unwrap(), None);
        assert_eq!(count_webhooks(&db).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_failure_keeps_the_last_success_and_counts_up() {
        let db = sea_test_db().await;
        insert(&db, hook("hook-1", NotificationFormat::Generic, None)).await;

        attempt(&db, "hook-1", 100, None).await;
        attempt(&db, "hook-1", 200, Some("boom")).await;
        attempt(&db, "hook-1", 300, Some("boom again")).await;

        let row = get_webhook(&db, "hook-1").await.unwrap().unwrap();
        assert_eq!(row.consecutive_failures, 2);
        assert_eq!(row.last_attempt_at, Some(300));
        assert_eq!(
            row.last_success_at,
            Some(100),
            "'it worked at 09:00 and has failed since' is the useful sentence"
        );
        assert_eq!(row.last_error.as_deref(), Some("failed"));

        attempt(&db, "hook-1", 400, None).await;
        let row = get_webhook(&db, "hook-1").await.unwrap().unwrap();
        assert_eq!(row.consecutive_failures, 0);
        assert_eq!(row.last_error, None);

        let missing = db
            .write(async |tx| record_delivery_attempt(tx, "hook-9", 500, None).await)
            .await;
        assert!(matches!(missing, Err(DbErr::RecordNotFound(_))), "{missing:?}");
    }

    /// The invariant the whole feature rests on: raising is not reporting.
    #[tokio::test]
    async fn raising_an_alert_does_not_mark_it_reported() {
        let db = sea_test_db().await;
        let raise = async |fingerprint: &str, now: EpochMs| {
            db.write(async |tx| record_raised(tx, "balance:p1", fingerprint, now).await)
                .await
                .unwrap()
        };

        let state = raise("low:CNY:3", 100).await;
        assert_eq!(state.first_raised_at, 100);
        assert_eq!(state.last_notified_at, None, "nobody has been told yet");

        let state = raise("low:CNY:2", 200).await;
        assert_eq!(state.first_raised_at, 100, "the first sighting is kept");
        assert_eq!(state.last_raised_at, 200);
        assert_eq!(state.fingerprint, "low:CNY:2");
        assert_eq!(state.last_notified_at, None);

        let notified = db
            .write(async |tx| record_notified(tx, "balance:p1", 250).await)
            .await
            .unwrap();
        assert_eq!(notified, 1);
        let state = raise("low:CNY:1", 300).await;
        assert_eq!(
            state.last_notified_at,
            Some(250),
            "raising again must not erase that somebody was told"
        );

        let cleared = db.write(async |tx| clear_alert(tx, "balance:p1").await).await.unwrap();
        assert_eq!(cleared, 1);
        assert!(get_alert_state(&db, "balance:p1").await.unwrap().is_none());
        assert_eq!(
            db.write(async |tx| record_notified(tx, "balance:p1", 400).await)
                .await
                .unwrap(),
            0,
            "a cleared alert has no row to mark"
        );
    }

    #[tokio::test]
    async fn legacy_error_scrub_is_idempotent_and_preserves_other_fields() {
        let db = sea_test_db().await;
        for (id, error) in [
            ("legacy", "http 500: token-in-path/body"),
            ("known", "vendor_rejected"),
            ("empty", ""),
        ] {
            insert(
                &db,
                hook(id, NotificationFormat::Custom, Some(serde_json::json!({"keep": true}))),
            )
            .await;
            attempt(&db, id, 42, Some("network")).await;
            db.write(async |tx| {
                notification_webhook::Entity::update_many()
                    .set(notification_webhook::ActiveModel {
                        last_error: Set(Some(error.to_string())),
                        ..Default::default()
                    })
                    .filter(notification_webhook::Column::Id.eq(id))
                    .exec(tx.conn()?)
                    .await
            })
            .await
            .unwrap();
        }
        insert(&db, hook("null", NotificationFormat::Generic, None)).await;
        let before = db
            .read(async |tx| {
                notification_webhook::Entity::find()
                    .order_by_asc(notification_webhook::Column::Id)
                    .all(tx.conn()?)
                    .await
            })
            .await
            .unwrap();
        assert_eq!(
            db.write(async |tx| scrub_legacy_webhook_errors(tx).await)
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            db.write(async |tx| scrub_legacy_webhook_errors(tx).await)
                .await
                .unwrap(),
            0
        );
        let after = db
            .read(async |tx| {
                notification_webhook::Entity::find()
                    .order_by_asc(notification_webhook::Column::Id)
                    .all(tx.conn()?)
                    .await
            })
            .await
            .unwrap();
        assert_eq!(before.len(), after.len());
        for (mut old, new) in before.into_iter().zip(after) {
            if old.id == "legacy" || old.id == "empty" {
                old.last_error = Some("failed".into());
            }
            assert_eq!(old, new);
        }
    }

    #[tokio::test]
    async fn legacy_error_is_never_exposed_by_health_reads() {
        let db = sea_test_db().await;
        insert(&db, hook("old", NotificationFormat::Generic, None)).await;
        execute_for_tests(&db, "UPDATE notification_webhooks SET last_error = 'legacy-sentinel'")
            .await
            .unwrap();
        for row in [
            get_webhook(&db, "old").await.unwrap().unwrap(),
            list_webhooks(&db).await.unwrap().remove(0),
            list_enabled_webhooks(&db).await.unwrap().remove(0),
        ] {
            assert_eq!(row.last_error.as_deref(), Some("failed"));
        }
    }

    #[test]
    fn only_fixed_classes_are_persisted() {
        assert_eq!(
            delivery_error_class("https://user:secret@example.invalid/path-token?query=secret"),
            "failed"
        );
        assert_eq!(delivery_error_class("network"), "network");
    }

    /// Lists come back in creation order, with the id as the tie-break, and
    /// only the enabled filter separates the two.
    #[tokio::test]
    async fn lists_are_ordered_by_creation_and_filtered_by_the_flag() {
        let db = sea_test_db().await;
        let mut late = hook("a-late", NotificationFormat::Generic, None);
        late.created_at = 5;
        let mut off = hook("b-off", NotificationFormat::Slack, None);
        off.is_enabled = SqlBool::FALSE;
        insert(&db, late).await;
        insert(&db, off).await;
        insert(&db, hook("c-early", NotificationFormat::Feishu, None)).await;

        let ids = |rows: Vec<notification_webhook::Model>| rows.into_iter().map(|row| row.id).collect::<Vec<_>>();
        assert_eq!(ids(list_webhooks(&db).await.unwrap()), ["b-off", "c-early", "a-late"]);
        assert_eq!(ids(list_enabled_webhooks(&db).await.unwrap()), ["c-early", "a-late"]);
    }

    /// `Some(None)` clears the template and `None` leaves it alone, and a
    /// cleared column reads back as NULL rather than as `"null"`.
    #[tokio::test]
    async fn a_doubly_wrapped_none_clears_the_template_and_a_single_one_keeps_it() {
        let db = sea_test_db().await;
        let template = serde_json::json!({"title": "{{title}}"});
        insert(&db, hook("hook-1", NotificationFormat::Custom, Some(template.clone()))).await;

        let kept = db
            .write(async |tx| {
                update_webhook(
                    tx,
                    "hook-1",
                    NotificationWebhookChangeset {
                        name: Some("renamed".into()),
                        updated_at: Some(2),
                        ..Default::default()
                    },
                )
                .await
            })
            .await
            .unwrap();
        assert_eq!(kept.name, "renamed");
        assert_eq!(kept.updated_at, 2);
        assert_eq!(kept.body_template, Some(BodyTemplate::from(template)));

        let cleared = db
            .write(async |tx| {
                update_webhook(
                    tx,
                    "hook-1",
                    NotificationWebhookChangeset {
                        format: Some(NotificationFormat::Slack),
                        body_template: Some(None),
                        updated_at: Some(3),
                        ..Default::default()
                    },
                )
                .await
            })
            .await
            .unwrap();
        assert_eq!(cleared.format, NotificationFormat::Slack);
        assert_eq!(cleared.body_template, None);
        assert_eq!(
            get_webhook(&db, "hook-1").await.unwrap().unwrap().body_template,
            None,
            "the clear reached the row"
        );

        let missing = db
            .write(async |tx| update_webhook(tx, "hook-9", NotificationWebhookChangeset::default()).await)
            .await;
        assert!(matches!(missing, Err(DbErr::RecordNotFound(_))), "{missing:?}");
        assert_eq!(
            db.write(async |tx| delete_webhook(tx, "hook-1").await).await.unwrap(),
            1
        );
        assert_eq!(
            db.write(async |tx| delete_webhook(tx, "hook-1").await).await.unwrap(),
            0
        );
    }

    /// A row whose JSON columns will not decode fails the read. Read as an
    /// empty list it would silently unsubscribe the endpoint; read as `{}` it
    /// would post an empty document and be recorded as delivered.
    #[tokio::test]
    async fn a_row_with_unreadable_json_fails_the_list_rather_than_narrowing_it() {
        let db = sea_test_db().await;
        insert(&db, hook("hook-1", NotificationFormat::Generic, None)).await;
        insert(&db, hook("hook-2", NotificationFormat::Generic, None)).await;

        execute_for_tests(
            &db,
            "UPDATE notification_webhooks SET events = 'not json' WHERE id = 'hook-2'",
        )
        .await
        .unwrap();
        let error = list_webhooks(&db).await.unwrap_err().to_string();
        assert!(error.contains("malformed notification events"), "{error}");
        assert!(list_enabled_webhooks(&db).await.is_err());
        assert!(get_webhook(&db, "hook-2").await.is_err());
        assert!(
            get_webhook(&db, "hook-1").await.is_ok(),
            "the healthy row still reads alone"
        );

        execute_for_tests(
            &db,
            "UPDATE notification_webhooks SET events = '[\"balance_low\",\"balance_low\"]' WHERE id = 'hook-2'",
        )
        .await
        .unwrap();
        let error = list_webhooks(&db).await.unwrap_err().to_string();
        assert!(error.contains("repeats"), "{error}");

        execute_for_tests(
            &db,
            "UPDATE notification_webhooks SET events = '[\"balance_low\"]', body_template = '{not json' WHERE id = 'hook-2'",
        )
        .await
        .unwrap();
        let error = list_webhooks(&db).await.unwrap_err().to_string();
        assert!(error.contains("malformed body template"), "{error}");

        // The delivery record still lands on such a row: it reads one column.
        attempt(&db, "hook-2", 10, Some("refused")).await;
        execute_for_tests(
            &db,
            "UPDATE notification_webhooks SET body_template = NULL WHERE id = 'hook-2'",
        )
        .await
        .unwrap();
        let row = get_webhook(&db, "hook-2").await.unwrap().unwrap();
        assert_eq!(row.consecutive_failures, 1);
        assert_eq!(row.last_error.as_deref(), Some("failed"));
    }
}

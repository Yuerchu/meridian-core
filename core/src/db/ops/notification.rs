//! Reading and writing the two notification tables.
//!
//! Nothing here decides *whether* to notify — that is `notify::alert`, which
//! owns the cooldown and the fingerprint comparison. This layer only records
//! what happened, and its one rule is that raising an alert and reporting it
//! are two separate writes.

use diesel::SqliteConnection;
use diesel::prelude::*;

use crate::db::models::notification::*;
use crate::db::schema::{notification_alert_state, notification_webhooks};

pub fn list_webhooks(conn: &mut SqliteConnection) -> QueryResult<Vec<NotificationWebhookRow>> {
    notification_webhooks::table
        .order((notification_webhooks::created_at.asc(), notification_webhooks::id.asc()))
        .select(NotificationWebhookRow::as_select())
        .load(conn)
}

pub fn list_enabled_webhooks(conn: &mut SqliteConnection) -> QueryResult<Vec<NotificationWebhookRow>> {
    notification_webhooks::table
        .filter(notification_webhooks::is_enabled.eq(1))
        .order((notification_webhooks::created_at.asc(), notification_webhooks::id.asc()))
        .select(NotificationWebhookRow::as_select())
        .load(conn)
}

pub fn get_webhook(conn: &mut SqliteConnection, id: &str) -> QueryResult<NotificationWebhookRow> {
    notification_webhooks::table
        .find(id)
        .select(NotificationWebhookRow::as_select())
        .first(conn)
}

pub fn count_webhooks(conn: &mut SqliteConnection) -> QueryResult<i64> {
    notification_webhooks::table.count().get_result(conn)
}

pub fn create_webhook(
    conn: &mut SqliteConnection,
    insert: &NotificationWebhookInsert<'_>,
) -> QueryResult<NotificationWebhookRow> {
    diesel::insert_into(notification_webhooks::table)
        .values(insert)
        .execute(conn)?;
    get_webhook(conn, insert.id)
}

pub fn update_webhook(
    conn: &mut SqliteConnection,
    id: &str,
    changeset: &NotificationWebhookChangeset,
) -> QueryResult<NotificationWebhookRow> {
    diesel::update(notification_webhooks::table.find(id))
        .set(changeset)
        .execute(conn)?;
    get_webhook(conn, id)
}

pub fn delete_webhook(conn: &mut SqliteConnection, id: &str) -> QueryResult<usize> {
    diesel::delete(notification_webhooks::table.find(id)).execute(conn)
}

/// Record one delivery attempt against the endpoint that made it.
///
/// Success clears the error and the counter; failure keeps the last successful
/// timestamp, because "it worked at 09:00 and has failed since" is the sentence
/// somebody needs and clearing it would leave only "it is failing".
pub fn record_delivery_attempt(
    conn: &mut SqliteConnection,
    id: &str,
    now: i64,
    error: Option<&str>,
) -> QueryResult<()> {
    conn.immediate_transaction(|conn| {
        let previous: i32 = notification_webhooks::table
            .find(id)
            .select(notification_webhooks::consecutive_failures)
            .first(conn)?;
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
                last_error: Some(Some(truncate_error(message))),
                consecutive_failures: Some(previous.saturating_add(1)),
                updated_at: Some(now),
            },
        };
        diesel::update(notification_webhooks::table.find(id))
            .set(&changeset)
            .execute(conn)?;
        Ok(())
    })
}

/// Upstream error bodies can be long, and this column is read in a list. The
/// cut is on a character boundary because a byte slice through UTF-8 panics.
fn truncate_error(message: &str) -> String {
    const LIMIT: usize = 500;
    if message.chars().count() <= LIMIT {
        return message.to_string();
    }
    message.chars().take(LIMIT).collect::<String>() + "…"
}

pub fn get_alert_state(conn: &mut SqliteConnection, alert_key: &str) -> QueryResult<Option<NotificationAlertStateRow>> {
    notification_alert_state::table
        .find(alert_key)
        .select(NotificationAlertStateRow::as_select())
        .first(conn)
        .optional()
}

/// Note that the condition is still true, without claiming anybody was told.
///
/// `last_notified_at` is carried across untouched — that separation is the
/// whole point of having two writes. A changed fingerprint is stored so the
/// policy can see that this is a different (usually worse) situation than the
/// one already reported.
pub fn record_raised(
    conn: &mut SqliteConnection,
    alert_key: &str,
    fingerprint: &str,
    now: i64,
) -> QueryResult<NotificationAlertStateRow> {
    conn.immediate_transaction(|conn| {
        match get_alert_state(conn, alert_key)? {
            Some(_) => {
                diesel::update(notification_alert_state::table.find(alert_key))
                    .set((
                        notification_alert_state::last_raised_at.eq(now),
                        notification_alert_state::fingerprint.eq(fingerprint),
                    ))
                    .execute(conn)?;
            }
            None => {
                diesel::insert_into(notification_alert_state::table)
                    .values(&NotificationAlertStateInsert {
                        alert_key,
                        first_raised_at: now,
                        last_raised_at: now,
                        last_notified_at: None,
                        fingerprint,
                    })
                    .execute(conn)?;
            }
        }
        notification_alert_state::table
            .find(alert_key)
            .select(NotificationAlertStateRow::as_select())
            .first(conn)
    })
}

/// Somebody was actually told.
///
/// Called only after at least one enabled endpoint accepted the delivery. The
/// watcher this replaces marked an alert as announced before the send and had
/// to guard the "nothing is connected" case by hand; keeping the write here
/// means the guard is the call site not being reached.
pub fn record_notified(conn: &mut SqliteConnection, alert_key: &str, now: i64) -> QueryResult<usize> {
    diesel::update(notification_alert_state::table.find(alert_key))
        .set(notification_alert_state::last_notified_at.eq(Some(now)))
        .execute(conn)
}

/// The condition cleared, so the next occurrence is a new alert.
///
/// Deleting rather than flagging: an absent row and a resolved row would be two
/// spellings of one state, and the policy would have to handle both.
pub fn clear_alert(conn: &mut SqliteConnection, alert_key: &str) -> QueryResult<usize> {
    diesel::delete(notification_alert_state::table.find(alert_key)).execute(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    fn insert(conn: &mut SqliteConnection, id: &str) -> NotificationWebhookRow {
        create_webhook(
            conn,
            &NotificationWebhookInsert {
                id,
                name: "test",
                url: "https://example.invalid/hook",
                format: "generic",
                events: r#"["balance_low"]"#,
                is_enabled: 1,
                body_template: None,
                created_at: 1,
                updated_at: 1,
            },
        )
        .unwrap()
    }

    #[test]
    fn a_failure_keeps_the_last_success_and_counts_up() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        insert(&mut conn, "hook-1");

        record_delivery_attempt(&mut conn, "hook-1", 100, None).unwrap();
        record_delivery_attempt(&mut conn, "hook-1", 200, Some("boom")).unwrap();
        record_delivery_attempt(&mut conn, "hook-1", 300, Some("boom again")).unwrap();

        let row = get_webhook(&mut conn, "hook-1").unwrap();
        assert_eq!(row.consecutive_failures, 2);
        assert_eq!(row.last_attempt_at, Some(300));
        assert_eq!(
            row.last_success_at,
            Some(100),
            "'it worked at 09:00 and has failed since' is the useful sentence"
        );
        assert_eq!(row.last_error.as_deref(), Some("boom again"));

        record_delivery_attempt(&mut conn, "hook-1", 400, None).unwrap();
        let row = get_webhook(&mut conn, "hook-1").unwrap();
        assert_eq!(row.consecutive_failures, 0);
        assert_eq!(row.last_error, None);
    }

    /// The invariant the whole feature rests on: raising is not reporting.
    #[test]
    fn raising_an_alert_does_not_mark_it_reported() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();

        let state = record_raised(&mut conn, "balance:p1", "low:CNY:3", 100).unwrap();
        assert_eq!(state.first_raised_at, 100);
        assert_eq!(state.last_notified_at, None, "nobody has been told yet");

        let state = record_raised(&mut conn, "balance:p1", "low:CNY:2", 200).unwrap();
        assert_eq!(state.first_raised_at, 100, "the first sighting is kept");
        assert_eq!(state.last_raised_at, 200);
        assert_eq!(state.fingerprint, "low:CNY:2");
        assert_eq!(state.last_notified_at, None);

        record_notified(&mut conn, "balance:p1", 250).unwrap();
        let state = record_raised(&mut conn, "balance:p1", "low:CNY:1", 300).unwrap();
        assert_eq!(
            state.last_notified_at,
            Some(250),
            "raising again must not erase that somebody was told"
        );

        clear_alert(&mut conn, "balance:p1").unwrap();
        assert!(get_alert_state(&mut conn, "balance:p1").unwrap().is_none());
    }

    #[test]
    fn a_long_upstream_error_is_cut_on_a_character_boundary() {
        let long = "错".repeat(600);
        let cut = truncate_error(&long);
        assert_eq!(cut.chars().count(), 501);
        assert!(cut.ends_with('…'));
        assert_eq!(truncate_error("short"), "short");
    }
}

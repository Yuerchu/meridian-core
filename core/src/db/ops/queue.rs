//! Reading and writing the prompt queue, on Diesel: what the remaining
//! Diesel turn roots (the prompt rows of `chat` and the hosted session) still
//! share. Everything else moved to `db::sea::ops::queue` (`docs/dual-impl.md`).

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::entity::queued_prompt;
use crate::db::models::queue::{Delivery, QueuedPromptInsert, QueuedPromptRow};
use crate::db::schema::queued_prompts;

/// A Diesel row as the entity model; a stored delivery mode this build cannot
/// read fails the read, as it does on the SeaORM side.
fn model(row: QueuedPromptRow) -> QueryResult<queued_prompt::Model> {
    queued_prompt::Model::try_from(row).map_err(contract_error)
}

fn contract_error(message: String) -> diesel::result::Error {
    diesel::result::Error::QueryBuilderError(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, message)))
}

/// Everything queued for a conversation, in the order it will be delivered.
///
/// Includes settled rows: the front end draws them as they leave, and dropping
/// them here would make an item vanish a beat before its message appears.
/// Callers that only want work use `next_pending`.
#[cfg(test)]
pub(super) fn list(conn: &mut SqliteConnection, conversation_id: &str) -> QueryResult<Vec<queued_prompt::Model>> {
    queued_prompts::table
        .filter(queued_prompts::conversation_id.eq(conversation_id))
        .order((queued_prompts::position.asc(), queued_prompts::created_at.asc()))
        .select(QueuedPromptRow::as_select())
        .load::<QueuedPromptRow>(conn)?
        .into_iter()
        .map(model)
        .collect()
}

/// Add one to the back.
///
/// `position` is one past whatever is there now rather than a count of the
/// rows: settled rows keep their positions, so counting would collide with the
/// last live one.
///
/// **Immediate, not deferred.** Reading the last position and inserting one
/// past it is a read-then-write, and a deferred transaction takes its write
/// lock at the insert — by which time another enqueue may have read the same
/// maximum and be about to claim the same number. Two rows sharing a position
/// leaves their order to whatever the query planner feels like, in the one
/// feature whose entire promise is that the instructions run in the order they
/// were written. `BEGIN IMMEDIATE` takes the lock before the read instead.
pub fn enqueue(
    conn: &mut SqliteConnection,
    id: &str,
    conversation_id: &str,
    content: &str,
    delivery: Delivery,
    now: i64,
) -> QueryResult<queued_prompt::Model> {
    enqueue_with_context(conn, id, conversation_id, content, delivery, &[], now)
}

pub(super) fn enqueue_with_context(
    conn: &mut SqliteConnection,
    id: &str,
    conversation_id: &str,
    content: &str,
    delivery: Delivery,
    context: &[crate::workspace::reference::PreparedContextItem],
    now: i64,
) -> QueryResult<queued_prompt::Model> {
    conn.immediate_transaction(|conn| {
        enqueue_with_context_in_transaction(conn, id, conversation_id, content, delivery, context, now)
    })
}

/// Enqueue within the caller's immediate transaction. The plan-review guard
/// holds that lock across its barrier check and this write; opening another
/// immediate transaction here would fail with `AlreadyInTransaction`.
/// The caller must propagate errors so a failed context write rolls back the
/// prompt alongside its snapshots.
pub fn enqueue_with_context_in_transaction(
    conn: &mut SqliteConnection,
    id: &str,
    conversation_id: &str,
    content: &str,
    delivery: Delivery,
    context: &[crate::workspace::reference::PreparedContextItem],
    now: i64,
) -> QueryResult<queued_prompt::Model> {
    // The same rule `set_delivery` keeps for the steer button, kept where the
    // row is written so it holds for every caller rather than for the one
    // command that happens to check first.
    if delivery == Delivery::Interject && carries_attachments(content)? {
        return Err(contract_error(
            "attachments can only be queued as follow-up messages".into(),
        ));
    }
    let last: Option<i32> = queued_prompts::table
        .filter(queued_prompts::conversation_id.eq(conversation_id))
        .select(diesel::dsl::max(queued_prompts::position))
        .first(conn)?;

    diesel::insert_into(queued_prompts::table)
        .values(&QueuedPromptInsert {
            id,
            conversation_id,
            content,
            delivery: delivery.as_str(),
            position: last.unwrap_or(-1) + 1,
            created_at: now,
        })
        .execute(conn)?;

    crate::db::ops::queued_prompt_context_item::insert_prepared(conn, id, context, now)?;

    queued_prompts::table
        .find(id)
        .select(QueuedPromptRow::as_select())
        .first::<QueuedPromptRow>(conn)
        .and_then(model)
}

/// Whether a queued message carries anything besides text.
///
/// An interjection goes in mid-turn as text — `_session/steering` on a hosted
/// session, a steered message on a native one — and neither has anywhere to
/// put an image or a file. Damaged parts are an error, never "no attachments".
pub(super) fn carries_attachments(content: &str) -> QueryResult<bool> {
    let parts = crate::provider::decode_message_parts(content).map_err(contract_error)?;
    Ok(parts.is_some_and(|parts| {
        parts
            .iter()
            .any(|part| !matches!(part, crate::provider::MessageContentPart::Text { .. }))
    }))
}

/// Stop the queue, because the turn in front of it did not finish.
///
/// Everything still waiting, not just the head: the instructions were written
/// as a sequence, and the ones after a failure rest on the same assumption the
/// failed step broke. "Now rename that function" means nothing if the function
/// was never created.
///
/// **Everything unsettled, including a row already claimed**, and that is the
/// one filter it is tempting to add back. A claim is not a delivery: `steer`
/// marks a row dispatched *before* asking the adapter, and the answer can be
/// `promptRequired` — the agent saying it did not take the message — which
/// [`undispatch`] puts back on the queue. Skipping claimed rows, a hold landing
/// inside that window is missed by the only row it was about: the turn fails,
/// `hold_all` steps over the claimed item, `undispatch` returns it as plainly
/// `Queued`, and the instruction runs on a premise that died — with no
/// `queue_release` from anybody.
///
/// Marking it costs nothing while it is claimed, because [`QueueState`] reads
/// `dispatched_at` first and an in-doubt row is already a barrier. The flag only
/// starts meaning something at the moment `undispatch` clears the dispatch,
/// which is exactly when it should.
pub(super) fn hold_all(conn: &mut SqliteConnection, conversation_id: &str, now: i64) -> QueryResult<usize> {
    diesel::update(
        queued_prompts::table
            .filter(queued_prompts::conversation_id.eq(conversation_id))
            .filter(queued_prompts::settled_at.is_null())
            .filter(queued_prompts::held_at.is_null()),
    )
    .set(queued_prompts::held_at.eq(Some(now)))
    .execute(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::diesel_test_db;

    #[test]
    fn an_unknown_delivery_mode_is_rejected() {
        let pool = diesel_test_db();
        let mut conn = pool.get().unwrap();
        crate::db::ops::conversation::create_conversation(&mut conn, "c1", Some("q"), None, None, 0).unwrap();
        let first = enqueue(&mut conn, "q1", "c1", "one", Delivery::FollowUp, 0).unwrap();
        diesel::update(queued_prompts::table.find(&first.id))
            .set(queued_prompts::delivery.eq("pre_empt_everything"))
            .execute(&mut conn)
            .unwrap();

        let error = list(&mut conn, "c1").unwrap_err();
        assert!(error.to_string().contains("unknown queue delivery mode"));
    }
}

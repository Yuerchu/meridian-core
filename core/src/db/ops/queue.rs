//! Reading and writing the prompt queue.
//!
//! The one function here that matters is [`take_next`]. Everything else is
//! bookkeeping around it.

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::message::MessageInsert;
use crate::db::models::queue::{Delivery, QueueState, QueuedPromptInsert, QueuedPromptRow};
use crate::db::schema::queued_prompts;

fn contract_error(message: String) -> diesel::result::Error {
    diesel::result::Error::QueryBuilderError(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, message)))
}

/// Everything queued for a conversation, in the order it will be delivered.
///
/// Includes settled rows: the front end draws them as they leave, and dropping
/// them here would make an item vanish a beat before its message appears.
/// Callers that only want work use [`next_deliverable`].
pub fn list(conn: &mut SqliteConnection, conversation_id: &str) -> QueryResult<Vec<QueuedPromptRow>> {
    queued_prompts::table
        .filter(queued_prompts::conversation_id.eq(conversation_id))
        .order((queued_prompts::position.asc(), queued_prompts::created_at.asc()))
        .select(QueuedPromptRow::as_select())
        .load(conn)
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
) -> QueryResult<QueuedPromptRow> {
    enqueue_with_context(conn, id, conversation_id, content, delivery, &[], now)
}

pub fn enqueue_with_context(
    conn: &mut SqliteConnection,
    id: &str,
    conversation_id: &str,
    content: &str,
    delivery: Delivery,
    context: &[crate::workspace::reference::PreparedContextItem],
    now: i64,
) -> QueryResult<QueuedPromptRow> {
    conn.immediate_transaction(|conn| {
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
            .first(conn)
    })
}

/// Drop one that has not been delivered.
///
/// Refuses a settled or in-doubt row: the first has already become part of the
/// transcript, and the second is the one whose fate is unknown — deleting it
/// would destroy the only record that the agent may be about to act on it.
/// Constrained to the conversation the caller named, like every other write
/// here. A `queued_prompts.id` is a uuid so nobody is guessing one, but the
/// caller always knows which conversation it is acting on and saying so costs a
/// clause — the alternative is a command that takes a conversation id and does
/// not use it, which reads as a check and is not one.
pub fn remove(conn: &mut SqliteConnection, conversation_id: &str, id: &str) -> QueryResult<usize> {
    diesel::delete(
        queued_prompts::table
            .find(id)
            .filter(queued_prompts::conversation_id.eq(conversation_id))
            .filter(queued_prompts::dispatched_at.is_null())
            .filter(queued_prompts::settled_at.is_null()),
    )
    .execute(conn)
}

/// Rewrite the order, wholesale.
///
/// The ids are trusted to be this conversation's — the filter makes a stray one
/// a no-op rather than a way to move somebody else's row.
///
/// **Only rows that are still waiting may move, and none of them may move in
/// front of a barrier.** A `held` or `in_doubt` row stops the queue
/// ([`next_pending`]), and that is the whole of what stops a follow-up running
/// on a premise nobody has confirmed. Left free, a drag could put a queued row
/// ahead of an in-doubt one and the barrier would simply not be reached — the
/// instructions were written as a sequence, and stepping around the unresolved
/// one runs them out of order. So the barrier keeps its position and anything
/// dragged in front of it lands immediately behind it instead.
pub fn reorder(conn: &mut SqliteConnection, conversation_id: &str, ids: &[String]) -> QueryResult<()> {
    conn.transaction(|conn| {
        // The last position occupied by something the user may not move. Every
        // reordered row is placed after it, so the barrier still bites.
        let floor = list(conn, conversation_id)?
            .iter()
            .filter(|item| matches!(item.state(), QueueState::Held | QueueState::InDoubt))
            .map(|item| item.position)
            .max();

        let mut next = floor.map_or(0, |p| p + 1);
        for id in ids {
            let updated = diesel::update(
                queued_prompts::table
                    .find(id)
                    .filter(queued_prompts::conversation_id.eq(conversation_id))
                    // Undelivered only. A settled row's position means nothing
                    // and a barrier's is what everything else is measured
                    // against.
                    .filter(queued_prompts::dispatched_at.is_null())
                    .filter(queued_prompts::settled_at.is_null())
                    .filter(queued_prompts::held_at.is_null()),
            )
            .set(queued_prompts::position.eq(next))
            .execute(conn)?;
            if updated > 0 {
                next += 1;
            }
        }
        Ok(())
    })
}

/// Change one item's delivery mode while it is still waiting.
pub fn set_delivery(
    conn: &mut SqliteConnection,
    conversation_id: &str,
    id: &str,
    delivery: Delivery,
) -> QueryResult<usize> {
    diesel::update(
        queued_prompts::table
            .find(id)
            .filter(queued_prompts::conversation_id.eq(conversation_id))
            .filter(queued_prompts::dispatched_at.is_null())
            .filter(queued_prompts::settled_at.is_null()),
    )
    .set(queued_prompts::delivery.eq(delivery.as_str()))
    .execute(conn)
}

/// The front of the queue, whatever mode it is in.
///
/// Skips settled rows without skipping *past* a stopped one: a held or
/// in-doubt row hides everything behind it, because the instructions were
/// written as a sequence and delivering number three while number two is
/// unresolved runs them out of order.
///
/// This is what an idle runner asks for. Which mode an item carries only means
/// something while a turn is running — `interject` is "do not wait for the turn
/// to finish", and with no turn there is nothing to wait for. Asking for one
/// mode here instead would leave an `interject` queued after its turn had
/// already ended blocking the queue for ever, waiting to interrupt something
/// that will never run.
pub fn next_pending(conn: &mut SqliteConnection, conversation_id: &str) -> QueryResult<Option<QueuedPromptRow>> {
    for item in list(conn, conversation_id)? {
        match item.state() {
            // Already gone by, in one way or another.
            QueueState::Settled => continue,
            // The queue is stopped here and everything after it waits.
            QueueState::Held | QueueState::InDoubt => return Ok(None),
            QueueState::Queued => return Ok(Some(item)),
        }
    }
    Ok(None)
}

/// The next item a runner may deliver *in a particular mode*, if there is one.
///
/// What a runner with a turn in flight asks: it can only take an `interject`,
/// and a `follow_up` at the front is not skipped — it is still the next thing
/// the user meant to happen.
pub fn next_deliverable(
    conn: &mut SqliteConnection,
    conversation_id: &str,
    delivery: Delivery,
) -> QueryResult<Option<QueuedPromptRow>> {
    let Some(item) = next_pending(conn, conversation_id)? else {
        return Ok(None);
    };
    let stored = item.delivery().map_err(contract_error)?;
    Ok((stored == delivery).then_some(item))
}

/// Take an item off the queue *and* write the message it becomes, atomically.
///
/// This is the whole reason the queue is a table rather than a channel. The
/// process can be killed between any two statements, and there are exactly two
/// outcomes worth having:
///
///   * killed before the commit — the item is still `queued` and the message
///     row does not exist. Delivering it again is correct and safe.
///   * killed after the commit — the item is `settled` and the row exists.
///     Delivering it again would say the same thing twice.
///
/// One transaction is what removes every other outcome. Doing the two writes
/// separately leaves a window in which the message exists and the queue still
/// thinks it owes one, and the repair for that window is guesswork about
/// whether an agent has already acted.
///
/// It does *not* prove the agent saw it — see the migration on why a hosted
/// session cannot have that — only that this app's record of the two facts
/// cannot disagree.
pub fn take_next(
    conn: &mut SqliteConnection,
    conversation_id: &str,
    delivery: Delivery,
    turn_id: &str,
    message: &MessageInsert,
    parent: Option<&str>,
    now: i64,
) -> QueryResult<Option<QueuedPromptRow>> {
    // Plain, because the caller is already inside one — `take_one` has to peek
    // before it can build the row this writes, so the transaction that spans
    // the read and the write is *its*, and that is the one that is immediate.
    // A `BEGIN IMMEDIATE` here would be a nested begin, which SQLite refuses.
    conn.transaction(|conn| {
        let Some(item) = next_deliverable(conn, conversation_id, delivery)? else {
            return Ok(None);
        };

        // `append_message` rather than a bare insert: it links the row and
        // moves the conversation's head in the same breath, which is the only
        // sanctioned way to add to a transcript.
        let row = crate::db::ops::message::append_message(conn, message, parent)?;

        diesel::update(queued_prompts::table.find(&item.id))
            .set((
                queued_prompts::dispatched_at.eq(Some(now)),
                queued_prompts::dispatched_turn_id.eq(Some(turn_id)),
                queued_prompts::settled_at.eq(Some(now)),
                queued_prompts::settled_message_id.eq(Some(row.id.as_str())),
            ))
            .execute(conn)?;

        queued_prompts::table
            .find(&item.id)
            .select(QueuedPromptRow::as_select())
            .first(conn)
            .map(Some)
    })
}

/// Mark an item as handed over without a message row of our own.
///
/// The hosted case: `_session/steering` puts the text into the agent's own
/// conversation, and the row this app writes is a copy for the transcript
/// rather than the thing the agent reads. So the two cannot be made atomic with
/// respect to *the agent*, and this records the attempt before it is made —
/// which is what turns a kill in the gap into a reportable doubt instead of a
/// silent loss.
///
/// **It is a claim on the deliverable front, not on an id.** Everything a
/// runner checked before it got here — that the item is not held, that it is
/// still first, that it is still the mode that was read — can change in the
/// gap, which for a hosted steer spans a round trip to a child process. Only
/// `dispatched_at` was being re-checked, so a queue held by a person between
/// the read and the claim was delivered anyway, and so was an item somebody had
/// just dragged out of first place.
///
/// `expect` is the mode the caller intends to deliver *in*: `Some(Interject)`
/// for a steer, which is void if the user switched the row to `follow_up`, and
/// `None` for a turn of its own, where any mode is deliverable because there is
/// nothing to wait for.
///
/// Assumes the caller is inside a transaction — two of the three are, writing
/// the message row in the same one. [`super::super::agent::queue`]'s steer path
/// opens its own, because there the claim stands alone.
pub fn mark_dispatched(
    conn: &mut SqliteConnection,
    conversation_id: &str,
    id: &str,
    expect: Option<Delivery>,
    turn_id: &str,
    now: i64,
) -> QueryResult<usize> {
    // The front is the first row that is not settled — which is exactly what
    // `next_pending` walks to, since a settled row is stepped over and a held
    // or in-doubt one stops it. So being the front and being deliverable are
    // one question asked of one row.
    let Some(front) = list(conn, conversation_id)?
        .into_iter()
        .find(|item| item.state() != QueueState::Settled)
    else {
        return Ok(0);
    };
    let delivery = front.delivery().map_err(contract_error)?;
    if front.id != id || front.state() != QueueState::Queued || expect.is_some_and(|mode| delivery != mode) {
        return Ok(0);
    }

    diesel::update(
        queued_prompts::table
            .find(id)
            .filter(queued_prompts::conversation_id.eq(conversation_id))
            .filter(queued_prompts::dispatched_at.is_null()),
    )
    .set((
        queued_prompts::dispatched_at.eq(Some(now)),
        queued_prompts::dispatched_turn_id.eq(Some(turn_id)),
    ))
    .execute(conn)
}

/// And the other half: the send came back, so the doubt is resolved.
///
/// `None` for the row means "not yet" — a steered message is settled the
/// moment the agent takes it, and the transcript row arrives at the next
/// round boundary. Writing NULL over an id that `attach_message` already
/// filled in is how a concurrent `session/prompt` finish erases the link.
pub fn mark_settled(conn: &mut SqliteConnection, id: &str, message_id: Option<&str>, now: i64) -> QueryResult<usize> {
    match message_id {
        Some(message_id) => diesel::update(queued_prompts::table.find(id))
            .set((
                queued_prompts::settled_at.eq(Some(now)),
                queued_prompts::settled_message_id.eq(Some(message_id)),
            ))
            .execute(conn),
        None => diesel::update(queued_prompts::table.find(id))
            .set(queued_prompts::settled_at.eq(Some(now)))
            .execute(conn),
    }
}

/// Name the transcript row a settled item became, once it has one.
///
/// Split from [`mark_settled`] because for a steered message the two facts
/// arrive apart. The agent's `injected` is what resolves the doubt and it comes
/// back in milliseconds; the row is written at the next round boundary, which
/// is however long the tool call in flight takes. Waiting for the row to settle
/// the item would leave the queue stopped behind it for that whole time, on a
/// message that has demonstrably arrived.
pub fn attach_message(conn: &mut SqliteConnection, id: &str, message_id: &str) -> QueryResult<usize> {
    diesel::update(queued_prompts::table.find(id))
        .set(queued_prompts::settled_message_id.eq(Some(message_id)))
        .execute(conn)
}

/// Undo a dispatch the agent has told us it did not take.
///
/// The one case where clearing `dispatched_at` is safe, and it is safe for a
/// reason that does not generalise: `promptRequired` is the agent saying *it
/// did not consume the message* — evidence about the delivery itself, not the
/// absence of evidence that in-doubt exists for. Anything else (a timeout, a
/// dead pipe, an unreadable reply) says nothing about whether the agent acted,
/// and must stay in doubt.
pub fn undispatch(conn: &mut SqliteConnection, id: &str) -> QueryResult<usize> {
    diesel::update(
        queued_prompts::table
            .find(id)
            .filter(queued_prompts::settled_at.is_null()),
    )
    .set((
        queued_prompts::dispatched_at.eq(None::<i64>),
        queued_prompts::dispatched_turn_id.eq(None::<String>),
    ))
    .execute(conn)
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
pub fn hold_all(conn: &mut SqliteConnection, conversation_id: &str, now: i64) -> QueryResult<usize> {
    diesel::update(
        queued_prompts::table
            .filter(queued_prompts::conversation_id.eq(conversation_id))
            .filter(queued_prompts::settled_at.is_null())
            .filter(queued_prompts::held_at.is_null()),
    )
    .set(queued_prompts::held_at.eq(Some(now)))
    .execute(conn)
}

/// Let a held queue go again, after a person has looked at it.
pub fn release_all(conn: &mut SqliteConnection, conversation_id: &str) -> QueryResult<usize> {
    diesel::update(
        queued_prompts::table
            .filter(queued_prompts::conversation_id.eq(conversation_id))
            .filter(queued_prompts::held_at.is_not_null()),
    )
    .set(queued_prompts::held_at.eq(None::<i64>))
    .execute(conn)
}

/// Items whose delivery is in doubt and which the agent has not been told
/// about.
///
/// The same shape as `turns::unreported_for_conversation`, and settled the same
/// way: reading this is not telling anyone, so nothing is written here.
pub fn unreported_in_doubt(conn: &mut SqliteConnection, conversation_id: &str) -> QueryResult<Vec<QueuedPromptRow>> {
    queued_prompts::table
        .filter(queued_prompts::conversation_id.eq(conversation_id))
        .filter(queued_prompts::dispatched_at.is_not_null())
        .filter(queued_prompts::settled_at.is_null())
        .filter(queued_prompts::reported_at.is_null())
        .order(queued_prompts::position.asc())
        .select(QueuedPromptRow::as_select())
        .load(conn)
}

/// Record that the agent has now been told about these.
///
/// Called at the one moment that proves it — a reply read to the end — for the
/// same reason `turns::mark_reported` is. Failing to write it is a repeated
/// warning, which is the direction to fail in.
pub fn mark_reported(conn: &mut SqliteConnection, ids: &[String], now: i64) -> QueryResult<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    diesel::update(queued_prompts::table.filter(queued_prompts::id.eq_any(ids)))
        .set(queued_prompts::reported_at.eq(Some(now)))
        .execute(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    fn conversation(conn: &mut SqliteConnection, id: &str) {
        crate::db::ops::conversation::create_conversation(conn, id, Some("q"), None, None, 0).unwrap();
    }

    fn user_row<'a>(id: &'a str, conversation_id: &'a str, content: &'a str, turn_id: &'a str) -> MessageInsert<'a> {
        MessageInsert {
            id,
            conversation_id,
            role: "user",
            content,
            provider_id: None,
            model_id: None,
            input_tokens: None,
            output_tokens: None,
            tool_calls: None,
            tool_call_id: None,
            sort_order: 0,
            created_at: 0,
            reasoning_content: None,
            rating: None,
            schema_version: 2,
            is_compact_summary: 0,
            sender_id: None,
            parent_id: None,
            compact_anchor_id: None,
            source: None,
            turn_id: Some(turn_id),
            tool_outcome: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            server_tool_calls: None,
            provider_name: None,
        }
    }

    fn add(conn: &mut SqliteConnection, conv: &str, text: &str, delivery: Delivery) -> QueuedPromptRow {
        let id = uuid::Uuid::new_v4().to_string();
        enqueue(conn, &id, conv, text, delivery, 0).unwrap()
    }

    /// The state machine, stated once.
    #[test]
    fn which_timestamps_are_set_is_the_state() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");

        let item = add(&mut conn, "c1", "one", Delivery::FollowUp);
        assert_eq!(item.state(), QueueState::Queued);

        mark_dispatched(&mut conn, "c1", &item.id, None, "t1", 1).unwrap();
        let item = list(&mut conn, "c1").unwrap().remove(0);
        assert_eq!(item.state(), QueueState::InDoubt, "dispatched and unanswered");

        mark_settled(&mut conn, &item.id, Some("m1"), 2).unwrap();
        let item = list(&mut conn, "c1").unwrap().remove(0);
        assert_eq!(item.state(), QueueState::Settled);
    }

    /// The reason the queue is a table.
    ///
    /// Taking the item and writing the message it becomes is one transaction,
    /// so a kill lands on one side or the other: either the item is still
    /// queued and no row exists, or the row exists and the item is settled.
    /// There is no third outcome for a repair to have to guess at.
    #[test]
    fn taking_an_item_and_writing_its_message_cannot_come_apart() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        add(&mut conn, "c1", "do the thing", Delivery::FollowUp);

        let taken = take_next(
            &mut conn,
            "c1",
            Delivery::FollowUp,
            "t1",
            &user_row("m1", "c1", "do the thing", "t1"),
            None,
            5,
        )
        .unwrap()
        .expect("an item was waiting");

        assert_eq!(taken.state(), QueueState::Settled);
        assert_eq!(taken.settled_message_id.as_deref(), Some("m1"));
        // And the row is really there, on the conversation's path.
        let rows = crate::db::ops::message::list_messages(&mut conn, "c1").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].content, "do the thing");
    }

    /// A failed transaction leaves *nothing*: no message, and the item still
    /// deliverable. This is the "killed before the commit" side, forced by
    /// making the message write fail on a duplicate id.
    #[test]
    fn a_failed_take_leaves_the_item_deliverable_and_writes_no_row() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        add(&mut conn, "c1", "first", Delivery::FollowUp);

        // Land a row on the id the take below will try to use.
        crate::db::ops::message::append_message(&mut conn, &user_row("dup", "c1", "in the way", "t0"), None).unwrap();

        let failed = take_next(
            &mut conn,
            "c1",
            Delivery::FollowUp,
            "t1",
            &user_row("dup", "c1", "first", "t1"),
            None,
            5,
        );
        assert!(failed.is_err(), "a duplicate message id must fail the whole take");

        let item = list(&mut conn, "c1").unwrap().remove(0);
        assert_eq!(item.state(), QueueState::Queued, "the item was not consumed");
        assert_eq!(
            crate::db::ops::message::list_messages(&mut conn, "c1").unwrap().len(),
            1,
            "and nothing new was written"
        );
    }

    /// An item whose delivery is in doubt stops the queue rather than being
    /// stepped over. Delivering the one behind it would run the user's
    /// instructions out of order, and re-delivering the doubtful one itself is
    /// the thing this design exists to refuse.
    #[test]
    fn an_item_in_doubt_blocks_the_queue_and_is_never_redelivered() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        let first = add(&mut conn, "c1", "first", Delivery::FollowUp);
        add(&mut conn, "c1", "second", Delivery::FollowUp);

        mark_dispatched(&mut conn, "c1", &first.id, None, "t1", 1).unwrap();

        assert!(
            next_deliverable(&mut conn, "c1", Delivery::FollowUp).unwrap().is_none(),
            "nothing is deliverable while the head's fate is unknown"
        );
        // And it is what gets reported instead.
        let owed = unreported_in_doubt(&mut conn, "c1").unwrap();
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].id, first.id);
    }

    /// **Reordering cannot step around a barrier.**
    ///
    /// An in-doubt row stops the queue, and that is the whole of what keeps a
    /// follow-up from running on a premise nobody has confirmed. Dragging a
    /// later message in front of it would reach it first and run it — out of
    /// the order the instructions were written in, which is the one thing the
    /// queue promises.
    #[test]
    fn reordering_cannot_move_anything_in_front_of_an_unresolved_item() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        let doubtful = add(&mut conn, "c1", "delete the old migration", Delivery::FollowUp);
        let second = add(&mut conn, "c1", "then rename it", Delivery::FollowUp);
        let third = add(&mut conn, "c1", "and run the tests", Delivery::FollowUp);
        mark_dispatched(&mut conn, "c1", &doubtful.id, None, "t1", 1).unwrap();

        // What a drag of the third row to the very front asks for.
        reorder(
            &mut conn,
            "c1",
            &[third.id.clone(), doubtful.id.clone(), second.id.clone()],
        )
        .unwrap();

        let order: Vec<String> = list(&mut conn, "c1").unwrap().into_iter().map(|i| i.content).collect();
        assert_eq!(
            order,
            ["delete the old migration", "and run the tests", "then rename it"],
            "the two queued rows swapped, and neither got past the one in doubt",
        );
        assert!(
            next_pending(&mut conn, "c1").unwrap().is_none(),
            "and the queue is still stopped",
        );
    }

    /// **A claim is on the deliverable front, not on an id.**
    ///
    /// A runner reads the next item, then goes away to a child process and
    /// comes back to claim it. Everything it checked can have changed in that
    /// gap, and each of these was getting through: a queue the user held, a row
    /// they dragged out of first place, a row they switched to the other
    /// delivery mode.
    #[test]
    fn a_claim_is_refused_once_the_item_stops_being_deliverable() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");

        // Held between the read and the claim.
        let a = add(&mut conn, "c1", "first", Delivery::FollowUp);
        hold_all(&mut conn, "c1", 5).unwrap();
        assert_eq!(mark_dispatched(&mut conn, "c1", &a.id, None, "t1", 6).unwrap(), 0);
        release_all(&mut conn, "c1").unwrap();

        // No longer the front: something was dragged ahead of it.
        let b = add(&mut conn, "c1", "second", Delivery::FollowUp);
        reorder(&mut conn, "c1", &[b.id.clone(), a.id.clone()]).unwrap();
        assert_eq!(
            mark_dispatched(&mut conn, "c1", &a.id, None, "t1", 7).unwrap(),
            0,
            "the row behind the front cannot be taken out of turn"
        );

        // Switched to the other mode while a steer was in flight for it.
        set_delivery(&mut conn, "c1", &b.id, Delivery::FollowUp).unwrap();
        assert_eq!(
            mark_dispatched(&mut conn, "c1", &b.id, Some(Delivery::Interject), "t1", 8).unwrap(),
            0,
            "a steer's claim is void once the row is no longer an interjection"
        );

        // And the honest case still goes through, exactly once.
        assert_eq!(mark_dispatched(&mut conn, "c1", &b.id, None, "t1", 9).unwrap(), 1);
        assert_eq!(
            mark_dispatched(&mut conn, "c1", &b.id, None, "t2", 10).unwrap(),
            0,
            "and a second pump gets nothing"
        );

        // Another conversation's row is not claimable through this one.
        conversation(&mut conn, "c2");
        let elsewhere = add(&mut conn, "c2", "theirs", Delivery::FollowUp);
        assert_eq!(
            mark_dispatched(&mut conn, "c1", &elsewhere.id, None, "t1", 11).unwrap(),
            0
        );
    }

    /// A held queue is the same barrier by a different name — that is a person
    /// having been asked and not yet answered.
    #[test]
    fn reordering_cannot_move_anything_in_front_of_a_held_item() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        let first = add(&mut conn, "c1", "first", Delivery::FollowUp);
        let second = add(&mut conn, "c1", "second", Delivery::FollowUp);
        hold_all(&mut conn, "c1", 5).unwrap();

        reorder(&mut conn, "c1", &[second.id.clone(), first.id.clone()]).unwrap();

        let order: Vec<String> = list(&mut conn, "c1").unwrap().into_iter().map(|i| i.content).collect();
        assert_eq!(order, ["first", "second"], "a held row does not move at all");
    }

    /// Reporting is settled by a separate write, so a crash between reading and
    /// telling repeats the warning rather than losing it.
    #[test]
    fn a_doubtful_item_stays_owed_until_it_is_marked_reported() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        let item = add(&mut conn, "c1", "one", Delivery::FollowUp);
        mark_dispatched(&mut conn, "c1", &item.id, None, "t1", 1).unwrap();

        assert_eq!(unreported_in_doubt(&mut conn, "c1").unwrap().len(), 1);
        // Read again without telling anyone: still owed.
        assert_eq!(unreported_in_doubt(&mut conn, "c1").unwrap().len(), 1);

        mark_reported(&mut conn, std::slice::from_ref(&item.id), 9).unwrap();
        assert!(unreported_in_doubt(&mut conn, "c1").unwrap().is_empty());
    }

    /// A held queue is stopped, not filtered: the instructions after a failure
    /// rest on the step that failed.
    #[test]
    fn holding_stops_everything_behind_it() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        add(&mut conn, "c1", "first", Delivery::FollowUp);
        add(&mut conn, "c1", "second", Delivery::FollowUp);

        assert_eq!(hold_all(&mut conn, "c1", 3).unwrap(), 2);
        assert!(next_deliverable(&mut conn, "c1", Delivery::FollowUp).unwrap().is_none());

        release_all(&mut conn, "c1").unwrap();
        let next = next_deliverable(&mut conn, "c1", Delivery::FollowUp).unwrap();
        assert_eq!(next.map(|i| i.content), Some("first".to_string()));
    }

    /// A hold that lands while an item is claimed still applies to it.
    ///
    /// The sequence is an ordinary steer: `mark_dispatched` claims the row
    /// *before* the adapter is asked, and the turn it was meant to interrupt can
    /// fail during that round trip. Skipping claimed rows, `hold_all` steps over
    /// the only row it was about, `undispatch` returns it plainly `Queued`, and
    /// the instruction runs on a premise that died — with nobody having pressed
    /// release.
    ///
    /// Being marked costs nothing while the claim stands: `state()` reads
    /// `dispatched_at` first, so the row is `InDoubt` either way and already a
    /// barrier.
    #[test]
    fn a_hold_during_a_claim_is_not_lost_when_the_claim_comes_back() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        let steered = add(&mut conn, "c1", "actually, stop", Delivery::Interject);
        add(&mut conn, "c1", "then write it up", Delivery::FollowUp);

        // The steer claims it, and the adapter is asked.
        assert_eq!(
            mark_dispatched(&mut conn, "c1", &steered.id, Some(Delivery::Interject), "t1", 2).unwrap(),
            1
        );
        assert_eq!(list(&mut conn, "c1").unwrap()[0].state(), QueueState::InDoubt);

        // The turn it was interrupting dies while that round trip is open.
        assert_eq!(
            hold_all(&mut conn, "c1", 3).unwrap(),
            2,
            "the claimed row is held too, not stepped over"
        );

        // And the adapter answers `promptRequired`: it did not take the message.
        undispatch(&mut conn, &steered.id).unwrap();

        let rows = list(&mut conn, "c1").unwrap();
        assert_eq!(
            rows[0].state(),
            QueueState::Held,
            "back on the queue, and behind the barrier the failed turn put up"
        );
        assert!(
            next_pending(&mut conn, "c1").unwrap().is_none(),
            "nothing runs until a person releases it"
        );

        // Which is exactly what release is for, and it works on this row like
        // any other.
        release_all(&mut conn, "c1").unwrap();
        assert_eq!(
            next_pending(&mut conn, "c1").unwrap().map(|i| i.content),
            Some("actually, stop".to_string())
        );
    }

    /// Release does not resurrect an item whose delivery is still unresolved.
    ///
    /// `hold_all` now marks claimed rows, so `release_all` clears the flag on
    /// them as well — and that must not turn "we do not know whether the agent
    /// acted on this" into "send it again". `dispatched_at` outranks `held_at`
    /// in [`QueueState`], which is what keeps the two apart.
    #[test]
    fn releasing_a_held_queue_leaves_an_unresolved_item_unresolved() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        let claimed = add(&mut conn, "c1", "delete the old migration", Delivery::Interject);

        mark_dispatched(&mut conn, "c1", &claimed.id, Some(Delivery::Interject), "t1", 2).unwrap();
        hold_all(&mut conn, "c1", 3).unwrap();
        release_all(&mut conn, "c1").unwrap();

        assert_eq!(list(&mut conn, "c1").unwrap()[0].state(), QueueState::InDoubt);
        assert!(next_pending(&mut conn, "c1").unwrap().is_none());
    }

    /// The two modes are asked for separately, and an item of the other kind at
    /// the head is not skipped — it is still the next thing the user meant to
    /// happen. Answering "nothing to interject" while a follow-up waits is
    /// correct; delivering the follow-up early is not.
    #[test]
    fn the_head_belongs_to_one_mode_and_does_not_let_the_other_past() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        add(&mut conn, "c1", "after you finish", Delivery::FollowUp);
        add(&mut conn, "c1", "actually, stop", Delivery::Interject);

        assert!(
            next_deliverable(&mut conn, "c1", Delivery::Interject)
                .unwrap()
                .is_none(),
            "the interject is behind a follow-up and waits its turn"
        );
        assert_eq!(
            next_deliverable(&mut conn, "c1", Delivery::FollowUp)
                .unwrap()
                .map(|i| i.content),
            Some("after you finish".to_string())
        );
    }

    /// A settled row keeps its place in the list — the front end draws it
    /// leaving — but never blocks or is re-taken.
    #[test]
    fn settled_items_are_stepped_over() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        add(&mut conn, "c1", "first", Delivery::FollowUp);
        add(&mut conn, "c1", "second", Delivery::FollowUp);

        take_next(
            &mut conn,
            "c1",
            Delivery::FollowUp,
            "t1",
            &user_row("m1", "c1", "first", "t1"),
            None,
            5,
        )
        .unwrap()
        .unwrap();

        assert_eq!(list(&mut conn, "c1").unwrap().len(), 2, "both rows are still listed");
        assert_eq!(
            next_deliverable(&mut conn, "c1", Delivery::FollowUp)
                .unwrap()
                .map(|i| i.content),
            Some("second".to_string())
        );
    }

    /// Deleting is for things that have not gone anywhere. An in-doubt row is
    /// the record that the agent may be acting on it, and destroying that is
    /// destroying the only reason anyone would find out.
    #[test]
    fn an_item_already_sent_cannot_be_deleted() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        let item = add(&mut conn, "c1", "one", Delivery::FollowUp);

        assert_eq!(remove(&mut conn, "c1", &item.id).unwrap(), 1, "a queued one goes");

        let item = add(&mut conn, "c1", "two", Delivery::FollowUp);
        mark_dispatched(&mut conn, "c1", &item.id, None, "t1", 1).unwrap();
        assert_eq!(remove(&mut conn, "c1", &item.id).unwrap(), 0, "a doubtful one stays");
    }

    /// While a turn runs the modes are asked for separately; with nothing
    /// running the distinction has no referent, and an `interject` left over
    /// from a turn that has already ended is delivered rather than left to
    /// block the queue waiting to interrupt something that will never happen.
    #[test]
    fn an_idle_runner_takes_the_front_of_the_queue_whatever_mode_it_is() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        add(&mut conn, "c1", "actually, stop", Delivery::Interject);
        add(&mut conn, "c1", "and then this", Delivery::FollowUp);

        assert!(
            next_deliverable(&mut conn, "c1", Delivery::FollowUp).unwrap().is_none(),
            "a running turn cannot take it"
        );
        assert_eq!(
            next_pending(&mut conn, "c1").unwrap().map(|i| i.content),
            Some("actually, stop".to_string()),
            "an idle one can"
        );
    }

    /// `promptRequired` is the agent saying it did not take the message. That
    /// is evidence about the delivery, so the item goes back to deliverable —
    /// unlike every other way a send can end badly, which is an *absence* of
    /// evidence and stays in doubt for ever.
    #[test]
    fn a_refused_delivery_is_returned_to_the_queue() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        let item = add(&mut conn, "c1", "one", Delivery::Interject);

        mark_dispatched(&mut conn, "c1", &item.id, None, "t1", 1).unwrap();
        undispatch(&mut conn, &item.id).unwrap();

        let item = list(&mut conn, "c1").unwrap().remove(0);
        assert_eq!(item.state(), QueueState::Queued);
        assert!(
            item.dispatched_turn_id.is_none(),
            "and the turn it was aimed at goes too"
        );
        assert!(unreported_in_doubt(&mut conn, "c1").unwrap().is_empty());
    }

    /// A settled item that has no row yet gets one later, and settling first is
    /// the point: the queue must not stay stopped behind a message the agent
    /// has demonstrably read while a tool call finishes.
    #[test]
    fn a_row_can_be_attached_after_the_doubt_is_already_resolved() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        let first = add(&mut conn, "c1", "one", Delivery::Interject);
        add(&mut conn, "c1", "two", Delivery::Interject);

        mark_dispatched(&mut conn, "c1", &first.id, None, "t1", 1).unwrap();
        mark_settled(&mut conn, &first.id, None, 2).unwrap();
        assert_eq!(
            next_pending(&mut conn, "c1").unwrap().map(|i| i.content),
            Some("two".to_string()),
            "the queue moves on without waiting for the row"
        );

        attach_message(&mut conn, &first.id, "m1").unwrap();
        let first = list(&mut conn, "c1").unwrap().remove(0);
        assert_eq!(first.settled_message_id.as_deref(), Some("m1"));
        assert_eq!(first.state(), QueueState::Settled);
    }

    /// Steer settles without a row; the prompt path may already have written
    /// one. A second settle with `None` must not wipe that id.
    #[test]
    fn settling_without_a_row_does_not_erase_one_already_attached() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        let first = add(&mut conn, "c1", "one", Delivery::Interject);
        mark_dispatched(&mut conn, "c1", &first.id, None, "t1", 1).unwrap();
        attach_message(&mut conn, &first.id, "m1").unwrap();
        mark_settled(&mut conn, &first.id, None, 2).unwrap();
        let first = list(&mut conn, "c1").unwrap().remove(0);
        assert_eq!(first.settled_message_id.as_deref(), Some("m1"));
        assert!(first.settled_at.is_some());
    }

    #[test]
    fn an_unknown_delivery_mode_is_rejected() {
        assert_eq!(Delivery::parse("interject").unwrap(), Delivery::Interject);
        assert_eq!(Delivery::parse("follow_up").unwrap(), Delivery::FollowUp);
        assert!(Delivery::parse("pre_empt_everything").is_err());

        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        let first = add(&mut conn, "c1", "one", Delivery::FollowUp);
        diesel::update(queued_prompts::table.find(&first.id))
            .set(queued_prompts::delivery.eq("pre_empt_everything"))
            .execute(&mut conn)
            .unwrap();

        let error = next_deliverable(&mut conn, "c1", Delivery::FollowUp).unwrap_err();
        assert!(error.to_string().contains("unknown queue delivery mode"));
    }
}

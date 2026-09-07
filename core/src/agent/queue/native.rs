//! The queue, as an ordinary turn sees it.
//!
//! One type, and it is a `Steering` port: the loop drains it between rounds and
//! the queue never has to know where the turn has got to. That is the whole
//! difference from the hosted path next door, which asks an adapter and has to
//! believe the answer.
//!
//! Only interjections come through the port. A follow-up is a turn of its own,
//! and with no turn running there is nothing to hand it to — so [`pump`] starts
//! one, through the `StartTurn` the shell registered.

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::agent::engine::{Steered, SteeredOrigin, Steering};
use crate::db::DbPool;
use crate::db::models::message::MessageInsert;
use crate::db::models::queue::Delivery;
use crate::events::EventBus;
use crate::services::Services;
use crate::util::{get_conn, now_ms};

/// Give the next item a turn of its own, if there is one and nothing is running.
///
/// The mirror of the hosted pump, and shorter for one reason: while a turn is
/// running there is nothing for this to do. An interjection is taken by the
/// loop's own drain of [`Interjections`], not pushed at it — so the only case
/// left here is an idle conversation, where the front of the queue goes
/// whatever mode it carries.
pub(super) async fn pump(services: &Services, conversation_id: &str) {
    // A turn already has the conversation, and it is the one that drains
    // interjections. Starting a second would be refused by the lease anyway;
    // asking first is what keeps that refusal from looking like a failure.
    if services.turns.held_turn(conversation_id).is_some() {
        return;
    }
    let Some(starter) = services.turn_starter.get() else {
        // No desktop runner in this build. Nothing is lost: the item is still
        // queued and still on screen.
        return;
    };
    let Some(next) = super::read(services, conversation_id, false).await else {
        return;
    };

    // Spends the item in the transaction that writes its row — see
    // `StartTurn::start`. A failure leaves it queued, which is right: no row
    // was written either.
    if let Err(e) = starter.start(conversation_id, &next).await {
        tracing::warn!(error = %e, conversation_id, "a queued prompt failed as a turn");
    }
}

/// What a desktop turn is handed for `ports.steering`.
///
/// Built per turn, because both ids are: a row written here belongs to the turn
/// it interrupted, which is where the model reads it.
pub struct Interjections {
    pool: DbPool,
    conversation_id: String,
    turn_id: String,
}

impl Interjections {
    pub fn new(pool: DbPool, conversation_id: String, turn_id: String) -> Self {
        Self {
            pool,
            conversation_id,
            turn_id,
        }
    }
}

#[async_trait::async_trait]
impl Steering for Interjections {
    /// Everything queued as an interjection, in order, already written down.
    ///
    /// A loop rather than one item: several can be stacked while a round runs,
    /// and the round boundary is the only place any of them can go in. Stopping
    /// after the first would leave the rest for the next boundary, which may
    /// never come — a turn that answers without calling another tool has no
    /// more.
    async fn drain(&self) -> Vec<Steered> {
        let pool = self.pool.clone();
        let conversation_id = self.conversation_id.clone();
        let turn_id = self.turn_id.clone();

        let taken = tokio::task::spawn_blocking(move || {
            let mut conn = get_conn(&pool)?;
            let mut out = Vec::new();
            loop {
                match take_one(&mut conn, &conversation_id, &turn_id, now_ms()) {
                    Ok(Some(item)) => out.push(item),
                    Ok(None) => break,
                    // Stop rather than skip. The queue is a sequence, and the
                    // right response to not being able to read it is to deliver
                    // nothing this round — whatever is in there is still in
                    // there, and the next boundary asks again.
                    Err(e) => {
                        tracing::warn!(error = %e, conversation_id, "could not take from the queue");
                        break;
                    }
                }
            }
            Ok::<_, String>(out)
        })
        .await;

        let taken = match taken {
            Ok(Ok(items)) => items,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "the queue could not be reached mid-turn");
                Vec::new()
            }
            Err(e) => {
                tracing::warn!(error = %e, "taking from the queue panicked");
                Vec::new()
            }
        };

        taken
            .into_iter()
            .map(|(row, text)| Steered {
                text,
                // Typed by the person watching, who has no chat identity. Not
                // `System`: sent as context it would reach the model as ambient
                // noise rather than as the instruction it is.
                origin: SteeredOrigin::User(None),
                // Already written, in the same transaction that spent the
                // queue item. The loop adopts it as the cursor rather than
                // writing a second row.
                row: Some(row),
            })
            .collect()
    }
}

/// [`Interjections`], plus telling the window what it just took.
///
/// A decorator rather than a field on the port, for the same reason the port
/// exists: the turn loop drains `Steering` without knowing whether it is fed by
/// a queue or by a sub-agent's inbox, and `Interjections` is built from a pool
/// and two ids. Knowing that a delivery is worth announcing is knowing it came
/// from the queue, so it belongs to whoever already knows that.
///
/// Only a non-empty drain says anything. The loop asks at every round boundary
/// and most of them have nothing waiting.
pub struct Announcing {
    inner: Interjections,
    events: EventBus,
}

impl Announcing {
    pub fn wrap(events: EventBus, inner: Interjections) -> Self {
        Self { inner, events }
    }
}

#[async_trait::async_trait]
impl Steering for Announcing {
    async fn drain(&self) -> Vec<Steered> {
        let taken = self.inner.drain().await;
        if !taken.is_empty() {
            super::emit_delivered(&self.events, &self.inner.conversation_id);
        }
        taken
    }
}

/// Take the front of the queue and write the message it becomes, or answer that
/// there is nothing to take.
///
/// The peek and the take are one transaction. `take_next` selects the item
/// again inside it, which looks redundant and is not: the content has to be
/// known before the row can be built, and the row has to be built before
/// `take_next` can be called. Doing the peek outside would leave a window in
/// which the queue changed and the row was written with the wrong text.
///
/// The parent is the conversation's head, read in the same breath. During a
/// turn that *is* the loop's cursor — every row the engine writes goes through
/// `append_message`, which moves the head — so this stays on one path without
/// the port having to be told where the turn has got to.
fn take_one(
    conn: &mut SqliteConnection,
    conversation_id: &str,
    turn_id: &str,
    now: i64,
) -> QueryResult<Option<(String, String)>> {
    // Immediate: this is the transaction that spans the peek and the take, so
    // it is the one that has to hold the write lock across both. Deferred, it
    // takes the lock at the first write — after the peek — and two drains could
    // read the same item.
    conn.immediate_transaction(|conn| {
        let Some(item) = crate::db::ops::queue::next_deliverable(conn, conversation_id, Delivery::Interject)? else {
            return Ok(None);
        };
        let head = crate::db::ops::conversation::get_conversation(conn, conversation_id)
            .ok()
            .and_then(|c| c.head_message_id);
        let message_id = uuid::Uuid::new_v4().to_string();

        let row = MessageInsert {
            id: &message_id,
            conversation_id,
            role: "user",
            content: &item.content,
            provider_id: None,
            model_id: None,
            input_tokens: None,
            output_tokens: None,
            tool_calls: None,
            tool_call_id: None,
            sort_order: 0,
            created_at: now,
            reasoning_content: None,
            rating: None,
            schema_version: 2,
            is_compact_summary: 0,
            sender_id: None,
            parent_id: None,
            compact_anchor_id: None,
            source: None,
            // The turn it interrupted, not a turn of its own. That is where the
            // model reads it, so a reader who found it filed elsewhere would be
            // looking at a different conversation than the model was.
            turn_id: Some(turn_id),
            tool_outcome: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            server_tool_calls: None,
            provider_name: None,
        };

        let taken = crate::db::ops::queue::take_next(
            conn,
            conversation_id,
            Delivery::Interject,
            turn_id,
            &row,
            head.as_deref(),
            now,
        )?;
        Ok(taken.map(|_| (message_id, item.content)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ops::queue::{enqueue, list};
    use crate::db::test_db;

    fn conversation(conn: &mut SqliteConnection, id: &str) {
        crate::db::ops::conversation::create_conversation(conn, id, Some("q"), None, None, 0).unwrap();
    }

    fn add(conn: &mut SqliteConnection, text: &str, delivery: Delivery) {
        let id = uuid::Uuid::new_v4().to_string();
        enqueue(conn, &id, "c1", text, delivery, 0).unwrap();
    }

    /// Counts what reached the window, per channel.
    #[derive(Default)]
    struct Heard(std::sync::Mutex<Vec<(String, serde_json::Value)>>);

    impl crate::events::EventSink for Heard {
        fn emit(&self, channel: &str, payload: &serde_json::Value) -> Result<(), String> {
            self.0.lock().unwrap().push((channel.to_string(), payload.clone()));
            Ok(())
        }
    }

    /// Taking an interjection mid-turn spends the queue item and writes the row
    /// in one transaction, and until this wrapper existed nothing said so.
    ///
    /// What that cost was not subtle: the front end went on showing the item as
    /// `queued` for the rest of the turn, so the message appeared to be waiting
    /// while it had in fact already been delivered — and the delete the row was
    /// still offering came back "that message has already been sent", because
    /// `remove` refuses anything dispatched. The message it became was on no
    /// screen either, since the transcript is only re-read when the turn ends.
    #[tokio::test]
    async fn taking_an_interjection_tells_the_window_and_an_empty_round_does_not() {
        let pool = test_db();
        {
            let mut conn = pool.get().unwrap();
            conversation(&mut conn, "c1");
            add(&mut conn, "actually, stop", Delivery::Interject);
        }

        let events = EventBus::new();
        let heard = std::sync::Arc::new(Heard::default());
        events.register(heard.clone(), false);

        let port = Announcing::wrap(
            events.clone(),
            Interjections::new(pool.clone(), "c1".into(), "t1".into()),
        );

        assert_eq!(port.drain().await.len(), 1);
        {
            let seen = heard.0.lock().unwrap();
            assert_eq!(seen.len(), 1, "the delivery is announced exactly once");
            assert_eq!(seen[0].0, "queue-updated");
            // The flag is what separates this from an enqueue or a drag: only
            // this one changes the transcript, and only this one is worth a
            // snapshot of a running turn.
            assert_eq!(seen[0].1["delivered"], serde_json::json!(true));
            assert_eq!(seen[0].1["conversation_id"], serde_json::json!("c1"));
        }

        // The loop asks at every round boundary, and most of them have nothing
        // waiting. A round that took nothing must not re-read the conversation.
        assert!(port.drain().await.is_empty());
        assert_eq!(heard.0.lock().unwrap().len(), 1, "an empty round says nothing");
    }

    /// The whole port in one: interjections come out in order, already written,
    /// and a follow-up behind them stays where it is.
    #[tokio::test]
    async fn interjections_arrive_written_and_a_follow_up_waits() {
        let pool = test_db();
        {
            let mut conn = pool.get().unwrap();
            conversation(&mut conn, "c1");
            add(&mut conn, "actually, stop", Delivery::Interject);
            add(&mut conn, "and check the tests", Delivery::Interject);
            add(&mut conn, "then write it up", Delivery::FollowUp);
        }

        let port = Interjections::new(pool.clone(), "c1".into(), "t1".into());
        let drained = port.drain().await;

        assert_eq!(drained.len(), 2, "both interjections, and not the follow-up");
        assert_eq!(drained[0].text, "actually, stop");
        assert_eq!(drained[1].text, "and check the tests");
        assert!(
            drained.iter().all(|s| s.row.is_some()),
            "each one is written before it is handed over"
        );

        // The rows are really there, on the conversation's path, under the turn
        // they interrupted.
        let mut conn = pool.get().unwrap();
        let rows = crate::db::ops::message::list_messages(&mut conn, "c1").unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.turn_id.as_deref() == Some("t1")));
        assert_eq!(rows[1].parent_id.as_deref(), Some(rows[0].id.as_str()), "one path");

        // And the follow-up is untouched.
        let left: Vec<_> = list(&mut conn, "c1")
            .unwrap()
            .into_iter()
            .filter(|i| i.settled_at.is_none())
            .collect();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].content, "then write it up");
    }

    /// Draining twice does not deliver the same message twice — the item is
    /// spent in the transaction that wrote its row.
    #[tokio::test]
    async fn a_second_drain_in_the_same_turn_finds_nothing() {
        let pool = test_db();
        {
            let mut conn = pool.get().unwrap();
            conversation(&mut conn, "c1");
            add(&mut conn, "once", Delivery::Interject);
        }

        let port = Interjections::new(pool.clone(), "c1".into(), "t1".into());
        assert_eq!(port.drain().await.len(), 1);
        assert!(port.drain().await.is_empty());
        assert_eq!(
            crate::db::ops::message::list_messages(&mut pool.get().unwrap(), "c1")
                .unwrap()
                .len(),
            1,
            "and exactly one row exists for it"
        );
    }

    /// An empty queue costs one read and produces nothing, which is what
    /// happens between every round of every desktop turn.
    #[tokio::test]
    async fn an_empty_queue_is_silent() {
        let pool = test_db();
        conversation(&mut pool.get().unwrap(), "c1");
        let port = Interjections::new(pool, "c1".into(), "t1".into());
        assert!(port.drain().await.is_empty());
    }
}

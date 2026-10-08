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

use crate::agent::engine::{Steered, SteeredOrigin, Steering};
use crate::db::entity::message;
use crate::db::models::queue::Delivery;
use crate::db::sea::DbErr;
use crate::db::sea::cap::Db;
use crate::db::sea::ops::{conversation as conversation_ops, queue as queue_ops};
use crate::db::types::SqlBool;
use crate::events::EventBus;
use crate::services::Services;
use crate::util::now_ms;

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
    // A background task that ended goes first, and goes even with the queue
    // held. It is not an instruction resting on a premise the last turn may
    // have broken — it is something that happened, which the model is owed
    // before anything typed after it is answered.
    #[cfg(not(target_os = "android"))]
    if crate::background::has_wake(&services.sea, conversation_id).await {
        if let Err(e) = starter.start_unprompted(conversation_id).await {
            tracing::warn!(error = %e, conversation_id, "a background task's turn failed");
        }
        return;
    }
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
    db: Db,
    conversation_id: String,
    turn_id: String,
}

impl Interjections {
    pub fn new(db: Db, conversation_id: String, turn_id: String) -> Self {
        Self {
            db,
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
        let conversation_id = self.conversation_id.as_str();
        let mut taken = Vec::new();
        loop {
            // One instant per item: the row's `created_at` and the live
            // message's `received_at` are the same value by construction.
            let now = now_ms();
            match take_one(&self.db, conversation_id, &self.turn_id, now).await {
                Ok(Some((row, text))) => taken.push((row, text, now)),
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

        taken
            .into_iter()
            .map(|(row, text, received_at)| Steered {
                text,
                // Typed by the person watching, who has no chat identity. Not
                // `System`: sent as context it would reach the model as ambient
                // noise rather than as the instruction it is.
                origin: SteeredOrigin::User(None),
                // Already written, in the same transaction that spent the
                // queue item. The loop adopts it as the cursor rather than
                // writing a second row.
                row: Some(row),
                received_at,
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
async fn take_one(db: &Db, conversation_id: &str, turn_id: &str, now: i64) -> Result<Option<(String, String)>, DbErr> {
    // `Db::write` is IMMEDIATE, and this is the transaction that spans the
    // peek and the take, so it is the one that has to hold the write lock
    // across both. Deferred, it takes the lock at the first write — after the
    // peek — and two drains could read the same item.
    db.write(async |tx| {
        let Some(item) = queue_ops::next_deliverable(tx, conversation_id, Delivery::Interject).await? else {
            return Ok(None);
        };
        let head = conversation_ops::get_conversation(tx, conversation_id)
            .await?
            .and_then(|c| c.head_message_id);
        let message_id = uuid::Uuid::new_v4().to_string();

        let row = message::Model {
            id: message_id.clone(),
            conversation_id: conversation_id.to_owned(),
            role: "user".to_owned(),
            content: item.content.clone(),
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
            is_compact_summary: SqlBool::FALSE,
            sender_id: None,
            parent_id: None,
            compact_anchor_id: None,
            source: None,
            // The turn it interrupted, not a turn of its own. That is where the
            // model reads it, so a reader who found it filed elsewhere would be
            // looking at a different conversation than the model was.
            turn_id: Some(turn_id.to_owned()),
            tool_outcome: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            provider_name: None,
            provider_state: None,
            auto_review: None,
            server_tool_calls: None,
            tool_diffs: None,
            response_model_id: None,
        };

        let taken = queue_ops::take_next(
            tx,
            conversation_id,
            Delivery::Interject,
            turn_id,
            row,
            head.as_deref(),
            now,
        )
        .await?;
        Ok(taken.map(|_| (message_id, item.content)))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::sea_test_db;

    async fn conversation(db: &Db, id: &str) {
        db.write(async |tx| conversation_ops::create_conversation(tx, id, Some("q"), None, None, 0).await)
            .await
            .unwrap();
    }

    async fn add(db: &Db, text: &str, delivery: Delivery) {
        let id = uuid::Uuid::new_v4().to_string();
        db.write(async |tx| queue_ops::enqueue(tx, &id, "c1", text, delivery, 0).await)
            .await
            .unwrap();
    }

    async fn messages(db: &Db) -> Vec<message::Model> {
        crate::db::sea::ops::message::list_messages(db, "c1").await.unwrap()
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
        let db = sea_test_db().await;
        conversation(&db, "c1").await;
        add(&db, "actually, stop", Delivery::Interject).await;

        let events = EventBus::new();
        let heard = std::sync::Arc::new(Heard::default());
        events.register(heard.clone(), false);

        let port = Announcing::wrap(events.clone(), Interjections::new(db.clone(), "c1".into(), "t1".into()));

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
        let db = sea_test_db().await;
        conversation(&db, "c1").await;
        add(&db, "actually, stop", Delivery::Interject).await;
        add(&db, "and check the tests", Delivery::Interject).await;
        add(&db, "then write it up", Delivery::FollowUp).await;

        let port = Interjections::new(db.clone(), "c1".into(), "t1".into());
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
        let rows = messages(&db).await;
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.turn_id.as_deref() == Some("t1")));
        assert_eq!(rows[1].parent_id.as_deref(), Some(rows[0].id.as_str()), "one path");

        // And the follow-up is untouched.
        let left: Vec<_> = queue_ops::list(&db, "c1")
            .await
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
        let db = sea_test_db().await;
        conversation(&db, "c1").await;
        add(&db, "once", Delivery::Interject).await;

        let port = Interjections::new(db.clone(), "c1".into(), "t1".into());
        assert_eq!(port.drain().await.len(), 1);
        assert!(port.drain().await.is_empty());
        assert_eq!(messages(&db).await.len(), 1, "and exactly one row exists for it");
    }

    /// An empty queue costs one read and produces nothing, which is what
    /// happens between every round of every desktop turn.
    #[tokio::test]
    async fn an_empty_queue_is_silent() {
        let db = sea_test_db().await;
        conversation(&db, "c1").await;
        let port = Interjections::new(db, "c1".into(), "t1".into());
        assert!(port.drain().await.is_empty());
    }
}

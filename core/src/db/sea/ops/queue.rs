//! Reading and writing the prompt queue, on SeaORM.
//!
//! Where an item has got to is read from which timestamps are set
//! (`queued_prompt::Model::state`). A held or in-doubt row is a barrier: it
//! stops the queue, because the instructions were written as a sequence and
//! delivering number three while number two is unresolved runs them out of
//! order.
//!
//! No function here opens a transaction of its own. The read-then-write ones
//! (`enqueue_with_context`, `set_delivery`, `mark_dispatched`, `reorder`)
//! rely on the caller's `Db::write` being `BEGIN IMMEDIATE`, which takes the
//! lock before their first read.

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

use crate::db::entity::queued_prompt;
use crate::db::entity::queued_prompt::{Delivery, QueueState};
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::sea::ops::queued_prompt_context_item;
use crate::db::types::EpochMs;
use crate::workspace::reference::PreparedContextItem;

/// A first-party contract broken by stored or requested content.
fn contract_error(message: String) -> DbErr {
    DbErr::Custom(message)
}

/// Everything queued for a conversation, in the order it will be delivered,
/// settled rows included: the front end draws them as they leave.
pub async fn list(db: &impl Read, conversation_id: &str) -> Result<Vec<queued_prompt::Model>, DbErr> {
    queued_prompt::Entity::find()
        .filter(queued_prompt::Column::ConversationId.eq(conversation_id))
        .order_by_asc(queued_prompt::Column::Position)
        .order_by_asc(queued_prompt::Column::CreatedAt)
        .all(db.conn()?)
        .await
}

pub async fn get(db: &impl Read, id: &str) -> Result<Option<queued_prompt::Model>, DbErr> {
    queued_prompt::Entity::find_by_id(id).one(db.conn()?).await
}

/// Add one to the back, with the `@` context frozen for it. `position` is one
/// past whatever is there now (settled rows keep theirs), read under the
/// caller's write lock so two enqueues cannot claim the same number. A
/// failed context write fails the whole enqueue with it.
#[allow(clippy::too_many_arguments)]
pub async fn enqueue_with_context(
    tx: &WriteTx,
    id: &str,
    conversation_id: &str,
    content: &str,
    delivery: Delivery,
    context: &[PreparedContextItem],
    now: EpochMs,
) -> Result<queued_prompt::Model, DbErr> {
    // The same rule `set_delivery` keeps for the steer button, kept where the
    // row is written so it holds for every caller.
    if delivery == Delivery::Interject && carries_attachments(content)? {
        return Err(contract_error(
            "attachments can only be queued as follow-up messages".into(),
        ));
    }
    let last: Option<i32> = queued_prompt::Entity::find()
        .filter(queued_prompt::Column::ConversationId.eq(conversation_id))
        .select_only()
        .column_as(queued_prompt::Column::Position.max(), "last")
        .into_tuple::<Option<i32>>()
        .one(tx.conn()?)
        .await?
        .flatten();
    queued_prompt::Entity::insert(queued_prompt::ActiveModel {
        id: Set(id.to_owned()),
        conversation_id: Set(conversation_id.to_owned()),
        content: Set(content.to_owned()),
        delivery: Set(delivery),
        position: Set(last.map_or(0, |last| last + 1)),
        created_at: Set(now),
        dispatched_at: Set(None),
        dispatched_turn_id: Set(None),
        settled_at: Set(None),
        settled_message_id: Set(None),
        held_at: Set(None),
        reported_at: Set(None),
    })
    .exec_without_returning(tx.conn()?)
    .await?;
    queued_prompt_context_item::insert_prepared(tx, id, context, now).await?;
    get(tx, id)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound(format!("queued prompt `{id}`")))
}

pub async fn enqueue(
    tx: &WriteTx,
    id: &str,
    conversation_id: &str,
    content: &str,
    delivery: Delivery,
    now: EpochMs,
) -> Result<queued_prompt::Model, DbErr> {
    enqueue_with_context(tx, id, conversation_id, content, delivery, &[], now).await
}

/// Drop one that has not been delivered: never a settled row (already part of
/// the transcript) nor an in-doubt one (the only record that the agent may be
/// about to act on it). Constrained to the conversation the caller named.
pub async fn remove(tx: &WriteTx, conversation_id: &str, id: &str) -> Result<u64, DbErr> {
    Ok(queued_prompt::Entity::delete_many()
        .filter(queued_prompt::Column::Id.eq(id))
        .filter(queued_prompt::Column::ConversationId.eq(conversation_id))
        .filter(queued_prompt::Column::DispatchedAt.is_null())
        .filter(queued_prompt::Column::SettledAt.is_null())
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

/// Rewrite the order, wholesale. Only rows still waiting may move, and none
/// of them in front of a barrier: anything dragged ahead of a held or
/// in-doubt row lands immediately behind it, so the barrier still bites.
pub async fn reorder(tx: &WriteTx, conversation_id: &str, ids: &[String]) -> Result<(), DbErr> {
    let floor = list(tx, conversation_id)
        .await?
        .iter()
        .filter(|item| matches!(item.state(), QueueState::Held | QueueState::InDoubt))
        .map(|item| item.position)
        .max();
    let mut next = floor.map_or(0, |p| p + 1);
    for id in ids {
        let updated = queued_prompt::Entity::update_many()
            .col_expr(queued_prompt::Column::Position, Expr::value(next))
            .filter(queued_prompt::Column::Id.eq(id))
            .filter(queued_prompt::Column::ConversationId.eq(conversation_id))
            .filter(queued_prompt::Column::DispatchedAt.is_null())
            .filter(queued_prompt::Column::SettledAt.is_null())
            .filter(queued_prompt::Column::HeldAt.is_null())
            .exec(tx.conn()?)
            .await?
            .rows_affected;
        if updated > 0 {
            next += 1;
        }
    }
    Ok(())
}

/// What became of a request to change an item's delivery mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryChange {
    Changed,
    /// Already delivered, in doubt, or never this conversation's.
    NotWaiting,
    /// It carries attachments, and an interjection is text only.
    CarriesAttachments,
    /// It carries frozen `@` context, which only a new turn has a place for.
    CarriesContext,
}

/// Whether a queued message carries anything besides text. An interjection
/// goes in mid-turn as text and has nowhere to put an image or a file.
/// Damaged parts are an error, never "no attachments".
pub fn carries_attachments(content: &str) -> Result<bool, DbErr> {
    let parts = crate::provider::decode_message_parts(content).map_err(contract_error)?;
    Ok(parts.is_some_and(|parts| {
        parts
            .iter()
            .any(|part| !matches!(part, crate::provider::MessageContentPart::Text { .. }))
    }))
}

/// Change one item's delivery mode while it is still waiting. Switching to
/// `interject` is refused for what only a follow-up can carry — attachments
/// and frozen `@` context — in the same write, because the queue's steer
/// button arrives here and not at enqueue.
pub async fn set_delivery(
    tx: &WriteTx,
    conversation_id: &str,
    id: &str,
    delivery: Delivery,
) -> Result<DeliveryChange, DbErr> {
    let waiting = || {
        queued_prompt::Entity::find()
            .filter(queued_prompt::Column::Id.eq(id))
            .filter(queued_prompt::Column::ConversationId.eq(conversation_id))
            .filter(queued_prompt::Column::DispatchedAt.is_null())
            .filter(queued_prompt::Column::SettledAt.is_null())
    };
    if delivery == Delivery::Interject {
        let Some(item) = waiting().one(tx.conn()?).await? else {
            return Ok(DeliveryChange::NotWaiting);
        };
        if carries_attachments(&item.content)? {
            return Ok(DeliveryChange::CarriesAttachments);
        }
        if !queued_prompt_context_item::list_prepared(tx, id).await?.is_empty() {
            return Ok(DeliveryChange::CarriesContext);
        }
    }
    let updated = queued_prompt::Entity::update_many()
        .col_expr(queued_prompt::Column::Delivery, Expr::value(delivery))
        .filter(queued_prompt::Column::Id.eq(id))
        .filter(queued_prompt::Column::ConversationId.eq(conversation_id))
        .filter(queued_prompt::Column::DispatchedAt.is_null())
        .filter(queued_prompt::Column::SettledAt.is_null())
        .exec(tx.conn()?)
        .await?
        .rows_affected;
    Ok(if updated == 0 {
        DeliveryChange::NotWaiting
    } else {
        DeliveryChange::Changed
    })
}

/// The front of the queue, whatever mode it is in: settled rows are stepped
/// over, and a held or in-doubt one stops the walk.
pub async fn next_pending(db: &impl Read, conversation_id: &str) -> Result<Option<queued_prompt::Model>, DbErr> {
    for item in list(db, conversation_id).await? {
        match item.state() {
            QueueState::Settled => continue,
            QueueState::Held | QueueState::InDoubt => return Ok(None),
            QueueState::Queued => return Ok(Some(item)),
        }
    }
    Ok(None)
}

/// The next item a runner may deliver in a particular mode: a `follow_up` at
/// the front is not skipped for an `interject` behind it.
pub async fn next_deliverable(
    db: &impl Read,
    conversation_id: &str,
    delivery: Delivery,
) -> Result<Option<queued_prompt::Model>, DbErr> {
    Ok(next_pending(db, conversation_id)
        .await?
        .filter(|item| item.delivery == delivery))
}

/// Take the next deliverable item off the queue *and* write the message it
/// becomes, atomically: the reason the queue is a table rather than a channel.
/// Killed before the commit, the item is still queued and no row exists;
/// after it, the item is settled and the row is on the path. Nothing in
/// between, and a failure partway (a duplicate message id) undoes both, in a
/// savepoint of the caller's write.
#[allow(clippy::too_many_arguments)]
pub async fn take_next(
    tx: &WriteTx,
    conversation_id: &str,
    delivery: Delivery,
    turn_id: &str,
    message: crate::db::entity::message::Model,
    parent: Option<&str>,
    now: EpochMs,
) -> Result<Option<queued_prompt::Model>, DbErr> {
    tx.nested(async |tx| {
        let Some(item) = next_deliverable(tx, conversation_id, delivery).await? else {
            return Ok(None);
        };
        let row = crate::db::sea::ops::message::append_message(tx, message, parent).await?;
        queued_prompt::Entity::update_many()
            .set(queued_prompt::ActiveModel {
                dispatched_at: Set(Some(now)),
                dispatched_turn_id: Set(Some(turn_id.to_owned())),
                settled_at: Set(Some(now)),
                settled_message_id: Set(Some(row.id)),
                ..Default::default()
            })
            .filter(queued_prompt::Column::Id.eq(&item.id))
            .exec(tx.conn()?)
            .await?;
        get(tx, &item.id).await
    })
    .await
}

/// Mark an item as handed over without a message row of our own (a hosted
/// steer), recording the attempt before it is made. A claim on the
/// deliverable front, not on an id: the row must still be first, still
/// queued, and — when `expect` says so — still in the mode that was read.
pub async fn mark_dispatched(
    tx: &WriteTx,
    conversation_id: &str,
    id: &str,
    expect: Option<Delivery>,
    turn_id: &str,
    now: EpochMs,
) -> Result<u64, DbErr> {
    let Some(front) = list(tx, conversation_id)
        .await?
        .into_iter()
        .find(|item| item.state() != QueueState::Settled)
    else {
        return Ok(0);
    };
    if front.id != id || front.state() != QueueState::Queued || expect.is_some_and(|mode| front.delivery != mode) {
        return Ok(0);
    }
    Ok(queued_prompt::Entity::update_many()
        .set(queued_prompt::ActiveModel {
            dispatched_at: Set(Some(now)),
            dispatched_turn_id: Set(Some(turn_id.to_owned())),
            ..Default::default()
        })
        .filter(queued_prompt::Column::Id.eq(id))
        .filter(queued_prompt::Column::ConversationId.eq(conversation_id))
        .filter(queued_prompt::Column::DispatchedAt.is_null())
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

/// Spend a queued item on the turn of its own that `message_id` opens: claim
/// it, settle it onto that row and drop its frozen context, in the caller's
/// write — the one that wrote the row. A claim refused (somebody else took
/// it, or it was held or dragged out of first place since it was read) is an
/// error, so the row goes back with it rather than a second copy landing.
/// `None` for the mode: with nothing running, every mode is deliverable.
pub async fn spend(
    tx: &WriteTx,
    conversation_id: &str,
    id: &str,
    turn_id: &str,
    message_id: &str,
    now: EpochMs,
) -> Result<(), DbErr> {
    if mark_dispatched(tx, conversation_id, id, None, turn_id, now).await? == 0 {
        return Err(DbErr::Custom(format!(
            "queued message '{id}' is no longer the one to deliver"
        )));
    }
    mark_settled(tx, id, Some(message_id), now).await?;
    queued_prompt_context_item::delete_for_queue(tx, id).await?;
    Ok(())
}

/// The send came back, so the doubt is resolved. `None` for the message means
/// "not yet": writing NULL over an id `attach_message` already filled in is
/// how a concurrent finish erases the link, so it is left alone.
pub async fn mark_settled(tx: &WriteTx, id: &str, message_id: Option<&str>, now: EpochMs) -> Result<u64, DbErr> {
    let mut row = queued_prompt::ActiveModel {
        settled_at: Set(Some(now)),
        ..Default::default()
    };
    if let Some(message_id) = message_id {
        row.settled_message_id = Set(Some(message_id.to_owned()));
    }
    Ok(queued_prompt::Entity::update_many()
        .set(row)
        .filter(queued_prompt::Column::Id.eq(id))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

/// Name the transcript row a settled item became, once it has one.
pub async fn attach_message(tx: &WriteTx, id: &str, message_id: &str) -> Result<u64, DbErr> {
    Ok(queued_prompt::Entity::update_many()
        .col_expr(queued_prompt::Column::SettledMessageId, Expr::value(message_id))
        .filter(queued_prompt::Column::Id.eq(id))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

/// Undo a dispatch the agent has told us it did not take (`promptRequired`):
/// evidence about the delivery itself, which nothing else is.
pub async fn undispatch(tx: &WriteTx, id: &str) -> Result<u64, DbErr> {
    Ok(queued_prompt::Entity::update_many()
        .set(queued_prompt::ActiveModel {
            dispatched_at: Set(None),
            dispatched_turn_id: Set(None),
            ..Default::default()
        })
        .filter(queued_prompt::Column::Id.eq(id))
        .filter(queued_prompt::Column::SettledAt.is_null())
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

/// Stop the queue because the turn in front of it did not finish: everything
/// unsettled, a claimed row included — a claim is not a delivery, and
/// `undispatch` can hand it back as plainly queued.
pub async fn hold_all(tx: &WriteTx, conversation_id: &str, now: EpochMs) -> Result<u64, DbErr> {
    Ok(queued_prompt::Entity::update_many()
        .col_expr(queued_prompt::Column::HeldAt, Expr::value(now))
        .filter(queued_prompt::Column::ConversationId.eq(conversation_id))
        .filter(queued_prompt::Column::SettledAt.is_null())
        .filter(queued_prompt::Column::HeldAt.is_null())
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

/// Let a held queue go again, after a person has looked at it.
pub async fn release_all(tx: &WriteTx, conversation_id: &str) -> Result<u64, DbErr> {
    Ok(queued_prompt::Entity::update_many()
        .col_expr(queued_prompt::Column::HeldAt, Expr::value(Option::<EpochMs>::None))
        .filter(queued_prompt::Column::ConversationId.eq(conversation_id))
        .filter(queued_prompt::Column::HeldAt.is_not_null())
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

/// Items whose delivery is in doubt and which the agent has not been told
/// about. Reading this is not telling anyone, so nothing is written here.
pub async fn unreported_in_doubt(db: &impl Read, conversation_id: &str) -> Result<Vec<queued_prompt::Model>, DbErr> {
    queued_prompt::Entity::find()
        .filter(queued_prompt::Column::ConversationId.eq(conversation_id))
        .filter(queued_prompt::Column::DispatchedAt.is_not_null())
        .filter(queued_prompt::Column::SettledAt.is_null())
        .filter(queued_prompt::Column::ReportedAt.is_null())
        .order_by_asc(queued_prompt::Column::Position)
        .all(db.conn()?)
        .await
}

/// Record that the agent has now been told about these.
pub async fn mark_reported(tx: &WriteTx, ids: &[String], now: EpochMs) -> Result<u64, DbErr> {
    if ids.is_empty() {
        return Ok(0);
    }
    Ok(queued_prompt::Entity::update_many()
        .col_expr(queued_prompt::Column::ReportedAt, Expr::value(now))
        .filter(queued_prompt::Column::Id.is_in(ids))
        .exec(tx.conn()?)
        .await?
        .rows_affected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::cap::Db;
    use crate::db::sea::{execute_for_tests, sea_test_db};

    async fn with_conversations(ids: &[&str]) -> Db {
        let db = sea_test_db().await;
        for id in ids {
            execute_for_tests(
                &db,
                &format!("INSERT INTO conversations (id, title, created_at, updated_at) VALUES ('{id}', 'q', 0, 0)"),
            )
            .await
            .unwrap();
        }
        db
    }

    async fn add(db: &Db, conversation: &str, text: &str, delivery: Delivery) -> queued_prompt::Model {
        let id = uuid::Uuid::new_v4().to_string();
        db.write(async |tx| enqueue(tx, &id, conversation, text, delivery, 0).await)
            .await
            .unwrap()
    }

    async fn claim(db: &Db, conversation: &str, id: &str, expect: Option<Delivery>, turn: &str, now: EpochMs) -> u64 {
        db.write(async |tx| mark_dispatched(tx, conversation, id, expect, turn, now).await)
            .await
            .unwrap()
    }

    async fn hold(db: &Db, conversation: &str, now: EpochMs) -> u64 {
        db.write(async |tx| hold_all(tx, conversation, now).await)
            .await
            .unwrap()
    }

    async fn release(db: &Db, conversation: &str) -> u64 {
        db.write(async |tx| release_all(tx, conversation).await).await.unwrap()
    }

    async fn order(db: &Db, conversation: &str, ids: Vec<String>) {
        db.write(async |tx| reorder(tx, conversation, &ids).await)
            .await
            .unwrap();
    }

    async fn switch(db: &Db, conversation: &str, id: &str, delivery: Delivery) -> DeliveryChange {
        db.write(async |tx| set_delivery(tx, conversation, id, delivery).await)
            .await
            .unwrap()
    }

    async fn front(db: &Db, conversation: &str) -> Option<String> {
        next_pending(db, conversation).await.unwrap().map(|i| i.content)
    }

    async fn deliverable(db: &Db, conversation: &str, delivery: Delivery) -> Option<String> {
        next_deliverable(db, conversation, delivery)
            .await
            .unwrap()
            .map(|i| i.content)
    }

    async fn contents(db: &Db, conversation: &str) -> Vec<String> {
        list(db, conversation)
            .await
            .unwrap()
            .into_iter()
            .map(|i| i.content)
            .collect()
    }

    /// The state machine, stated once.
    #[tokio::test]
    async fn which_timestamps_are_set_is_the_state() {
        let db = with_conversations(&["c1"]).await;
        let item = add(&db, "c1", "one", Delivery::FollowUp).await;
        assert_eq!(item.state(), QueueState::Queued);

        claim(&db, "c1", &item.id, None, "t1", 1).await;
        assert_eq!(
            list(&db, "c1").await.unwrap()[0].state(),
            QueueState::InDoubt,
            "dispatched and unanswered"
        );
        db.write(async |tx| mark_settled(tx, &item.id, Some("m1"), 2).await)
            .await
            .unwrap();
        assert_eq!(list(&db, "c1").await.unwrap()[0].state(), QueueState::Settled);
    }

    /// An interjection is text only, and the steer button reaches this write
    /// without passing enqueue — so this is where an item carrying an
    /// attachment or a frozen snapshot is kept a follow-up.
    #[tokio::test]
    async fn only_what_text_can_carry_may_become_an_interjection() {
        let db = with_conversations(&["c1"]).await;
        let plain = add(&db, "c1", "then rename it", Delivery::FollowUp).await;
        assert_eq!(
            switch(&db, "c1", &plain.id, Delivery::Interject).await,
            DeliveryChange::Changed
        );

        let parts = r#"[{"type":"text","text":"and this"},{"type":"image_url","image_url":{"url":"file:///x.png"}}]"#;
        let attached = add(&db, "c1", parts, Delivery::FollowUp).await;
        assert_eq!(
            switch(&db, "c1", &attached.id, Delivery::Interject).await,
            DeliveryChange::CarriesAttachments
        );
        let text_parts = add(
            &db,
            "c1",
            r#"[{"type":"text","text":"only words"}]"#,
            Delivery::FollowUp,
        )
        .await;
        assert_eq!(
            switch(&db, "c1", &text_parts.id, Delivery::Interject).await,
            DeliveryChange::Changed,
            "text-only parts are still text"
        );

        let snapshot = PreparedContextItem {
            id: "ctx".into(),
            kind: crate::workspace::reference::MessageContextKind::ProjectFile,
            content: "frozen bytes".into(),
            display_path: Some("src/lib.rs".into()),
            line_start: None,
            line_end: None,
            content_hash: "hash".into(),
            byte_count: 12,
            line_count: 1,
            token_count: 3,
            truncated: 0,
            metadata: None,
        };
        let referenced = db
            .write(async |tx| {
                enqueue_with_context(
                    tx,
                    "q-ctx",
                    "c1",
                    "look at @src/lib.rs",
                    Delivery::FollowUp,
                    std::slice::from_ref(&snapshot),
                    0,
                )
                .await
            })
            .await
            .unwrap();
        assert_eq!(
            switch(&db, "c1", &referenced.id, Delivery::Interject).await,
            DeliveryChange::CarriesContext
        );
        assert_eq!(
            queued_prompt_context_item::list_prepared(&db, "q-ctx").await.unwrap(),
            [snapshot],
            "the snapshot reads back as it was frozen"
        );

        // Nor may one be queued as an interjection in the first place, and the
        // refusal writes nothing.
        let direct = db
            .write(async |tx| enqueue(tx, "q-direct", "c1", parts, Delivery::Interject, 0).await)
            .await;
        assert!(direct.is_err());
        assert_eq!(
            get(&db, "q-direct").await.unwrap(),
            None,
            "a refused enqueue leaves no row"
        );

        // The refusals changed nothing, and going the other way is never refused.
        let modes: Vec<_> = list(&db, "c1").await.unwrap().into_iter().map(|i| i.delivery).collect();
        assert_eq!(
            modes,
            [
                Delivery::Interject,
                Delivery::FollowUp,
                Delivery::Interject,
                Delivery::FollowUp
            ]
        );
        assert_eq!(
            switch(&db, "c1", &attached.id, Delivery::FollowUp).await,
            DeliveryChange::Changed
        );
        assert_eq!(
            switch(&db, "c1", "no-such-row", Delivery::Interject).await,
            DeliveryChange::NotWaiting
        );
    }

    /// An item whose delivery is in doubt stops the queue rather than being
    /// stepped over, and is what gets reported instead.
    #[tokio::test]
    async fn an_item_in_doubt_blocks_the_queue_and_is_never_redelivered() {
        let db = with_conversations(&["c1"]).await;
        let first = add(&db, "c1", "first", Delivery::FollowUp).await;
        add(&db, "c1", "second", Delivery::FollowUp).await;
        claim(&db, "c1", &first.id, None, "t1", 1).await;

        assert_eq!(deliverable(&db, "c1", Delivery::FollowUp).await, None);
        let owed = unreported_in_doubt(&db, "c1").await.unwrap();
        assert_eq!(
            owed.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            [first.id.as_str()]
        );
    }

    /// Reordering cannot step around a barrier: a row dragged in front of an
    /// in-doubt one lands behind it.
    #[tokio::test]
    async fn reordering_cannot_move_anything_in_front_of_an_unresolved_item() {
        let db = with_conversations(&["c1"]).await;
        let doubtful = add(&db, "c1", "delete the old migration", Delivery::FollowUp).await;
        let second = add(&db, "c1", "then rename it", Delivery::FollowUp).await;
        let third = add(&db, "c1", "and run the tests", Delivery::FollowUp).await;
        claim(&db, "c1", &doubtful.id, None, "t1", 1).await;

        order(
            &db,
            "c1",
            vec![third.id.clone(), doubtful.id.clone(), second.id.clone()],
        )
        .await;
        assert_eq!(
            contents(&db, "c1").await,
            ["delete the old migration", "and run the tests", "then rename it"],
            "the two queued rows swapped, and neither got past the one in doubt"
        );
        assert_eq!(front(&db, "c1").await, None, "and the queue is still stopped");
    }

    /// A claim is on the deliverable front, not on an id.
    #[tokio::test]
    async fn a_claim_is_refused_once_the_item_stops_being_deliverable() {
        let db = with_conversations(&["c1", "c2"]).await;

        // Held between the read and the claim.
        let a = add(&db, "c1", "first", Delivery::FollowUp).await;
        hold(&db, "c1", 5).await;
        assert_eq!(claim(&db, "c1", &a.id, None, "t1", 6).await, 0);
        release(&db, "c1").await;

        // No longer the front: something was dragged ahead of it.
        let b = add(&db, "c1", "second", Delivery::FollowUp).await;
        order(&db, "c1", vec![b.id.clone(), a.id.clone()]).await;
        assert_eq!(claim(&db, "c1", &a.id, None, "t1", 7).await, 0, "not out of turn");

        // Switched to the other mode while a steer was in flight for it.
        switch(&db, "c1", &b.id, Delivery::FollowUp).await;
        assert_eq!(
            claim(&db, "c1", &b.id, Some(Delivery::Interject), "t1", 8).await,
            0,
            "a steer's claim is void once the row is no longer an interjection"
        );

        // And the honest case still goes through, exactly once.
        assert_eq!(claim(&db, "c1", &b.id, None, "t1", 9).await, 1);
        assert_eq!(
            claim(&db, "c1", &b.id, None, "t2", 10).await,
            0,
            "a second pump gets nothing"
        );

        // Another conversation's row is not claimable through this one.
        let elsewhere = add(&db, "c2", "theirs", Delivery::FollowUp).await;
        assert_eq!(claim(&db, "c1", &elsewhere.id, None, "t1", 11).await, 0);
    }

    #[tokio::test]
    async fn reordering_cannot_move_anything_in_front_of_a_held_item() {
        let db = with_conversations(&["c1"]).await;
        let first = add(&db, "c1", "first", Delivery::FollowUp).await;
        let second = add(&db, "c1", "second", Delivery::FollowUp).await;
        hold(&db, "c1", 5).await;
        order(&db, "c1", vec![second.id.clone(), first.id.clone()]).await;
        assert_eq!(
            contents(&db, "c1").await,
            ["first", "second"],
            "a held row does not move at all"
        );
    }

    /// Reporting is settled by a separate write, so reading is not telling.
    #[tokio::test]
    async fn a_doubtful_item_stays_owed_until_it_is_marked_reported() {
        let db = with_conversations(&["c1"]).await;
        let item = add(&db, "c1", "one", Delivery::FollowUp).await;
        claim(&db, "c1", &item.id, None, "t1", 1).await;

        assert_eq!(unreported_in_doubt(&db, "c1").await.unwrap().len(), 1);
        assert_eq!(unreported_in_doubt(&db, "c1").await.unwrap().len(), 1);
        db.write(async |tx| mark_reported(tx, std::slice::from_ref(&item.id), 9).await)
            .await
            .unwrap();
        assert!(unreported_in_doubt(&db, "c1").await.unwrap().is_empty());
    }

    /// A held queue is stopped, not filtered.
    #[tokio::test]
    async fn holding_stops_everything_behind_it() {
        let db = with_conversations(&["c1"]).await;
        add(&db, "c1", "first", Delivery::FollowUp).await;
        add(&db, "c1", "second", Delivery::FollowUp).await;

        assert_eq!(hold(&db, "c1", 3).await, 2);
        assert_eq!(deliverable(&db, "c1", Delivery::FollowUp).await, None);
        release(&db, "c1").await;
        assert_eq!(
            deliverable(&db, "c1", Delivery::FollowUp).await.as_deref(),
            Some("first")
        );
    }

    /// A hold that lands while an item is claimed still applies to it, so an
    /// `undispatch` cannot hand it back as plainly queued.
    #[tokio::test]
    async fn a_hold_during_a_claim_is_not_lost_when_the_claim_comes_back() {
        let db = with_conversations(&["c1"]).await;
        let steered = add(&db, "c1", "actually, stop", Delivery::Interject).await;
        add(&db, "c1", "then write it up", Delivery::FollowUp).await;

        assert_eq!(
            claim(&db, "c1", &steered.id, Some(Delivery::Interject), "t1", 2).await,
            1
        );
        assert_eq!(list(&db, "c1").await.unwrap()[0].state(), QueueState::InDoubt);
        assert_eq!(
            hold(&db, "c1", 3).await,
            2,
            "the claimed row is held too, not stepped over"
        );

        db.write(async |tx| undispatch(tx, &steered.id).await).await.unwrap();
        assert_eq!(list(&db, "c1").await.unwrap()[0].state(), QueueState::Held);
        assert_eq!(front(&db, "c1").await, None, "nothing runs until a person releases it");

        release(&db, "c1").await;
        assert_eq!(front(&db, "c1").await.as_deref(), Some("actually, stop"));
    }

    /// Release does not resurrect an item whose delivery is still unresolved.
    #[tokio::test]
    async fn releasing_a_held_queue_leaves_an_unresolved_item_unresolved() {
        let db = with_conversations(&["c1"]).await;
        let claimed = add(&db, "c1", "delete the old migration", Delivery::Interject).await;
        claim(&db, "c1", &claimed.id, Some(Delivery::Interject), "t1", 2).await;
        hold(&db, "c1", 3).await;
        release(&db, "c1").await;

        assert_eq!(list(&db, "c1").await.unwrap()[0].state(), QueueState::InDoubt);
        assert_eq!(front(&db, "c1").await, None);
    }

    /// The head belongs to one mode and does not let the other past.
    #[tokio::test]
    async fn the_head_belongs_to_one_mode_and_does_not_let_the_other_past() {
        let db = with_conversations(&["c1"]).await;
        add(&db, "c1", "after you finish", Delivery::FollowUp).await;
        add(&db, "c1", "actually, stop", Delivery::Interject).await;

        assert_eq!(deliverable(&db, "c1", Delivery::Interject).await, None);
        assert_eq!(
            deliverable(&db, "c1", Delivery::FollowUp).await.as_deref(),
            Some("after you finish")
        );
    }

    /// Deleting is for things that have not gone anywhere.
    #[tokio::test]
    async fn an_item_already_sent_cannot_be_deleted() {
        let db = with_conversations(&["c1"]).await;
        let item = add(&db, "c1", "one", Delivery::FollowUp).await;
        let drop = |id: String| {
            let db = db.clone();
            async move { db.write(async |tx| remove(tx, "c1", &id).await).await.unwrap() }
        };
        assert_eq!(drop(item.id.clone()).await, 1, "a queued one goes");

        let item = add(&db, "c1", "two", Delivery::FollowUp).await;
        claim(&db, "c1", &item.id, None, "t1", 1).await;
        assert_eq!(drop(item.id.clone()).await, 0, "a doubtful one stays");
    }

    /// With nothing running, an `interject` left from an ended turn is the
    /// front like any other.
    #[tokio::test]
    async fn an_idle_runner_takes_the_front_of_the_queue_whatever_mode_it_is() {
        let db = with_conversations(&["c1"]).await;
        add(&db, "c1", "actually, stop", Delivery::Interject).await;
        add(&db, "c1", "and then this", Delivery::FollowUp).await;

        assert_eq!(
            deliverable(&db, "c1", Delivery::FollowUp).await,
            None,
            "a running turn cannot take it"
        );
        assert_eq!(
            front(&db, "c1").await.as_deref(),
            Some("actually, stop"),
            "an idle one can"
        );
    }

    /// `promptRequired` is evidence the agent did not take the message.
    #[tokio::test]
    async fn a_refused_delivery_is_returned_to_the_queue() {
        let db = with_conversations(&["c1"]).await;
        let item = add(&db, "c1", "one", Delivery::Interject).await;
        claim(&db, "c1", &item.id, None, "t1", 1).await;
        db.write(async |tx| undispatch(tx, &item.id).await).await.unwrap();

        let item = list(&db, "c1").await.unwrap().remove(0);
        assert_eq!((item.state(), item.dispatched_turn_id), (QueueState::Queued, None));
        assert!(unreported_in_doubt(&db, "c1").await.unwrap().is_empty());
    }

    /// A settled item gets its row later, and the queue moves on meanwhile.
    #[tokio::test]
    async fn a_row_can_be_attached_after_the_doubt_is_already_resolved() {
        let db = with_conversations(&["c1"]).await;
        let first = add(&db, "c1", "one", Delivery::Interject).await;
        add(&db, "c1", "two", Delivery::Interject).await;

        claim(&db, "c1", &first.id, None, "t1", 1).await;
        db.write(async |tx| mark_settled(tx, &first.id, None, 2).await)
            .await
            .unwrap();
        assert_eq!(front(&db, "c1").await.as_deref(), Some("two"));

        db.write(async |tx| attach_message(tx, &first.id, "m1").await)
            .await
            .unwrap();
        let first = list(&db, "c1").await.unwrap().remove(0);
        assert_eq!(
            (first.settled_message_id.as_deref(), first.state()),
            (Some("m1"), QueueState::Settled)
        );
    }

    /// A second settle with `None` must not wipe an attached id.
    #[tokio::test]
    async fn settling_without_a_row_does_not_erase_one_already_attached() {
        let db = with_conversations(&["c1"]).await;
        let first = add(&db, "c1", "one", Delivery::Interject).await;
        claim(&db, "c1", &first.id, None, "t1", 1).await;
        db.write(async |tx| {
            attach_message(tx, &first.id, "m1").await?;
            mark_settled(tx, &first.id, None, 2).await
        })
        .await
        .unwrap();
        let first = list(&db, "c1").await.unwrap().remove(0);
        assert_eq!(first.settled_message_id.as_deref(), Some("m1"));
        assert!(first.settled_at.is_some());
    }

    /// An unknown stored delivery mode fails the read.
    #[tokio::test]
    async fn an_unknown_delivery_mode_is_rejected() {
        assert_eq!(Delivery::parse("interject").unwrap(), Delivery::Interject);
        assert!(Delivery::parse("pre_empt_everything").is_err());

        let db = with_conversations(&["c1"]).await;
        let first = add(&db, "c1", "one", Delivery::FollowUp).await;
        execute_for_tests(
            &db,
            &format!(
                "UPDATE queued_prompts SET delivery = 'pre_empt_everything' WHERE id = '{}'",
                first.id
            ),
        )
        .await
        .unwrap();
        let error = next_deliverable(&db, "c1", Delivery::FollowUp).await.unwrap_err();
        assert!(error.to_string().contains("unknown queue delivery mode"), "{error}");
    }

    /// Two enqueues read the last position under the write lock, so each
    /// gets its own: no two rows share a place in the order.
    #[tokio::test]
    async fn concurrent_enqueues_take_distinct_positions() {
        let dir = tempfile::tempdir().unwrap();
        let (_pool, db) = crate::db::sea::shared_test_db(dir.path()).await;
        execute_for_tests(
            &db,
            "INSERT INTO conversations (id, created_at, updated_at) VALUES ('c1', 0, 0)",
        )
        .await
        .unwrap();
        let racers: Vec<_> = (0..8)
            .map(|i| {
                let db = db.clone();
                tokio::spawn(async move {
                    let id = format!("q{i}");
                    db.write(async |tx| enqueue(tx, &id, "c1", "x", Delivery::FollowUp, 0).await)
                        .await
                        .unwrap()
                })
            })
            .collect();
        for racer in racers {
            racer.await.unwrap();
        }
        let mut positions: Vec<_> = list(&db, "c1").await.unwrap().into_iter().map(|i| i.position).collect();
        positions.sort();
        assert_eq!(positions, (0..8).collect::<Vec<_>>());
    }

    fn user_row(id: &str, conversation_id: &str, content: &str, turn_id: &str) -> crate::db::entity::message::Model {
        crate::db::entity::message::Model {
            id: id.into(),
            conversation_id: conversation_id.into(),
            role: "user".into(),
            content: content.into(),
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
            is_compact_summary: crate::db::types::SqlBool::FALSE,
            sender_id: None,
            parent_id: None,
            compact_anchor_id: None,
            source: None,
            turn_id: Some(turn_id.into()),
            tool_outcome: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            provider_name: None,
            provider_state: None,
            auto_review: None,
            server_tool_calls: None,
            tool_diffs: None,
            response_model_id: None,
        }
    }

    async fn take(db: &Db, message: crate::db::entity::message::Model) -> Result<Option<queued_prompt::Model>, DbErr> {
        db.write(async |tx| take_next(tx, "c1", Delivery::FollowUp, "t1", message, None, 5).await)
            .await
    }

    async fn messages(db: &Db) -> Vec<String> {
        crate::db::sea::ops::message::list_messages(db, "c1")
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.content)
            .collect()
    }

    /// Taking the item and writing the message it becomes cannot come apart:
    /// the item is settled against the row, and the row is on the path.
    #[tokio::test]
    async fn taking_an_item_and_writing_its_message_cannot_come_apart() {
        let db = with_conversations(&["c1"]).await;
        add(&db, "c1", "do the thing", Delivery::FollowUp).await;

        let taken = take(&db, user_row("m1", "c1", "do the thing", "t1"))
            .await
            .unwrap()
            .expect("an item was waiting");
        assert_eq!(taken.state(), QueueState::Settled);
        assert_eq!(
            (taken.settled_message_id.as_deref(), taken.dispatched_turn_id.as_deref()),
            (Some("m1"), Some("t1"))
        );
        assert_eq!(messages(&db).await, ["do the thing"]);
    }

    /// A failed take leaves nothing — no message, and the item still
    /// deliverable — even when the caller's write goes on: the take has its
    /// own savepoint.
    #[tokio::test]
    async fn a_failed_take_leaves_the_item_deliverable_and_writes_no_row() {
        let db = with_conversations(&["c1"]).await;
        add(&db, "c1", "first", Delivery::FollowUp).await;
        db.write(async |tx| {
            crate::db::sea::ops::message::append_message(tx, user_row("dup", "c1", "in the way", "t0"), None).await
        })
        .await
        .unwrap();

        let outcome = db
            .write(async |tx| {
                let failed = take_next(
                    tx,
                    "c1",
                    Delivery::FollowUp,
                    "t1",
                    user_row("dup", "c1", "first", "t1"),
                    None,
                    5,
                )
                .await;
                // The caller carries on and commits its own write.
                Ok::<_, DbErr>(failed.is_err())
            })
            .await
            .unwrap();
        assert!(outcome, "a duplicate message id must fail the whole take");
        assert_eq!(
            list(&db, "c1").await.unwrap()[0].state(),
            QueueState::Queued,
            "not consumed"
        );
        assert_eq!(messages(&db).await, ["in the way"], "and nothing new was written");
    }

    /// A settled row keeps its place in the list but is never re-taken.
    #[tokio::test]
    async fn a_refused_spend_takes_the_row_written_beside_it_back() {
        let db = with_conversations(&["c1"]).await;
        db.write(async |tx| {
            enqueue(tx, "q1", "c1", "first", Delivery::FollowUp, 1).await?;
            enqueue(tx, "q2", "c1", "second", Delivery::FollowUp, 2).await
        })
        .await
        .unwrap();
        let row = |id: &str| crate::db::sea::ops::message::new_row(id, "c1", "user", "x", 3);

        // q2 is not the front: the claim is refused and the row goes with it.
        let refused = db
            .write(async |tx| {
                crate::db::sea::ops::message::append_message(tx, row("m2"), None).await?;
                spend(tx, "c1", "q2", "t1", "m2", 3).await
            })
            .await;
        assert!(refused.is_err());
        assert!(
            crate::db::sea::ops::message::list_messages(&db, "c1")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(get(&db, "q2").await.unwrap().unwrap().state(), QueueState::Queued);

        db.write(async |tx| {
            crate::db::sea::ops::message::append_message(tx, row("m1"), None).await?;
            spend(tx, "c1", "q1", "t1", "m1", 4).await
        })
        .await
        .unwrap();
        let spent = get(&db, "q1").await.unwrap().unwrap();
        assert_eq!(spent.state(), QueueState::Settled);
        assert_eq!(spent.settled_message_id.as_deref(), Some("m1"));
    }

    #[tokio::test]
    async fn settled_items_are_stepped_over() {
        let db = with_conversations(&["c1"]).await;
        add(&db, "c1", "first", Delivery::FollowUp).await;
        add(&db, "c1", "second", Delivery::FollowUp).await;
        take(&db, user_row("m1", "c1", "first", "t1")).await.unwrap().unwrap();

        assert_eq!(list(&db, "c1").await.unwrap().len(), 2, "both rows are still listed");
        assert_eq!(
            deliverable(&db, "c1", Delivery::FollowUp).await.as_deref(),
            Some("second")
        );
    }
}

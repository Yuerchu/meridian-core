//! Unsent composer drafts, one row per composer.
//!
//! Every write carries a revision the writer chose, and is applied only when
//! that revision is higher than the stored one. Saves are debounced and sent
//! as independent async invokes, which the runtime does not order: a save set
//! off first can land last. Without the guard the landing order would decide
//! what the draft says, and the loser would be whatever was typed most
//! recently.
//!
//! An empty draft is not stored; writing one deletes the row under the same
//! guard. That leaves one hole the guard cannot see, and it is accepted: once
//! the row is gone there is no stored revision, so a stale save arriving
//! *after* a clear is applied. A single window cannot produce that order — the
//! client chains its writes per slot — so it takes two devices typing into the
//! same draft at once, where last-writer-wins is the only answer anyway.
//!
//! No function here opens a transaction of its own: a write takes the caller's
//! `WriteTx`, and the caller's `Db::write` is the `BEGIN IMMEDIATE` that makes
//! the guard's read and the write one step.

use sea_orm::sea_query::OnConflict;
use sea_orm::{DbErr, EntityTrait, IntoActiveModel, QuerySelect};

use crate::db::entity::composer_draft;
use crate::db::entity::composer_draft::DraftAttachment;
use crate::db::sea::cap::sealed::Access;
use crate::db::sea::cap::{Read, WriteTx};
use crate::db::types::{EpochMs, Json};

/// Which composer a draft belongs to.
///
/// There is one composer per conversation and one on the welcome screen, and
/// nothing else types into the database. Kept as an enum rather than an
/// `Option<&str>` so the key the row is stored under is derived in one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftSlot {
    /// The welcome composer, before its first send creates a conversation.
    NewConversation,
    Conversation(String),
}

impl DraftSlot {
    /// `None` is the welcome composer — the caller's "no conversation yet".
    pub fn for_conversation(conversation_id: Option<String>) -> Self {
        match conversation_id {
            Some(id) => Self::Conversation(id),
            None => Self::NewConversation,
        }
    }

    /// The primary key. The table's CHECK pins the same two spellings.
    pub fn key(&self) -> String {
        match self {
            Self::NewConversation => "new".to_string(),
            Self::Conversation(id) => format!("conversation:{id}"),
        }
    }

    pub fn conversation_id(&self) -> Option<&str> {
        match self {
            Self::NewConversation => None,
            Self::Conversation(id) => Some(id),
        }
    }
}

/// What a composer holds, decoded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComposerDraftContent {
    pub body: String,
    pub attachments: Vec<DraftAttachment>,
    pub conversation_refs: Vec<String>,
    pub sticker_id: Option<String>,
}

impl ComposerDraftContent {
    /// Nothing worth keeping. An empty draft is stored as no row at all, so
    /// "is there a draft" and "does a row exist" are the same question.
    ///
    /// Whitespace counts as content: it is what is in the field, and a draft
    /// that comes back without the blank line somebody was about to type after
    /// would be a draft that changed on its own.
    pub fn is_empty(&self) -> bool {
        self.body.is_empty()
            && self.attachments.is_empty()
            && self.conversation_refs.is_empty()
            && self.sticker_id.is_none()
    }
}

impl From<composer_draft::Model> for ComposerDraftContent {
    fn from(row: composer_draft::Model) -> Self {
        Self {
            body: row.body,
            attachments: row.attachments.into_inner(),
            conversation_refs: row.conversation_refs.into_inner(),
            sticker_id: row.sticker_id,
        }
    }
}

/// What a guarded write did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftWriteOutcome {
    /// Written (or, for an empty draft, removed). The stored revision is now
    /// the one that was asked for.
    Applied,
    /// A newer revision was already stored and nothing changed. Carries that
    /// revision so the writer can continue above it.
    Stale { current_revision: i64 },
}

/// The draft on record for a composer, if there is one.
pub async fn get(db: &impl Read, slot: &DraftSlot) -> Result<Option<composer_draft::Model>, DbErr> {
    composer_draft::Entity::find_by_id(slot.key()).one(db.conn()?).await
}

/// Write a draft, or remove it if it is empty, unless a newer one is stored.
///
/// The insert names every column and the conflict rewrites every column but
/// the key and `created_at`, so a part the new content leaves out — a sticker
/// taken off, the last attachment removed — is written as such rather than
/// left as it was.
pub async fn save(
    tx: &WriteTx,
    slot: &DraftSlot,
    content: &ComposerDraftContent,
    revision: i64,
    now: EpochMs,
) -> Result<DraftWriteOutcome, DbErr> {
    let key = slot.key();
    let stored: Option<i64> = composer_draft::Entity::find_by_id(key.clone())
        .select_only()
        .column(composer_draft::Column::Revision)
        .into_tuple()
        .one(tx.conn()?)
        .await?;
    if let Some(current_revision) = stored
        && current_revision >= revision
    {
        return Ok(DraftWriteOutcome::Stale { current_revision });
    }

    if content.is_empty() {
        composer_draft::Entity::delete_by_id(key).exec(tx.conn()?).await?;
        return Ok(DraftWriteOutcome::Applied);
    }

    let row = composer_draft::Model {
        slot: key,
        conversation_id: slot.conversation_id().map(str::to_owned),
        body: content.body.clone(),
        attachments: Json(content.attachments.clone()),
        conversation_refs: Json(content.conversation_refs.clone()),
        sticker_id: content.sticker_id.clone(),
        revision,
        created_at: now,
        updated_at: now,
    };
    composer_draft::Entity::insert(row.into_active_model())
        .on_conflict(
            OnConflict::column(composer_draft::Column::Slot)
                .update_columns([
                    composer_draft::Column::Body,
                    composer_draft::Column::Attachments,
                    composer_draft::Column::ConversationRefs,
                    composer_draft::Column::StickerId,
                    composer_draft::Column::Revision,
                    composer_draft::Column::UpdatedAt,
                ])
                .to_owned(),
        )
        .exec_without_returning(tx.conn()?)
        .await?;
    Ok(DraftWriteOutcome::Applied)
}

/// Remove a draft, under the same guard as [`save`]: it is a save of nothing.
///
/// Called once what the draft held has been sent. A clear whose revision is
/// not newer than the stored one means something was typed after the send
/// was set off, and that is kept.
pub async fn clear(tx: &WriteTx, slot: &DraftSlot, revision: i64, now: EpochMs) -> Result<DraftWriteOutcome, DbErr> {
    save(tx, slot, &ComposerDraftContent::default(), revision, now).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::entity::emoji::EmojiSemanticStatus;
    use crate::db::entity::emoji_pack::EmojiPackKind;
    use crate::db::sea::cap::Db;
    use crate::db::sea::ops::{emoji, emoji_pack};
    use crate::db::sea::{execute_for_tests, sea_test_db};

    async fn conversation(db: &Db, id: &str) {
        execute_for_tests(
            db,
            &format!("INSERT INTO conversations (id, title, created_at, updated_at) VALUES ('{id}', 'draft', 0, 0)"),
        )
        .await
        .unwrap();
    }

    fn text(body: &str) -> ComposerDraftContent {
        ComposerDraftContent {
            body: body.to_string(),
            ..ComposerDraftContent::default()
        }
    }

    fn slot(id: &str) -> DraftSlot {
        DraftSlot::Conversation(id.to_string())
    }

    async fn write(
        db: &Db,
        slot: &DraftSlot,
        content: &ComposerDraftContent,
        revision: i64,
        now: EpochMs,
    ) -> Result<DraftWriteOutcome, DbErr> {
        db.write(async |tx| save(tx, slot, content, revision, now).await).await
    }

    async fn erase(db: &Db, slot: &DraftSlot, revision: i64, now: EpochMs) -> DraftWriteOutcome {
        db.write(async |tx| clear(tx, slot, revision, now).await).await.unwrap()
    }

    /// The ordinary life of a draft: written, rewritten in place, read back
    /// with every part intact.
    #[tokio::test]
    async fn a_draft_round_trips_and_is_rewritten_in_place() {
        let db = sea_test_db().await;
        conversation(&db, "c1").await;
        conversation(&db, "c2").await;

        let content = ComposerDraftContent {
            body: "half a thought".to_string(),
            attachments: vec![DraftAttachment {
                path: "/work/notes.md".to_string(),
                name: "notes.md".to_string(),
            }],
            conversation_refs: vec!["c2".to_string()],
            sticker_id: None,
        };
        assert_eq!(
            write(&db, &slot("c1"), &content, 1, 100).await.unwrap(),
            DraftWriteOutcome::Applied
        );
        let row = get(&db, &slot("c1")).await.unwrap().expect("written");
        assert_eq!(row.conversation_id.as_deref(), Some("c1"));
        assert_eq!(row.revision, 1);
        assert_eq!(ComposerDraftContent::from(row), content);

        write(&db, &slot("c1"), &text("half a thought, finished"), 2, 200)
            .await
            .unwrap();
        let row = get(&db, &slot("c1")).await.unwrap().unwrap();
        assert_eq!(
            (row.revision, row.created_at, row.updated_at),
            (2, 100, 200),
            "first typed stays first typed"
        );
        assert_eq!(ComposerDraftContent::from(row), text("half a thought, finished"));
    }

    /// A rewrite replaces the draft whole: what the new content leaves out is
    /// gone from the row, not kept from the previous save.
    #[tokio::test]
    async fn a_rewrite_clears_what_it_leaves_out() {
        let db = sea_test_db().await;
        conversation(&db, "c1").await;
        emoji_pack::tests::insert(&db, emoji_pack::tests::pack("p1", EmojiPackKind::Manual, None, 0)).await;
        emoji::tests::insert(&db, emoji::tests::sticker("e1", "p1", EmojiSemanticStatus::Confirmed)).await;

        let full = ComposerDraftContent {
            body: "look".to_string(),
            attachments: vec![DraftAttachment {
                path: "/a".to_string(),
                name: "a".to_string(),
            }],
            conversation_refs: vec!["c1".to_string()],
            sticker_id: Some("e1".to_string()),
        };
        write(&db, &slot("c1"), &full, 1, 100).await.unwrap();
        write(&db, &slot("c1"), &text("look"), 2, 200).await.unwrap();
        let row = get(&db, &slot("c1")).await.unwrap().unwrap();
        assert_eq!(ComposerDraftContent::from(row), text("look"));
    }

    /// The guard. A save that set off before the one already stored lands
    /// after it and must change nothing — including an equal revision, which
    /// is a replay rather than something new.
    #[tokio::test]
    async fn an_older_or_equal_revision_is_refused() {
        let db = sea_test_db().await;
        conversation(&db, "c1").await;

        write(&db, &slot("c1"), &text("newest"), 5, 100).await.unwrap();
        for (body, revision) in [("older", 4), ("replay", 5)] {
            assert_eq!(
                write(&db, &slot("c1"), &text(body), revision, 200).await.unwrap(),
                DraftWriteOutcome::Stale { current_revision: 5 }
            );
        }
        let row = get(&db, &slot("c1")).await.unwrap().unwrap();
        assert_eq!((row.body.as_str(), row.updated_at), ("newest", 100));

        // Nor may a late clear take away what was typed after the send.
        assert_eq!(
            erase(&db, &slot("c1"), 3, 400).await,
            DraftWriteOutcome::Stale { current_revision: 5 }
        );
        assert!(get(&db, &slot("c1")).await.unwrap().is_some());
    }

    /// Two saves for one slot reach the database together: the guard's read
    /// and the write are one step, so exactly one of two equal revisions is
    /// applied and the other reports it.
    #[tokio::test]
    async fn racing_saves_of_one_revision_apply_once() {
        let dir = tempfile::tempdir().unwrap();
        let (_diesel, db) = crate::db::sea::shared_test_db(dir.path()).await;
        conversation(&db, "c1").await;
        let (slot, a, b) = (slot("c1"), text("a"), text("b"));
        let (a, b) = tokio::join!(write(&db, &slot, &a, 1, 100), write(&db, &slot, &b, 1, 100));
        let mut outcomes = [a.unwrap(), b.unwrap()];
        outcomes.sort_by_key(|o| matches!(o, DraftWriteOutcome::Stale { .. }));
        assert_eq!(
            outcomes,
            [
                DraftWriteOutcome::Applied,
                DraftWriteOutcome::Stale { current_revision: 1 }
            ]
        );
    }

    /// Empty is not stored. Writing nothing removes the row, so a composer
    /// that was emptied reads back as having no draft at all.
    #[tokio::test]
    async fn an_empty_draft_deletes_the_row() {
        let db = sea_test_db().await;
        conversation(&db, "c1").await;

        write(&db, &slot("c1"), &text("something"), 1, 100).await.unwrap();
        assert_eq!(
            write(&db, &slot("c1"), &text(""), 2, 200).await.unwrap(),
            DraftWriteOutcome::Applied
        );
        assert!(get(&db, &slot("c1")).await.unwrap().is_none());

        // An empty save with no row is a no-op, not an insert of an empty row.
        write(&db, &slot("c1"), &text(""), 3, 300).await.unwrap();
        assert!(get(&db, &slot("c1")).await.unwrap().is_none());

        // Clearing after a send is the same thing.
        write(&db, &slot("c1"), &text("again"), 4, 400).await.unwrap();
        erase(&db, &slot("c1"), 5, 500).await;
        assert!(get(&db, &slot("c1")).await.unwrap().is_none());

        // Whitespace is content: it is what is in the field.
        write(&db, &slot("c1"), &text("\n"), 6, 600).await.unwrap();
        assert!(get(&db, &slot("c1")).await.unwrap().is_some());
    }

    /// The draft goes with its conversation, through the foreign key rather
    /// than a line in `delete_conversation`.
    #[tokio::test]
    async fn deleting_the_conversation_takes_its_draft() {
        let db = sea_test_db().await;
        conversation(&db, "c1").await;
        write(&db, &slot("c1"), &text("unsent"), 1, 100).await.unwrap();

        execute_for_tests(&db, "DELETE FROM conversations WHERE id = 'c1'")
            .await
            .unwrap();
        let remaining = composer_draft::Entity::find().all(db.conn().unwrap()).await.unwrap();
        assert!(remaining.is_empty(), "{remaining:?}");
    }

    /// The welcome composer has one slot of its own, apart from every
    /// conversation's, and a draft cannot be filed under a conversation that
    /// does not exist.
    #[tokio::test]
    async fn the_welcome_slot_is_separate_and_conversation_slots_need_a_conversation() {
        let db = sea_test_db().await;
        conversation(&db, "c1").await;

        write(&db, &DraftSlot::NewConversation, &text("before any"), 1, 100)
            .await
            .unwrap();
        write(&db, &slot("c1"), &text("inside one"), 1, 100).await.unwrap();
        let welcome = get(&db, &DraftSlot::NewConversation).await.unwrap().unwrap();
        assert_eq!((welcome.body.as_str(), welcome.conversation_id), ("before any", None));
        assert_eq!(get(&db, &slot("c1")).await.unwrap().unwrap().body, "inside one");

        assert!(write(&db, &slot("missing"), &text("orphan"), 1, 100).await.is_err());
    }

    /// The CHECK is what keeps the key and the foreign key saying the same
    /// thing; a row written around `save` must not be able to split them.
    #[tokio::test]
    async fn the_slot_and_the_conversation_cannot_disagree() {
        let db = sea_test_db().await;
        conversation(&db, "c1").await;
        conversation(&db, "c2").await;

        for (slot, conversation_id) in [("conversation:c2", "c1"), ("new", "c1")] {
            let wrong = composer_draft::Model {
                slot: slot.into(),
                conversation_id: Some(conversation_id.into()),
                body: "x".into(),
                attachments: Json(Vec::new()),
                conversation_refs: Json(Vec::new()),
                sticker_id: None,
                revision: 1,
                created_at: 0,
                updated_at: 0,
            };
            let written = db
                .write(async |tx| {
                    composer_draft::Entity::insert(wrong.into_active_model())
                        .exec_without_returning(tx.conn()?)
                        .await
                })
                .await;
            assert!(written.is_err(), "{slot} under {conversation_id}");
        }
    }

    /// Deleting a sticker takes it off a draft rather than refusing the
    /// delete; the rest of the draft stays.
    #[tokio::test]
    async fn deleting_the_sticker_takes_it_off_the_draft() {
        let db = sea_test_db().await;
        conversation(&db, "c1").await;
        emoji_pack::tests::insert(&db, emoji_pack::tests::pack("p1", EmojiPackKind::Manual, None, 0)).await;
        emoji::tests::insert(&db, emoji::tests::sticker("e1", "p1", EmojiSemanticStatus::Confirmed)).await;

        let content = ComposerDraftContent {
            body: "look".to_string(),
            sticker_id: Some("e1".to_string()),
            ..ComposerDraftContent::default()
        };
        write(&db, &slot("c1"), &content, 1, 100).await.unwrap();
        db.write(async |tx| emoji::delete_emoji(tx, "e1").await).await.unwrap();

        let row = get(&db, &slot("c1")).await.unwrap().unwrap();
        assert_eq!((row.sticker_id, row.body.as_str()), (None, "look"));
    }

    /// A stored column that does not decode fails the read, not an empty list.
    #[tokio::test]
    async fn a_malformed_column_fails_the_read() {
        let db = sea_test_db().await;
        conversation(&db, "c1").await;
        execute_for_tests(
            &db,
            r#"INSERT INTO composer_drafts (slot, conversation_id, body, attachments, conversation_refs, revision, created_at, updated_at)
               VALUES ('conversation:c1', 'c1', 'x', '[{"path":"/a","name":"a","extra":1}]', '[]', 1, 0, 0)"#,
        )
        .await
        .unwrap();
        assert!(get(&db, &slot("c1")).await.is_err());
    }
}

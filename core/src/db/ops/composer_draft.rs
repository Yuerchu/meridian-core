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

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::models::composer_draft::{
    ComposerDraftChangeset, ComposerDraftContent, ComposerDraftInsert, ComposerDraftRow, DraftSlot,
};
use crate::db::schema::composer_drafts;

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
pub fn get(conn: &mut SqliteConnection, slot: &DraftSlot) -> QueryResult<Option<ComposerDraftRow>> {
    composer_drafts::table
        .find(slot.key())
        .select(ComposerDraftRow::as_select())
        .first(conn)
        .optional()
}

/// Write a draft, or remove it if it is empty, unless a newer one is stored.
///
/// `immediate_transaction`: the guard is a read followed by a write, and two
/// saves for one slot can reach the database back to back. `BEGIN IMMEDIATE`
/// takes the write lock before the read, so the second reads what the first
/// wrote.
pub fn save(
    conn: &mut SqliteConnection,
    slot: &DraftSlot,
    content: &ComposerDraftContent,
    revision: i64,
    now: i64,
) -> QueryResult<DraftWriteOutcome> {
    let key = slot.key();
    conn.immediate_transaction(|conn| {
        let stored = composer_drafts::table
            .find(&key)
            .select(composer_drafts::revision)
            .first::<i64>(conn)
            .optional()?;
        if let Some(current_revision) = stored
            && current_revision >= revision
        {
            return Ok(DraftWriteOutcome::Stale { current_revision });
        }

        if content.is_empty() {
            diesel::delete(composer_drafts::table.find(&key)).execute(conn)?;
            return Ok(DraftWriteOutcome::Applied);
        }

        let attachments = serde_json::to_string(&content.attachments)
            .map_err(|e| diesel::result::Error::SerializationError(Box::new(e)))?;
        let conversation_refs = serde_json::to_string(&content.conversation_refs)
            .map_err(|e| diesel::result::Error::SerializationError(Box::new(e)))?;
        diesel::insert_into(composer_drafts::table)
            .values(&ComposerDraftInsert {
                slot: &key,
                conversation_id: slot.conversation_id(),
                body: &content.body,
                attachments: &attachments,
                conversation_refs: &conversation_refs,
                sticker_id: content.sticker_id.as_deref(),
                revision,
                created_at: now,
                updated_at: now,
            })
            .on_conflict(composer_drafts::slot)
            .do_update()
            .set(&ComposerDraftChangeset {
                body: &content.body,
                attachments: &attachments,
                conversation_refs: &conversation_refs,
                sticker_id: content.sticker_id.as_deref(),
                revision,
                updated_at: now,
            })
            .execute(conn)?;
        Ok(DraftWriteOutcome::Applied)
    })
}

/// Remove a draft, under the same guard as [`save`]: it is a save of nothing.
///
/// Called once what the draft held has been sent. A clear whose revision is
/// not newer than the stored one means something was typed after the send
/// was set off, and that is kept.
pub fn clear(conn: &mut SqliteConnection, slot: &DraftSlot, revision: i64, now: i64) -> QueryResult<DraftWriteOutcome> {
    save(conn, slot, &ComposerDraftContent::default(), revision, now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::composer_draft::DraftAttachment;
    use crate::db::test_db;

    fn conversation(conn: &mut SqliteConnection, id: &str) {
        crate::db::ops::conversation::create_conversation(conn, id, Some("draft"), None, None, 0).unwrap();
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

    /// The ordinary life of a draft: written, rewritten in place, read back
    /// with every part intact.
    #[test]
    fn a_draft_round_trips_and_is_rewritten_in_place() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        conversation(&mut conn, "c2");

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
            save(&mut conn, &slot("c1"), &content, 1, 100).unwrap(),
            DraftWriteOutcome::Applied
        );
        let row = get(&mut conn, &slot("c1")).unwrap().expect("written");
        assert_eq!(row.conversation_id.as_deref(), Some("c1"));
        assert_eq!(row.content().unwrap(), content);
        assert_eq!(row.revision, 1);

        save(&mut conn, &slot("c1"), &text("half a thought, finished"), 2, 200).unwrap();
        let row = get(&mut conn, &slot("c1")).unwrap().unwrap();
        assert_eq!(row.content().unwrap(), text("half a thought, finished"));
        assert_eq!(row.revision, 2);
        assert_eq!(row.created_at, 100, "first typed stays first typed");
        assert_eq!(row.updated_at, 200);
    }

    /// The guard. A save that set off before the one already stored lands
    /// after it and must change nothing — including an equal revision, which
    /// is a replay rather than something new.
    #[test]
    fn an_older_or_equal_revision_is_refused() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");

        save(&mut conn, &slot("c1"), &text("newest"), 5, 100).unwrap();
        assert_eq!(
            save(&mut conn, &slot("c1"), &text("older"), 4, 200).unwrap(),
            DraftWriteOutcome::Stale { current_revision: 5 }
        );
        assert_eq!(
            save(&mut conn, &slot("c1"), &text("replay"), 5, 300).unwrap(),
            DraftWriteOutcome::Stale { current_revision: 5 }
        );
        let row = get(&mut conn, &slot("c1")).unwrap().unwrap();
        assert_eq!(row.body, "newest");
        assert_eq!(row.updated_at, 100);

        // Nor may a late clear take away what was typed after the send.
        assert_eq!(
            clear(&mut conn, &slot("c1"), 3, 400).unwrap(),
            DraftWriteOutcome::Stale { current_revision: 5 }
        );
        assert!(get(&mut conn, &slot("c1")).unwrap().is_some());
    }

    /// Empty is not stored. Writing nothing removes the row, so a composer
    /// that was emptied reads back as having no draft at all.
    #[test]
    fn an_empty_draft_deletes_the_row() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");

        save(&mut conn, &slot("c1"), &text("something"), 1, 100).unwrap();
        assert_eq!(
            save(&mut conn, &slot("c1"), &text(""), 2, 200).unwrap(),
            DraftWriteOutcome::Applied
        );
        assert!(get(&mut conn, &slot("c1")).unwrap().is_none());

        // An empty save with no row is a no-op, not an insert of an empty row.
        save(&mut conn, &slot("c1"), &text(""), 3, 300).unwrap();
        assert!(get(&mut conn, &slot("c1")).unwrap().is_none());

        // Clearing after a send is the same thing.
        save(&mut conn, &slot("c1"), &text("again"), 4, 400).unwrap();
        clear(&mut conn, &slot("c1"), 5, 500).unwrap();
        assert!(get(&mut conn, &slot("c1")).unwrap().is_none());

        // Whitespace is content: it is what is in the field.
        save(&mut conn, &slot("c1"), &text("\n"), 6, 600).unwrap();
        assert!(get(&mut conn, &slot("c1")).unwrap().is_some());
    }

    /// The draft goes with its conversation, through the foreign key rather
    /// than a line in `delete_conversation`.
    #[test]
    fn deleting_the_conversation_takes_its_draft() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        save(&mut conn, &slot("c1"), &text("unsent"), 1, 100).unwrap();

        crate::db::ops::conversation::delete_conversation(&mut conn, "c1").unwrap();
        assert!(get(&mut conn, &slot("c1")).unwrap().is_none());
        let remaining: i64 = composer_drafts::table.count().get_result(&mut conn).unwrap();
        assert_eq!(remaining, 0);
    }

    /// The welcome composer has one slot of its own, apart from every
    /// conversation's, and a draft cannot be filed under a conversation that
    /// does not exist.
    #[test]
    fn the_welcome_slot_is_separate_and_conversation_slots_need_a_conversation() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");

        save(&mut conn, &DraftSlot::NewConversation, &text("before any"), 1, 100).unwrap();
        save(&mut conn, &slot("c1"), &text("inside one"), 1, 100).unwrap();
        let welcome = get(&mut conn, &DraftSlot::NewConversation).unwrap().unwrap();
        assert_eq!(welcome.body, "before any");
        assert_eq!(welcome.conversation_id, None);
        assert_eq!(get(&mut conn, &slot("c1")).unwrap().unwrap().body, "inside one");

        assert!(save(&mut conn, &slot("missing"), &text("orphan"), 1, 100).is_err());
    }

    /// The CHECK is what keeps the key and the foreign key saying the same
    /// thing; a row written around `save` must not be able to split them.
    #[test]
    fn the_slot_and_the_conversation_cannot_disagree() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        conversation(&mut conn, "c2");

        let wrong = ComposerDraftInsert {
            slot: "conversation:c2",
            conversation_id: Some("c1"),
            body: "x",
            attachments: "[]",
            conversation_refs: "[]",
            sticker_id: None,
            revision: 1,
            created_at: 0,
            updated_at: 0,
        };
        assert!(
            diesel::insert_into(composer_drafts::table)
                .values(&wrong)
                .execute(&mut conn)
                .is_err()
        );
        let welcome_with_conversation = ComposerDraftInsert {
            slot: "new",
            conversation_id: Some("c1"),
            ..wrong
        };
        assert!(
            diesel::insert_into(composer_drafts::table)
                .values(&welcome_with_conversation)
                .execute(&mut conn)
                .is_err()
        );
    }

    /// Deleting a sticker takes it off a draft rather than refusing the
    /// delete; the rest of the draft stays.
    #[test]
    fn deleting_the_sticker_takes_it_off_the_draft() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        crate::db::ops::emoji_pack::create_pack(
            &mut conn,
            &crate::db::models::emoji_pack::EmojiPackInsert {
                id: "p1",
                name: "pack",
                description: None,
                cover_image: None,
                is_builtin: 0,
                sort_order: 0,
                created_at: 1,
                updated_at: 1,
                kind: "manual",
                source_account_id: None,
            },
        )
        .unwrap();
        crate::db::ops::emoji::create_emoji(
            &mut conn,
            &crate::db::models::emoji::EmojiInsert {
                id: "e1",
                pack_id: "p1",
                name: "wave",
                tags: None,
                file_name: "",
                file_format: "",
                sort_order: 0,
                created_at: 1,
                source: "local",
                source_key: None,
                native_payload: None,
                semantic_status: "confirmed",
                suggested_name: None,
                suggested_tags: None,
                file_size: 0,
                seen_count: 0,
                last_seen_at: None,
            },
        )
        .unwrap();

        let content = ComposerDraftContent {
            body: "look".to_string(),
            sticker_id: Some("e1".to_string()),
            ..ComposerDraftContent::default()
        };
        save(&mut conn, &slot("c1"), &content, 1, 100).unwrap();
        crate::db::ops::emoji::delete_emoji(&mut conn, "e1").unwrap();

        let row = get(&mut conn, &slot("c1")).unwrap().unwrap();
        assert_eq!(row.sticker_id, None);
        assert_eq!(row.body, "look");
    }

    /// A stored column that does not decode is an error, not an empty list.
    #[test]
    fn a_malformed_column_fails_to_decode() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        conversation(&mut conn, "c1");
        diesel::insert_into(composer_drafts::table)
            .values(&ComposerDraftInsert {
                slot: "conversation:c1",
                conversation_id: Some("c1"),
                body: "x",
                attachments: r#"[{"path":"/a","name":"a","extra":1}]"#,
                conversation_refs: "[]",
                sticker_id: None,
                revision: 1,
                created_at: 0,
                updated_at: 0,
            })
            .execute(&mut conn)
            .unwrap();
        assert!(get(&mut conn, &slot("c1")).unwrap().unwrap().content().is_err());
    }
}

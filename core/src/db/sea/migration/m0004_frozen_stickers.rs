//! Every sticker part already in a user message gets the `seen_as` that
//! `agent::freeze_sticker_parts` now records when a message is sent.
//!
//! Before it, a sticker was rendered from whatever its row said at the time of
//! each request, and an unlabelled one was shown as a picture only while its
//! message was the newest. Both rewrote messages already sent, which Anthropic
//! refuses once a signed thinking block follows them. Rendering now reads the
//! part alone and refuses one that was never frozen, so stored rows need the
//! field. What the sticker was when it was sent is not recorded anywhere; its
//! state today is the best answer available, and for a sticker nobody has
//! labelled since, it is also what the turn that first sent it saw. A sticker
//! whose row is gone keeps the text it has been rendered as since.
//!
//! The descriptions are spelled here as `freeze_sticker_parts` spelled them on
//! the day this was written, and stay that way if it changes: a migration
//! records what was true when it ran.
//!
//! Data only: no table or column changes. `queued_prompts` is left alone — a
//! queued message is frozen when it is sent, like any other.
//!
//! backend: sqlite-only — the JSON functions are SQLite's.

/// The statements SQLite runs, in order.
pub fn sqlite_statements() -> Vec<String> {
    vec![
        r#"UPDATE "messages" SET "content" = (
    SELECT json_group_array(
        CASE WHEN json_extract(p.value, '$.type') = 'sticker'
            THEN json_set(p.value, '$.seen_as', json(coalesce(
                (SELECT CASE WHEN e.semantic_status = 'confirmed'
                    THEN json_object('kind', 'described', 'text',
                        CASE WHEN e.tags IS NOT NULL AND trim(e.tags) <> ''
                            THEN '[sticker: ' || e.name || '; tags: ' || e.tags || ']'
                            ELSE '[sticker: ' || e.name || ']' END)
                    ELSE json_object('kind', 'unlabelled') END
                 FROM "emojis" e WHERE e.id = json_extract(p.value, '$.sticker_id')),
                json_object('kind', 'described', 'text', '[unavailable sticker]'))))
            ELSE json(p.value) END)
    FROM json_each("messages"."content") p)
WHERE "role" = 'user'
  AND "content" LIKE '[{%'
  AND CASE WHEN json_valid("content")
        THEN EXISTS (SELECT 1 FROM json_each("messages"."content") p
                     WHERE json_extract(p.value, '$.type') = 'sticker'
                       AND json_extract(p.value, '$.seen_as') IS NULL)
        ELSE 0 END"#
            .to_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};
    use sea_orm_migration::{MigrationTrait, MigratorTrait};

    use super::super::{M0001Baseline, M0002SkillKeys, M0003BackgroundTasks};
    use crate::db::sea::{bridge, memory_connection};
    use crate::provider::{MessageContentPart, StickerSeenAs, decode_message_parts};

    /// The schema as it was before this migration.
    struct Before;

    #[async_trait::async_trait]
    impl MigratorTrait for Before {
        fn migrations() -> Vec<Box<dyn MigrationTrait>> {
            vec![
                Box::new(M0001Baseline),
                Box::new(M0002SkillKeys),
                Box::new(M0003BackgroundTasks),
            ]
        }
    }

    async fn content(conn: &DatabaseConnection, id: &str) -> String {
        conn.query_one_raw(Statement::from_string(
            DbBackend::Sqlite,
            format!("SELECT content FROM messages WHERE id = '{id}'"),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index::<String>(0)
        .unwrap()
    }

    fn seen_as(content: &str) -> Vec<Option<StickerSeenAs>> {
        decode_message_parts(content)
            .unwrap()
            .unwrap()
            .into_iter()
            .filter_map(|part| match part {
                MessageContentPart::Sticker { seen_as, .. } => Some(seen_as),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn each_sticker_is_frozen_as_it_stands_and_nothing_else_moves() {
        let conn = memory_connection().await;
        conn.execute_unprepared("PRAGMA foreign_keys = OFF").await.unwrap();
        bridge::migrate_with::<Before>(&conn).await.unwrap();
        conn.execute_unprepared(
            r#"INSERT INTO emoji_packs (id, name, created_at, updated_at) VALUES ('p', 'P', 1, 1);
               INSERT INTO emojis (id, pack_id, name, tags, file_name, created_at, semantic_status) VALUES
                   ('known', 'p', 'wave', 'hi', 'a.gif', 1, 'confirmed'),
                   ('bare', 'p', 'nod', '  ', 'b.gif', 1, 'confirmed'),
                   ('new', 'p', 'x', NULL, 'c.gif', 1, 'pending');
               INSERT INTO conversations (id, created_at, updated_at) VALUES ('c', 1, 1);
               INSERT INTO messages (id, conversation_id, role, content, created_at) VALUES
                   ('stickers', 'c', 'user',
                    '[{"type":"text","text":"look"},{"type":"sticker","sticker_id":"known","name":"wave"},{"type":"sticker","sticker_id":"bare"},{"type":"sticker","sticker_id":"new"},{"type":"sticker","sticker_id":"gone"}]', 1),
                   ('plain', 'c', 'user', '[{ not json', 1),
                   ('image', 'c', 'user', '[{"type":"image_url","image_url":{"url":"file:///a.png"}}]', 1),
                   ('said', 'c', 'assistant', '[{"type":"sticker","sticker_id":"known"}]', 1);"#,
        )
        .await
        .unwrap();
        let untouched = [
            ("plain", content(&conn, "plain").await),
            ("image", content(&conn, "image").await),
            ("said", content(&conn, "said").await),
        ];

        bridge::migrate(&conn).await.unwrap();

        let frozen = content(&conn, "stickers").await;
        let described = |text: &str| Some(StickerSeenAs::Described { text: text.into() });
        assert_eq!(
            seen_as(&frozen),
            [
                described("[sticker: wave; tags: hi]"),
                described("[sticker: nod]"),
                Some(StickerSeenAs::Unlabelled),
                described("[unavailable sticker]"),
            ]
        );
        let parts = decode_message_parts(&frozen).unwrap().unwrap();
        assert_eq!(
            parts[0],
            MessageContentPart::Text { text: "look".into() },
            "order and text kept"
        );
        assert!(matches!(&parts[1], MessageContentPart::Sticker { name: Some(n), .. } if n == "wave"));
        for (id, before) in untouched {
            assert_eq!(content(&conn, id).await, before, "{id}");
        }
    }
}

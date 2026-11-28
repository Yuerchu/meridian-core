//! Generalise channel/external source identifiers so `meridian-core` carries no
//! OneBot-specific spellings while preserving legacy rows as read-only archive
//! records (and letting external servers share the same core schema).
//!
//! - Adds `projects.channel_kind` (nullable free-form adapter tag such as
//!   `"archived_channel"`, `"telegram"`, etc.).
//! - Maps legacy `onebot_*` stored enum strings across `projects`, `turns`,
//!   `audit_messages`, `memories`, `emojis`, `emoji_packs`, `voice_blobs`, and
//!   `voice_clips` to platform-neutral channel/external variants.

pub fn sqlite_statements() -> Vec<String> {
    vec![
        "ALTER TABLE projects ADD COLUMN channel_kind TEXT".into(),
        "UPDATE projects SET source_type = 'channel_direct', channel_kind = 'archived_channel' WHERE source_type = 'onebot_private'".into(),
        "UPDATE projects SET source_type = 'channel_group', channel_kind = 'archived_channel' WHERE source_type = 'onebot_group'".into(),
        "UPDATE turns SET origin = 'channel' WHERE origin = 'onebot'".into(),
        "UPDATE audit_messages SET turn_origin = 'channel' WHERE turn_origin = 'onebot'".into(),
        "UPDATE audit_messages SET source_type = 'channel_direct' WHERE source_type = 'onebot_private'".into(),
        "UPDATE audit_messages SET source_type = 'channel_group' WHERE source_type = 'onebot_group'".into(),
        "UPDATE memories SET scope_type = 'channel_global' WHERE scope_type = 'onebot_global'".into(),
        "UPDATE memories SET scope_type = 'external_subject' WHERE scope_type = 'onebot_user'".into(),
        "UPDATE emojis SET source = 'external' WHERE source IN ('onebot_face', 'onebot_mface', 'onebot_image')".into(),
        "UPDATE emoji_packs SET kind = 'external' WHERE kind = 'onebot'".into(),
        "UPDATE voice_blobs SET source_type = 'channel_group' WHERE source_type = 'onebot_group'".into(),
        "UPDATE voice_blobs SET source_type = 'channel_direct' WHERE source_type = 'onebot_private'".into(),
        "UPDATE voice_clips SET source_type = 'channel_group' WHERE source_type = 'onebot_group'".into(),
        "UPDATE voice_clips SET source_type = 'channel_direct' WHERE source_type = 'onebot_private'".into(),
    ]
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Database};

    #[tokio::test]
    async fn migrates_legacy_onebot_rows_to_platform_neutral_channel_variants() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        // Run all migrations up to m0015 first, seed legacy rows, then run m0016.
        let mut before = super::super::m0001_baseline::sqlite_statements();
        before.extend(super::super::m0002_skill_keys::sqlite_statements());
        before.extend(super::super::m0003_background_tasks::sqlite_statements());
        before.extend(super::super::m0004_frozen_stickers::sqlite_statements());
        before.extend(super::super::m0005_board_tasks::sqlite_statements());
        before.extend(super::super::m0006_local_inference_backend::sqlite_statements());
        before.extend(super::super::m0007_compact_keep_recent_tokens::sqlite_statements());
        before.extend(super::super::m0008_mcp_disabled_tools::sqlite_statements());
        before.extend(super::super::m0009_readable_mime_overrides::sqlite_statements());
        before.extend(super::super::m0010_stream_timing::sqlite_statements());
        before.extend(super::super::m0011_custom_tool_execution::sqlite_statements());
        before.extend(super::super::m0012_many_to_many_models::sqlite_statements());
        before.extend(super::super::m0013_media_generations::sqlite_statements());
        before.extend(super::super::m0014_emoji_embeddings::sqlite_statements());
        before.extend(super::super::m0015_cache_ttl_and_lifecycle::sqlite_statements());
        for sql in before {
            db.execute_unprepared(&sql).await.unwrap();
        }

        db.execute_unprepared(
            "INSERT INTO projects (id, name, source_type, source_id, created_at, updated_at) \
             VALUES ('p1', 'Group', 'onebot_group', '123', 1, 1), ('p2', 'DM', 'onebot_private', '456', 1, 1)",
        )
        .await
        .unwrap();
        db.execute_unprepared(
            "INSERT INTO conversations (id, project_id, created_at, updated_at) VALUES ('c1', 'p1', 1, 1)",
        )
        .await
        .unwrap();
        db.execute_unprepared(
            "INSERT INTO turns (id, conversation_id, origin, status, started_at, updated_at) \
             VALUES ('t1', 'c1', 'onebot', 'done', 1, 1)",
        )
        .await
        .unwrap();
        db.execute_unprepared(
            "INSERT INTO memories (id, scope_type, scope_id, key, content, memory_type, origin, visibility, created_at, updated_at) \
             VALUES ('m1', 'onebot_global', '_', 'k', 'v', 'general', 'admin', 'normal', 1, 1), \
                    ('m2', 'onebot_user', 'user:42', 'k2', 'v2', 'general', 'private', 'normal', 1, 1)",
        )
        .await
        .unwrap();

        for sql in super::sqlite_statements() {
            db.execute_unprepared(&sql).await.unwrap();
        }

        let p1 = db
            .query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                "SELECT source_type, channel_kind FROM projects WHERE id='p1'",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p1.try_get_by_index::<String>(0).unwrap(), "channel_group");
        assert_eq!(
            p1.try_get_by_index::<Option<String>>(1).unwrap().as_deref(),
            Some("archived_channel")
        );

        let origin = db
            .query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                "SELECT origin FROM turns WHERE id='t1'",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get_by_index::<String>(0)
            .unwrap();
        assert_eq!(origin, "channel");

        let scopes = db
            .query_all_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                "SELECT scope_type FROM memories ORDER BY id",
            ))
            .await
            .unwrap();
        assert_eq!(scopes[0].try_get_by_index::<String>(0).unwrap(), "channel_global");
        assert_eq!(scopes[1].try_get_by_index::<String>(0).unwrap(), "external_subject");
    }
}

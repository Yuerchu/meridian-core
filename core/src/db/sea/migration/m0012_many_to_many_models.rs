//! Evolve `model_profiles` (logical models) and `model_configs` (provider
//! deployments) into a first-class many-to-many architecture, enrich
//! `cached_models` with canonical merge suggestions, add non-secret provider
//! `config_json` / `capability_domain`, and add dual-dimension usage & sticky
//! routing references (`logical_model_id`, `deployment_id`) on `audit_messages`,
//! `assistants`, and `conversations`.
//!
//! All changes are additive `ALTER TABLE ADD COLUMN` + indexes + lossless
//! backfills from existing `(provider_id, model_id)` rows.

pub fn sqlite_statements() -> Vec<String> {
    vec![
        // 1. First-class logical models (`model_profiles`)
        "ALTER TABLE model_profiles ADD COLUMN canonical_id TEXT".into(),
        "ALTER TABLE model_profiles ADD COLUMN capability_domain TEXT NOT NULL DEFAULT 'chat'".into(),
        "ALTER TABLE model_profiles ADD COLUMN family TEXT".into(),
        "ALTER TABLE model_profiles ADD COLUMN icon TEXT".into(),
        "ALTER TABLE model_profiles ADD COLUMN routing_policy TEXT NOT NULL DEFAULT 'sticky_manual'".into(),
        "ALTER TABLE model_profiles ADD COLUMN is_archived INTEGER NOT NULL DEFAULT 0".into(),
        "CREATE INDEX idx_model_profiles_canonical_id ON model_profiles (canonical_id)".into(),
        // 2. Many-to-many provider deployments (`model_configs`)
        "ALTER TABLE model_configs ADD COLUMN protocol_override TEXT".into(),
        "ALTER TABLE model_configs ADD COLUMN is_enabled INTEGER NOT NULL DEFAULT 1".into(),
        "ALTER TABLE model_configs ADD COLUMN priority INTEGER NOT NULL DEFAULT 0".into(),
        "ALTER TABLE model_configs ADD COLUMN capability_overrides TEXT".into(),
        "CREATE INDEX idx_model_configs_profile_priority ON model_configs (profile_id, priority)".into(),
        // 3. Multi-domain & declarative config on `providers`
        "ALTER TABLE providers ADD COLUMN capability_domain TEXT NOT NULL DEFAULT 'chat'".into(),
        "ALTER TABLE providers ADD COLUMN config_json TEXT".into(),
        // 4. Canonical merge suggestions on discovery cache (`cached_models`)
        "ALTER TABLE cached_models ADD COLUMN suggested_canonical_id TEXT".into(),
        "ALTER TABLE cached_models ADD COLUMN suggested_protocol TEXT".into(),
        // 5. Dual-dimension usage ledger (`audit_messages`)
        "ALTER TABLE audit_messages ADD COLUMN logical_model_id TEXT".into(),
        "ALTER TABLE audit_messages ADD COLUMN deployment_id TEXT".into(),
        "CREATE INDEX idx_audit_messages_logical_model ON audit_messages (logical_model_id)".into(),
        "UPDATE audit_messages
         SET logical_model_id = (
             SELECT mc.profile_id FROM model_configs mc
             WHERE mc.provider_id = audit_messages.provider_id
               AND mc.model_id = audit_messages.model_id
             LIMIT 1
         ),
         deployment_id = (
             SELECT mc.id FROM model_configs mc
             WHERE mc.provider_id = audit_messages.provider_id
               AND mc.model_id = audit_messages.model_id
             LIMIT 1
         )
         WHERE provider_id IS NOT NULL AND model_id IS NOT NULL"
            .into(),
        // 6. Upstream logical model + sticky deployment bindings (`assistants`, `conversations`)
        "ALTER TABLE assistants ADD COLUMN logical_model_id TEXT".into(),
        "ALTER TABLE assistants ADD COLUMN deployment_id TEXT".into(),
        "UPDATE assistants
         SET logical_model_id = (
             SELECT mc.profile_id FROM model_configs mc
             WHERE mc.provider_id = assistants.provider_id
               AND mc.model_id = assistants.model_id
             LIMIT 1
         ),
         deployment_id = (
             SELECT mc.id FROM model_configs mc
             WHERE mc.provider_id = assistants.provider_id
               AND mc.model_id = assistants.model_id
             LIMIT 1
         )
         WHERE provider_id IS NOT NULL AND model_id IS NOT NULL"
            .into(),
        "ALTER TABLE conversations ADD COLUMN logical_model_id TEXT".into(),
        "ALTER TABLE conversations ADD COLUMN deployment_id TEXT".into(),
        "UPDATE conversations
         SET logical_model_id = (
             SELECT mc.profile_id FROM model_configs mc
             WHERE mc.provider_id = conversations.agent_provider_id
               AND mc.model_id = conversations.agent_model_id
             LIMIT 1
         ),
         deployment_id = (
             SELECT mc.id FROM model_configs mc
             WHERE mc.provider_id = conversations.agent_provider_id
               AND mc.model_id = conversations.agent_model_id
             LIMIT 1
         )
         WHERE agent_provider_id IS NOT NULL AND agent_model_id IS NOT NULL"
            .into(),
    ]
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Database, DbBackend, Statement};

    #[tokio::test]
    async fn lossless_backfill_maps_existing_configs_audit_and_assistants() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        for sql in super::super::m0001_baseline::sqlite_statements() {
            db.execute_unprepared(&sql).await.unwrap();
        }
        db.execute_unprepared(
            "INSERT INTO providers (id, name, base_url, created_at, updated_at)
             VALUES ('p1', 'Anthropic', 'https://api.anthropic.com', 1, 1)",
        )
        .await
        .unwrap();
        db.execute_unprepared(
            "INSERT INTO model_profiles (id, name, context_window, compact_threshold, created_at, updated_at)
             VALUES ('mp1', 'Claude Sonnet 4.6', 200000, 150000, 1, 1)",
        )
        .await
        .unwrap();
        db.execute_unprepared(
            "INSERT INTO model_configs (id, provider_id, model_id, profile_id, created_at, updated_at)
             VALUES ('mc1', 'p1', 'claude-sonnet-4-6', 'mp1', 1, 1)",
        )
        .await
        .unwrap();
        db.execute_unprepared(
            "INSERT INTO assistants (id, name, system_prompt, provider_id, model_id, created_at, updated_at)
             VALUES ('a1', 'Default', 'hi', 'p1', 'claude-sonnet-4-6', 1, 1)",
        )
        .await
        .unwrap();
        db.execute_unprepared(
            "INSERT INTO audit_messages (id, recorded_at, message_id, conversation_id, role, content, provider_id, model_id, created_at)
             VALUES ('am1', 1, 'm1', 'c1', 'assistant', 'ok', 'p1', 'claude-sonnet-4-6', 1)",
        )
        .await
        .unwrap();

        for sql in super::sqlite_statements() {
            db.execute_unprepared(&sql).await.unwrap();
        }

        let assistant = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT logical_model_id, deployment_id FROM assistants WHERE id = 'a1'",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            assistant
                .try_get::<Option<String>>("", "logical_model_id")
                .unwrap()
                .as_deref(),
            Some("mp1")
        );
        assert_eq!(
            assistant
                .try_get::<Option<String>>("", "deployment_id")
                .unwrap()
                .as_deref(),
            Some("mc1")
        );

        let audit = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT logical_model_id, deployment_id FROM audit_messages WHERE id = 'am1'",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            audit
                .try_get::<Option<String>>("", "logical_model_id")
                .unwrap()
                .as_deref(),
            Some("mp1")
        );
        assert_eq!(
            audit.try_get::<Option<String>>("", "deployment_id").unwrap().as_deref(),
            Some("mc1")
        );

        let cfg = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT is_enabled, priority, protocol_override FROM model_configs WHERE id = 'mc1'",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cfg.try_get::<i32>("", "is_enabled").unwrap(), 1);
        assert_eq!(cfg.try_get::<i32>("", "priority").unwrap(), 0);
        assert_eq!(cfg.try_get::<Option<String>>("", "protocol_override").unwrap(), None);
    }
}

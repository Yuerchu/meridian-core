//! The 65 Diesel migrations, embedded for replay through sqlx.
//!
//! They stay the schema's source until the SeaORM baseline lands, and after it
//! they are what the bridge replays to bring an old database up to the last
//! Diesel version before marking the baseline as applied. `sea_test_db` replays
//! them too, so the SeaORM side tests against the schema production has.
//! `every_migration_directory_is_embedded` keeps this list equal to the
//! `migrations/` directory.

use sea_orm::{ConnectionTrait, DbErr};

pub const LEGACY: &[(&str, &str)] = &[
    (
        "00000000000001_initial",
        include_str!("../../../migrations/00000000000001_initial/up.sql"),
    ),
    (
        "00000000000002_assistant_context",
        include_str!("../../../migrations/00000000000002_assistant_context/up.sql"),
    ),
    (
        "00000000000003_projects",
        include_str!("../../../migrations/00000000000003_projects/up.sql"),
    ),
    (
        "00000000000004_assistant_tools",
        include_str!("../../../migrations/00000000000004_assistant_tools/up.sql"),
    ),
    (
        "00000000000005_assistant_thinking",
        include_str!("../../../migrations/00000000000005_assistant_thinking/up.sql"),
    ),
    (
        "00000000000006_prompt_templates",
        include_str!("../../../migrations/00000000000006_prompt_templates/up.sql"),
    ),
    (
        "00000000000007_emoji_packs",
        include_str!("../../../migrations/00000000000007_emoji_packs/up.sql"),
    ),
    (
        "00000000000008_tool_system",
        include_str!("../../../migrations/00000000000008_tool_system/up.sql"),
    ),
    (
        "00000000000009_mcp_headers",
        include_str!("../../../migrations/00000000000009_mcp_headers/up.sql"),
    ),
    (
        "00000000000010_provider_api_format",
        include_str!("../../../migrations/00000000000010_provider_api_format/up.sql"),
    ),
    (
        "00000000000011_project_memory_system",
        include_str!("../../../migrations/00000000000011_project_memory_system/up.sql"),
    ),
    (
        "00000000000012_message_format_v2",
        include_str!("../../../migrations/00000000000012_message_format_v2/up.sql"),
    ),
    (
        "00000000000013_compaction",
        include_str!("../../../migrations/00000000000013_compaction/up.sql"),
    ),
    (
        "00000000000014_cached_models",
        include_str!("../../../migrations/00000000000014_cached_models/up.sql"),
    ),
    (
        "00000000000015_model_configs",
        include_str!("../../../migrations/00000000000015_model_configs/up.sql"),
    ),
    (
        "00000000000016_reasoning_prefs",
        include_str!("../../../migrations/00000000000016_reasoning_prefs/up.sql"),
    ),
    (
        "00000000000017_skills",
        include_str!("../../../migrations/00000000000017_skills/up.sql"),
    ),
    (
        "00000000000018_todos",
        include_str!("../../../migrations/00000000000018_todos/up.sql"),
    ),
    (
        "00000000000019_layered_memory",
        include_str!("../../../migrations/00000000000019_layered_memory/up.sql"),
    ),
    (
        "00000000000020_plan_mode",
        include_str!("../../../migrations/00000000000020_plan_mode/up.sql"),
    ),
    (
        "00000000000021_message_branching",
        include_str!("../../../migrations/00000000000021_message_branching/up.sql"),
    ),
    (
        "00000000000022_voice_source",
        include_str!("../../../migrations/00000000000022_voice_source/up.sql"),
    ),
    (
        "00000000000023_accept_edits",
        include_str!("../../../migrations/00000000000023_accept_edits/up.sql"),
    ),
    (
        "00000000000024_mcp_auto_connect",
        include_str!("../../../migrations/00000000000024_mcp_auto_connect/up.sql"),
    ),
    (
        "00000000000025_durable_turns",
        include_str!("../../../migrations/00000000000025_durable_turns/up.sql"),
    ),
    (
        "00000000000026_turn_reported",
        include_str!("../../../migrations/00000000000026_turn_reported/up.sql"),
    ),
    (
        "00000000000027_sub_agents",
        include_str!("../../../migrations/00000000000027_sub_agents/up.sql"),
    ),
    (
        "00000000000028_cache_tokens",
        include_str!("../../../migrations/00000000000028_cache_tokens/up.sql"),
    ),
    (
        "00000000000029_audit_log",
        include_str!("../../../migrations/00000000000029_audit_log/up.sql"),
    ),
    (
        "00000000000030_usage_reporting",
        include_str!("../../../migrations/00000000000030_usage_reporting/up.sql"),
    ),
    (
        "00000000000031_provider_state",
        include_str!("../../../migrations/00000000000031_provider_state/up.sql"),
    ),
    (
        "00000000000032_stickers",
        include_str!("../../../migrations/00000000000032_stickers/up.sql"),
    ),
    (
        "00000000000033_auto_review",
        include_str!("../../../migrations/00000000000033_auto_review/up.sql"),
    ),
    (
        "00000000000034_prompt_queue",
        include_str!("../../../migrations/00000000000034_prompt_queue/up.sql"),
    ),
    (
        "00000000000035_price_tiers",
        include_str!("../../../migrations/00000000000035_price_tiers/up.sql"),
    ),
    (
        "00000000000036_server_tools",
        include_str!("../../../migrations/00000000000036_server_tools/up.sql"),
    ),
    (
        "00000000000037_server_tool_cost",
        include_str!("../../../migrations/00000000000037_server_tool_cost/up.sql"),
    ),
    (
        "00000000000038_acp_sessions",
        include_str!("../../../migrations/00000000000038_acp_sessions/up.sql"),
    ),
    (
        "00000000000039_voice_corpus",
        include_str!("../../../migrations/00000000000039_voice_corpus/up.sql"),
    ),
    (
        "00000000000040_provider_catalog_id",
        include_str!("../../../migrations/00000000000040_provider_catalog_id/up.sql"),
    ),
    (
        "00000000000041_credential_and_billing",
        include_str!("../../../migrations/00000000000041_credential_and_billing/up.sql"),
    ),
    (
        "00000000000042_file_journal",
        include_str!("../../../migrations/00000000000042_file_journal/up.sql"),
    ),
    (
        "00000000000043_acp_external_billing",
        include_str!("../../../migrations/00000000000043_acp_external_billing/up.sql"),
    ),
    (
        "00000000000044_audit_conversation_turn_index",
        include_str!("../../../migrations/00000000000044_audit_conversation_turn_index/up.sql"),
    ),
    (
        "00000000000045_message_context_items",
        include_str!("../../../migrations/00000000000045_message_context_items/up.sql"),
    ),
    (
        "00000000000046_acp_context_deliveries",
        include_str!("../../../migrations/00000000000046_acp_context_deliveries/up.sql"),
    ),
    (
        "00000000000047_queued_prompt_context_items",
        include_str!("../../../migrations/00000000000047_queued_prompt_context_items/up.sql"),
    ),
    (
        "00000000000048_remove_dead_schema",
        include_str!("../../../migrations/00000000000048_remove_dead_schema/up.sql"),
    ),
    (
        "00000000000049_exact_decimal_money",
        include_str!("../../../migrations/00000000000049_exact_decimal_money/up.sql"),
    ),
    (
        "00000000000050_canonical_auto_review",
        include_str!("../../../migrations/00000000000050_canonical_auto_review/up.sql"),
    ),
    (
        "00000000000051_plan_review_documents",
        include_str!("../../../migrations/00000000000051_plan_review_documents/up.sql"),
    ),
    (
        "00000000000052_canonical_sandbox_mode",
        include_str!("../../../migrations/00000000000052_canonical_sandbox_mode/up.sql"),
    ),
    (
        "00000000000053_conversation_context_items",
        include_str!("../../../migrations/00000000000053_conversation_context_items/up.sql"),
    ),
    (
        "00000000000054_redaction_rules",
        include_str!("../../../migrations/00000000000054_redaction_rules/up.sql"),
    ),
    (
        "00000000000055_acp_session_notices",
        include_str!("../../../migrations/00000000000055_acp_session_notices/up.sql"),
    ),
    (
        "00000000000056_message_tool_diffs",
        include_str!("../../../migrations/00000000000056_message_tool_diffs/up.sql"),
    ),
    (
        "00000000000057_notification_alerts",
        include_str!("../../../migrations/00000000000057_notification_alerts/up.sql"),
    ),
    (
        "00000000000058_webhook_custom_body",
        include_str!("../../../migrations/00000000000058_webhook_custom_body/up.sql"),
    ),
    (
        "00000000000059_drop_prompt_templates",
        include_str!("../../../migrations/00000000000059_drop_prompt_templates/up.sql"),
    ),
    (
        "00000000000060_response_model_id",
        include_str!("../../../migrations/00000000000060_response_model_id/up.sql"),
    ),
    (
        "00000000000061_model_profiles",
        include_str!("../../../migrations/00000000000061_model_profiles/up.sql"),
    ),
    (
        "00000000000062_provider_icon",
        include_str!("../../../migrations/00000000000062_provider_icon/up.sql"),
    ),
    (
        "00000000000063_codex_request_shape",
        include_str!("../../../migrations/00000000000063_codex_request_shape/up.sql"),
    ),
    (
        "00000000000064_composer_drafts",
        include_str!("../../../migrations/00000000000064_composer_drafts/up.sql"),
    ),
    (
        "00000000000065_turn_trigger",
        include_str!("../../../migrations/00000000000065_turn_trigger/up.sql"),
    ),
];

/// Runs every embedded migration in order on `conn`.
///
/// Each file holds several statements, triggers among them; `execute_unprepared`
/// runs them all, and `sea_test_db_has_the_schema_diesel_builds` is what would
/// notice if a file stopped short. The caller decides the foreign-key
/// pragma: table rebuilds must not fire `ON DELETE` actions, so production and
/// `sea_test_db` both replay with foreign keys off.
pub async fn replay_all(conn: &impl ConnectionTrait) -> Result<(), DbErr> {
    replay(conn, 0..LEGACY.len()).await
}

/// Runs the embedded migrations at `range` (indexes into [`LEGACY`]), in order,
/// without recording anything. For the tests of a migration's data mapping,
/// which stop just before it, seed rows, and then run it.
pub async fn replay(conn: &impl ConnectionTrait, range: std::ops::Range<usize>) -> Result<(), DbErr> {
    for (_, sql) in &LEGACY[range] {
        conn.execute_unprepared(sql).await?;
    }
    Ok(())
}

/// Runs the migrations at `range` the way Diesel ran them: each in a
/// transaction of its own, so one that fails part-way leaves the rows it had
/// started on untouched. The tests of a migration that must refuse bad data
/// depend on that rollback.
#[cfg(any(test, feature = "test-support"))]
pub async fn replay_in_transactions(
    conn: &sea_orm::DatabaseConnection,
    range: std::ops::Range<usize>,
) -> Result<(), DbErr> {
    use sea_orm::TransactionTrait;

    for (_, sql) in &LEGACY[range] {
        let tx = conn.begin().await?;
        match tx.execute_unprepared(sql).await {
            Ok(_) => tx.commit().await?,
            Err(error) => {
                tx.rollback().await?;
                return Err(error);
            }
        }
    }
    Ok(())
}

/// The position in [`LEGACY`] of the migration with this 14-digit version.
#[cfg(any(test, feature = "test-support"))]
pub fn index_of(version: &str) -> usize {
    LEGACY
        .iter()
        .position(|(name, _)| name.starts_with(version) && name[version.len()..].starts_with('_'))
        .unwrap_or_else(|| panic!("no legacy migration has version {version}"))
}

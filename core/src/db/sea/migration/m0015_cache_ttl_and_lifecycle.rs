//! Per-model & per-assistant prompt cache TTL, 1-hour cache-write pricing, and
//! idle cache expiry lifecycle configuration.
//!
//! - `model_profiles.cache_ttl`: optional model-level default TTL (`5m`, `1h`,
//!   `in_memory`, `24h`). `NULL` follows the catalog default.
//! - `model_profiles.cache_write_1h_price` & `model_configs.cache_write_1h_price`:
//!   exact Decimal rate per 1M tokens for 1-hour extended cache writes (e.g.
//!   Anthropic's 2x write tier), following `overrides_pricing` as a whole set.
//! - `assistants.cache_ttl`: optional override (`NULL` follows the model).
//! - `assistants.cache_expiry_policy`: `off` | `warm` | `compact` |
//!   `warm_then_compact` (defaults to `compact`, compressing via the live warm
//!   prompt cache right before it expires).
//! - `assistants.cache_max_warm_count`: max consecutive idle warm refreshes
//!   before stopping or handing over to compaction.
//! - `assistants.cache_compact_min_tokens`: minimum prompt token threshold for
//!   expiry-triggered compaction (`0` follows the model's `compact_threshold`).
//! - `messages.cache_write_1h_tokens`, `audit_messages.cache_write_1h_tokens`,
//!   `audit_messages.cache_write_1h_price`: subset of `cache_write_tokens`
//!   billed at the 1-hour write rate, snapshotted at write time.

/// Pure DDL statements for SQLite. Existing `pricing_tiers` JSON documents are
/// normalised in `M0015CacheTtlAndLifecycle::up`.
pub fn sqlite_statements() -> Vec<String> {
    vec![
        "ALTER TABLE model_profiles ADD COLUMN cache_ttl TEXT".into(),
        "ALTER TABLE model_profiles ADD COLUMN cache_write_1h_price TEXT".into(),
        "ALTER TABLE model_configs ADD COLUMN cache_write_1h_price TEXT".into(),
        "ALTER TABLE assistants ADD COLUMN cache_ttl TEXT".into(),
        "ALTER TABLE assistants ADD COLUMN cache_expiry_policy TEXT NOT NULL DEFAULT 'compact'".into(),
        "ALTER TABLE assistants ADD COLUMN cache_max_warm_count INTEGER NOT NULL DEFAULT 3".into(),
        "ALTER TABLE assistants ADD COLUMN cache_compact_min_tokens INTEGER NOT NULL DEFAULT 40000".into(),
        "ALTER TABLE messages ADD COLUMN cache_write_1h_tokens INTEGER".into(),
        "ALTER TABLE audit_messages ADD COLUMN cache_write_1h_tokens INTEGER".into(),
        "ALTER TABLE audit_messages ADD COLUMN cache_write_1h_price TEXT".into(),
    ]
}

/// Ensure every stored `PriceTier` object carries the explicit nullable
/// `cache_write_1h_price` key required by the strict contract.
pub(super) fn backfill_tier_1h_price(raw: &str) -> Result<Option<String>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let mut tiers: Vec<serde_json::Map<String, serde_json::Value>> =
        serde_json::from_str(trimmed).map_err(|e| format!("invalid pricing_tiers JSON: {e}"))?;
    let mut changed = false;
    for tier in &mut tiers {
        if !tier.contains_key("cache_write_1h_price") {
            tier.insert("cache_write_1h_price".into(), serde_json::Value::Null);
            changed = true;
        }
    }
    if !changed {
        return Ok(None);
    }
    serde_json::to_string(&tiers)
        .map(Some)
        .map_err(|e| format!("could not encode pricing_tiers JSON: {e}"))
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Database};

    #[tokio::test]
    async fn existing_assistants_get_default_compact_expiry_policy() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        for sql in super::super::sqlite_statements() {
            db.execute_unprepared(&sql).await.unwrap();
        }
        db.execute_unprepared(
            "INSERT INTO assistants (id,name,system_prompt,is_default,sort_order,created_at,updated_at,\
             context_limit,compact_keep_recent_tokens,thinking_enabled,auto_compact_enabled) \
             VALUES ('a','A','',0,0,1,1,128000,20000,0,0)",
        )
        .await
        .unwrap();

        let row = db
            .query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                "SELECT cache_ttl, cache_expiry_policy, cache_max_warm_count, cache_compact_min_tokens \
                 FROM assistants WHERE id='a'",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get_by_index::<Option<String>>(0).unwrap(), None);
        assert_eq!(row.try_get_by_index::<String>(1).unwrap(), "compact");
        assert_eq!(row.try_get_by_index::<i64>(2).unwrap(), 3);
        assert_eq!(row.try_get_by_index::<i64>(3).unwrap(), 40000);
    }

    #[test]
    fn backfill_adds_null_1h_price_to_existing_tiers() {
        let legacy = r#"[{"min_prompt_tokens":200000,"input_price":"4","output_price":"12","cache_read_price":"1","cache_write_price":null}]"#;
        let updated = super::backfill_tier_1h_price(legacy).unwrap().expect("should rewrite");
        let parsed: serde_json::Value = serde_json::from_str(&updated).unwrap();
        assert!(parsed[0].get("cache_write_1h_price").is_some());
        assert!(parsed[0]["cache_write_1h_price"].is_null());
        assert_eq!(super::backfill_tier_1h_price(&updated).unwrap(), None);
    }
}

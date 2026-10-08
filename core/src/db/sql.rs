// backend: sqlite-only
//! The registered raw SQL: every native statement the SeaORM side runs, in
//! one place, so `docs/backend-neutrality.md` can list what a second backend
//! has to answer for. Nothing outside this module can build a [`ReadOnly`],
//! and the model-contract checker refuses `Statement::from_*` anywhere else
//! outside the bridge and baseline files.
//!
//! What is SQLite's here: the `?` positional placeholders and the backend the
//! statement is prepared for. `LIKE … ESCAPE '\'` and `NOT EXISTS` are
//! standard SQL. SQLite's own `?1` numbering is deliberately not used — the
//! values are passed in the order the placeholders appear.
//!
//! No write-side helper yet: nothing needs one.

use sea_orm::{ConnectionTrait, DbBackend, DbErr, QueryResult, Statement, Value};

use crate::db::sea::cap::Read;

/// A statement that only reads. The constructor is private to this module:
/// a value of this type is a statement reviewed and registered here.
pub struct ReadOnly(&'static str);

/// Every chain under a path prefix with its head sha — `NULL` for a dead head
/// — newest-updated first, capped. Values: the escaped `LIKE` pattern, then
/// the limit.
///
/// One statement rather than a per-candidate head query: this runs before and
/// after every shell command, and a project with a long journal history would
/// otherwise turn each command into thousands of queries.
pub const JOURNAL_CHAINS_UNDER_PREFIX: ReadOnly = ReadOnly(
    "SELECT f.id, f.norm_path, f.display_path, f.created_at, f.updated_at, v.new_sha AS head_sha
     FROM journal_files f
     JOIN journal_versions v ON v.file_id = f.id
      AND v.seq = (SELECT MAX(seq) FROM journal_versions v2 WHERE v2.file_id = f.id)
     WHERE f.norm_path LIKE ? ESCAPE '\\'
     ORDER BY f.updated_at DESC
     LIMIT ?",
);

/// The live subset of [`JOURNAL_CHAINS_UNDER_PREFIX`], same values. The
/// liveness test is in SQL so the cap counts live files — applied afterwards,
/// a window of freshly deleted chains would evict the tracked files a
/// tombstone scan exists to find.
pub const JOURNAL_LIVE_CHAINS_UNDER_PREFIX: ReadOnly = ReadOnly(
    "SELECT f.id, f.norm_path, f.display_path, f.created_at, f.updated_at, v.new_sha AS head_sha
     FROM journal_files f
     JOIN journal_versions v ON v.file_id = f.id
      AND v.seq = (SELECT MAX(seq) FROM journal_versions v2 WHERE v2.file_id = f.id)
     WHERE f.norm_path LIKE ? ESCAPE '\\' AND v.new_sha IS NOT NULL
     ORDER BY f.updated_at DESC
     LIMIT ?",
);

/// Blob shas no version references any more, as `sha`. No values.
pub const JOURNAL_UNREFERENCED_BLOBS: ReadOnly = ReadOnly(
    "SELECT b.sha256 AS sha FROM journal_blobs b
     WHERE NOT EXISTS (
         SELECT 1 FROM journal_versions v
         WHERE v.observed_old_sha = b.sha256 OR v.new_sha = b.sha256
     )",
);

/// The usage grouping over `audit_messages`, one statement per key: rows of
/// the billed roles, grouped by the key and every price and billing column,
/// with the conditional sums `db::sea::ops::usage` prices. `$key` is the
/// bucket expression; `$conversation` is the conversation filter, `$extra` any
/// further condition. Values: since, since, until, until, origin, origin, then
/// the conversation (twice for the nullable filter, once for the direct one).
///
/// The roles are written out because a registered statement is a literal;
/// `the_usage_statements_bill_exactly_the_billed_roles` holds them to
/// `audit::BILLED_ROLES`.
macro_rules! usage_grouping {
    ($key:literal, $conversation:literal, $extra:literal) => {
        ReadOnly(concat!(
            "SELECT ",
            $key,
            " AS bucket_key,
                provider_id, model_id,
                input_price, output_price, cache_read_price, cache_write_price,
                server_tool_price, billing_mode,
                COUNT(*) AS messages,
                COALESCE(SUM(CASE WHEN input_tokens IS NOT NULL
                                  THEN input_tokens
                                  ELSE COALESCE(cache_read_tokens, 0)
                                     + COALESCE(cache_write_tokens, 0)
                             END), 0) AS input_tokens,
                COALESCE(SUM(MAX(COALESCE(input_tokens, 0)
                                 - COALESCE(cache_read_tokens, 0)
                                 - COALESCE(cache_write_tokens, 0), 0)), 0)
                    AS uncached_input_tokens,
                COALESCE(SUM(output_tokens), 0) AS output_tokens,
                COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens,
                COALESCE(SUM(cache_write_tokens), 0) AS cache_write_tokens,
                COALESCE(SUM(server_tool_calls), 0) AS server_tool_calls,
                SUM(CASE WHEN COALESCE(server_tool_calls, 0) > 0 THEN 1 ELSE 0 END)
                    AS server_tool_messages,
                SUM(CASE WHEN input_tokens IS NULL
                               AND output_tokens IS NULL
                               AND cache_read_tokens IS NULL
                               AND cache_write_tokens IS NULL
                         THEN 1 ELSE 0 END) AS missing_token_usage_messages,
                SUM(CASE WHEN input_tokens IS NULL OR output_tokens IS NULL
                         THEN 1 ELSE 0 END) AS incomplete_token_usage_messages,
                SUM(CASE WHEN input_tokens IS NULL THEN 1 ELSE 0 END)
                    AS missing_input_messages,
                SUM(CASE WHEN output_tokens IS NULL THEN 1 ELSE 0 END)
                    AS missing_output_messages,
                SUM(CASE WHEN COALESCE(input_tokens, 0) > 0
                               OR COALESCE(output_tokens, 0) > 0
                               OR COALESCE(cache_read_tokens, 0) > 0
                               OR COALESCE(cache_write_tokens, 0) > 0
                         THEN 1 ELSE 0 END) AS positive_token_messages,
                SUM(CASE WHEN (input_tokens IS NULL OR output_tokens IS NULL)
                               OR COALESCE(server_tool_calls, 0) > 0
                         THEN 1 ELSE 0 END) AS incomplete_token_or_tool_messages,
                SUM(CASE WHEN input_tokens IS NULL
                               OR output_tokens IS NULL
                               OR COALESCE(input_tokens, 0) > 0
                               OR COALESCE(output_tokens, 0) > 0
                               OR COALESCE(cache_read_tokens, 0) > 0
                               OR COALESCE(cache_write_tokens, 0) > 0
                         THEN 1 ELSE 0 END) AS incomplete_or_positive_token_messages,
                SUM(CASE WHEN input_tokens IS NULL
                               OR MAX(COALESCE(input_tokens, 0)
                                      - COALESCE(cache_read_tokens, 0)
                                      - COALESCE(cache_write_tokens, 0), 0) > 0
                         THEN 1 ELSE 0 END) AS unpriced_input_usage_messages,
                SUM(CASE WHEN output_tokens IS NULL
                               OR COALESCE(output_tokens, 0) > 0
                         THEN 1 ELSE 0 END) AS unpriced_output_usage_messages,
                SUM(CASE WHEN (input_tokens IS NULL
                                AND output_tokens IS NULL
                                AND cache_read_tokens IS NULL
                                AND cache_write_tokens IS NULL)
                               OR COALESCE(cache_read_tokens, 0) > 0
                               OR COALESCE(cache_write_tokens, 0) > 0
                         THEN 1 ELSE 0 END) AS unpriced_cache_usage_messages,
                SUM(CASE WHEN input_tokens IS NULL
                               OR output_tokens IS NULL
                               OR COALESCE(input_tokens, 0) > 0
                               OR COALESCE(output_tokens, 0) > 0
                               OR COALESCE(cache_read_tokens, 0) > 0
                               OR COALESCE(cache_write_tokens, 0) > 0
                               OR COALESCE(server_tool_calls, 0) > 0
                         THEN 1 ELSE 0 END) AS unpriced_usage_messages,
                SUM(CASE WHEN COALESCE(input_tokens, 0) > 0
                               OR COALESCE(output_tokens, 0) > 0
                               OR COALESCE(cache_read_tokens, 0) > 0
                               OR COALESCE(cache_write_tokens, 0) > 0
                               OR COALESCE(server_tool_calls, 0) > 0
                         THEN 1 ELSE 0 END) AS positive_token_or_tool_messages,
                SUM(CASE WHEN input_tokens = 0
                               AND output_tokens = 0
                               AND COALESCE(cache_read_tokens, 0) = 0
                               AND COALESCE(cache_write_tokens, 0) = 0
                               AND COALESCE(server_tool_calls, 0) = 0
                         THEN 1 ELSE 0 END) AS explicit_zero_messages
           FROM audit_messages
          WHERE role IN ('assistant', 'auto_review', 'compaction', 'title', 'extraction')
            AND (? IS NULL OR created_at >= ?)
            AND (? IS NULL OR created_at < ?)
            AND (? IS NULL OR turn_origin = ?)
            ",
            $conversation,
            "
            ",
            $extra,
            "
       GROUP BY bucket_key, provider_id, model_id,
                input_price, output_price, cache_read_price, cache_write_price,
                server_tool_price, billing_mode"
        ))
    };
}

/// The nullable conversation scope every report dimension takes.
macro_rules! usage_by {
    ($key:literal) => {
        usage_grouping!($key, "AND (? IS NULL OR conversation_id = ?)", "")
    };
}

const USAGE_BY_TOTAL: ReadOnly = usage_by!("''");
const USAGE_BY_PROVIDER: ReadOnly = usage_by!("COALESCE(provider_name, provider_id, '')");
const USAGE_BY_MODEL: ReadOnly = usage_by!("COALESCE(model_id, '')");
const USAGE_BY_BOT: ReadOnly = usage_by!("COALESCE(CAST(self_id AS TEXT), '')");
const USAGE_BY_SOURCE: ReadOnly = usage_by!("COALESCE(source_type || ':' || source_id, '')");
const USAGE_BY_CONVERSATION: ReadOnly = usage_by!("conversation_id");
const USAGE_BY_DAY: ReadOnly = usage_by!("strftime('%Y-%m-%d', created_at / 1000, 'unixepoch', 'localtime')");
const USAGE_BY_HOUR: ReadOnly = usage_by!("strftime('%Y-%m-%dT%H', created_at / 1000, 'unixepoch', 'localtime')");
const USAGE_BY_KIND: ReadOnly = usage_by!("role");

/// Usage per turn of one conversation. The conversation is an equality, not
/// the nullable scope, so SQLite can seek the `(conversation_id, turn_id)`
/// index rather than scan the ledger every time a conversation opens; rows
/// with no turn id cannot be attached exactly and are left out.
pub const USAGE_BY_TURN: ReadOnly = usage_grouping!("turn_id", "AND conversation_id = ?", "AND turn_id IS NOT NULL");

/// The registered grouping for a report dimension. Its key is the
/// dimension's `key_expr`, which the tests hold the two to.
pub fn usage_grouping(dimension: crate::db::sea::ops::usage::UsageDimension) -> ReadOnly {
    use crate::db::sea::ops::usage::UsageDimension;
    match dimension {
        UsageDimension::Total => USAGE_BY_TOTAL,
        UsageDimension::Provider => USAGE_BY_PROVIDER,
        UsageDimension::Model => USAGE_BY_MODEL,
        UsageDimension::Bot => USAGE_BY_BOT,
        UsageDimension::Source => USAGE_BY_SOURCE,
        UsageDimension::Conversation => USAGE_BY_CONVERSATION,
        UsageDimension::Day => USAGE_BY_DAY,
        UsageDimension::Hour => USAGE_BY_HOUR,
        UsageDimension::Kind => USAGE_BY_KIND,
    }
}

/// Runs a registered read-only statement with its values, in placeholder order.
pub async fn query_all(db: &impl Read, sql: ReadOnly, values: Vec<Value>) -> Result<Vec<QueryResult>, DbErr> {
    db.conn()?
        .query_all_raw(Statement::from_sql_and_values(DbBackend::Sqlite, sql.0, values))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::ops::usage::UsageDimension;

    const DIMENSIONS: [UsageDimension; 9] = [
        UsageDimension::Total,
        UsageDimension::Provider,
        UsageDimension::Model,
        UsageDimension::Bot,
        UsageDimension::Source,
        UsageDimension::Conversation,
        UsageDimension::Day,
        UsageDimension::Hour,
        UsageDimension::Kind,
    ];

    /// Each dimension's statement groups by that dimension's key, and no two
    /// dimensions share a statement.
    #[test]
    fn each_dimension_is_grouped_by_its_own_key() {
        let mut seen = std::collections::HashSet::new();
        for dimension in DIMENSIONS {
            let statement = usage_grouping(dimension).0;
            let select = format!("SELECT {} AS bucket_key,", dimension.key_expr());
            assert!(statement.starts_with(&select), "{dimension:?}: {statement}");
            assert!(seen.insert(statement), "{dimension:?} shares a statement");
        }
        assert!(USAGE_BY_TURN.0.starts_with("SELECT turn_id AS bucket_key,"));
    }

    /// Conversation snapshots are opened far more often than global reports.
    /// The durable ledger is append-only, so a scan here grows forever: the
    /// registered per-turn statement itself has to seek the composite index.
    #[tokio::test]
    async fn the_turn_usage_statement_seeks_the_conversation_turn_index() {
        use crate::db::sea::cap::sealed::Access;

        let db = crate::db::sea::sea_test_db().await;
        let mut values: Vec<Value> = vec![Option::<i64>::None.into(); 4];
        values.extend([Option::<String>::None.into(), Option::<String>::None.into()]);
        values.push("c1".into());
        let plan = db
            .conn()
            .unwrap()
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                format!("EXPLAIN QUERY PLAN {}", USAGE_BY_TURN.0),
                values,
            ))
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get::<String>("", "detail").unwrap())
            .collect::<Vec<_>>();
        assert!(
            plan.iter().any(|detail| detail.contains("idx_audit_conversation_turn")),
            "the snapshot query stopped using the ledger index: {plan:?}"
        );
    }

    /// The literal role list is the billed one: a role added to
    /// `BILLED_ROLES` and not here would be spend the report never shows.
    #[test]
    fn the_usage_statements_bill_exactly_the_billed_roles() {
        let roles = crate::db::ops::audit::BILLED_ROLES
            .iter()
            .map(|role| format!("'{role}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let clause = format!("WHERE role IN ({roles})");
        for dimension in DIMENSIONS {
            assert!(usage_grouping(dimension).0.contains(&clause), "{dimension:?}");
        }
        assert!(USAGE_BY_TURN.0.contains(&clause));
    }
}

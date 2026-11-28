//! The registered raw SQL: every native statement the SeaORM side runs, in
//! one place, so `docs/backend-neutrality.md` can list what a second backend
//! has to answer for. Nothing outside this module can build a [`ReadOnly`],
//! and the model-contract checker refuses `Statement::from_*` anywhere else
//! outside the bridge and baseline files.
//!
//! Statements are written once in portable SQL and carry a second text only
//! where a dialect leaves no choice — today that is the calendar expression
//! the day and hour reports group by. Three rules keep the single text honest:
//!
//! - **Placeholders are `?`, numbered for the backend at run time.**
//!   PostgreSQL wants `$1`, `$2`, …; [`numbered`] rewrites them in placeholder
//!   order, skipping quoted literals. Values are passed in the order the
//!   placeholders appear, so SQLite's own `?1` numbering stays unused.
//! - **No two-argument `MAX`.** It is a scalar clamp on SQLite and a syntax
//!   error on PostgreSQL, which spells it `GREATEST`. `CASE WHEN … THEN …
//!   ELSE 0 END` is both, and a comparison against zero needs no clamp at all.
//! - **An integer column a reader decodes as `i64` is `CAST(… AS BIGINT)`
//!   here.** sqlx on PostgreSQL refuses to read `int4` as `i64`, where SQLite
//!   never cared; the width a reader gets is the statement's to declare.
//!   Aggregates are exempt: `SUM(int4)` is already `bigint`.
//!
//! What remains registered as SQLite-only is listed in
//! `docs/backend-neutrality.md`.
//!
//! No other raw statements live in ops or the capability wrappers.

use sea_orm::{ConnectionTrait, DbBackend, DbErr, QueryResult, Statement, Value};

use crate::db::sea::cap::Read;

/// A statement that only reads. The constructor is private to this module:
/// a value of this type is a statement reviewed and registered here.
pub struct ReadOnly {
    sqlite: &'static str,
    postgres: &'static str,
}

impl ReadOnly {
    /// One portable text, run on either backend.
    const fn same(text: &'static str) -> Self {
        Self {
            sqlite: text,
            postgres: text,
        }
    }

    /// Two texts, for a statement no dialect-free spelling covers. Every use
    /// is registered in `docs/backend-neutrality.md`.
    const fn dual(sqlite: &'static str, postgres: &'static str) -> Self {
        Self { sqlite, postgres }
    }

    fn text(&self, backend: DbBackend) -> &'static str {
        match backend {
            DbBackend::Postgres => self.postgres,
            _ => self.sqlite,
        }
    }
}

/// `?` placeholders rewritten as `$1`, `$2`, … in the order they appear.
///
/// A `?` inside a quoted literal is text, not a placeholder: `ESCAPE '\'` and
/// a `'%Y-%m-%d'` format are both in these statements, and a scanner that
/// counted them would shift every number after them. Doubled quotes inside a
/// literal (`''`) stay inside it, as SQL says.
fn numbered(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len() + 16);
    let mut next = 1;
    let mut in_literal = false;
    for ch in sql.chars() {
        match ch {
            '\'' => {
                in_literal = !in_literal;
                out.push(ch);
            }
            '?' if !in_literal => {
                out.push('$');
                out.push_str(&next.to_string());
                next += 1;
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Every chain under a path prefix with its head sha — `NULL` for a dead head
/// — newest-updated first, capped. Values: the escaped `LIKE` pattern, then
/// the limit.
///
/// One statement rather than a per-candidate head query: this runs before and
/// after every shell command, and a project with a long journal history would
/// otherwise turn each command into thousands of queries.
pub const JOURNAL_CHAINS_UNDER_PREFIX: ReadOnly = ReadOnly::same(
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
pub const JOURNAL_LIVE_CHAINS_UNDER_PREFIX: ReadOnly = ReadOnly::same(
    "SELECT f.id, f.norm_path, f.display_path, f.created_at, f.updated_at, v.new_sha AS head_sha
     FROM journal_files f
     JOIN journal_versions v ON v.file_id = f.id
      AND v.seq = (SELECT MAX(seq) FROM journal_versions v2 WHERE v2.file_id = f.id)
     WHERE f.norm_path LIKE ? ESCAPE '\\' AND v.new_sha IS NOT NULL
     ORDER BY f.updated_at DESC
     LIMIT ?",
);

/// Blob shas no version references any more, as `sha`. No values.
pub const JOURNAL_UNREFERENCED_BLOBS: ReadOnly = ReadOnly::same(
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
macro_rules! usage_grouping_text {
    ($key:literal, $conversation:literal, $extra:literal) => {
        concat!(
            "SELECT ",
            $key,
            " AS bucket_key,
                provider_id, model_id,
                input_price, output_price, cache_read_price, cache_write_price, cache_write_1h_price,
                server_tool_price, billing_mode,
                COUNT(*) AS messages,
                COALESCE(SUM(CASE WHEN input_tokens IS NOT NULL
                                  THEN input_tokens
                                  ELSE COALESCE(cache_read_tokens, 0)
                                     + COALESCE(cache_write_tokens, 0)
                             END), 0) AS input_tokens,
                COALESCE(SUM(CASE WHEN COALESCE(input_tokens, 0)
                                       - COALESCE(cache_read_tokens, 0)
                                       - COALESCE(cache_write_tokens, 0) > 0
                                  THEN COALESCE(input_tokens, 0)
                                       - COALESCE(cache_read_tokens, 0)
                                       - COALESCE(cache_write_tokens, 0)
                                  ELSE 0
                             END), 0)
                    AS uncached_input_tokens,
                COALESCE(SUM(output_tokens), 0) AS output_tokens,
                COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens,
                COALESCE(SUM(cache_write_tokens), 0) AS cache_write_tokens,
                COALESCE(SUM(cache_write_1h_tokens), 0) AS cache_write_1h_tokens,
                SUM(CASE WHEN COALESCE(cache_write_1h_tokens, 0) > 0 THEN 1 ELSE 0 END)
                    AS cache_write_1h_messages,
                COALESCE(SUM(input_tokens), 0) AS reported_input_tokens,
                COALESCE(SUM(CASE WHEN input_tokens IS NOT NULL
                                  THEN COALESCE(cache_read_tokens, 0)
                                  ELSE 0
                             END), 0) AS reported_input_cache_read_tokens,
                COALESCE(SUM(CASE WHEN input_tokens IS NOT NULL
                                       AND cache_read_tokens IS NOT NULL
                                  THEN input_tokens
                                  ELSE 0
                             END), 0) AS cache_reported_input_tokens,
                COALESCE(SUM(CASE WHEN input_tokens IS NOT NULL
                                       AND cache_read_tokens IS NOT NULL
                                  THEN cache_read_tokens
                                  ELSE 0
                             END), 0) AS cache_reported_input_cache_read_tokens,
                COALESCE(SUM(CASE WHEN input_tokens IS NOT NULL
                                       AND cache_read_tokens IS NOT NULL
                                  THEN 1
                                  ELSE 0
                             END), 0) AS cache_reported_input_messages,
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
                               OR COALESCE(input_tokens, 0)
                                  - COALESCE(cache_read_tokens, 0)
                                  - COALESCE(cache_write_tokens, 0) > 0
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
          WHERE role IN ('assistant', 'auto_review', 'compaction', 'title', 'extraction', 'cache_warm', 'dream')
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
                input_price, output_price, cache_read_price, cache_write_price, cache_write_1h_price,
                server_tool_price, billing_mode"
        )
    };
}

/// One portable grouping, under the nullable conversation scope.
macro_rules! usage_by {
    ($key:literal) => {
        ReadOnly::same(usage_grouping_text!($key, "AND (? IS NULL OR conversation_id = ?)", ""))
    };
}

/// A grouping whose bucket expression has no dialect-free spelling: the
/// calendar reports. Both texts are otherwise the same statement.
macro_rules! usage_by_calendar {
    ($sqlite_key:literal, $postgres_key:literal) => {
        ReadOnly::dual(
            usage_grouping_text!($sqlite_key, "AND (? IS NULL OR conversation_id = ?)", ""),
            usage_grouping_text!($postgres_key, "AND (? IS NULL OR conversation_id = ?)", ""),
        )
    };
}

const USAGE_BY_TOTAL: ReadOnly = usage_by!("''");
const USAGE_BY_PROVIDER: ReadOnly = usage_by!("COALESCE(provider_name, provider_id, '')");
const USAGE_BY_MODEL: ReadOnly = usage_by!("COALESCE(model_id, '')");
const USAGE_BY_LOGICAL_MODEL: ReadOnly = usage_by!("COALESCE(logical_model_id, model_id, '')");
const USAGE_BY_BOT: ReadOnly = usage_by!("COALESCE(CAST(self_id AS TEXT), '')");
const USAGE_BY_SOURCE: ReadOnly = usage_by!("COALESCE(source_type || ':' || source_id, '')");
const USAGE_BY_CONVERSATION: ReadOnly = usage_by!("conversation_id");
const USAGE_BY_DAY: ReadOnly = usage_by_calendar!(
    "strftime('%Y-%m-%d', created_at / 1000, 'unixepoch', 'localtime')",
    "to_char(to_timestamp(created_at / 1000), 'YYYY-MM-DD')"
);
const USAGE_BY_HOUR: ReadOnly = usage_by_calendar!(
    "strftime('%Y-%m-%dT%H', created_at / 1000, 'unixepoch', 'localtime')",
    "to_char(to_timestamp(created_at / 1000), 'YYYY-MM-DD\"T\"HH24')"
);
const USAGE_BY_KIND: ReadOnly = usage_by!("role");

/// Usage per turn of one conversation. The conversation is an equality, not
/// the nullable scope, so SQLite can seek the `(conversation_id, turn_id)`
/// index rather than scan the ledger every time a conversation opens; rows
/// with no turn id cannot be attached exactly and are left out.
pub const USAGE_BY_TURN: ReadOnly = ReadOnly::same(usage_grouping_text!(
    "turn_id",
    "AND conversation_id = ?",
    "AND turn_id IS NOT NULL"
));

/// Every reply and every compaction in the window, per conversation, in order.
///
/// Values match [`filter_values`]: since, since, until, until, origin, origin,
/// then the nullable conversation twice. A compaction row is included only as
/// a reset marker: the summary it writes replaces the prefix, so the next
/// request is new content rather than missed cache.
pub const USAGE_CACHE_MISS_ROWS: ReadOnly = ReadOnly::same(
    "SELECT conversation_id, role, created_at,
            CAST(input_tokens AS BIGINT) AS input_tokens,
            CAST(cache_read_tokens AS BIGINT) AS cache_read_tokens,
            CAST(cache_write_tokens AS BIGINT) AS cache_write_tokens,
            input_price, cache_write_price, cache_read_price
       FROM audit_messages
      WHERE role IN ('assistant', 'compaction')
        AND (? IS NULL OR created_at >= ?)
        AND (? IS NULL OR created_at < ?)
        AND (? IS NULL OR turn_origin = ?)
        AND (? IS NULL OR conversation_id = ?)
      ORDER BY conversation_id, created_at, id",
);

/// The rows that can carry a usage or pricing gap: a side of the usage missing,
/// or a rate not snapshotted. A superset on purpose — whether a row really has
/// a gap is the accumulator's answer, through the same `add` every report goes
/// through, so this only spares it the rows that cannot have one. A row with
/// both sides reported and every rate stored is priced exactly.
///
/// Written once and handed to each statement as its last argument, so the two
/// below cannot come to disagree about which rows they consider.
macro_rules! with_gap_candidate {
    ($statement:ident ! ( $($args:tt)* )) => {
        $statement!($($args)* "AND (input_tokens IS NULL OR output_tokens IS NULL
              OR input_price IS NULL OR output_price IS NULL
              OR cache_read_price IS NULL OR cache_write_price IS NULL
              OR server_tool_price IS NULL)")
    };
}

/// One group per candidate reply, under the nullable conversation scope.
pub const USAGE_GAPS: ReadOnly = ReadOnly::same(with_gap_candidate!(usage_grouping_text!(
    "id",
    "AND (? IS NULL OR conversation_id = ?)",
)));

/// What a person needs to find each candidate again, newest first: never the
/// content, only how long it was — an empty reply is most of the story. Same
/// filter and values as [`USAGE_GAPS`].
macro_rules! gap_details_text {
    ($candidate:literal) => {
        concat!(
            "SELECT id, created_at, conversation_id, role, provider_name, model_id, source_type,
            CAST(LENGTH(content) AS BIGINT) AS content_chars
       FROM audit_messages
      WHERE role IN ('assistant', 'auto_review', 'compaction', 'title', 'extraction', 'cache_warm', 'dream')
        AND (? IS NULL OR created_at >= ?)
        AND (? IS NULL OR created_at < ?)
        AND (? IS NULL OR turn_origin = ?)
        AND (? IS NULL OR conversation_id = ?)
        ",
            $candidate,
            "
   ORDER BY created_at DESC, id DESC"
        )
    };
}

pub const USAGE_GAP_DETAILS: ReadOnly = ReadOnly::same(with_gap_candidate!(gap_details_text!()));

/// The registered grouping for a report dimension. Its key is the
/// dimension's `key_expr`, which the tests hold the two to.
pub fn usage_grouping(dimension: crate::db::sea::ops::usage::UsageDimension) -> ReadOnly {
    use crate::db::sea::ops::usage::UsageDimension;
    match dimension {
        UsageDimension::Total => USAGE_BY_TOTAL,
        UsageDimension::Provider => USAGE_BY_PROVIDER,
        UsageDimension::Model => USAGE_BY_MODEL,
        UsageDimension::LogicalModel => USAGE_BY_LOGICAL_MODEL,
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
    let conn = db.conn()?;
    let backend = conn.get_database_backend();
    let text = sql.text(backend);
    let statement = match backend {
        DbBackend::Postgres => Statement::from_sql_and_values(backend, numbered(text), values),
        _ => Statement::from_sql_and_values(backend, text, values),
    };
    conn.query_all_raw(statement).await
}

/// Takes PostgreSQL's transaction-scoped writer lock for this capability.
///
/// The SQL stays in this registry rather than in `cap::Db::write`; the lock is
/// what preserves SQLite `BEGIN IMMEDIATE`'s one-writer-at-a-time contract on
/// the second backend. `key` is bound, not interpolated.
#[cfg(feature = "postgres")]
pub async fn lock_postgres_writer(tx: &crate::db::sea::cap::WriteTx, key: i64) -> Result<(), DbErr> {
    use crate::db::sea::cap::sealed::Access;

    tx.conn()?
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT pg_advisory_xact_lock($1)",
            [key.into()],
        ))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sea::ops::usage::UsageDimension;

    const DIMENSIONS: [UsageDimension; 10] = [
        UsageDimension::Total,
        UsageDimension::Provider,
        UsageDimension::Model,
        UsageDimension::LogicalModel,
        UsageDimension::Bot,
        UsageDimension::Source,
        UsageDimension::Conversation,
        UsageDimension::Day,
        UsageDimension::Hour,
        UsageDimension::Kind,
    ];

    const BACKENDS: [DbBackend; 2] = [DbBackend::Sqlite, DbBackend::Postgres];

    /// Each dimension's statement groups by that dimension's key, on both
    /// backends, and no two dimensions share a statement.
    #[test]
    fn each_dimension_is_grouped_by_its_own_key() {
        for backend in BACKENDS {
            let mut seen = std::collections::HashSet::new();
            for dimension in DIMENSIONS {
                let statement = usage_grouping(dimension).text(backend);
                let select = format!("SELECT {} AS bucket_key,", dimension.key_expr(backend));
                assert!(statement.starts_with(&select), "{backend:?} {dimension:?}: {statement}");
                assert!(seen.insert(statement), "{backend:?} {dimension:?} shares a statement");
            }
            assert!(USAGE_BY_TURN.text(backend).starts_with("SELECT turn_id AS bucket_key,"));
        }
    }

    /// Only the calendar dimensions carry a second text. Anything else that
    /// grew one would be a dialect split nobody registered.
    #[test]
    fn only_the_calendar_reports_differ_between_backends() {
        let dual = DIMENSIONS
            .into_iter()
            .filter(|dimension| {
                let statement = usage_grouping(*dimension);
                statement.sqlite != statement.postgres
            })
            .collect::<Vec<_>>();
        assert_eq!(dual, vec![UsageDimension::Day, UsageDimension::Hour]);
        for statement in [
            &JOURNAL_CHAINS_UNDER_PREFIX,
            &JOURNAL_LIVE_CHAINS_UNDER_PREFIX,
            &JOURNAL_UNREFERENCED_BLOBS,
            &USAGE_CACHE_MISS_ROWS,
            &USAGE_BY_TURN,
            &USAGE_GAPS,
            &USAGE_GAP_DETAILS,
        ] {
            assert_eq!(statement.sqlite, statement.postgres);
        }
    }

    /// Two-argument `MAX` is a scalar clamp on SQLite and a syntax error on
    /// PostgreSQL. Every registered statement has to be free of it.
    #[test]
    fn no_registered_statement_uses_a_two_argument_max() {
        for backend in BACKENDS {
            for statement in DIMENSIONS
                .into_iter()
                .map(|dimension| usage_grouping(dimension).text(backend))
                .chain([
                    USAGE_BY_TURN.text(backend),
                    USAGE_GAPS.text(backend),
                    USAGE_GAP_DETAILS.text(backend),
                    USAGE_CACHE_MISS_ROWS.text(backend),
                ])
            {
                let normalized = statement.split_whitespace().collect::<Vec<_>>().join(" ");
                assert!(!normalized.contains("MAX(COALESCE"), "{backend:?}: {normalized}");
            }
        }
    }

    /// Placeholders are numbered in the order they appear, and a `?` inside a
    /// quoted literal is text. Counting that one would bind later values to
    /// the wrong columns rather than failing.
    #[test]
    fn placeholders_are_numbered_past_quoted_literals() {
        assert_eq!(numbered("WHERE a = ? AND b = ?"), "WHERE a = $1 AND b = $2");
        assert_eq!(
            numbered("WHERE p LIKE ? ESCAPE '\\' AND q = ?"),
            "WHERE p LIKE $1 ESCAPE '\\' AND q = $2"
        );
        assert_eq!(numbered("SELECT '? ?' , ?"), "SELECT '? ?' , $1");
        assert_eq!(numbered("SELECT 'it''s ?', ?"), "SELECT 'it''s ?', $1");
        assert_eq!(numbered("SELECT 1"), "SELECT 1");
    }

    /// Every registered statement numbers densely from one, so a reader can
    /// count values off the text.
    #[test]
    fn numbering_is_dense_and_starts_at_one() {
        for backend in BACKENDS {
            for statement in DIMENSIONS
                .into_iter()
                .map(|dimension| usage_grouping(dimension).text(backend))
                .chain([USAGE_BY_TURN.text(backend), USAGE_GAPS.text(backend)])
            {
                let rewritten = numbered(statement);
                let highest = (1..=16).take_while(|n| rewritten.contains(&format!("${n}"))).count();
                assert!(highest >= 7, "{backend:?}: only ${highest} placeholders");
                assert!(!rewritten.contains('?'), "{backend:?}: a `?` survived numbering");
            }
        }
    }

    /// Conversation snapshots are opened far more often than global reports.
    /// The durable ledger is append-only, so a scan here grows forever: the
    /// registered per-turn statement itself has to seek the composite index.
    #[tokio::test]
    async fn the_turn_usage_statement_seeks_the_conversation_turn_index() {
        use crate::db::sea::cap::sealed::Access;

        let db = crate::db::sea::sea_test_db().await;
        let conn = db.conn().unwrap();
        let backend = conn.get_database_backend();
        let mut values: Vec<Value> = vec![Option::<i64>::None.into(); 4];
        values.extend([Option::<String>::None.into(), Option::<String>::None.into()]);
        values.push("c1".into());
        // Two planners, two spellings of "show me the plan", and two column
        // names to read it out of. Both are asked the same question.
        let (prefix, column) = match backend {
            DbBackend::Postgres => ("EXPLAIN ", "QUERY PLAN"),
            _ => ("EXPLAIN QUERY PLAN ", "detail"),
        };
        let text = match backend {
            DbBackend::Postgres => numbered(USAGE_BY_TURN.text(backend)),
            _ => USAGE_BY_TURN.text(backend).to_owned(),
        };
        let plan = conn
            .query_all_raw(Statement::from_sql_and_values(
                backend,
                format!("{prefix}{text}"),
                values,
            ))
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get::<String>("", column).unwrap())
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
        let roles = crate::db::sea::ops::audit::BILLED_ROLES
            .iter()
            .map(|role| format!("'{role}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let clause = format!("WHERE role IN ({roles})");
        for backend in BACKENDS {
            for dimension in DIMENSIONS {
                assert!(
                    usage_grouping(dimension).text(backend).contains(&clause),
                    "{backend:?} {dimension:?}"
                );
            }
            assert!(USAGE_BY_TURN.text(backend).contains(&clause));
            assert!(USAGE_GAPS.text(backend).contains(&clause));
            assert!(USAGE_GAP_DETAILS.text(backend).contains(&clause));
        }
    }
}

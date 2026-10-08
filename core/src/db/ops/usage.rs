//! What was spent, grouped by whichever thing is being asked about.
//!
//! Reads `audit_messages` rather than `messages`, for the reason that table
//! exists: a bill must not change because someone tidied their chat list.
//! Deleting a conversation removes its transcript and leaves the record of what
//! it cost, which is the only way "last month" can keep meaning the same thing
//! a month later.
//!
//! **The cost is not computed here.** Every group is handed to
//! `agent::pricing::compute_cost`, the same function the turn's own stop event
//! uses, because that formula carries a rule no summation expresses — a cached
//! token is billed at the cache rate *instead of* the input rate, not on top of
//! it. Writing `SUM(input_tokens * input_price)` in SQL would be a second
//! implementation of a thing that has already been wrong once, and the two would
//! disagree in exactly the case that matters.
//!
//! That is why the grouping carries the prices: SQL reduces millions of rows to
//! a few dozen (dimension × model × price set), and Rust prices those. A price
//! is edited a handful of times a year, so the price columns barely multiply the
//! group count — and they cannot be left out, because a rate that changed
//! mid-month has to bill each half at what it was.

use std::collections::HashMap;

use diesel::prelude::*;
use diesel::sql_types::{BigInt, Nullable, Text};
use diesel::sqlite::SqliteConnection;

use crate::agent::pricing::Prices;
use crate::db::sea::ops::usage::{GroupRow, accumulate};
pub use crate::db::sea::ops::usage::{TurnPricingStatus, TurnUsageSummary, UsageBucket, UsageDimension, UsageFilter};
use crate::decimal::Decimal;

/// One audit query for every turn in a conversation, then one current-price
/// query for the legacy fallback. The cost work is the same accumulator used by
/// [`report`], not a snapshot-specific formula and not one query per turn.
pub fn turn_summaries(
    conn: &mut SqliteConnection,
    conversation_id: &str,
) -> QueryResult<HashMap<String, TurnUsageSummary>> {
    let filter = UsageFilter {
        conversation_id: Some(conversation_id.to_string()),
        ..Default::default()
    };
    let groups = grouped_for_key(conn, "turn_id", "AND turn_id IS NOT NULL", &filter, true)?;
    let current = current_prices(conn)?;
    Ok(accumulate(groups, &current)
        .map_err(contract)?
        .into_iter()
        .map(|(turn_id, accumulator)| (turn_id, accumulator.turn_summary()))
        .collect())
}

/// Today's rates for every configured model, for rows that were written before
/// prices were snapshotted.
///
/// Resolved through `agent::model_config::effective` rather than read straight
/// off the table, because since migration 61 the columns alone do not say what
/// a model costs: a row that does not override is priced by its profile, and a
/// row that does carries rates the profile never saw. Reading the columns here
/// would report every non-overriding model as unpriced.
///
/// Base rates only, as before: a tier needs a prompt size, and this fallback is
/// reached from a `SUM` over rows that are no longer one request.
fn current_prices(conn: &mut SqliteConnection) -> QueryResult<HashMap<(String, String), Prices>> {
    Ok(crate::agent::model_config::load_all(conn)?
        .into_iter()
        .map(|config| {
            (
                (config.provider_id.clone(), config.model_id.clone()),
                Prices {
                    input_price: config.input_price,
                    output_price: config.output_price,
                    cache_read_price: config.cache_read_price,
                    cache_write_price: config.cache_write_price,
                    server_tool_price: config.server_tool_price,
                },
            )
        })
        .collect())
}

/// cost right, and nothing downstream should be tempted to re-derive it.
#[derive(Debug, QueryableByName)]
struct DieselGroupRow {
    #[diesel(sql_type = Text)]
    bucket_key: String,
    #[diesel(sql_type = Nullable<Text>)]
    provider_id: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    model_id: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    input_price: Option<Decimal>,
    #[diesel(sql_type = Nullable<Text>)]
    output_price: Option<Decimal>,
    #[diesel(sql_type = Nullable<Text>)]
    cache_read_price: Option<Decimal>,
    #[diesel(sql_type = Nullable<Text>)]
    cache_write_price: Option<Decimal>,
    #[diesel(sql_type = Nullable<Text>)]
    server_tool_price: Option<Decimal>,
    /// Grouped on, not just carried: the same model under the same rates can be
    /// billed two ways over its life — an API key today, a subscription
    /// tomorrow — and merging those into one group would price the subscription
    /// half at the metered half's rates.
    #[diesel(sql_type = Text)]
    billing_mode: String,
    #[diesel(sql_type = BigInt)]
    messages: i64,
    #[diesel(sql_type = BigInt)]
    input_tokens: i64,
    /// Uncached prompt tokens summed per reply. This cannot be derived from the
    /// group totals when one reply omitted its prompt count but still reported
    /// cache usage: subtracting that cache from another reply's prompt would
    /// erase a known cost.
    #[diesel(sql_type = BigInt)]
    uncached_input_tokens: i64,
    #[diesel(sql_type = BigInt)]
    output_tokens: i64,
    #[diesel(sql_type = BigInt)]
    cache_read_tokens: i64,
    #[diesel(sql_type = BigInt)]
    cache_write_tokens: i64,
    /// Provider-side invocations, which bill per call rather than per token.
    #[diesel(sql_type = BigInt)]
    server_tool_calls: i64,
    /// Replies in this group that actually made at least one billable provider
    /// tool call. A group can contain replies with and without calls, so using
    /// `messages` when the tool rate is missing would overstate the gap.
    #[diesel(sql_type = BigInt)]
    server_tool_messages: i64,
    /// Replies whose provider supplied none of the four token usage fields.
    /// Tool calls are independent: a provider can report them while leaving
    /// token usage unknown.
    #[diesel(sql_type = BigInt)]
    missing_token_usage_messages: i64,
    /// Replies missing either input or output usage. These are the two required
    /// sides for an exact token bill; cache fields remain optional.
    #[diesel(sql_type = BigInt)]
    incomplete_token_usage_messages: i64,
    #[diesel(sql_type = BigInt)]
    missing_input_messages: i64,
    #[diesel(sql_type = BigInt)]
    missing_output_messages: i64,
    /// Replies with at least one positive token count. Kept apart from missing
    /// usage so explicit zeroes remain exact even without configured rates.
    #[diesel(sql_type = BigInt)]
    positive_token_messages: i64,
    /// Union of incomplete token usage and positive tool calls.
    #[diesel(sql_type = BigInt)]
    incomplete_token_or_tool_messages: i64,
    /// Union of incomplete and positive token usage. The two overlap when a
    /// provider reports only one side, so adding their counts would overstate
    /// the number of affected replies.
    #[diesel(sql_type = BigInt)]
    incomplete_or_positive_token_messages: i64,
    /// Component-specific pricing gaps. These let the turn hover card keep a
    /// reported output cost exact when only the input side is absent, while the
    /// overall turn remains a lower bound.
    #[diesel(sql_type = BigInt)]
    unpriced_input_usage_messages: i64,
    #[diesel(sql_type = BigInt)]
    unpriced_output_usage_messages: i64,
    #[diesel(sql_type = BigInt)]
    unpriced_cache_usage_messages: i64,
    /// Union of incomplete/positive token usage and positive tool calls.
    #[diesel(sql_type = BigInt)]
    unpriced_usage_messages: i64,
    /// Union of positive token usage and positive tool calls. This is used to
    /// count current-price fallbacks without marking explicit zeroes as
    /// estimates: any rate multiplied by zero is still an exact zero.
    #[diesel(sql_type = BigInt)]
    positive_token_or_tool_messages: i64,
    /// Replies whose input/output were explicitly zero, with no positive cache
    /// or tool usage. Every possible rate yields the same exact local zero.
    #[diesel(sql_type = BigInt)]
    explicit_zero_messages: i64,
}

impl From<DieselGroupRow> for GroupRow {
    fn from(row: DieselGroupRow) -> Self {
        Self {
            bucket_key: row.bucket_key,
            provider_id: row.provider_id,
            model_id: row.model_id,
            input_price: row.input_price,
            output_price: row.output_price,
            cache_read_price: row.cache_read_price,
            cache_write_price: row.cache_write_price,
            server_tool_price: row.server_tool_price,
            billing_mode: row.billing_mode,
            messages: row.messages,
            input_tokens: row.input_tokens,
            uncached_input_tokens: row.uncached_input_tokens,
            output_tokens: row.output_tokens,
            cache_read_tokens: row.cache_read_tokens,
            cache_write_tokens: row.cache_write_tokens,
            server_tool_calls: row.server_tool_calls,
            server_tool_messages: row.server_tool_messages,
            missing_token_usage_messages: row.missing_token_usage_messages,
            incomplete_token_usage_messages: row.incomplete_token_usage_messages,
            missing_input_messages: row.missing_input_messages,
            missing_output_messages: row.missing_output_messages,
            positive_token_messages: row.positive_token_messages,
            incomplete_token_or_tool_messages: row.incomplete_token_or_tool_messages,
            incomplete_or_positive_token_messages: row.incomplete_or_positive_token_messages,
            unpriced_input_usage_messages: row.unpriced_input_usage_messages,
            unpriced_output_usage_messages: row.unpriced_output_usage_messages,
            unpriced_cache_usage_messages: row.unpriced_cache_usage_messages,
            unpriced_usage_messages: row.unpriced_usage_messages,
            positive_token_or_tool_messages: row.positive_token_or_tool_messages,
            explicit_zero_messages: row.explicit_zero_messages,
        }
    }
}

/// A first-party contract broken by stored content.
fn contract(message: String) -> diesel::result::Error {
    diesel::result::Error::DeserializationError(message.into())
}

/// `key` and `extra_filter` are internal static SQL fragments, never caller
/// input. The turn reader uses the same statement shape with `turn_id` as its
/// key and excludes legacy NULL ids that cannot be attached exactly.
fn grouped_for_key(
    conn: &mut SqliteConnection,
    key: &'static str,
    extra_filter: &'static str,
    filter: &UsageFilter,
    direct_conversation: bool,
) -> QueryResult<Vec<GroupRow>> {
    // The two roles that carry tokens. A question carries none and has no
    // model, so every user row would land in a single group keyed on nothing
    // and inflate the reply count the other figures are read against.
    //
    // `auto_review` rows are traffic the user never asked for directly, and
    // leaving them out would report a smaller number than was actually spent —
    // the same failure as counting unpriced messages as free. They are told
    // apart by `UsageDimension::Kind`.
    //
    // Each filter is bound twice against the same `?`-pair rather than being
    // appended conditionally: one statement, one shape, and no arm of a builder
    // that can be reached only by a combination nobody tested.
    // Snapshot reads always name one conversation. Keep that equality direct
    // rather than hiding it behind the generic nullable-filter OR, so SQLite
    // can seek the `(conversation_id, turn_id)` index instead of scanning the
    // whole durable ledger whenever the conversation opens.
    let conversation_filter = if direct_conversation {
        "AND conversation_id = ?"
    } else {
        "AND (? IS NULL OR conversation_id = ?)"
    };
    let sql = format!(
        "SELECT {key} AS bucket_key,
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
          WHERE role IN ({roles})
            AND (? IS NULL OR created_at >= ?)
            AND (? IS NULL OR created_at < ?)
            AND (? IS NULL OR turn_origin = ?)
            {conversation_filter}
            {extra_filter}
       GROUP BY bucket_key, provider_id, model_id,
                input_price, output_price, cache_read_price, cache_write_price,
                server_tool_price, billing_mode",
        // A `&'static str` built from a constant, never a caller's string — the
        // same rule the key expression follows.
        roles = crate::db::ops::audit::BILLED_ROLES
            .iter()
            .map(|role| format!("'{role}'"))
            .collect::<Vec<_>>()
            .join(", "),
    );

    let origin = filter.origin.map(|value| value.as_str().to_string());
    let query = diesel::sql_query(sql)
        .bind::<Nullable<BigInt>, _>(filter.since_ms)
        .bind::<Nullable<BigInt>, _>(filter.since_ms)
        .bind::<Nullable<BigInt>, _>(filter.until_ms)
        .bind::<Nullable<BigInt>, _>(filter.until_ms)
        .bind::<Nullable<Text>, _>(origin.clone())
        .bind::<Nullable<Text>, _>(origin);
    if direct_conversation {
        query
            .bind::<Text, _>(
                filter
                    .conversation_id
                    .as_deref()
                    .expect("turn grouping names a conversation"),
            )
            .load::<DieselGroupRow>(conn)
            .map(|rows| rows.into_iter().map(Into::into).collect())
    } else {
        query
            .bind::<Nullable<Text>, _>(filter.conversation_id.clone())
            .bind::<Nullable<Text>, _>(filter.conversation_id.clone())
            .load::<DieselGroupRow>(conn)
            .map(|rows| rows.into_iter().map(Into::into).collect())
    }
}

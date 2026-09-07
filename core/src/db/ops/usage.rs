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
use serde::{Deserialize, Serialize};

use crate::agent::pricing::{BilledTokens, BillingMode, Prices, cost_of};
use crate::db::schema::{conversations, model_configs, projects};
use crate::decimal::Decimal;
use crate::turn::TurnOrigin;

/// Which window, and whose traffic.
///
/// Every field is optional and every one means "no restriction" when absent, so
/// the default value is the whole log.
///
/// **`conversation_id` is a scope, not a convenience.** Without it the only way
/// to look at one conversation was `UsageDimension::Conversation`, which
/// *groups* by conversation and still reads every row — so a caller that must
/// see one conversation and no other had no way to say so, and "wrap
/// [`report`]" meant handing over the whole ledger. That is exactly the caller
/// the ACP bridge is: `tools::usage` fills this in from the turn it is running
/// under and ignores anything the model asks for. Being a filter rather than a
/// grouping is what makes it hold across every dimension, including
/// `Conversation` itself — otherwise changing the dimension would be the way
/// out of the scope.
///
/// There is deliberately no `project_id` beside it. `audit_messages` has no such
/// column, so it would mean joining `conversations` — and a conversation that
/// has since moved projects, or been deleted, would answer differently from the
/// row it is about. The one caller that needs a scope needs this one.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageFilter {
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    /// Matched against the snapshotted `turn_origin`.
    pub origin: Option<TurnOrigin>,
    /// One conversation and nothing else. See the note above.
    pub conversation_id: Option<String>,
}

/// What the rows are grouped by.
///
/// `Total` is the same query with a constant key, which is what keeps the
/// headline figures and the breakdown that is supposed to add up to them from
/// being computed two different ways.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageDimension {
    Total,
    Provider,
    Model,
    Bot,
    Source,
    Conversation,
    Day,
    Hour,
    /// Answering the user, versus reviewing whether a tool call was allowed to
    /// happen. The second is spend nobody asked for directly, and a total that
    /// cannot separate the two is one nobody can act on.
    Kind,
}

impl UsageDimension {
    /// The SQL that produces this dimension's key.
    ///
    /// A `&'static str` chosen by a match, never a caller's string — the value
    /// is interpolated into the statement and this is the only reason that is
    /// safe. Everything a caller supplies is bound.
    fn key_expr(self) -> &'static str {
        match self {
            UsageDimension::Total => "''",
            // The name as it was at the time, because that is what the row
            // keeps; the id only stands in when a row predates that column.
            UsageDimension::Provider => "COALESCE(provider_name, provider_id, '')",
            UsageDimension::Model => "COALESCE(model_id, '')",
            UsageDimension::Bot => "COALESCE(CAST(self_id AS TEXT), '')",
            // Type and id together: a private chat with user 12345 and a group
            // numbered 12345 are different places with the same number.
            UsageDimension::Source => "COALESCE(source_type || ':' || source_id, '')",
            UsageDimension::Conversation => "conversation_id",
            // Local time, not UTC. A day boundary eight hours off puts an
            // evening's work under the wrong date, which is immediately visible
            // and reads as the numbers being wrong rather than the grouping.
            UsageDimension::Day => "strftime('%Y-%m-%d', created_at / 1000, 'unixepoch', 'localtime')",
            UsageDimension::Hour => "strftime('%Y-%m-%dT%H', created_at / 1000, 'unixepoch', 'localtime')",
            UsageDimension::Kind => "role",
        }
    }

    /// Time reads forwards; everything else reads biggest-first.
    fn is_series(self) -> bool {
        matches!(self, UsageDimension::Day | UsageDimension::Hour)
    }
}

/// One row of a report.
#[derive(Debug, Clone, Serialize)]
pub struct UsageBucket {
    pub key: String,
    /// A name for `key`, when one can still be found. `None` means the thing it
    /// refers to has been deleted — which is a fact worth showing rather than a
    /// row worth dropping.
    pub label: Option<String>,
    /// Replies, not messages. Only assistant rows carry tokens, and a count that
    /// included the questions would not divide into anything beside it.
    pub messages: i64,
    /// How many replies contribute to Meridian's local metered amount versus a
    /// subscription or an external ledger. Without these, an all-external
    /// bucket is indistinguishable from an exact local zero.
    pub metered_messages: i64,
    pub subscription_messages: i64,
    pub external_messages: i64,
    /// Replies for which the provider supplied none of the four token usage
    /// fields. This is independent of billing mode: external traffic can have
    /// unknown usage without representing a missing price.
    pub missing_token_usage_messages: i64,
    /// Replies missing either required side of the usage report. Cache fields
    /// are optional — a NULL cache count means no cache activity — but a NULL
    /// input or output count leaves that side of the bill unknown.
    pub incomplete_token_usage_messages: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub input_cost: Decimal,
    pub output_cost: Decimal,
    pub cache_cost: Decimal,
    pub tool_cost: Decimal,
    pub total_cost: Decimal,
    /// Replies whose token usage or token rates are incomplete. This is a
    /// component counter: unlike `unpriced_messages`, a reply can also appear
    /// in `unpriced_tool_messages`.
    pub unpriced_token_messages: i64,
    /// Replies with provider tool calls but no rate for those calls.
    pub unpriced_tool_messages: i64,
    /// Replies whose known amount used today's exact provider/model price
    /// because the historical audit row predates that price snapshot.
    pub estimated_token_messages: i64,
    pub estimated_tool_messages: i64,
    /// Union of the two component estimate counters above.
    pub estimated_messages: i64,
    /// How many of `messages` contain at least one metered component with no
    /// configured rate, or no provider-reported usage at all.
    ///
    /// Known components remain in the four cost fields and `total_cost`. With no
    /// current-price fallback that amount is a lower bound; with one it is a
    /// partial estimate that may be higher or lower than the historical bill.
    /// The estimate and gap counters let callers say which instead of calling
    /// either case free.
    pub unpriced_messages: i64,
}

impl UsageBucket {
    fn empty(key: String) -> Self {
        Self {
            key,
            label: None,
            messages: 0,
            metered_messages: 0,
            subscription_messages: 0,
            external_messages: 0,
            missing_token_usage_messages: 0,
            incomplete_token_usage_messages: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            input_cost: Decimal::zero(),
            output_cost: Decimal::zero(),
            cache_cost: Decimal::zero(),
            tool_cost: Decimal::zero(),
            total_cost: Decimal::zero(),
            unpriced_token_messages: 0,
            unpriced_tool_messages: 0,
            estimated_token_messages: 0,
            estimated_tool_messages: 0,
            estimated_messages: 0,
            unpriced_messages: 0,
        }
    }
}

/// A group exactly as the database returns it: one row per key, model and set of
/// prices. Not public — the prices are an implementation detail of getting the
/// cost right, and nothing downstream should be tempted to re-derive it.
#[derive(Debug, QueryableByName)]
struct GroupRow {
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

/// Rates resolved under the historical-snapshot rules, plus which part of the
/// result is actually known. Token and provider-tool prices are independent: a
/// missing tool rate must not erase known token spend, and a known tool rate is
/// still a useful lower bound when the token rates are blank.
struct ResolvedPrices {
    prices: Prices,
    token_prices_known: bool,
    token_prices_from_current: bool,
    tool_price_from_current: bool,
    used_current_fallback: bool,
}

/// Pricing state for one durable turn. Cost fields are present only when a
/// local metered amount is known. The status distinguishes an exact historical
/// snapshot, a current-price estimate, a true lower bound, and traffic paid
/// outside Meridian from an exactly-zero local bill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPricingStatus {
    Exact,
    Estimated,
    LowerBound,
    Subscription,
    External,
    Unavailable,
}

/// Persisted usage and cost for one `turn_id`, derived from `audit_messages`.
/// Historical assistant rows without a turn id cannot be attached exactly and
/// are deliberately absent rather than guessed onto a neighbouring turn.
#[derive(Debug, Clone, Serialize)]
pub struct TurnUsageSummary {
    pub messages: i64,
    /// Replies for which the provider supplied none of the four token usage
    /// fields. This includes subscription and external traffic, which may omit
    /// usage without representing a price the user failed to configure.
    pub missing_token_usage_messages: i64,
    /// Replies missing either input or output usage. This is a pricing gap even
    /// when the other side is known and retained as a lower bound.
    pub incomplete_token_usage_messages: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub server_tool_calls: i64,
    pub input_cost: Option<Decimal>,
    pub output_cost: Option<Decimal>,
    pub cache_cost: Option<Decimal>,
    pub tool_cost: Option<Decimal>,
    pub total_cost: Option<Decimal>,
    pub unpriced_token_messages: i64,
    pub unpriced_input_messages: i64,
    pub unpriced_output_messages: i64,
    pub unpriced_cache_messages: i64,
    pub unpriced_tool_messages: i64,
    pub estimated_token_messages: i64,
    pub estimated_tool_messages: i64,
    pub estimated_messages: i64,
    pub unpriced_messages: i64,
    pub metered_messages: i64,
    pub subscription_messages: i64,
    pub external_messages: i64,
    pub pricing_status: TurnPricingStatus,
}

struct UsageAccumulator {
    bucket: UsageBucket,
    server_tool_calls: i64,
    metered_server_tool_calls: i64,
    metered_messages: i64,
    subscription_messages: i64,
    external_messages: i64,
    has_known_input_amount: bool,
    has_known_output_amount: bool,
    has_known_cache_amount: bool,
    has_priced_tool_calls: bool,
    unpriced_input_messages: i64,
    unpriced_output_messages: i64,
    unpriced_cache_messages: i64,
}

impl UsageAccumulator {
    fn empty(key: String) -> Self {
        Self {
            bucket: UsageBucket::empty(key),
            server_tool_calls: 0,
            metered_server_tool_calls: 0,
            metered_messages: 0,
            subscription_messages: 0,
            external_messages: 0,
            has_known_input_amount: false,
            has_known_output_amount: false,
            has_known_cache_amount: false,
            has_priced_tool_calls: false,
            unpriced_input_messages: 0,
            unpriced_output_messages: 0,
            unpriced_cache_messages: 0,
        }
    }

    fn add(&mut self, group: &GroupRow, resolved: Option<ResolvedPrices>) -> QueryResult<()> {
        self.bucket.messages += group.messages;
        self.bucket.input_tokens += group.input_tokens;
        self.bucket.output_tokens += group.output_tokens;
        self.bucket.cache_read_tokens += group.cache_read_tokens;
        self.bucket.cache_write_tokens += group.cache_write_tokens;
        self.server_tool_calls += group.server_tool_calls;
        self.bucket.missing_token_usage_messages += group.missing_token_usage_messages;
        self.bucket.incomplete_token_usage_messages += group.incomplete_token_usage_messages;

        match billing_mode_of(group)? {
            BillingMode::Metered => {
                self.metered_messages += group.messages;
                self.bucket.metered_messages += group.messages;
                self.metered_server_tool_calls += group.server_tool_calls;
            }
            BillingMode::Subscription => {
                self.subscription_messages += group.messages;
                self.bucket.subscription_messages += group.messages;
            }
            BillingMode::External => {
                self.external_messages += group.messages;
                self.bucket.external_messages += group.messages;
            }
        }

        let Some(resolved) = resolved else {
            // Subscription and external requests have no per-request rate to
            // find, so neither is an actionable pricing gap.
            return Ok(());
        };
        let tokens = BilledTokens {
            uncached_input: group.uncached_input_tokens,
            output: group.output_tokens,
            cache_read: group.cache_read_tokens,
            cache_write: group.cache_write_tokens,
            server_tool_calls: group.server_tool_calls,
        };
        let cost = cost_of(&tokens, &resolved.prices);
        self.bucket.input_cost += cost.input_cost;
        self.bucket.output_cost += cost.output_cost;
        self.bucket.cache_cost += cost.cache_cost;
        self.bucket.tool_cost += cost.tool_cost;
        self.bucket.total_cost += cost.total_cost;

        if resolved.token_prices_known {
            // Preserve each reported side independently. A provider is allowed
            // to send output without input (and vice versa); the known side is
            // still a useful lower bound, but the absent side is not zero.
            self.has_known_input_amount |= group.messages > group.missing_input_messages;
            self.has_known_output_amount |= group.messages > group.missing_output_messages;
            self.has_known_cache_amount |= group.messages > group.missing_token_usage_messages;
        } else if group.explicit_zero_messages > 0 {
            // Unknown rates still cannot change an explicitly reported all-zero
            // request. Keep that exact zero without treating a partial zero as
            // evidence that another missing component was free.
            self.has_known_input_amount = true;
            self.has_known_output_amount = true;
            self.has_known_cache_amount = true;
        }
        self.has_priced_tool_calls |= group.server_tool_messages > 0 && resolved.prices.server_tool_price.is_some();

        let token_incomplete = if resolved.token_prices_known {
            group.incomplete_token_usage_messages
        } else {
            group.incomplete_or_positive_token_messages
        };
        if resolved.token_prices_known {
            self.unpriced_input_messages += group.missing_input_messages;
            self.unpriced_output_messages += group.missing_output_messages;
            self.unpriced_cache_messages += group.missing_token_usage_messages;
        } else {
            self.unpriced_input_messages += group.unpriced_input_usage_messages;
            self.unpriced_output_messages += group.unpriced_output_usage_messages;
            self.unpriced_cache_messages += group.unpriced_cache_usage_messages;
        }
        let tool_incomplete = if resolved.prices.server_tool_price.is_some() {
            0
        } else {
            group.server_tool_messages
        };
        self.bucket.unpriced_token_messages += token_incomplete;
        self.bucket.unpriced_tool_messages += tool_incomplete;

        // Keep the known half as a lower bound, while making the missing half
        // visible. When neither half is known the whole group counts once,
        // never once for tokens and again for tools.
        self.bucket.unpriced_messages +=
            match (resolved.token_prices_known, resolved.prices.server_tool_price.is_some()) {
                (true, true) => group.incomplete_token_usage_messages,
                (true, false) => group.incomplete_token_or_tool_messages,
                (false, true) => group.incomplete_or_positive_token_messages,
                (false, false) => group.unpriced_usage_messages,
            };

        if resolved.used_current_fallback {
            self.bucket.estimated_token_messages += if resolved.token_prices_from_current {
                group.positive_token_messages
            } else {
                Default::default()
            };
            self.bucket.estimated_tool_messages += if resolved.tool_price_from_current {
                group.server_tool_messages
            } else {
                Default::default()
            };
            self.bucket.estimated_messages +=
                match (resolved.token_prices_from_current, resolved.tool_price_from_current) {
                    (true, true) => group.positive_token_or_tool_messages,
                    (true, false) => group.positive_token_messages,
                    (false, true) => group.server_tool_messages,
                    (false, false) => 0,
                };
        }
        Ok(())
    }

    fn turn_summary(self) -> TurnUsageSummary {
        let has_metered = self.metered_messages > 0;
        let has_subscription = self.subscription_messages > 0;
        let has_external = self.external_messages > 0;
        let has_known_token_amount =
            self.has_known_input_amount || self.has_known_output_amount || self.has_known_cache_amount;
        let pricing_status = if has_metered {
            // A current-price fallback is an estimate, not a lower bound: the
            // historical rate may have been either higher or lower. Component
            // gap counters remain populated when this estimate is also partial.
            if self.bucket.estimated_messages > 0 {
                TurnPricingStatus::Estimated
            } else if self.bucket.unpriced_messages > 0 || has_subscription || has_external {
                if has_known_token_amount || self.has_priced_tool_calls {
                    TurnPricingStatus::LowerBound
                } else {
                    TurnPricingStatus::Unavailable
                }
            } else if has_known_token_amount || self.has_priced_tool_calls {
                TurnPricingStatus::Exact
            } else {
                // A metered row with neither usage nor a known component is
                // not evidence of a zero bill.
                TurnPricingStatus::Unavailable
            }
        } else {
            match (has_subscription, has_external) {
                (true, false) => TurnPricingStatus::Subscription,
                (false, true) => TurnPricingStatus::External,
                _ => TurnPricingStatus::Unavailable,
            }
        };
        let has_local_amount = matches!(
            pricing_status,
            TurnPricingStatus::Exact | TurnPricingStatus::Estimated | TurnPricingStatus::LowerBound
        );
        let token_cost = |known, value: &Decimal| (has_local_amount && known).then(|| value.clone());
        let tool_cost = (has_local_amount && (self.metered_server_tool_calls == 0 || self.has_priced_tool_calls))
            .then(|| self.bucket.tool_cost.clone());

        TurnUsageSummary {
            messages: self.bucket.messages,
            missing_token_usage_messages: self.bucket.missing_token_usage_messages,
            incomplete_token_usage_messages: self.bucket.incomplete_token_usage_messages,
            input_tokens: self.bucket.input_tokens,
            output_tokens: self.bucket.output_tokens,
            cache_read_tokens: self.bucket.cache_read_tokens,
            cache_write_tokens: self.bucket.cache_write_tokens,
            server_tool_calls: self.server_tool_calls,
            input_cost: token_cost(self.has_known_input_amount, &self.bucket.input_cost),
            output_cost: token_cost(self.has_known_output_amount, &self.bucket.output_cost),
            cache_cost: token_cost(self.has_known_cache_amount, &self.bucket.cache_cost),
            tool_cost,
            total_cost: has_local_amount.then(|| self.bucket.total_cost.clone()),
            unpriced_token_messages: self.bucket.unpriced_token_messages,
            unpriced_input_messages: self.unpriced_input_messages,
            unpriced_output_messages: self.unpriced_output_messages,
            unpriced_cache_messages: self.unpriced_cache_messages,
            unpriced_tool_messages: self.bucket.unpriced_tool_messages,
            estimated_token_messages: self.bucket.estimated_token_messages,
            estimated_tool_messages: self.bucket.estimated_tool_messages,
            estimated_messages: self.bucket.estimated_messages,
            unpriced_messages: self.bucket.unpriced_messages,
            metered_messages: self.metered_messages,
            subscription_messages: self.subscription_messages,
            external_messages: self.external_messages,
            pricing_status,
        }
    }
}

/// Group the log, price each group, and add the groups up.
pub fn report(
    conn: &mut SqliteConnection,
    dimension: UsageDimension,
    filter: &UsageFilter,
) -> QueryResult<Vec<UsageBucket>> {
    let groups = grouped(conn, dimension, filter)?;
    let current = current_prices(conn)?;
    let mut out: Vec<UsageBucket> = accumulate(groups, &current)?
        .into_values()
        .map(|accumulator| accumulator.bucket)
        .collect();
    if dimension.is_series() {
        out.sort_by(|a, b| a.key.cmp(&b.key));
    } else {
        // Tokens break the tie, so unpriced rows — every one of which costs 0 —
        // still come back in an order that puts the largest first.
        out.sort_by(|a, b| {
            b.total_cost
                .cmp(&a.total_cost)
                .then((b.input_tokens + b.output_tokens).cmp(&(a.input_tokens + a.output_tokens)))
        });
    }
    label(conn, dimension, &mut out)?;
    Ok(out)
}

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
    Ok(accumulate(groups, &current)?
        .into_iter()
        .map(|(turn_id, accumulator)| (turn_id, accumulator.turn_summary()))
        .collect())
}

fn accumulate(
    groups: Vec<GroupRow>,
    current: &HashMap<(String, String), Prices>,
) -> QueryResult<HashMap<String, UsageAccumulator>> {
    let mut buckets = HashMap::new();
    for group in groups {
        let resolved = resolve(&group, current)?;
        buckets
            .entry(group.bucket_key.clone())
            .or_insert_with(|| UsageAccumulator::empty(group.bucket_key.clone()))
            .add(&group, resolved)?;
    }
    Ok(buckets)
}

/// The snapshot if there is one, today's configuration if there is not, and
/// zeroes for any part nobody has priced. `None` is reserved for billing modes
/// where no per-request price is owed.
///
/// The middle case is the retroactive pricing migration 30 exists to end, and it
/// only applies to rows written before it. Reporting those at today's rate is
/// wrong in a way that cannot be fixed — there is no other number — but it beats
/// reporting them as free.
///
/// **The mode is checked before any of that**, and it has to be: the fallback
/// looks the request's model up in today's `model_configs`, so a subscription
/// request through a provider that happens to have rates on file would be priced
/// at them. "No rate was stored" and "no rate exists" are the same shape in the
/// row and opposite in meaning, and only `billing_mode` tells them apart.
fn resolve(group: &GroupRow, current: &HashMap<(String, String), Prices>) -> QueryResult<Option<ResolvedPrices>> {
    if !billing_mode_of(group)?.is_priced() {
        return Ok(None);
    }
    let snapshot = match (group.input_price.clone(), group.output_price.clone()) {
        (Some(input), Some(output)) => Some(Prices {
            input_price: Some(input),
            output_price: Some(output),
            cache_read_price: group.cache_read_price.clone(),
            cache_write_price: group.cache_write_price.clone(),
            server_tool_price: group.server_tool_price.clone(),
        }),
        _ => None,
    }
    // A complete historical rate wins unchanged. Legacy rows with NULL rates
    // remain eligible for the current-price estimate.
    .filter(Prices::known);
    let current_prices = group.provider_id.as_ref().and_then(|provider| {
        let model = group.model_id.as_ref()?;
        current.get(&(provider.clone(), model.clone())).cloned()
    });
    let token_prices_from_current = snapshot.is_none() && current_prices.as_ref().is_some_and(Prices::known);
    let tool_price_from_current = group.server_tool_price.is_none()
        && current_prices
            .as_ref()
            .is_some_and(|prices| prices.server_tool_price.is_some());
    let mut prices = snapshot.or_else(|| current_prices.clone()).unwrap_or_default();

    // A tool rate is independent of the token rates. Preserve an explicit
    // historical value even on a legacy/zero token snapshot; otherwise a
    // deleted model would lose the one part of its cost the row did know.
    prices.server_tool_price = group
        .server_tool_price
        .clone()
        .or(prices.server_tool_price)
        // Migration 37 added the tool-rate snapshot after token snapshots
        // already existed. Its legacy NULL has the same best-available answer
        // as migration 30's NULL token rates: today's exact provider/model
        // config, never a same-named model under a different provider.
        .or_else(|| current_prices.and_then(|current| current.server_tool_price));

    Ok(Some(ResolvedPrices {
        token_prices_known: prices.known(),
        prices,
        token_prices_from_current,
        tool_price_from_current,
        used_current_fallback: token_prices_from_current || tool_price_from_current,
    }))
}

/// Persisted enum values are strict. Unknown data is a contract violation,
/// never a request to guess the closest current variant.
fn billing_mode_of(group: &GroupRow) -> QueryResult<BillingMode> {
    group
        .billing_mode
        .parse()
        .map_err(|error| diesel::result::Error::DeserializationError(Box::new(error)))
}

fn current_prices(conn: &mut SqliteConnection) -> QueryResult<HashMap<(String, String), Prices>> {
    let rows = model_configs::table
        .select((
            model_configs::provider_id,
            model_configs::model_id,
            model_configs::input_price,
            model_configs::output_price,
            model_configs::cache_read_price,
            model_configs::cache_write_price,
            model_configs::server_tool_price,
        ))
        .load::<(
            String,
            String,
            Option<Decimal>,
            Option<Decimal>,
            Option<Decimal>,
            Option<Decimal>,
            Option<Decimal>,
        )>(conn)?;
    Ok(rows
        .into_iter()
        .map(
            |(provider, model, input, output, cache_read, cache_write, server_tool)| {
                (
                    (provider, model),
                    Prices {
                        input_price: input,
                        output_price: output,
                        cache_read_price: cache_read,
                        cache_write_price: cache_write,
                        server_tool_price: server_tool,
                    },
                )
            },
        )
        .collect())
}

fn grouped(conn: &mut SqliteConnection, dimension: UsageDimension, filter: &UsageFilter) -> QueryResult<Vec<GroupRow>> {
    grouped_for_key(conn, dimension.key_expr(), "", filter, false)
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
            .load::<GroupRow>(conn)
    } else {
        query
            .bind::<Nullable<Text>, _>(filter.conversation_id.clone())
            .bind::<Nullable<Text>, _>(filter.conversation_id.clone())
            .load::<GroupRow>(conn)
    }
}

/// Put a name to the keys that are ids.
///
/// Provider, model and bot are already readable — the key *is* the name, either
/// snapshotted or a number. The other two point at rows that may be gone, which
/// is why this leaves `label` as `None` rather than substituting the id: a
/// deleted conversation should read as deleted, not as a title nobody chose.
fn label(conn: &mut SqliteConnection, dimension: UsageDimension, buckets: &mut [UsageBucket]) -> QueryResult<()> {
    match dimension {
        UsageDimension::Conversation => {
            let ids: Vec<&str> = buckets.iter().map(|b| b.key.as_str()).collect();
            let titles: HashMap<String, Option<String>> = conversations::table
                .filter(conversations::id.eq_any(&ids))
                .select((conversations::id, conversations::title))
                .load::<(String, Option<String>)>(conn)?
                .into_iter()
                .collect();
            for bucket in buckets.iter_mut() {
                // `Some("")` means the conversation exists and is navigable
                // but has never been titled. `None` alone means its row is
                // gone. Flattening the nullable title collapsed those two and
                // made a real untitled conversation look deleted to the UI.
                bucket.label = titles.get(&bucket.key).map(|title| title.clone().unwrap_or_default());
            }
        }
        UsageDimension::Source => {
            let named: HashMap<String, String> = projects::table
                .filter(projects::source_id.is_not_null())
                .select((projects::source_type, projects::source_id, projects::name))
                .load::<(String, Option<String>, String)>(conn)?
                .into_iter()
                .filter_map(|(kind, id, name)| Some((format!("{kind}:{}", id?), name)))
                .collect();
            for bucket in buckets.iter_mut() {
                bucket.label = named.get(&bucket.key).cloned();
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::audit::AuditMessageInsert;
    use crate::db::schema::audit_messages;
    use crate::db::test_db;

    fn decimal(raw: &str) -> Decimal {
        raw.parse().unwrap()
    }

    /// Straight into the table. `record` is exercised by its own module's tests;
    /// what these need is control over prices and timestamps, which a real turn
    /// does not offer.
    #[allow(clippy::too_many_arguments)]
    fn reply(
        conn: &mut SqliteConnection,
        id: &str,
        model: &str,
        created_at: i64,
        tokens: (i32, i32, i32, i32),
        prices: Option<(&str, &str)>,
        origin: &str,
    ) {
        let (input, output, cache_read, cache_write) = tokens;
        diesel::insert_into(audit_messages::table)
            .values(&AuditMessageInsert {
                id,
                recorded_at: created_at,
                message_id: id,
                conversation_id: "c1",
                turn_id: None,
                source_type: Some("onebot_group"),
                source_id: Some("900"),
                turn_origin: Some(origin),
                role: "assistant",
                content: "",
                sender_id: None,
                sender_name: None,
                provider_id: Some("p1"),
                provider_name: Some("Acme"),
                model_id: Some(model),
                input_tokens: Some(input),
                output_tokens: Some(output),
                cache_read_tokens: Some(cache_read),
                cache_write_tokens: Some(cache_write),
                created_at,
                input_price: prices.map(|p| decimal(p.0)),
                output_price: prices.map(|p| decimal(p.1)),
                cache_read_price: None,
                cache_write_price: None,
                server_tool_calls: None,
                server_tool_price: None,
                billing_mode: "metered",
                self_id: Some(10001),
            })
            .execute(conn)
            .unwrap();
    }

    /// A provider and a priced model, so the fallback in `resolve` has something
    /// to find. Without this the tests below could not tell "refused to price"
    /// from "had no price to use".
    fn seed_model(conn: &mut SqliteConnection, model: &str, input: &str, output: &str) {
        use crate::db::models::model_config::ModelConfigInsert;
        use crate::db::models::provider::ProviderInsert;

        diesel::insert_into(crate::db::schema::providers::table)
            .values(&ProviderInsert {
                id: "p1",
                name: "Acme",
                provider_type: "openai",
                base_url: "https://example.invalid",
                is_enabled: 1,
                sort_order: 0,
                created_at: 0,
                updated_at: 0,
                api_format: "chat",
                catalog_id: None,
                credential_kind: "api_key",
                transport_profile: "standard",
            })
            .execute(conn)
            .unwrap();
        crate::db::ops::model_config::upsert(
            conn,
            &ModelConfigInsert {
                id: "mc1",
                provider_id: "p1",
                model_id: model,
                display_name: None,
                context_window: 1,
                compact_threshold: 1,
                max_output_tokens: None,
                input_price: Some(decimal(input)),
                output_price: Some(decimal(output)),
                cache_read_price: None,
                cache_write_price: None,
                created_at: 0,
                updated_at: 0,
                capability_overrides: None,
                pricing_tiers: None,
                server_tools: None,
                server_tool_price: None,
            },
        )
        .unwrap();
    }

    /// A reply under a given billing mode, with no snapshotted rates — the shape
    /// that makes the price fallback reachable.
    fn reply_billed(conn: &mut SqliteConnection, id: &str, model: &str, mode: &str, tokens: (i32, i32)) {
        diesel::insert_into(audit_messages::table)
            .values(&AuditMessageInsert {
                id,
                recorded_at: 1,
                message_id: id,
                conversation_id: "c1",
                turn_id: None,
                source_type: None,
                source_id: None,
                turn_origin: Some("desktop"),
                role: "assistant",
                content: "",
                sender_id: None,
                sender_name: None,
                provider_id: Some("p1"),
                provider_name: Some("Acme"),
                model_id: Some(model),
                input_tokens: Some(tokens.0),
                output_tokens: Some(tokens.1),
                cache_read_tokens: Some(0),
                cache_write_tokens: Some(0),
                created_at: 1,
                input_price: None,
                output_price: None,
                cache_read_price: None,
                cache_write_price: None,
                server_tool_calls: None,
                server_tool_price: None,
                billing_mode: mode,
                self_id: None,
            })
            .execute(conn)
            .unwrap();
    }

    fn attach_to_turn(conn: &mut SqliteConnection, audit_id: &str, turn_id: &str) {
        diesel::update(audit_messages::table.find(audit_id))
            .set(audit_messages::turn_id.eq(Some(turn_id)))
            .execute(conn)
            .unwrap();
    }

    /// The same shape as [`reply_billed`], in a conversation of its own.
    ///
    /// Separate because every other fixture here writes `c1`, and a scope test
    /// needs at least two conversations to mean anything.
    fn reply_in(conn: &mut SqliteConnection, id: &str, conversation: &str, tokens: (i32, i32)) {
        diesel::insert_into(audit_messages::table)
            .values(&AuditMessageInsert {
                id,
                recorded_at: 1,
                message_id: id,
                conversation_id: conversation,
                turn_id: None,
                source_type: None,
                source_id: None,
                turn_origin: Some("desktop"),
                role: "assistant",
                content: "",
                sender_id: None,
                sender_name: None,
                provider_id: Some("p1"),
                provider_name: Some("Acme"),
                model_id: Some("m1"),
                input_tokens: Some(tokens.0),
                output_tokens: Some(tokens.1),
                cache_read_tokens: Some(0),
                cache_write_tokens: Some(0),
                created_at: 1,
                input_price: None,
                output_price: None,
                cache_read_price: None,
                cache_write_price: None,
                server_tool_calls: None,
                server_tool_price: None,
                billing_mode: "metered",
                self_id: None,
            })
            .execute(conn)
            .unwrap();
    }

    /// The scope the ACP bridge rests on: one conversation, nothing else.
    #[test]
    fn a_conversation_filter_excludes_every_other_conversation() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_model(&mut conn, "m1", "1", "2");
        reply_in(&mut conn, "a1", "mine", (10, 10));
        reply_in(&mut conn, "b1", "theirs", (500, 500));
        reply_in(&mut conn, "b2", "theirs", (500, 500));

        let scoped = report(
            &mut conn,
            UsageDimension::Total,
            &UsageFilter {
                conversation_id: Some("mine".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(scoped[0].messages, 1, "somebody else's replies were counted");
        assert_eq!(scoped[0].input_tokens, 10);

        let unscoped = report(&mut conn, UsageDimension::Total, &UsageFilter::default()).unwrap();
        assert_eq!(unscoped[0].messages, 3, "an absent filter still means the whole log");
    }

    /// Changing the grouping must not be the way out of the scope.
    ///
    /// `Conversation` is the dimension that would do it if the scope were a
    /// grouping rather than a filter: asked to break the ledger down by
    /// conversation, a scoped report may answer with exactly one row and no
    /// other conversation may appear in it — not even as a key.
    #[test]
    fn no_dimension_widens_a_scoped_report() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_model(&mut conn, "m1", "1", "2");
        reply_in(&mut conn, "a1", "mine", (10, 10));
        reply_in(&mut conn, "b1", "theirs", (10, 10));

        let filter = UsageFilter {
            conversation_id: Some("mine".into()),
            ..Default::default()
        };
        for dimension in [
            UsageDimension::Total,
            UsageDimension::Provider,
            UsageDimension::Model,
            UsageDimension::Bot,
            UsageDimension::Source,
            UsageDimension::Conversation,
            UsageDimension::Day,
            UsageDimension::Hour,
            UsageDimension::Kind,
        ] {
            let out = report(&mut conn, dimension, &filter).unwrap();
            let messages: i64 = out.iter().map(|b| b.messages).sum();
            assert_eq!(messages, 1, "{dimension:?} let another conversation's rows in");
            assert!(
                !out.iter().any(|b| b.key == "theirs"),
                "{dimension:?} named a conversation outside the scope"
            );
        }
    }

    /// The one that would have been silently wrong: a subscription request must
    /// not be priced off today's `model_configs`.
    ///
    /// Both rows here carry no snapshotted rate, so both reach the fallback —
    /// and the fixture's model *is* priced in `model_configs`. Told apart only
    /// by `billing_mode`, the metered one bills and the subscription one does
    /// not. Without the mode check they would bill identically, which is how a
    /// plan the user already paid for would appear a second time on this bill.
    #[test]
    fn a_subscription_is_not_priced_from_todays_configuration() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_model(&mut conn, "m1", "1", "2");

        reply_billed(&mut conn, "a1", "m1", "metered", (1_000_000, 1_000_000));
        let metered = report(&mut conn, UsageDimension::Total, &UsageFilter::default()).unwrap();
        assert_eq!(
            metered[0].total_cost,
            decimal("3"),
            "a metered row still falls back to today's rates"
        );

        let pool2 = test_db();
        let mut conn2 = pool2.get().unwrap();
        seed_model(&mut conn2, "m1", "1", "2");
        reply_billed(&mut conn2, "b1", "m1", "subscription", (1_000_000, 1_000_000));
        let sub = report(&mut conn2, UsageDimension::Total, &UsageFilter::default()).unwrap();
        assert_eq!(sub[0].total_cost, Decimal::zero(), "a subscription must not be priced");
        assert_eq!(sub[0].input_tokens, 1_000_000, "its tokens are still counted");
    }

    /// A subscription has no rate to go and find, so reporting it as a shortfall
    /// produces a warning nobody can clear.
    #[test]
    fn only_metered_traffic_can_be_unpriced() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        // No `seed_model`, so nothing is priced anywhere.
        reply_billed(&mut conn, "a1", "unpriced", "metered", (10, 10));
        reply_billed(&mut conn, "b1", "unpriced", "subscription", (10, 10));
        reply_billed(&mut conn, "c1", "unpriced", "external", (10, 10));

        let out = report(&mut conn, UsageDimension::Total, &UsageFilter::default()).unwrap();
        assert_eq!(out[0].messages, 3, "all three are still counted as replies");
        assert_eq!(
            (
                out[0].metered_messages,
                out[0].subscription_messages,
                out[0].external_messages
            ),
            (1, 1, 1),
            "the UI must be able to distinguish a local zero from non-local billing"
        );
        assert_eq!(out[0].unpriced_messages, 1, "only the metered one is a missing price");
    }

    /// One batched turn query must preserve the distinction between an exact
    /// zero/local bill and traffic paid elsewhere. Mixed modes are not exact,
    /// and legacy rows without `turn_id` are not guessed onto a turn.
    #[test]
    fn turn_summaries_carry_persisted_cost_and_pricing_status() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();

        reply(
            &mut conn,
            "exact-row",
            "m",
            1,
            (1_000_000, 100_000, 0, 0),
            Some(("10", "20")),
            "desktop",
        );
        attach_to_turn(&mut conn, "exact-row", "exact");

        reply_billed(&mut conn, "external-row", "hosted", "external", (50, 25));
        attach_to_turn(&mut conn, "external-row", "external");
        diesel::update(audit_messages::table.find("external-row"))
            .set((
                audit_messages::input_tokens.eq(None::<i32>),
                audit_messages::output_tokens.eq(None::<i32>),
                audit_messages::cache_read_tokens.eq(None::<i32>),
                audit_messages::cache_write_tokens.eq(None::<i32>),
            ))
            .execute(&mut conn)
            .unwrap();
        reply_billed(&mut conn, "subscription-row", "plan", "subscription", (50, 25));
        attach_to_turn(&mut conn, "subscription-row", "subscription");

        reply(&mut conn, "unknown-row", "unknown", 2, (50, 25, 0, 0), None, "desktop");
        attach_to_turn(&mut conn, "unknown-row", "unavailable");

        reply(
            &mut conn,
            "mixed-local",
            "m",
            3,
            (1_000_000, 0, 0, 0),
            Some(("10", "20")),
            "desktop",
        );
        attach_to_turn(&mut conn, "mixed-local", "mixed");
        reply_billed(&mut conn, "mixed-external", "hosted", "external", (50, 25));
        attach_to_turn(&mut conn, "mixed-external", "mixed");

        // Not attachable, by design.
        reply(
            &mut conn,
            "legacy",
            "m",
            4,
            (10, 0, 0, 0),
            Some(("10", "20")),
            "desktop",
        );

        let summaries = turn_summaries(&mut conn, "c1").unwrap();
        assert_eq!(summaries.len(), 5);
        assert!(!summaries.contains_key(""));

        let exact = &summaries["exact"];
        assert_eq!(exact.pricing_status, TurnPricingStatus::Exact);
        assert_eq!(exact.missing_token_usage_messages, 0);
        assert_eq!(exact.input_tokens, 1_000_000);
        assert_eq!(exact.total_cost, Some(decimal("12")));

        let external = &summaries["external"];
        assert_eq!(external.pricing_status, TurnPricingStatus::External);
        assert_eq!(external.missing_token_usage_messages, 1);
        assert_eq!((external.input_tokens, external.output_tokens), (0, 0));
        assert_eq!(external.external_messages, 1);
        assert_eq!(external.tool_cost, None);
        assert_eq!(external.total_cost, None, "paid elsewhere is not a free local request");

        let subscription = &summaries["subscription"];
        assert_eq!(subscription.pricing_status, TurnPricingStatus::Subscription);
        assert_eq!(subscription.subscription_messages, 1);
        assert_eq!(subscription.tool_cost, None);
        assert_eq!(subscription.total_cost, None);

        let unavailable = &summaries["unavailable"];
        assert_eq!(unavailable.pricing_status, TurnPricingStatus::Unavailable);
        assert_eq!(unavailable.unpriced_messages, 1);
        assert_eq!(unavailable.total_cost, None);

        let mixed = &summaries["mixed"];
        assert_eq!(mixed.pricing_status, TurnPricingStatus::LowerBound);
        assert_eq!(mixed.total_cost, Some(decimal("10")));
        assert_eq!((mixed.metered_messages, mixed.external_messages), (1, 1));

        let all = total(&mut conn, &UsageFilter::default());
        assert_eq!(
            all.missing_token_usage_messages, 1,
            "global usage keeps missing external token reports distinct from zero"
        );
    }

    /// Conversation snapshots are opened far more often than global reports.
    /// The durable ledger is append-only, so a scan here grows forever; keep
    /// the real direct conversation predicate on the composite turn index.
    #[test]
    fn turn_summary_query_seeks_the_conversation_turn_index() {
        #[derive(QueryableByName)]
        struct QueryPlanDetail {
            #[diesel(sql_type = Text)]
            detail: String,
        }

        let pool = test_db();
        let mut conn = pool.get().unwrap();
        let plan = diesel::sql_query(
            "EXPLAIN QUERY PLAN
             SELECT turn_id, provider_id, model_id, COUNT(*)
               FROM audit_messages
              WHERE role IN ('assistant', 'auto_review')
                AND (NULL IS NULL OR created_at >= NULL)
                AND (NULL IS NULL OR created_at < NULL)
                AND (NULL IS NULL OR turn_origin = NULL)
                AND conversation_id = 'c1'
                AND turn_id IS NOT NULL
           GROUP BY turn_id, provider_id, model_id",
        )
        .load::<QueryPlanDetail>(&mut conn)
        .unwrap();

        assert!(
            plan.iter()
                .any(|row| row.detail.contains("idx_audit_conversation_turn")),
            "snapshot query stopped using the ledger index: {:?}",
            plan.iter().map(|row| row.detail.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn missing_usage_is_not_conflated_with_an_explicit_zero() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        for (id, turn_id) in [("missing-row", "missing"), ("zero-row", "zero")] {
            reply(&mut conn, id, "m", 1, (0, 0, 0, 0), Some(("10", "20")), "desktop");
            attach_to_turn(&mut conn, id, turn_id);
        }
        diesel::update(audit_messages::table.find("missing-row"))
            .set((
                audit_messages::input_tokens.eq(None::<i32>),
                audit_messages::output_tokens.eq(None::<i32>),
                audit_messages::cache_read_tokens.eq(None::<i32>),
                audit_messages::cache_write_tokens.eq(None::<i32>),
                audit_messages::server_tool_calls.eq(None::<i32>),
            ))
            .execute(&mut conn)
            .unwrap();

        reply(
            &mut conn,
            "unknown-zero-row",
            "unpriced",
            2,
            (0, 0, 0, 0),
            Some(("0", "0")),
            "desktop",
        );
        attach_to_turn(&mut conn, "unknown-zero-row", "unknown-zero");

        reply(
            &mut conn,
            "tool-only-row",
            "m",
            3,
            (0, 0, 0, 0),
            Some(("10", "20")),
            "desktop",
        );
        attach_to_turn(&mut conn, "tool-only-row", "tool-only");
        diesel::update(audit_messages::table.find("tool-only-row"))
            .set((
                audit_messages::input_tokens.eq(None::<i32>),
                audit_messages::output_tokens.eq(None::<i32>),
                audit_messages::cache_read_tokens.eq(None::<i32>),
                audit_messages::cache_write_tokens.eq(None::<i32>),
                audit_messages::server_tool_calls.eq(Some(2)),
                audit_messages::server_tool_price.eq(Some(decimal("15"))),
            ))
            .execute(&mut conn)
            .unwrap();

        reply(
            &mut conn,
            "overlap-row",
            "m",
            4,
            (0, 0, 0, 0),
            Some(("10", "20")),
            "desktop",
        );
        attach_to_turn(&mut conn, "overlap-row", "overlap");
        diesel::update(audit_messages::table.find("overlap-row"))
            .set((
                audit_messages::input_tokens.eq(None::<i32>),
                audit_messages::output_tokens.eq(None::<i32>),
                audit_messages::cache_read_tokens.eq(None::<i32>),
                audit_messages::cache_write_tokens.eq(None::<i32>),
                audit_messages::server_tool_calls.eq(Some(2)),
                audit_messages::server_tool_price.eq(None::<Decimal>),
            ))
            .execute(&mut conn)
            .unwrap();

        let summaries = turn_summaries(&mut conn, "c1").unwrap();
        let missing = &summaries["missing"];
        assert_eq!(missing.pricing_status, TurnPricingStatus::Unavailable);
        assert_eq!(missing.missing_token_usage_messages, 1);
        assert_eq!(missing.unpriced_token_messages, 1);
        assert_eq!(missing.unpriced_tool_messages, 0);
        assert_eq!(missing.unpriced_messages, 1);
        assert_eq!(missing.input_cost, None, "no usage means no known token amount");
        assert_eq!(missing.total_cost, None, "a missing usage report is not an exact zero");

        let zero = &summaries["zero"];
        assert_eq!(zero.pricing_status, TurnPricingStatus::Exact);
        assert_eq!(zero.missing_token_usage_messages, 0);
        assert_eq!(zero.unpriced_messages, 0);
        assert_eq!(zero.input_cost, Some(Decimal::zero()));
        assert_eq!(
            zero.total_cost,
            Some(Decimal::zero()),
            "the provider explicitly reported zero usage"
        );

        let unknown_zero = &summaries["unknown-zero"];
        assert_eq!(unknown_zero.pricing_status, TurnPricingStatus::Exact);
        assert_eq!(
            unknown_zero.unpriced_messages, 0,
            "no price is needed when every unit is explicitly zero"
        );
        assert_eq!(unknown_zero.total_cost, Some(Decimal::zero()));
        assert_eq!(unknown_zero.input_cost, Some(Decimal::zero()));

        let tool_only = &summaries["tool-only"];
        assert_eq!(tool_only.pricing_status, TurnPricingStatus::LowerBound);
        assert_eq!(tool_only.missing_token_usage_messages, 1);
        assert_eq!(
            tool_only.unpriced_messages, 1,
            "known tool usage does not make missing token usage exact"
        );
        assert_eq!(tool_only.unpriced_token_messages, 1);
        assert_eq!(tool_only.unpriced_tool_messages, 0);
        assert_eq!(tool_only.input_cost, None);
        assert_eq!(tool_only.tool_cost, Some(decimal("0.03")));
        assert_eq!(tool_only.total_cost, Some(decimal("0.03")));

        let overlap = &summaries["overlap"];
        assert_eq!(overlap.pricing_status, TurnPricingStatus::Unavailable);
        assert_eq!(overlap.unpriced_token_messages, 1);
        assert_eq!(overlap.unpriced_tool_messages, 1);
        assert_eq!(
            overlap.unpriced_messages, 1,
            "one row missing token usage and tool price is still one incomplete reply"
        );
        assert_eq!(overlap.tool_cost, None);
        assert_eq!(overlap.total_cost, None);

        let all = total(&mut conn, &UsageFilter::default());
        assert_eq!(all.messages, 5);
        assert_eq!(
            all.unpriced_messages, 3,
            "the global ledger preserves the same distinctions and unions overlap"
        );
        assert_eq!(all.unpriced_token_messages, 3);
        assert_eq!(all.unpriced_tool_messages, 1);
    }

    /// Input and output arrive on different stream events for some providers,
    /// and either side can be absent after a truncated response. The reported
    /// side remains billable, but the absent side is not an exact zero.
    #[test]
    fn partial_token_reports_keep_the_known_component_as_a_lower_bound() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();

        reply(
            &mut conn,
            "input-missing-row",
            "m",
            1,
            (0, 100_000, 900_000, 0),
            Some(("10", "20")),
            "desktop",
        );
        attach_to_turn(&mut conn, "input-missing-row", "input-missing");
        diesel::update(audit_messages::table.find("input-missing-row"))
            .set(audit_messages::input_tokens.eq(None::<i32>))
            .execute(&mut conn)
            .unwrap();

        reply(
            &mut conn,
            "output-missing-row",
            "m",
            2,
            (1_000_000, 0, 0, 0),
            Some(("10", "20")),
            "desktop",
        );
        attach_to_turn(&mut conn, "output-missing-row", "output-missing");
        diesel::update(audit_messages::table.find("output-missing-row"))
            .set(audit_messages::output_tokens.eq(None::<i32>))
            .execute(&mut conn)
            .unwrap();

        let summaries = turn_summaries(&mut conn, "c1").unwrap();
        let input_missing = &summaries["input-missing"];
        assert_eq!(input_missing.missing_token_usage_messages, 0);
        assert_eq!(input_missing.incomplete_token_usage_messages, 1);
        assert_eq!(
            (input_missing.input_tokens, input_missing.output_tokens),
            (900_000, 100_000)
        );
        assert_eq!(input_missing.pricing_status, TurnPricingStatus::LowerBound);
        assert_eq!(input_missing.unpriced_token_messages, 1);
        assert_eq!(input_missing.unpriced_input_messages, 1);
        assert_eq!(input_missing.unpriced_output_messages, 0);
        assert_eq!(input_missing.unpriced_cache_messages, 0);
        assert_eq!(input_missing.input_cost, None);
        assert_eq!(input_missing.cache_cost, Some(decimal("9")));
        assert_eq!(input_missing.output_cost, Some(decimal("2")));
        assert_eq!(input_missing.total_cost, Some(decimal("11")));

        let output_missing = &summaries["output-missing"];
        assert_eq!(output_missing.missing_token_usage_messages, 0);
        assert_eq!(output_missing.incomplete_token_usage_messages, 1);
        assert_eq!(output_missing.pricing_status, TurnPricingStatus::LowerBound);
        assert_eq!(output_missing.unpriced_input_messages, 0);
        assert_eq!(output_missing.unpriced_output_messages, 1);
        assert_eq!(output_missing.unpriced_cache_messages, 0);
        assert_eq!(output_missing.input_cost, Some(decimal("10")));
        assert_eq!(output_missing.output_cost, None);
        assert_eq!(output_missing.cache_cost, Some(Decimal::zero()));
        assert_eq!(output_missing.total_cost, Some(decimal("10")));

        let all = total(&mut conn, &UsageFilter::default());
        assert_eq!(all.missing_token_usage_messages, 0);
        assert_eq!(all.incomplete_token_usage_messages, 2);
        assert_eq!((all.input_tokens, all.output_tokens), (1_900_000, 100_000));
        assert_eq!(all.unpriced_token_messages, 2);
        assert_eq!(all.unpriced_messages, 2);
        assert_eq!(all.total_cost, decimal("21"));
    }

    /// The same model billed two ways over its life must not be merged into one
    /// group — the subscription half would be priced at the metered half's rates.
    #[test]
    fn the_two_modes_are_grouped_apart() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_model(&mut conn, "m1", "1", "2");
        reply_billed(&mut conn, "a1", "m1", "metered", (1_000_000, 0));
        reply_billed(&mut conn, "b1", "m1", "subscription", (1_000_000, 0));

        let out = report(&mut conn, UsageDimension::Total, &UsageFilter::default()).unwrap();
        assert_eq!(out[0].messages, 2);
        assert_eq!(out[0].input_tokens, 2_000_000, "both rows' tokens are reported");
        assert_eq!(
            out[0].total_cost,
            decimal("1"),
            "but only the metered million is charged for"
        );
    }

    /// The same, filed as an automatic review rather than as an answer.
    fn review(conn: &mut SqliteConnection, id: &str, created_at: i64, input: i32, price: &str) {
        diesel::insert_into(audit_messages::table)
            .values(&AuditMessageInsert {
                id,
                recorded_at: created_at,
                message_id: id,
                conversation_id: "c1",
                turn_id: None,
                source_type: None,
                source_id: None,
                turn_origin: Some("desktop"),
                role: crate::db::ops::audit::AUTO_REVIEW_ROLE,
                content: "",
                sender_id: None,
                sender_name: None,
                provider_id: Some("p1"),
                provider_name: Some("Acme"),
                model_id: Some("cheap"),
                input_tokens: Some(input),
                output_tokens: Some(0),
                cache_read_tokens: Some(0),
                cache_write_tokens: Some(0),
                created_at,
                input_price: Some(decimal(price)),
                output_price: Some(Decimal::zero()),
                cache_read_price: None,
                cache_write_price: None,
                server_tool_calls: None,
                server_tool_price: None,
                billing_mode: "metered",
                self_id: None,
            })
            .execute(conn)
            .unwrap();
    }

    fn total(conn: &mut SqliteConnection, filter: &UsageFilter) -> UsageBucket {
        report(conn, UsageDimension::Total, filter)
            .unwrap()
            .pop()
            .unwrap_or_else(|| UsageBucket::empty(String::new()))
    }

    /// The whole point of snapshotting: a rate that changed mid-window bills
    /// each half at what it was, so the two halves cannot be added at one price.
    #[test]
    fn each_half_of_a_price_change_is_billed_at_its_own_rate() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        // A million input tokens at 10/M, then another million at 20/M.
        reply(
            &mut conn,
            "a",
            "m",
            1_000,
            (1_000_000, 0, 0, 0),
            Some(("10", "0")),
            "desktop",
        );
        reply(
            &mut conn,
            "b",
            "m",
            2_000,
            (1_000_000, 0, 0, 0),
            Some(("20", "0")),
            "desktop",
        );

        let all = total(&mut conn, &UsageFilter::default());
        assert_eq!(all.messages, 2);
        assert_eq!(all.total_cost, decimal("30"), "10 + 20, not 2 x either");
        assert_eq!(all.unpriced_messages, 0);
    }

    /// A model with tiered rates reaches this query as two price sets, and needs
    /// no special handling because of it.
    ///
    /// The tier was resolved when each row was written, from that request's own
    /// prompt — the one place it can be. Here there is only a `SUM` over rows
    /// that were separate requests, so re-deciding would mean inventing a prompt
    /// size, and the report would then disagree with the stop event the user was
    /// already shown.
    #[test]
    fn a_tiered_model_bills_each_side_of_the_threshold_at_its_own_rate() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        // grok-4.6: 100k under the 200k threshold at 2/M, then 250k over it at
        // 4/M — the whole prompt, not the excess.
        reply(
            &mut conn,
            "a",
            "grok-4.6",
            1_000,
            (100_000, 0, 0, 0),
            Some(("2", "6")),
            "desktop",
        );
        reply(
            &mut conn,
            "b",
            "grok-4.6",
            2_000,
            (250_000, 0, 0, 0),
            Some(("4", "12")),
            "desktop",
        );

        let all = total(&mut conn, &UsageFilter::default());
        assert_eq!(all.messages, 2);
        assert_eq!(all.total_cost, decimal("1.2"), "0.2 + 1.0, got {}", all.total_cost);
        // And the model is still one row in the breakdown: the tier is a price,
        // not an identity.
        let by_model = report(&mut conn, UsageDimension::Model, &UsageFilter::default()).unwrap();
        assert_eq!(by_model.len(), 1);
        assert_eq!(by_model[0].input_tokens, 350_000);
    }

    /// Reviews are spend the user did not ask for directly, so a total that
    /// leaves them out is smaller than the truth — the same failure as
    /// reporting unpriced traffic as free.
    #[test]
    fn a_review_counts_towards_the_total() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        reply(
            &mut conn,
            "a",
            "m",
            1_000,
            (1_000_000, 0, 0, 0),
            Some(("10", "0")),
            "desktop",
        );
        review(&mut conn, "r", 1_100, 1_000_000, "2");

        let all = total(&mut conn, &UsageFilter::default());
        assert_eq!(all.messages, 2);
        assert_eq!(all.total_cost, decimal("12"), "the review is part of what was spent");
    }

    /// And they have to be separable, or the total is a number nobody can act
    /// on: turning the reviewer off is only a decision you can make if you can
    /// see what it costs.
    #[test]
    fn the_two_kinds_of_spend_can_be_told_apart() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        reply(
            &mut conn,
            "a",
            "m",
            1_000,
            (1_000_000, 0, 0, 0),
            Some(("10", "0")),
            "desktop",
        );
        review(&mut conn, "r", 1_100, 1_000_000, "2");

        let by_kind = report(&mut conn, UsageDimension::Kind, &UsageFilter::default()).unwrap();
        let of = |key: &str| {
            by_kind
                .iter()
                .find(|bucket| bucket.key == key)
                .map(|bucket| bucket.total_cost.clone())
                .unwrap_or_else(Decimal::zero)
        };
        assert_eq!(of("assistant"), decimal("10"));
        assert_eq!(of("auto_review"), decimal("2"));
    }

    /// A cached token is billed once. The same rule `compute_cost` is tested for
    /// has to survive being reached through a `GROUP BY`.
    #[test]
    fn the_cache_discount_survives_aggregation() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        // 900k of the million prompt tokens came from the cache, and the cache
        // read price is blank — so they bill at the input rate, once.
        reply(
            &mut conn,
            "a",
            "m",
            1_000,
            (1_000_000, 0, 900_000, 0),
            Some(("10", "0")),
            "desktop",
        );

        let all = total(&mut conn, &UsageFilter::default());
        assert_eq!(all.total_cost, decimal("10"), "the prompt is charged once over");
        assert_eq!(all.cache_read_tokens, 900_000);
    }

    /// The public breakdown is accumulated from the same `cost_of` result as
    /// the total, including cache replacement and per-thousand tool pricing.
    #[test]
    fn cost_components_add_up_to_the_reported_total() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        reply(
            &mut conn,
            "a",
            "m",
            1_000,
            (1_000_000, 2_000_000, 200_000, 100_000),
            Some(("10", "30")),
            "desktop",
        );
        diesel::update(audit_messages::table.find("a"))
            .set((
                audit_messages::cache_read_price.eq(Some(decimal("1"))),
                audit_messages::cache_write_price.eq(Some(decimal("12.5"))),
                audit_messages::server_tool_calls.eq(Some(2)),
                audit_messages::server_tool_price.eq(Some(decimal("15"))),
            ))
            .execute(&mut conn)
            .unwrap();

        let all = total(&mut conn, &UsageFilter::default());
        assert_eq!(all.input_cost, decimal("7"));
        assert_eq!(all.output_cost, decimal("60"));
        assert_eq!(all.cache_cost, decimal("1.45"));
        assert_eq!(all.tool_cost, decimal("0.03"));
        assert_eq!(all.total_cost, decimal("68.48"));
        assert_eq!(
            all.total_cost,
            all.input_cost + all.output_cost + all.cache_cost + all.tool_cost
        );
    }

    /// A missing provider-tool rate is not permission to call the tool free.
    /// The known token half remains in the lower-bound total, and only the reply
    /// that actually used the tool is marked as containing unpriced usage.
    #[test]
    fn a_missing_tool_rate_keeps_token_cost_and_marks_only_calling_replies() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        for (id, created_at) in [("with-tool", 1_000), ("tokens-only", 2_000)] {
            reply(
                &mut conn,
                id,
                "m",
                created_at,
                (1_000_000, 0, 0, 0),
                Some(("10", "20")),
                "desktop",
            );
        }
        diesel::update(audit_messages::table.find("with-tool"))
            .set(audit_messages::server_tool_calls.eq(Some(2)))
            .execute(&mut conn)
            .unwrap();
        attach_to_turn(&mut conn, "with-tool", "with-tool");

        let all = total(&mut conn, &UsageFilter::default());
        assert_eq!(all.messages, 2);
        assert_eq!(all.input_cost, decimal("20"), "the known token spend remains");
        assert_eq!(all.tool_cost, Decimal::zero(), "an absent rate is not guessed");
        assert_eq!(all.total_cost, decimal("20"), "the total is the known lower bound");
        assert_eq!(all.unpriced_token_messages, 0);
        assert_eq!(all.unpriced_tool_messages, 1);
        assert_eq!(all.unpriced_messages, 1, "the token-only reply is fully priced");

        let summary = turn_summaries(&mut conn, "c1").unwrap().remove("with-tool").unwrap();
        assert_eq!(summary.pricing_status, TurnPricingStatus::LowerBound);
        assert_eq!(summary.input_cost, Some(decimal("10")));
        assert_eq!(summary.tool_cost, None, "a missing tool rate is not an exact zero");
        assert_eq!(summary.total_cost, Some(decimal("10")));
    }

    /// The inverse partial-price case: a configured per-call rate remains a
    /// known lower bound even while the model's token rates are still blank.
    /// The reply is unpriced once, not once per missing component.
    #[test]
    fn a_known_tool_rate_survives_unknown_token_rates() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        reply(&mut conn, "a", "m", 1_000, (100, 50, 0, 0), None, "desktop");
        diesel::update(audit_messages::table.find("a"))
            .set((
                audit_messages::server_tool_calls.eq(Some(2)),
                audit_messages::server_tool_price.eq(Some(decimal("15"))),
            ))
            .execute(&mut conn)
            .unwrap();
        attach_to_turn(&mut conn, "a", "tool-known");

        let all = total(&mut conn, &UsageFilter::default());
        assert_eq!(all.input_cost, Decimal::zero());
        assert_eq!(all.output_cost, Decimal::zero());
        assert_eq!(all.tool_cost, decimal("0.03"));
        assert_eq!(all.total_cost, decimal("0.03"), "known tool spend is retained");
        assert_eq!(all.unpriced_token_messages, 1);
        assert_eq!(all.unpriced_tool_messages, 0);
        assert_eq!(all.unpriced_messages, 1, "one reply, despite two unknown token rates");

        let summary = turn_summaries(&mut conn, "c1").unwrap().remove("tool-known").unwrap();
        assert_eq!(summary.pricing_status, TurnPricingStatus::LowerBound);
        assert_eq!(summary.input_cost, None);
        assert_eq!(summary.tool_cost, Some(decimal("0.03")));
        assert_eq!(summary.total_cost, Some(decimal("0.03")));
    }

    /// Rows between migrations 30 and 37 can carry historical token rates but
    /// no tool-rate snapshot. A later exact provider/model config is the same
    /// best-available fallback used for legacy token NULLs; an explicit
    /// historical tool rate would still win above it.
    #[test]
    fn a_legacy_null_tool_rate_falls_back_to_the_current_exact_model() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_model(&mut conn, "m", "99", "99");
        diesel::update(model_configs::table.filter(model_configs::model_id.eq("m")))
            .set(model_configs::server_tool_price.eq(Some(decimal("15"))))
            .execute(&mut conn)
            .unwrap();
        reply(
            &mut conn,
            "a",
            "m",
            1_000,
            (1_000_000, 0, 0, 0),
            Some(("10", "20")),
            "desktop",
        );
        diesel::update(audit_messages::table.find("a"))
            .set(audit_messages::server_tool_calls.eq(Some(2)))
            .execute(&mut conn)
            .unwrap();

        let all = total(&mut conn, &UsageFilter::default());
        assert_eq!(all.input_cost, decimal("10"), "historical token rate still wins");
        assert_eq!(
            all.tool_cost,
            decimal("0.03"),
            "legacy tool NULL uses today's exact model"
        );
        assert_eq!(all.estimated_token_messages, 0);
        assert_eq!(all.estimated_tool_messages, 1);
        assert_eq!(all.estimated_messages, 1);
        assert_eq!(all.unpriced_messages, 0);
    }

    /// Traffic on an unpriced model is counted and reported as unpriced. It must
    /// not be dropped — that loses the tokens — and must not be billed at zero
    /// without saying so.
    #[test]
    fn traffic_with_no_price_is_counted_apart_rather_than_billed_at_nothing() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        reply(
            &mut conn,
            "a",
            "priced",
            1_000,
            (100, 200, 0, 0),
            Some(("10", "30")),
            "desktop",
        );
        reply(&mut conn, "b", "unpriced", 2_000, (500, 500, 0, 0), None, "desktop");

        let all = total(&mut conn, &UsageFilter::default());
        assert_eq!(all.messages, 2);
        assert_eq!(all.unpriced_messages, 1);
        assert_eq!(all.input_tokens, 600, "the tokens are still counted");
        assert!(all.total_cost > Decimal::zero(), "the priced half still bills");
    }

    /// A row from before migration 30 has no price on it. Today's configuration
    /// is the only number that exists for it, and reporting the traffic as free
    /// would be a worse answer than a retroactive one.
    #[test]
    fn a_row_recorded_before_prices_were_kept_falls_back_to_the_current_one() {
        use crate::db::models::model_config::ModelConfigInsert;
        use crate::db::models::provider::ProviderInsert;

        let pool = test_db();
        let mut conn = pool.get().unwrap();
        diesel::insert_into(crate::db::schema::providers::table)
            .values(&ProviderInsert {
                id: "p1",
                name: "Acme",
                provider_type: "openai",
                base_url: "https://example.invalid",
                is_enabled: 1,
                sort_order: 0,
                created_at: 0,
                updated_at: 0,
                api_format: "chat",
                catalog_id: None,
                credential_kind: "api_key",
                transport_profile: "standard",
            })
            .execute(&mut conn)
            .unwrap();
        crate::db::ops::model_config::upsert(
            &mut conn,
            &ModelConfigInsert {
                id: "mc1",
                provider_id: "p1",
                model_id: "m",
                display_name: None,
                context_window: 1,
                compact_threshold: 1,
                max_output_tokens: None,
                input_price: Some(decimal("10")),
                output_price: Some(decimal("0")),
                cache_read_price: None,
                cache_write_price: None,
                created_at: 0,
                updated_at: 0,
                capability_overrides: None,
                pricing_tiers: None,
                server_tools: None,
                server_tool_price: None,
            },
        )
        .unwrap();

        reply(&mut conn, "a", "m", 1_000, (1_000_000, 0, 0, 0), None, "desktop");
        attach_to_turn(&mut conn, "a", "legacy-price");

        let all = total(&mut conn, &UsageFilter::default());
        assert_eq!(all.unpriced_messages, 0, "there is a price, just not on the row");
        assert_eq!(all.estimated_token_messages, 1);
        assert_eq!(all.estimated_messages, 1);
        assert_eq!(all.total_cost, decimal("10"));

        let summary = turn_summaries(&mut conn, "c1").unwrap().remove("legacy-price").unwrap();
        assert_eq!(summary.pricing_status, TurnPricingStatus::Estimated);
        assert_eq!(summary.estimated_token_messages, 1);
        assert_eq!(summary.unpriced_messages, 0);
        assert_eq!(summary.total_cost, Some(decimal("10")));
    }

    /// Zero is a configured price, not a placeholder. A historical free request
    /// must stay free even when the model is priced later.
    #[test]
    fn an_explicit_zero_snapshot_never_falls_back_to_a_current_price() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        seed_model(&mut conn, "m", "10", "20");
        reply(
            &mut conn,
            "a",
            "m",
            1_000,
            (1_000_000, 100_000, 0, 0),
            Some(("0", "0")),
            "desktop",
        );

        let all = total(&mut conn, &UsageFilter::default());
        assert_eq!(all.unpriced_messages, 0);
        assert_eq!(all.estimated_token_messages, 0);
        assert_eq!(all.estimated_messages, 0);
        assert_eq!(all.total_cost, Decimal::zero());
    }

    /// A zero rate is explicitly free. Only NULL means the price is unknown.
    #[test]
    fn an_explicit_zero_price_reads_as_free() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        reply(
            &mut conn,
            "a",
            "m",
            1_000,
            (100, 200, 0, 0),
            Some(("0", "0")),
            "desktop",
        );

        let all = total(&mut conn, &UsageFilter::default());
        assert_eq!(all.unpriced_messages, 0);
        assert_eq!(all.total_cost, Decimal::zero());
    }

    /// The window is half-open, so two adjacent reports over adjacent windows
    /// count every reply exactly once.
    #[test]
    fn the_window_excludes_its_upper_bound() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        reply(&mut conn, "a", "m", 1_000, (10, 0, 0, 0), Some(("1", "1")), "desktop");
        reply(&mut conn, "b", "m", 2_000, (10, 0, 0, 0), Some(("1", "1")), "desktop");

        let first = UsageFilter {
            until_ms: Some(2_000),
            ..Default::default()
        };
        let second = UsageFilter {
            since_ms: Some(2_000),
            ..Default::default()
        };
        assert_eq!(total(&mut conn, &first).messages, 1);
        assert_eq!(total(&mut conn, &second).messages, 1);
    }

    #[test]
    fn origin_separates_bot_traffic_from_the_desktop() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        reply(&mut conn, "a", "m", 1_000, (10, 0, 0, 0), Some(("1", "1")), "desktop");
        reply(&mut conn, "b", "m", 2_000, (20, 0, 0, 0), Some(("1", "1")), "onebot");
        reply(&mut conn, "c", "m", 3_000, (30, 0, 0, 0), Some(("1", "1")), "onebot");

        let bots = UsageFilter {
            origin: Some(TurnOrigin::OneBot),
            ..Default::default()
        };
        assert_eq!(total(&mut conn, &bots).messages, 2);
        assert_eq!(total(&mut conn, &bots).input_tokens, 50);
    }

    /// Every breakdown has to add up to the headline it sits under. They are the
    /// same query with a different key for exactly this reason.
    #[test]
    fn a_breakdown_adds_up_to_the_total_above_it() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        reply(
            &mut conn,
            "a",
            "big",
            1_000,
            (1_000, 500, 0, 0),
            Some(("10", "30")),
            "desktop",
        );
        reply(
            &mut conn,
            "b",
            "small",
            2_000,
            (300, 100, 0, 0),
            Some(("2", "6")),
            "onebot",
        );
        reply(
            &mut conn,
            "c",
            "big",
            3_000,
            (700, 200, 0, 0),
            Some(("10", "30")),
            "onebot",
        );

        let all = total(&mut conn, &UsageFilter::default());
        let by_model = report(&mut conn, UsageDimension::Model, &UsageFilter::default()).unwrap();

        assert_eq!(by_model.len(), 2);
        assert_eq!(by_model.iter().map(|b| b.messages).sum::<i64>(), all.messages);
        let summed = by_model
            .iter()
            .fold(Decimal::zero(), |sum, bucket| sum + bucket.total_cost.clone());
        assert_eq!(summed, all.total_cost);
        assert_eq!(by_model[0].key, "big", "the expensive one comes first");
    }

    /// A conversation that has been deleted still owes its share of the bill,
    /// and says so by having no title rather than by disappearing.
    #[test]
    fn a_deleted_conversation_keeps_its_row_and_loses_its_name() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        crate::db::ops::conversation::create_conversation(&mut conn, "c1", Some("Named"), None, None, 1).unwrap();
        reply(&mut conn, "a", "m", 1_000, (100, 0, 0, 0), Some(("10", "0")), "desktop");

        let named = report(&mut conn, UsageDimension::Conversation, &UsageFilter::default()).unwrap();
        assert_eq!(named[0].label.as_deref(), Some("Named"));

        crate::db::ops::conversation::delete_conversation(&mut conn, "c1").unwrap();

        let orphaned = report(&mut conn, UsageDimension::Conversation, &UsageFilter::default()).unwrap();
        assert_eq!(orphaned.len(), 1, "the cost outlives the transcript");
        assert_eq!(orphaned[0].key, "c1");
        assert_eq!(orphaned[0].label, None);
    }

    #[test]
    fn an_untitled_conversation_stays_distinct_from_a_deleted_one() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        crate::db::ops::conversation::create_conversation(&mut conn, "c1", None, None, None, 1).unwrap();
        reply(&mut conn, "a", "m", 1_000, (100, 0, 0, 0), Some(("10", "0")), "desktop");

        let existing = report(&mut conn, UsageDimension::Conversation, &UsageFilter::default()).unwrap();
        assert_eq!(
            existing[0].label.as_deref(),
            Some(""),
            "empty title still means the row exists"
        );

        crate::db::ops::conversation::delete_conversation(&mut conn, "c1").unwrap();
        let deleted = report(&mut conn, UsageDimension::Conversation, &UsageFilter::default()).unwrap();
        assert_eq!(deleted[0].label, None, "only a missing row is non-navigable");
    }

    /// A private chat and a group can carry the same number. Keying on the id
    /// alone would add two different places together.
    #[test]
    fn a_source_key_carries_its_kind() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        reply(&mut conn, "a", "m", 1_000, (100, 0, 0, 0), Some(("10", "0")), "onebot");

        let sources = report(&mut conn, UsageDimension::Source, &UsageFilter::default()).unwrap();
        assert_eq!(sources[0].key, "onebot_group:900");
    }

    #[test]
    fn a_series_reads_forwards_and_splits_on_the_local_day() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        // Two days apart, so no timezone puts them in the same bucket.
        reply(
            &mut conn,
            "a",
            "m",
            1_700_000_000_000,
            (10, 0, 0, 0),
            Some(("1", "1")),
            "desktop",
        );
        reply(
            &mut conn,
            "b",
            "m",
            1_700_180_000_000,
            (20, 0, 0, 0),
            Some(("1", "1")),
            "desktop",
        );

        let days = report(&mut conn, UsageDimension::Day, &UsageFilter::default()).unwrap();
        assert_eq!(days.len(), 2);
        assert!(days[0].key < days[1].key, "oldest first");
        assert_eq!(days[0].input_tokens, 10);
    }
}

#[cfg(test)]
mod decimal_tests {
    use super::*;
    use crate::db::models::audit::AuditMessageInsert;
    use crate::db::schema::audit_messages;
    use crate::db::test_db;

    fn decimal(raw: &str) -> Decimal {
        raw.parse().unwrap()
    }

    fn insert_reply(conn: &mut SqliteConnection, id: &str, billing_mode: &str) -> QueryResult<()> {
        diesel::insert_into(audit_messages::table)
            .values(&AuditMessageInsert {
                id,
                recorded_at: 1,
                message_id: id,
                conversation_id: "conversation",
                turn_id: Some("turn"),
                source_type: None,
                source_id: None,
                turn_origin: Some("desktop"),
                role: "assistant",
                content: "answer",
                sender_id: None,
                sender_name: None,
                provider_id: Some("provider"),
                provider_name: Some("Provider"),
                model_id: Some("model"),
                input_tokens: Some(1_000_000),
                output_tokens: Some(1_000_000),
                cache_read_tokens: Some(0),
                cache_write_tokens: Some(0),
                created_at: 1,
                input_price: Some(decimal("2.123456789012345678")),
                output_price: Some(decimal("6.000000000000000001")),
                cache_read_price: None,
                cache_write_price: None,
                self_id: None,
                server_tool_calls: Some(1),
                server_tool_price: Some(decimal("5")),
                billing_mode,
            })
            .execute(conn)?;
        Ok(())
    }

    #[test]
    fn report_preserves_full_decimal_precision_and_serializes_strings() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        insert_reply(&mut conn, "reply", "metered").unwrap();

        let buckets = report(&mut conn, UsageDimension::Total, &UsageFilter::default()).unwrap();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].input_cost, decimal("2.123456789012345678"));
        assert_eq!(buckets[0].output_cost, decimal("6.000000000000000001"));
        assert_eq!(buckets[0].tool_cost, decimal("0.005"));
        assert_eq!(buckets[0].total_cost, decimal("8.128456789012345679"));

        let json = serde_json::to_value(&buckets[0]).unwrap();
        assert_eq!(json["total_cost"], "8.128456789012345679");
        assert!(json["total_cost"].is_string());
    }

    #[test]
    fn database_rejects_unknown_billing_modes() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        assert!(insert_reply(&mut conn, "bad", "future_mode").is_err());
    }
}

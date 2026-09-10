//! What one conversation has cost, for an agent that wants to know.
//!
//! **The conversation is a constructor parameter, not an argument**, which is
//! the whole of this tool's security story. `db::ops::usage::report` reads the
//! entire ledger — every conversation, every provider, back to the first row —
//! and the only thing that narrows it is [`UsageFilter::conversation_id`].
//! Taking that from the model would make the scope a suggestion: it could ask
//! about somebody else's work by naming it, and it would not even have to guess
//! an id, since `UsageDimension::Conversation` hands them out.
//!
//! So the id is baked in where the tool is built, exactly as
//! [`super::app_logs::ReadAppLogsTool`] bakes in the log directory and for the
//! same stated reason: a tool that takes no path has nothing to traverse. The
//! schema below has no field naming a conversation, a project or a user, and
//! there is nothing to validate at execution time because there is nothing the
//! model can say.
//!
//! **Not in the registry.** This is built by [`crate::acp::bridge`] and nothing
//! else. Registering it would put a spending report in every native turn's tool
//! array — a change to what the desktop offers, made in passing by a feature
//! about hosting somebody else's agent — and it would have to be unscoped there
//! to be useful, which is the thing this module exists not to be.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Permission, Tool, ToolContext};
use crate::db::ops::usage::{UsageBucket, UsageDimension, UsageFilter, report};

/// How far back a request may look, in days.
///
/// Not a security boundary — the scope is — but a conversation that has run for
/// months would otherwise render a row per day for all of them.
const MAX_DAYS: i64 = 90;
const DEFAULT_DAYS: i64 = 30;
/// Rows rendered. A breakdown longer than this is not being read.
const MAX_ROWS: usize = 20;

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum UsageGroupBy {
    #[default]
    Total,
    Model,
    Day,
    Kind,
}

impl UsageGroupBy {
    fn dimension(self) -> UsageDimension {
        match self {
            Self::Total => UsageDimension::Total,
            Self::Model => UsageDimension::Model,
            Self::Day => UsageDimension::Day,
            Self::Kind => UsageDimension::Kind,
        }
    }
}

fn default_days() -> i64 {
    DEFAULT_DAYS
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConversationUsageRequest {
    #[serde(default)]
    group_by: UsageGroupBy,
    #[serde(default = "default_days")]
    days: i64,
}

fn parse_request(args: Value) -> Result<(UsageDimension, i64), String> {
    let request: ConversationUsageRequest =
        serde_json::from_value(args).map_err(|e| format!("invalid conversation_usage arguments: {e}"))?;
    if !(1..=MAX_DAYS).contains(&request.days) {
        return Err(format!("conversation_usage days must be between 1 and {MAX_DAYS}"));
    }
    Ok((request.group_by.dimension(), request.days))
}

pub struct ConversationUsageTool {
    conversation_id: String,
}

impl ConversationUsageTool {
    pub fn new(conversation_id: String) -> Self {
        Self { conversation_id }
    }
}

#[async_trait]
impl Tool for ConversationUsageTool {
    fn name(&self) -> &str {
        "conversation_usage"
    }

    fn description(&self) -> &str {
        "Report the tokens and cost recorded for THIS conversation. Covers only the current \
         conversation — there is no way to ask about another one, or about the application as a \
         whole. Figures come from the billing ledger, which keeps a request's price as it was when \
         the request was made."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "group_by": {
                    "type": "string",
                    "enum": ["total", "model", "day", "kind"],
                    "description": "How to break the figures down. 'total' is one line and is the \
                                    default. 'kind' separates answering the user from the requests \
                                    the app makes on its own behalf, such as summarising."
                },
                "days": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_DAYS,
                    "description": "How far back to look. Defaults to 30."
                }
            },
            "required": []
        })
    }

    /// Read-only and confined to the conversation it is already part of, so
    /// there is nothing for a person to weigh.
    fn default_permission(&self) -> Permission {
        Permission::Always
    }

    fn supports_parallel(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, context: &ToolContext) -> Result<String, String> {
        let (dimension, days) = parse_request(args)?;

        // `self`, never `context.conversation_id` — which would look equivalent
        // and is not. The bridge builds this tool once per session, while the
        // context is rebuilt for each call out of whatever turn is running, so
        // reading the scope from there would make it depend on the moment the
        // call happened to arrive. The context is consulted for the pool alone.
        let filter = UsageFilter {
            since_ms: Some(crate::util::now_ms() - days * 86_400_000),
            conversation_id: Some(self.conversation_id.clone()),
            ..Default::default()
        };

        let pool = context
            .db_pool
            .as_ref()
            .ok_or("Usage is unavailable: no database handle")?
            .clone();
        let buckets = tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| e.to_string())?;
            report(&mut conn, dimension, &filter).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())??;

        Ok(render(&buckets, dimension, days))
    }
}

/// Match the UI's cost precision without ever turning a real positive charge
/// into a printed zero. Prices have no stored currency, so this formats only
/// the amount.
fn format_cost(value: &crate::decimal::Decimal) -> String {
    value.to_string()
}

fn render(buckets: &[UsageBucket], dimension: UsageDimension, days: i64) -> String {
    if buckets.is_empty() {
        return format!("No recorded usage for this conversation in the last {days} days.");
    }

    let mut out = format!("Usage for this conversation, last {days} days:\n");
    for bucket in buckets.iter().take(MAX_ROWS) {
        let name = bucket.label.as_deref().unwrap_or(&bucket.key);
        let label = if dimension == UsageDimension::Total {
            "total".to_string()
        } else {
            name.to_string()
        };
        out.push_str(&format!("- {label}: {} replies, ", bucket.messages));
        if bucket.messages > 0 && bucket.missing_token_usage_messages >= bucket.messages {
            out.push_str("token usage unavailable");
        } else {
            if bucket.incomplete_token_usage_messages > 0 {
                out.push_str("at least ");
            }
            out.push_str(&format!(
                "{} in / {} out tokens",
                bucket.input_tokens, bucket.output_tokens
            ));
            if bucket.cache_read_tokens > 0 || bucket.cache_write_tokens > 0 {
                out.push_str(&format!(
                    " ({} cached read, {} cached write)",
                    bucket.cache_read_tokens, bucket.cache_write_tokens
                ));
            }
            if bucket.incomplete_token_usage_messages > 0 {
                let noun = if bucket.incomplete_token_usage_messages == 1 {
                    "reply"
                } else {
                    "replies"
                };
                out.push_str(&format!(
                    "; {} {noun} did not report complete token usage",
                    bucket.incomplete_token_usage_messages
                ));
            }
        }
        match (
            bucket.metered_messages,
            bucket.subscription_messages,
            bucket.external_messages,
        ) {
            (0, subscription, 0) if subscription > 0 => out.push_str(", cost covered by subscription\n"),
            (0, 0, external) if external > 0 => out.push_str(", cost settled externally\n"),
            (0, _, _) => out.push_str(", local cost unavailable\n"),
            (_, 0, 0) => out.push_str(&format!(", cost {}\n", format_cost(&bucket.total_cost))),
            _ => out.push_str(&format!(", locally metered cost {}\n", format_cost(&bucket.total_cost))),
        }
    }

    if buckets.len() > MAX_ROWS {
        out.push_str(&format!("({} more rows omitted.)\n", buckets.len() - MAX_ROWS));
    }

    // Never dropped, and said as a sentence rather than a number in a column.
    // A missing component makes a snapshotted amount a lower bound, while a
    // legacy row priced at today's rate is an estimate that can move either
    // way. Keep those two uncertainties distinct, including when both occur.
    let unpriced: i64 = buckets.iter().map(|b| b.unpriced_messages).sum();
    let estimated: i64 = buckets.iter().map(|b| b.estimated_messages).sum();
    match (estimated, unpriced) {
        (0, 0) => {}
        (0, unpriced) => out.push_str(&format!(
            "\nNote: {unpriced} of these replies have incomplete usage or pricing, so the cost \
             above is a lower bound.\n"
        )),
        (estimated, 0) => out.push_str(&format!(
            "\nNote: {estimated} of these replies use current provider/model prices because \
             their historical rates were not recorded. The cost above is an estimate, not an \
             exact historical total.\n"
        )),
        (estimated, unpriced) => out.push_str(&format!(
            "\nNote: {estimated} of these replies use current provider/model prices because \
             their historical rates were not recorded, and {unpriced} have incomplete usage or \
             pricing. The cost above is a partial estimate, not an exact total.\n"
        )),
    }
    let subscription: i64 = buckets.iter().map(|b| b.subscription_messages).sum();
    let external: i64 = buckets.iter().map(|b| b.external_messages).sum();
    if subscription > 0 || external > 0 {
        out.push_str(&format!(
            "\nNote: {subscription} replies were covered by subscriptions and {external} were settled externally; those costs are not included in Meridian's locally metered amount.\n"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_closed_and_do_not_coerce_or_clamp() {
        assert_eq!(parse_request(json!({})).unwrap(), (UsageDimension::Total, DEFAULT_DAYS));
        assert_eq!(
            parse_request(json!({ "group_by": "model", "days": 7 })).unwrap(),
            (UsageDimension::Model, 7)
        );
        assert!(parse_request(json!({ "group_by": "future" })).is_err());
        assert!(parse_request(json!({ "group_by": 1 })).is_err());
        assert!(parse_request(json!({ "days": 0 })).is_err());
        assert!(parse_request(json!({ "days": MAX_DAYS + 1 })).is_err());
        assert!(parse_request(json!({ "extra": true })).is_err());
    }

    fn bucket(key: &str, messages: i64, total_cost: &str, unpriced: i64) -> UsageBucket {
        let total_cost: crate::decimal::Decimal = total_cost.parse().unwrap();
        UsageBucket {
            key: key.into(),
            label: None,
            messages,
            metered_messages: messages,
            subscription_messages: 0,
            external_messages: 0,
            missing_token_usage_messages: 0,
            incomplete_token_usage_messages: 0,
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            input_cost: total_cost.clone(),
            output_cost: crate::decimal::Decimal::zero(),
            cache_cost: crate::decimal::Decimal::zero(),
            tool_cost: crate::decimal::Decimal::zero(),
            total_cost,
            unpriced_token_messages: unpriced,
            unpriced_tool_messages: 0,
            estimated_token_messages: 0,
            estimated_tool_messages: 0,
            estimated_messages: 0,
            unpriced_messages: unpriced,
        }
    }

    /// The scope is not expressible, which is the point of the whole module.
    /// A field naming a conversation, a project or an id would be a way to ask
    /// about somebody else's work.
    #[test]
    fn the_schema_offers_no_way_to_name_another_conversation() {
        let schema = ConversationUsageTool::new("c-1".into()).parameters_schema();
        let properties = schema.get("properties").and_then(Value::as_object).unwrap();
        assert_eq!(properties.len(), 2, "an unexpected parameter appeared: {properties:?}");
        for name in properties.keys() {
            assert!(
                !name.contains("conversation") && !name.contains("project") && !name.contains("id"),
                "`{name}` lets the model choose a scope"
            );
        }
    }

    #[test]
    fn an_empty_report_says_so_rather_than_erroring() {
        let out = render(&[], UsageDimension::Total, 30);
        assert!(out.contains("No recorded usage"), "{out}");
    }

    /// The one thing `UsageBucket` asks every caller to do.
    #[test]
    fn unpriced_replies_are_reported_beside_the_total() {
        let out = render(&[bucket("total", 5, "1.25", 2)], UsageDimension::Total, 30);
        assert!(out.contains("cost 1.25"), "{out}");
        assert!(out.contains("2 of these replies"), "{out}");
        assert!(out.contains("is a lower bound"), "{out}");
    }

    #[test]
    fn a_fully_priced_report_carries_no_warning() {
        let out = render(&[bucket("total", 5, "1.25", 0)], UsageDimension::Total, 30);
        assert!(!out.contains("incomplete usage or pricing"), "{out}");
        assert!(!out.contains("estimate"), "{out}");
    }

    #[test]
    fn a_current_price_fallback_is_called_an_estimate_not_a_lower_bound() {
        let mut row = bucket("total", 5, "1.25", 0);
        row.estimated_token_messages = 2;
        row.estimated_messages = 2;
        let out = render(&[row], UsageDimension::Total, 30);
        assert!(
            out.contains("2 of these replies use current provider/model prices"),
            "{out}"
        );
        assert!(out.contains("is an estimate"), "{out}");
        assert!(!out.contains("lower bound"), "{out}");
    }

    #[test]
    fn estimated_and_unpriced_traffic_is_called_a_partial_estimate() {
        let mut row = bucket("total", 5, "1.25", 1);
        row.estimated_tool_messages = 2;
        row.estimated_messages = 2;
        let out = render(&[row], UsageDimension::Total, 30);
        assert!(
            out.contains("2 of these replies use current provider/model prices"),
            "{out}"
        );
        assert!(out.contains("1 have incomplete usage or pricing"), "{out}");
        assert!(out.contains("partial estimate"), "{out}");
        assert!(!out.contains("lower bound"), "{out}");
    }

    #[test]
    fn a_sub_micro_cost_is_never_rendered_as_zero() {
        let out = render(&[bucket("total", 1, "0.0000004", 0)], UsageDimension::Total, 30);
        assert!(out.contains("cost 0.0000004"), "{out}");
        assert!(!out.lines().any(|line| line.trim_end().ends_with("cost 0")), "{out}");
    }

    #[test]
    fn externally_settled_usage_is_not_rendered_as_a_free_local_request() {
        let mut external = bucket("total", 2, "0", 0);
        external.metered_messages = 0;
        external.external_messages = 2;

        let out = render(&[external], UsageDimension::Total, 30);
        assert!(out.contains("cost settled externally"), "{out}");
        assert!(!out.contains("cost 0.00"), "{out}");
        assert!(out.contains("2 were settled externally"), "{out}");
    }

    #[test]
    fn missing_token_usage_is_not_rendered_as_zero() {
        let mut unknown = bucket("total", 2, "0", 0);
        unknown.input_tokens = 0;
        unknown.output_tokens = 0;
        unknown.missing_token_usage_messages = 2;
        let out = render(&[unknown], UsageDimension::Total, 30);
        assert!(out.contains("token usage unavailable"), "{out}");
        assert!(!out.contains("0 in / 0 out tokens"), "{out}");

        let mut partial = bucket("total", 2, "0.1", 0);
        partial.missing_token_usage_messages = 1;
        partial.incomplete_token_usage_messages = 1;
        let out = render(&[partial], UsageDimension::Total, 30);
        assert!(out.contains("at least 100 in / 50 out tokens"), "{out}");
        assert!(out.contains("1 reply did not report complete token usage"), "{out}");
    }

    #[test]
    fn a_long_breakdown_is_cut_and_says_how_much_it_dropped() {
        let rows: Vec<UsageBucket> = (0..MAX_ROWS + 5)
            .map(|i| bucket(&format!("m{i}"), 1, "0.1", 0))
            .collect();
        let out = render(&rows, UsageDimension::Model, 30);
        assert!(out.contains("5 more rows omitted"), "{out}");
    }
}

//! Noticing that this hour costs far more than the last week did.
//!
//! The judgement is a ratio against the install's own history rather than a
//! budget somebody had to think of in advance — a runaway loop is recognisable
//! precisely because it does not look like the weeks before it.
//!
//! **Nothing here divides.** The rule is "the recent window cost more than K
//! times the baseline average", and written that way it needs `B / N`. Written
//! as `R × N > K × B` it needs only multiplication, which `Decimal` does
//! exactly — and money in this app is `Decimal` specifically so that no
//! decision rests on a rounded value.

use crate::db::ops::usage::{UsageBucket, UsageDimension, UsageFilter, report};
use crate::decimal::Decimal;

use super::alert::{Alert, AlertDetail, UsageAlert, UsageSlice};
use crate::db::models::notification::NotificationEventKind;

pub const USAGE_ALERT_KEY: &str = "usage_surge";

/// How many lines of "what was it spent on" ride along with the alert.
///
/// A runaway is almost always one conversation, and a total with no breakdown
/// is a number nobody can act on. Three is enough to name the culprit without
/// turning a chat notification into a report.
const TOP_SLICES: usize = 3;

const HOUR_MS: i64 = 60 * 60 * 1000;
const DAY_MS: i64 = 24 * HOUR_MS;

#[derive(Debug, Clone)]
pub struct SurgeThresholds {
    pub window_hours: u32,
    pub baseline_days: u32,
    pub multiplier: Decimal,
    /// The floor, and the only thing standing between a ratio rule and constant
    /// false alarms: thirty times nothing is still nothing. A window under this
    /// is never a surge however far above its baseline it lands.
    pub min_cost: Decimal,
}

/// One window's worth of the ledger, reduced to what the judgement needs.
///
/// Scalars rather than a `UsageBucket` so the rule can be tested without a
/// database, and so it is obvious that exactly five numbers decide this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowSample {
    pub cost: Decimal,
    pub messages: i64,
    pub metered_messages: i64,
    pub unpriced_messages: i64,
    pub estimated_messages: i64,
}

impl WindowSample {
    pub fn zero() -> Self {
        Self {
            cost: Decimal::zero(),
            messages: 0,
            metered_messages: 0,
            unpriced_messages: 0,
            estimated_messages: 0,
        }
    }

    /// A `Total` report is one bucket, or none at all when the window is empty.
    fn from_total(buckets: Vec<UsageBucket>) -> Self {
        match buckets.into_iter().next() {
            None => Self::zero(),
            Some(bucket) => Self {
                cost: bucket.total_cost,
                messages: bucket.messages,
                metered_messages: bucket.metered_messages,
                unpriced_messages: bucket.unpriced_messages,
                estimated_messages: bucket.estimated_messages,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurgeVerdict {
    Surge {
        /// The recent window contains replies nobody has a price for, so its
        /// cost is at least this much. Reported rather than suppressed: a lower
        /// bound that already crosses the line is a real crossing.
        is_lower_bound: bool,
    },
    /// Not a surge, with the reason. Carried rather than collapsed to a bool
    /// because "the baseline is unusable" and "spending is normal" are opposite
    /// situations that both produce silence.
    NoAlert(NoAlertReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoAlertReason {
    /// The ledger does not reach back across the whole baseline span, so the
    /// baseline is an artefact of when the app was installed.
    LedgerTooShort,
    NoBaselineTraffic,
    /// The baseline span has traffic but none of it was billed to this install
    /// — a subscription or a hosted agent. Zero metered spend is an absence of
    /// a baseline, not a baseline of zero.
    NoMeteredBaseline,
    /// Some of the baseline has no price on file, so the baseline figure is a
    /// lower bound. An understated baseline inflates the ratio, which is the
    /// direction that invents surges that never happened.
    BaselinePricingIncomplete,
    BaselineShorterThanWindow,
    BelowFloor,
    WithinBaseline,
}

impl NoAlertReason {
    pub fn as_str(self) -> &'static str {
        match self {
            NoAlertReason::LedgerTooShort => "the ledger does not cover the baseline span",
            NoAlertReason::NoBaselineTraffic => "the baseline span has no traffic",
            NoAlertReason::NoMeteredBaseline => "the baseline span has no metered spend",
            NoAlertReason::BaselinePricingIncomplete => "the baseline span is not fully priced",
            NoAlertReason::BaselineShorterThanWindow => "the baseline span is shorter than one window",
            NoAlertReason::BelowFloor => "the window is under the configured floor",
            NoAlertReason::WithinBaseline => "spending is within the baseline",
        }
    }
}

/// The rule, as a pure function.
///
/// The refusals come first and in this order because each of them makes the
/// comparison below meaningless rather than merely negative.
pub fn evaluate(
    recent: &WindowSample,
    baseline: &WindowSample,
    baseline_windows: i64,
    ledger_covers_baseline: bool,
    thresholds: &SurgeThresholds,
) -> SurgeVerdict {
    if !ledger_covers_baseline {
        return SurgeVerdict::NoAlert(NoAlertReason::LedgerTooShort);
    }
    if baseline_windows <= 0 {
        return SurgeVerdict::NoAlert(NoAlertReason::BaselineShorterThanWindow);
    }
    if baseline.messages == 0 {
        return SurgeVerdict::NoAlert(NoAlertReason::NoBaselineTraffic);
    }
    if baseline.metered_messages == 0 {
        return SurgeVerdict::NoAlert(NoAlertReason::NoMeteredBaseline);
    }
    if baseline.unpriced_messages > 0 || baseline.estimated_messages > 0 {
        return SurgeVerdict::NoAlert(NoAlertReason::BaselinePricingIncomplete);
    }
    if recent.cost < thresholds.min_cost {
        return SurgeVerdict::NoAlert(NoAlertReason::BelowFloor);
    }

    // R × N > K × B. Strictly greater: landing exactly on the multiple is the
    // threshold being met, not exceeded, and a rule that fires there would fire
    // on a perfectly steady install whose K happens to be 1.
    let scaled_recent = recent.cost.clone() * Decimal::from(baseline_windows);
    let scaled_baseline = thresholds.multiplier.clone() * baseline.cost.clone();
    if scaled_recent > scaled_baseline {
        SurgeVerdict::Surge {
            is_lower_bound: recent.unpriced_messages > 0,
        }
    } else {
        SurgeVerdict::NoAlert(NoAlertReason::WithinBaseline)
    }
}

/// How many whole windows fit in the baseline span.
///
/// Integer division on a duration, not on money. Truncating downwards makes the
/// baseline average slightly *larger* and so the rule slightly quieter, which
/// is the right direction for the error to fall.
pub fn baseline_window_count(window_hours: u32, baseline_days: u32) -> i64 {
    if window_hours == 0 {
        return 0;
    }
    (i64::from(baseline_days) * 24) / i64::from(window_hours)
}

/// Read both windows out of the ledger and judge them.
///
/// Blocking: every call inside is a database read. Callers run it on a blocking
/// thread.
pub fn collect(
    conn: &mut diesel::SqliteConnection,
    thresholds: &SurgeThresholds,
    now: i64,
) -> Result<Option<Alert>, String> {
    let window_ms = i64::from(thresholds.window_hours) * HOUR_MS;
    let baseline_ms = i64::from(thresholds.baseline_days) * DAY_MS;
    let window_start = now - window_ms;
    let baseline_start = window_start - baseline_ms;

    let recent_filter = UsageFilter {
        since_ms: Some(window_start),
        until_ms: Some(now),
        ..Default::default()
    };
    // The baseline stops where the measured window begins. Overlapping them
    // lets the spike being judged raise the very baseline it is judged against,
    // which is worst exactly when the spike is largest.
    let baseline_filter = UsageFilter {
        since_ms: Some(baseline_start),
        until_ms: Some(window_start),
        ..Default::default()
    };

    let recent = WindowSample::from_total(report(conn, UsageDimension::Total, &recent_filter).map_err(err)?);
    let baseline = WindowSample::from_total(report(conn, UsageDimension::Total, &baseline_filter).map_err(err)?);

    let oldest = crate::db::ops::usage::oldest_audit_created_at(conn).map_err(err)?;
    let covers = oldest.is_some_and(|oldest| oldest <= baseline_start);

    let windows = baseline_window_count(thresholds.window_hours, thresholds.baseline_days);
    let is_lower_bound = match evaluate(&recent, &baseline, windows, covers, thresholds) {
        SurgeVerdict::Surge { is_lower_bound } => is_lower_bound,
        SurgeVerdict::NoAlert(reason) => {
            tracing::debug!(reason = reason.as_str(), "no usage surge");
            return Ok(None);
        }
    };

    let top_providers = slices(conn, UsageDimension::Provider, &recent_filter)?;
    let top_conversations = slices(conn, UsageDimension::Conversation, &recent_filter)?;

    let detail = UsageAlert {
        window_hours: thresholds.window_hours,
        baseline_days: thresholds.baseline_days,
        multiplier: thresholds.multiplier.clone(),
        window_start_ms: window_start,
        window_cost: recent.cost.clone(),
        baseline_cost: baseline.cost.clone(),
        baseline_windows: windows,
        is_lower_bound,
        top_providers,
        top_conversations,
    };
    Ok(Some(Alert {
        event: NotificationEventKind::UsageSurge,
        raised_at: now,
        alert_key: USAGE_ALERT_KEY.to_string(),
        title: "用量异常增长".to_string(),
        summary: summary(&detail),
        detail: AlertDetail::Usage(detail),
    }))
}

fn slices(
    conn: &mut diesel::SqliteConnection,
    dimension: UsageDimension,
    filter: &UsageFilter,
) -> Result<Vec<UsageSlice>, String> {
    // `report` already sorts biggest-first for a non-series dimension.
    Ok(report(conn, dimension, filter)
        .map_err(err)?
        .into_iter()
        .take(TOP_SLICES)
        .map(|bucket| UsageSlice {
            key: bucket.key,
            label: bucket.label,
            cost: bucket.total_cost,
            messages: bucket.messages,
        })
        .collect())
}

fn err(error: diesel::result::Error) -> String {
    format!("could not read the usage ledger: {error}")
}

/// The sentence a person reads.
///
/// It states the baseline as a total and a window count rather than as an
/// average, for the reason at the top of this file: the average does not exist
/// anywhere in this code, and printing one would mean computing a number the
/// decision never used.
fn summary(alert: &UsageAlert) -> String {
    let mut text = format!(
        "最近 {} 小时花费 {}，超过基线的 {} 倍（过去 {} 天共 {}，折合 {} 个同长度窗口）。",
        alert.window_hours,
        alert.window_cost,
        alert.multiplier,
        alert.baseline_days,
        alert.baseline_cost,
        alert.baseline_windows,
    );
    if alert.is_lower_bound {
        text.push_str("\n窗口内有未定价的请求，实际花费不低于这个数。");
    }
    if let Some(top) = alert.top_conversations.first() {
        let name = top.label.as_deref().unwrap_or("(已删除的会话)");
        text.push_str(&format!("\n花费最多的会话：{name}（{}）", top.cost));
    }
    if let Some(top) = alert.top_providers.first() {
        text.push_str(&format!("\n花费最多的供应商：{}（{}）", top.key, top.cost));
    }
    text
}

#[cfg(test)]
mod ledger_tests {
    //! `collect` against a real ledger. The pure rule is tested below; what
    //! needs a database is the pair of windows it is handed, because that is
    //! where the boundary between them is decided.

    use super::*;
    use crate::db::models::audit::AuditMessageInsert;
    use crate::db::schema::audit_messages;
    use crate::db::test_db;
    use diesel::RunQueryDsl;

    const HOUR: i64 = HOUR_MS;
    const NOW: i64 = 1_700_000_000_000;

    fn thresholds() -> SurgeThresholds {
        SurgeThresholds {
            window_hours: 1,
            baseline_days: 7,
            multiplier: "3".parse().unwrap(),
            min_cost: "0".parse().unwrap(),
        }
    }

    /// One priced reply, straight into the table: these tests need control over
    /// the timestamp, which a real turn does not offer.
    fn reply(conn: &mut diesel::SqliteConnection, id: &str, created_at: i64, output_tokens: i32) {
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
                role: "assistant",
                content: "",
                sender_id: None,
                sender_name: None,
                provider_id: Some("p1"),
                provider_name: Some("Acme"),
                model_id: Some("m1"),
                input_tokens: Some(0),
                output_tokens: Some(output_tokens),
                cache_read_tokens: Some(0),
                cache_write_tokens: Some(0),
                created_at,
                // A price on the row itself, so nothing falls back to today's
                // configuration and the figures are exactly these tokens.
                input_price: Some("0".parse().unwrap()),
                output_price: Some("1".parse().unwrap()),
                cache_read_price: Some("0".parse().unwrap()),
                cache_write_price: Some("0".parse().unwrap()),
                server_tool_calls: None,
                server_tool_price: None,
                billing_mode: "metered",
                self_id: None,
                response_model_id: None,
            })
            .execute(conn)
            .unwrap();
    }

    /// A thousand output tokens an hour, for as many hours back as asked.
    ///
    /// The rate above is per *million* tokens, so a quiet hour costs 0.001 and
    /// the 168-hour baseline comes to 0.168.
    fn quiet_hours(conn: &mut diesel::SqliteConnection, hours: i64) {
        for hour in 1..=hours {
            reply(conn, &format!("old-{hour}"), NOW - hour * HOUR, 1_000);
        }
    }

    /// The measured window must not raise the baseline it is measured against.
    ///
    /// Overlapped — the baseline running to `now` rather than to the start of
    /// the window — the spike lands inside its own baseline, and does so worst
    /// exactly when the spike is largest.
    #[test]
    fn the_baseline_stops_where_the_measured_window_begins() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        quiet_hours(&mut conn, 200);
        reply(&mut conn, "spike", NOW - HOUR / 2, 500_000);

        let alert = collect(&mut conn, &thresholds(), NOW).unwrap().expect("a surge");
        let AlertDetail::Usage(detail) = &alert.detail else {
            panic!("a usage alert");
        };
        // 500_000 for the spike plus the 1_000 of the reply sitting exactly on
        // the window's opening edge: `since` is inclusive and `until` exclusive,
        // so a row on the boundary belongs to the newer window.
        assert_eq!(detail.window_cost, "0.501".parse().unwrap());
        assert_eq!(
            detail.baseline_cost,
            "0.168".parse().unwrap(),
            "168 quiet hours and not one token of the spike"
        );
        assert_eq!(detail.baseline_windows, 168);
        assert!(!detail.is_lower_bound);
    }

    /// The same ledger without the spike is ordinary spending and says nothing.
    #[test]
    fn a_steady_ledger_raises_nothing() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        quiet_hours(&mut conn, 200);
        assert!(collect(&mut conn, &thresholds(), NOW).unwrap().is_none());
    }

    /// A ledger that does not reach back across the baseline span has no
    /// baseline, only an install date.
    #[test]
    fn a_young_ledger_raises_nothing_however_large_the_hour() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        quiet_hours(&mut conn, 10);
        reply(&mut conn, "spike", NOW - HOUR / 2, 5_000_000);
        assert!(collect(&mut conn, &thresholds(), NOW).unwrap().is_none());
    }

    /// The alert has to name something actionable: a runaway is usually one
    /// conversation, and a total with no breakdown is a number nobody can act on.
    #[test]
    fn the_alert_carries_a_breakdown_of_the_window() {
        let pool = test_db();
        let mut conn = pool.get().unwrap();
        quiet_hours(&mut conn, 200);
        reply(&mut conn, "spike", NOW - HOUR / 2, 500_000);

        let alert = collect(&mut conn, &thresholds(), NOW).unwrap().expect("a surge");
        let AlertDetail::Usage(detail) = &alert.detail else {
            panic!("a usage alert");
        };
        assert_eq!(
            detail.top_providers.first().map(|slice| slice.key.as_str()),
            Some("Acme")
        );
        assert_eq!(
            detail.top_conversations.first().map(|slice| slice.key.as_str()),
            Some("c1")
        );
        assert!(alert.summary.contains("0.501"), "{}", alert.summary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decimal(raw: &str) -> Decimal {
        raw.parse().unwrap()
    }

    fn thresholds(multiplier: &str, min_cost: &str) -> SurgeThresholds {
        SurgeThresholds {
            window_hours: 1,
            baseline_days: 7,
            multiplier: decimal(multiplier),
            min_cost: decimal(min_cost),
        }
    }

    fn sample(cost: &str) -> WindowSample {
        WindowSample {
            cost: decimal(cost),
            messages: 10,
            metered_messages: 10,
            unpriced_messages: 0,
            estimated_messages: 0,
        }
    }

    /// 168 windows of an hour in seven days; a baseline of 168 is an average of
    /// exactly 1 per window, so 3× is anything over 3.
    #[test]
    fn the_rule_is_the_ratio_and_the_boundary_is_exclusive() {
        let t = thresholds("3", "0");
        let baseline = sample("168");
        assert_eq!(
            evaluate(&sample("3.01"), &baseline, 168, true, &t),
            SurgeVerdict::Surge { is_lower_bound: false }
        );
        assert_eq!(
            evaluate(&sample("3"), &baseline, 168, true, &t),
            SurgeVerdict::NoAlert(NoAlertReason::WithinBaseline),
            "landing exactly on the multiple is the threshold met, not exceeded"
        );
        assert_eq!(
            evaluate(&sample("2.99"), &baseline, 168, true, &t),
            SurgeVerdict::NoAlert(NoAlertReason::WithinBaseline)
        );
    }

    /// The arithmetic must stay exact at scales where a float would not. 0.1
    /// has no binary representation; three of them are not 0.3 in `f64`.
    #[test]
    fn the_comparison_is_exact_at_a_scale_that_would_defeat_a_float() {
        let t = thresholds("3", "0");
        // Three windows holding 0.3 in total, so an average of 0.1 and a 3×
        // threshold of exactly 0.3. Both sides of the comparison come out at
        // 0.9, which is the boundary and must not fire.
        let baseline = WindowSample {
            cost: decimal("0.3"),
            ..sample("0")
        };
        assert_eq!(
            evaluate(&sample("0.3"), &baseline, 3, true, &t),
            SurgeVerdict::NoAlert(NoAlertReason::WithinBaseline)
        );
        // One unit in the eighteenth decimal place decides it. A `f64` cannot
        // hold either operand, let alone tell these two apart.
        assert_eq!(
            evaluate(&sample("0.300000000000000001"), &baseline, 3, true, &t),
            SurgeVerdict::Surge { is_lower_bound: false }
        );
    }

    /// The floor is the whole defence of a ratio rule. Without it a install
    /// that normally spends a fraction of a cent reports a surge whenever
    /// somebody asks two questions in an hour.
    #[test]
    fn the_floor_holds_back_a_large_ratio_on_a_trivial_amount() {
        let t = thresholds("3", "1");
        let baseline = WindowSample {
            cost: decimal("0.0168"),
            ..sample("0")
        };
        // 0.5 against an average of 0.0001 is a 5000× jump, and still nothing.
        assert_eq!(
            evaluate(&sample("0.5"), &baseline, 168, true, &t),
            SurgeVerdict::NoAlert(NoAlertReason::BelowFloor)
        );
        assert_eq!(
            evaluate(&sample("1"), &baseline, 168, true, &t),
            SurgeVerdict::Surge { is_lower_bound: false },
            "at the floor exactly, the ratio decides"
        );
    }

    /// An understated baseline inflates the ratio, so it is refused. An
    /// understated recent window only understates the ratio, so it is reported
    /// with a label. The asymmetry is the point.
    #[test]
    fn an_unpriced_baseline_refuses_while_an_unpriced_window_only_gets_a_label() {
        let t = thresholds("3", "0");
        let dodgy_baseline = WindowSample {
            unpriced_messages: 1,
            ..sample("168")
        };
        assert_eq!(
            evaluate(&sample("100"), &dodgy_baseline, 168, true, &t),
            SurgeVerdict::NoAlert(NoAlertReason::BaselinePricingIncomplete)
        );
        let estimated_baseline = WindowSample {
            estimated_messages: 1,
            ..sample("168")
        };
        assert_eq!(
            evaluate(&sample("100"), &estimated_baseline, 168, true, &t),
            SurgeVerdict::NoAlert(NoAlertReason::BaselinePricingIncomplete)
        );

        let dodgy_recent = WindowSample {
            unpriced_messages: 1,
            ..sample("100")
        };
        assert_eq!(
            evaluate(&dodgy_recent, &sample("168"), 168, true, &t),
            SurgeVerdict::Surge { is_lower_bound: true }
        );
    }

    /// A fresh install has no baseline, and `R × N > K × 0` is true for every
    /// non-zero R. This is the first alert a new user would ever get, and it
    /// would be wrong.
    #[test]
    fn a_cold_start_never_alerts() {
        let t = thresholds("3", "0");
        assert_eq!(
            evaluate(&sample("100"), &WindowSample::zero(), 168, false, &t),
            SurgeVerdict::NoAlert(NoAlertReason::LedgerTooShort)
        );
        assert_eq!(
            evaluate(&sample("100"), &WindowSample::zero(), 168, true, &t),
            SurgeVerdict::NoAlert(NoAlertReason::NoBaselineTraffic),
            "an empty baseline is not a baseline of zero"
        );
    }

    /// Traffic that was never billed here is not a baseline either — otherwise
    /// moving one conversation off a subscription reads as a runaway.
    #[test]
    fn a_baseline_of_purely_external_traffic_is_not_a_baseline() {
        let t = thresholds("3", "0");
        let external_only = WindowSample {
            cost: Decimal::zero(),
            messages: 500,
            metered_messages: 0,
            unpriced_messages: 0,
            estimated_messages: 0,
        };
        assert_eq!(
            evaluate(&sample("100"), &external_only, 168, true, &t),
            SurgeVerdict::NoAlert(NoAlertReason::NoMeteredBaseline)
        );
    }

    #[test]
    fn a_baseline_shorter_than_one_window_cannot_be_averaged() {
        let t = thresholds("3", "0");
        assert_eq!(
            evaluate(&sample("100"), &sample("1"), 0, true, &t),
            SurgeVerdict::NoAlert(NoAlertReason::BaselineShorterThanWindow)
        );
        assert_eq!(baseline_window_count(1, 7), 168);
        assert_eq!(baseline_window_count(24, 7), 7);
        assert_eq!(baseline_window_count(48, 1), 0, "a day holds no two-day window");
        assert_eq!(baseline_window_count(0, 7), 0, "no division by zero");
    }
}

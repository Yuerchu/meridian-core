//! What happened, and whether it is worth saying again.
//!
//! An alert is raised by a source (`notify::balance`, `notify::usage`) and
//! carried to every sink unchanged. The two decisions that live here are the
//! fingerprint — what counts as *the same* situation — and the cooldown.

use serde::Serialize;

use crate::db::models::notification::NotificationEventKind;
use crate::db::models::notification::{NotificationAlertStateRow, NotificationFormat};
use crate::decimal::Decimal;
use crate::provider::balance::BalanceAccount;

/// One thing worth telling somebody about.
///
/// Deliberately **not** `Serialize`. There is exactly one wire shape for an
/// alert and it lives in `notify::webhook`; a derive here would be a second
/// one, free to drift from the contract every receiver is written against.
#[derive(Debug, Clone)]
pub struct Alert {
    pub event: NotificationEventKind,
    pub raised_at: i64,
    /// What this alert is *about*, which is what the state table is keyed on.
    /// One key per condition, not per occurrence.
    pub alert_key: String,
    pub title: String,
    pub summary: String,
    pub detail: AlertDetail,
}

/// The structured half. The wire shape carries a member for each variant with
/// all but one null, so a receiver can switch on `event` and find the matching
/// object rather than guessing whether an absent key means anything.
#[derive(Debug, Clone)]
pub enum AlertDetail {
    Balance(BalanceAlert),
    Usage(UsageAlert),
    /// A delivery test sent by hand. Carries nothing but the format: its whole
    /// purpose is to prove that the URL and the signing scheme are right.
    Test(TestAlert),
}

#[derive(Debug, Clone, Serialize)]
pub struct BalanceAlert {
    pub provider_id: String,
    /// Snapshotted rather than looked up by the receiver: the alert is about a
    /// moment, and a provider renamed afterwards should not rewrite it.
    pub provider_name: String,
    pub is_available: bool,
    pub threshold: Decimal,
    pub accounts: Vec<BalanceAccount>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageAlert {
    pub window_hours: u32,
    pub baseline_days: u32,
    pub multiplier: Decimal,
    pub window_start_ms: i64,
    pub window_cost: Decimal,
    /// The whole baseline span's spend, and how many windows fit in it.
    ///
    /// There is deliberately no "average per window" field. The comparison this
    /// alert is made of never divides — `R × N > K × B` keeps both sides exact,
    /// and money here is `Decimal` precisely so nothing rounds on the way to a
    /// decision. A receiver that wants the average has both numbers.
    pub baseline_cost: Decimal,
    pub baseline_windows: i64,
    /// True when the recent window contains replies whose price is unknown, so
    /// `window_cost` is at least this much rather than exactly this much.
    ///
    /// A lower bound that already crosses the threshold is still a real surge,
    /// which is why this labels the alert instead of suppressing it. The
    /// opposite case — an understated *baseline* — inflates the ratio and is
    /// refused outright before an alert is ever built.
    pub is_lower_bound: bool,
    pub top_providers: Vec<UsageSlice>,
    pub top_conversations: Vec<UsageSlice>,
}

/// One line of the "what was it spent on" breakdown.
#[derive(Debug, Clone, Serialize)]
pub struct UsageSlice {
    pub key: String,
    /// `None` means the thing this key names has been deleted since. Shown as
    /// deleted rather than dropped.
    pub label: Option<String>,
    pub cost: Decimal,
    pub messages: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TestAlert {
    pub format: NotificationFormat,
}

impl Alert {
    /// What counts as the same situation.
    ///
    /// Deliberately coarse for a balance: the exact figure moves every few
    /// hours, and a fingerprint carrying it would report "still low, now
    /// 4.97" for ever. What it does carry is whether the upstream has stopped
    /// serving requests and which currencies are under the floor — the two
    /// things that make this a *different* problem rather than the same one
    /// continuing.
    pub fn fingerprint(&self) -> String {
        match &self.detail {
            AlertDetail::Balance(balance) => {
                let mut low: Vec<&str> = balance
                    .accounts
                    .iter()
                    .filter(|account| account.total_balance < balance.threshold)
                    .map(|account| account.currency.as_str())
                    .collect();
                low.sort_unstable();
                format!("available={};low={}", balance.is_available, low.join(","))
            }
            // One shape of surge. Repeats are governed by the cooldown, because
            // a surge that is still going is the same incident and a second
            // message about it teaches the reader to mute the channel.
            AlertDetail::Usage(_) => "surge".to_string(),
            AlertDetail::Test(_) => "test".to_string(),
        }
    }
}

/// Whether this raising is owed a notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyDecision {
    /// Nobody has been told about this condition yet — either it is new, or an
    /// earlier attempt to tell them failed.
    Send(SendReason),
    /// Told recently enough, and nothing about it has changed.
    Suppress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendReason {
    New,
    /// Raised before but never successfully delivered. This is the case the
    /// two-write split exists for: a failed send must not look like a sent one.
    NeverDelivered,
    /// The situation itself changed — a low balance became an unusable account.
    /// Not subject to the cooldown: the cooldown is for repetition, and this is
    /// not a repeat.
    Changed,
    CooldownElapsed,
}

/// The policy, as a pure function of the stored row.
///
/// A `None` row means the condition has never been raised, or was cleared when
/// it resolved. Both mean the same thing to a reader, which is why
/// `clear_alert` deletes rather than flags.
pub fn decide(
    state: Option<&NotificationAlertStateRow>,
    fingerprint: &str,
    now: i64,
    cooldown_ms: i64,
) -> NotifyDecision {
    let Some(state) = state else {
        return NotifyDecision::Send(SendReason::New);
    };
    let Some(notified_at) = state.last_notified_at else {
        return NotifyDecision::Send(SendReason::NeverDelivered);
    };
    if state.fingerprint != fingerprint {
        return NotifyDecision::Send(SendReason::Changed);
    }
    // Suppress only for an age that is genuinely inside the window. A stored
    // timestamp in the *future* — a clock that moved backwards, a machine that
    // resumed from suspend with a bad RTC — is not evidence that anybody was
    // told recently, and reading it as one silences the channel until the clock
    // catches up. One duplicate is the cheaper mistake.
    match now.checked_sub(notified_at) {
        Some(age) if (0..cooldown_ms).contains(&age) => NotifyDecision::Suppress,
        _ => NotifyDecision::Send(SendReason::CooldownElapsed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(fingerprint: &str, notified_at: Option<i64>) -> NotificationAlertStateRow {
        NotificationAlertStateRow {
            alert_key: "balance:p1".into(),
            first_raised_at: 0,
            last_raised_at: 0,
            last_notified_at: notified_at,
            fingerprint: fingerprint.into(),
        }
    }

    #[test]
    fn a_condition_nobody_has_been_told_about_is_always_sent() {
        assert_eq!(decide(None, "f", 1_000, 10_000), NotifyDecision::Send(SendReason::New));
        assert_eq!(
            decide(Some(&state("f", None)), "f", 1_000, 10_000),
            NotifyDecision::Send(SendReason::NeverDelivered),
            "a raise that never reached anybody is not a delivery"
        );
    }

    #[test]
    fn the_cooldown_holds_a_repeat_but_never_a_change() {
        let told = state("available=true;low=CNY", Some(1_000));
        assert_eq!(
            decide(Some(&told), "available=true;low=CNY", 5_000, 10_000),
            NotifyDecision::Suppress
        );
        assert_eq!(
            decide(Some(&told), "available=true;low=CNY", 11_000, 10_000),
            NotifyDecision::Send(SendReason::CooldownElapsed)
        );
        assert_eq!(
            decide(Some(&told), "available=false;low=CNY", 1_100, 10_000),
            NotifyDecision::Send(SendReason::Changed),
            "an account that stopped working is not a repeat of one running low"
        );
    }

    /// A timestamp in the future is not evidence of a recent delivery.
    ///
    /// Written with `saturating_sub` first, which saturates towards `i64::MIN`
    /// rather than towards zero — so a clock that moved backwards produced a
    /// negative age, compared false against every cooldown, and silenced the
    /// channel until the clock caught up.
    #[test]
    fn a_backwards_clock_does_not_suppress_for_ever() {
        let told = state("f", Some(9_000));
        assert_eq!(
            decide(Some(&told), "f", 1_000, 10_000),
            NotifyDecision::Send(SendReason::CooldownElapsed)
        );
        assert_eq!(
            decide(Some(&told), "f", 9_000, 10_000),
            NotifyDecision::Suppress,
            "an age of exactly zero is still inside the window"
        );
    }

    /// A zero cooldown means every raising is sent, which is what makes the
    /// range check `0..cooldown` rather than a `>=` on the age.
    #[test]
    fn a_zero_cooldown_suppresses_nothing() {
        let told = state("f", Some(1_000));
        assert_eq!(
            decide(Some(&told), "f", 1_000, 0),
            NotifyDecision::Send(SendReason::CooldownElapsed)
        );
    }

    fn balance_alert(is_available: bool, totals: &[(&str, &str)], threshold: &str) -> Alert {
        Alert {
            event: NotificationEventKind::BalanceLow,
            raised_at: 0,
            alert_key: "balance:p1".into(),
            title: String::new(),
            summary: String::new(),
            detail: AlertDetail::Balance(BalanceAlert {
                provider_id: "p1".into(),
                provider_name: "DeepSeek".into(),
                is_available,
                threshold: threshold.parse().unwrap(),
                accounts: totals
                    .iter()
                    .map(|(currency, total)| BalanceAccount {
                        currency: (*currency).into(),
                        total_balance: total.parse().unwrap(),
                        granted_balance: None,
                        topped_up_balance: None,
                    })
                    .collect(),
            }),
        }
    }

    /// The figure drifts every check. If it were in the fingerprint, every
    /// single check of a low account would count as a new situation.
    #[test]
    fn a_drifting_balance_is_the_same_situation() {
        let first = balance_alert(true, &[("CNY", "4.97")], "5");
        let later = balance_alert(true, &[("CNY", "4.12")], "5");
        assert_eq!(first.fingerprint(), later.fingerprint());

        let dead = balance_alert(false, &[("CNY", "4.12")], "5");
        assert_ne!(first.fingerprint(), dead.fingerprint());

        let second_currency = balance_alert(true, &[("CNY", "4.12"), ("USD", "0.10")], "5");
        assert_ne!(
            first.fingerprint(),
            second_currency.fingerprint(),
            "a second currency going under the floor is new information"
        );
    }

    /// Currency order comes from the upstream and is not guaranteed. Unsorted,
    /// the same situation would alternate between two fingerprints and report
    /// itself as changed on every check.
    #[test]
    fn the_fingerprint_does_not_depend_on_the_upstreams_ordering() {
        let one = balance_alert(true, &[("CNY", "1"), ("USD", "1")], "5");
        let other = balance_alert(true, &[("USD", "1"), ("CNY", "1")], "5");
        assert_eq!(one.fingerprint(), other.fingerprint());
    }
}

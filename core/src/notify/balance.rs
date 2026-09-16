//! Asking the upstreams that publish a balance, and saying so before it runs out.
//!
//! Almost none of them do — see `provider::balance`. That is a fact about the
//! vendors rather than a gap here, and it is why a check that finds nothing to
//! ask is silent rather than an error.
//!
//! This loop used to live in the OneBot server, which made a QQ private message
//! the only way an account could ever warn anybody: with the bot switched off
//! there was no balance monitoring at all. It is here now, and QQ is one sink
//! among several.

use crate::db::models::notification::NotificationEventKind;
use crate::decimal::Decimal;
use crate::provider::balance::{ProviderBalance, ProviderIdentity, fetch_balance, supports_balance};
use crate::services::Services;

use super::alert::{Alert, AlertDetail, BalanceAlert};

/// What one provider's check concluded.
pub enum BalanceOutcome {
    /// Under the floor, or refused by the upstream.
    Alert(Box<Alert>),
    /// Healthy. Any standing alert for this provider is cleared, so the *next*
    /// time it drops is announced rather than suppressed as a repeat.
    Healthy,
    /// The question could not be asked. Deliberately neither of the above: a
    /// network blip reported as an empty account is an alert the user learns to
    /// ignore, and one that silently clears a real alert is worse.
    Unknown,
}

pub fn alert_key(provider_id: &str) -> String {
    format!("balance:{provider_id}")
}

/// Ask every enabled provider that publishes a balance.
///
/// Returns one outcome per provider it was able to consider, keyed by id.
pub async fn check_all(services: &Services, threshold: &Decimal) -> Vec<(String, BalanceOutcome)> {
    let pool = services.db.clone();
    let providers = match tokio::task::spawn_blocking(move || {
        let mut conn = pool.get().map_err(|error| error.to_string())?;
        crate::db::ops::provider::list_providers(&mut conn).map_err(|error| error.to_string())
    })
    .await
    {
        Ok(Ok(providers)) => providers,
        Ok(Err(error)) => {
            tracing::warn!(%error, "could not list providers for the balance check");
            return Vec::new();
        }
        Err(error) => {
            tracing::warn!(%error, "the balance check could not read the provider list");
            return Vec::new();
        }
    };

    let mut out = Vec::new();
    for provider in providers {
        let identity = ProviderIdentity::new(
            provider.catalog_id.as_deref(),
            &provider.provider_type,
            &provider.base_url,
        );
        if provider.is_enabled == 0 || !supports_balance(identity) {
            continue;
        }
        let Some(api_key) = crate::agent::get_provider_api_key(&services.secrets, &provider.id) else {
            continue;
        };
        let outcome = match fetch_balance(identity, &api_key).await {
            Ok(balance) => {
                if balance.is_low(threshold) {
                    BalanceOutcome::Alert(Box::new(build(&provider.id, &provider.name, &balance, threshold)))
                } else {
                    BalanceOutcome::Healthy
                }
            }
            Err(error) => {
                tracing::warn!(provider_id = %provider.id, %error, "could not read the account balance");
                BalanceOutcome::Unknown
            }
        };
        out.push((provider.id.clone(), outcome));
    }
    out
}

pub fn build(provider_id: &str, provider_name: &str, balance: &ProviderBalance, threshold: &Decimal) -> Alert {
    let detail = BalanceAlert {
        provider_id: provider_id.to_string(),
        provider_name: provider_name.to_string(),
        is_available: balance.is_available,
        threshold: threshold.clone(),
        accounts: balance.accounts.clone(),
    };
    // The two are separate events rather than one with a flag, because they call
    // for different urgency and a subscriber may reasonably want only the second.
    let event = if balance.is_available {
        NotificationEventKind::BalanceLow
    } else {
        NotificationEventKind::BalanceUnavailable
    };
    let title = if balance.is_available {
        format!("{provider_name} 余额偏低")
    } else {
        format!("{provider_name} 账户不可用")
    };
    Alert {
        event,
        raised_at: crate::util::now_ms(),
        alert_key: alert_key(provider_id),
        title,
        summary: summary(&detail),
        detail: AlertDetail::Balance(detail),
    }
}

/// What a person reads.
///
/// The upstream's own verdict goes first when it is the reason: "the account
/// has stopped working" and "the account is getting low" need different
/// urgency, and the figures alone do not distinguish them — a postpaid account
/// can be refused while showing a healthy total.
fn summary(alert: &BalanceAlert) -> String {
    let mut text = if alert.is_available {
        format!("{} 余额偏低（阈值 {}）", alert.provider_name, alert.threshold)
    } else {
        format!("{} 已停止服务：上游报告账户不可用", alert.provider_name)
    };
    for account in &alert.accounts {
        text.push_str(&format!("\n{} {}", account.currency, account.total_balance));
        // The split matters: a total propped up by expiring promotional credit
        // is closer to empty than it looks.
        if let (Some(topped_up), Some(granted)) = (&account.topped_up_balance, &account.granted_balance) {
            text.push_str(&format!("（充值 {topped_up} / 赠送 {granted}）"));
        }
    }
    if alert.accounts.is_empty() {
        text.push_str("\n上游没有给出余额明细。");
    }
    text.push_str("\n\n充值后本提醒会自动重置。");
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::balance::BalanceAccount;

    fn decimal(raw: &str) -> Decimal {
        raw.parse().unwrap()
    }

    fn account(currency: &str, total: &str) -> BalanceAccount {
        let total: Decimal = decimal(total);
        BalanceAccount {
            currency: currency.into(),
            total_balance: total.clone(),
            granted_balance: Some(decimal("10")),
            topped_up_balance: Some(total - decimal("10")),
        }
    }

    /// A refused account and a merely low one read differently and are
    /// different events, because they call for different urgency — one has
    /// already stopped working.
    #[test]
    fn the_alert_says_which_of_the_two_situations_this_is() {
        let low = ProviderBalance {
            is_available: true,
            accounts: vec![account("CNY", "12")],
        };
        let alert = build("p1", "DeepSeek", &low, &decimal("50"));
        assert_eq!(alert.event, NotificationEventKind::BalanceLow);
        assert!(alert.summary.contains("余额偏低"));
        assert!(alert.summary.contains("CNY 12"));
        assert!(alert.summary.contains("充值 2 / 赠送 10"), "{}", alert.summary);

        let dead = ProviderBalance {
            is_available: false,
            accounts: vec![account("CNY", "0")],
        };
        let alert = build("p1", "DeepSeek", &dead, &decimal("50"));
        assert_eq!(alert.event, NotificationEventKind::BalanceUnavailable);
        assert!(alert.summary.contains("不可用"));
    }

    /// An upstream that refuses requests without itemising anything still has
    /// to produce a message that says something.
    #[test]
    fn an_alert_with_no_figures_still_reads_as_a_sentence() {
        let bare = ProviderBalance {
            is_available: false,
            accounts: vec![],
        };
        let alert = build("p1", "Acme", &bare, &decimal("1"));
        assert!(alert.summary.contains("Acme"));
        assert!(alert.summary.contains("没有给出余额明细"));
    }

    #[test]
    fn the_key_is_the_provider_so_two_of_them_do_not_share_one_alert() {
        assert_eq!(alert_key("p1"), "balance:p1");
        assert_ne!(alert_key("p1"), alert_key("p2"));
    }
}

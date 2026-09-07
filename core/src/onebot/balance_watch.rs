//! Tell the admins before the credit runs out.
//!
//! Lives in the OneBot server rather than beside `bootstrap`, and that is a
//! decision rather than an accident: the notification *is* a QQ private message,
//! so a watcher that outlived the listener would have discovered the problem and
//! had nowhere to say it. It starts and stops with the server, and the desktop's
//! own view of a balance is a button in provider settings, checked when somebody
//! looks.
//!
//! Off unless a threshold is configured *and* there is an admin to tell. Both
//! halves matter — this is a background process making network requests with the
//! user's API keys, and it should exist only because somebody asked for it.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::watch;

use super::SharedState;
use super::format;
use super::protocol::OneBotAction;
use crate::provider::balance::{ProviderBalance, fetch_balance, supports_balance};

/// Balances move slowly and this spends real requests against the user's key, so
/// it asks rarely. A run that burns through a whole account between two checks
/// is one an alert was never going to get ahead of anyway.
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Long enough that a restart loop does not turn into a request loop, short
/// enough that "I just set this up" gets an answer while the user is still
/// looking at it.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(60);

pub fn spawn(state: Arc<SharedState>, mut shutdown_rx: watch::Receiver<bool>) {
    let Some(threshold) = state.config.balance_alert_threshold.clone() else {
        return;
    };
    if state.config.admin_users.is_empty() {
        tracing::info!("a balance threshold is set but no admin is configured; nobody would be told");
        return;
    }

    tokio::spawn(async move {
        // Which providers have already been reported, so a low balance is
        // announced once rather than every six hours until it is topped up.
        // Cleared per provider the moment it recovers, which is what lets the
        // *next* time be announced too.
        let mut announced: HashSet<String> = HashSet::new();
        let mut delay = FIRST_CHECK_DELAY;
        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    // Err = the sender is gone because this server was replaced.
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
                _ = tokio::time::sleep(delay) => {
                    check_once(&state, &threshold, &mut announced).await;
                    delay = CHECK_INTERVAL;
                }
            }
        }
    });
}

async fn check_once(state: &Arc<SharedState>, threshold: &crate::decimal::Decimal, announced: &mut HashSet<String>) {
    let pool = state.services.db.clone();
    let providers = match tokio::task::spawn_blocking(move || {
        let mut conn = pool.get().map_err(|e| e.to_string())?;
        crate::db::ops::provider::list_providers(&mut conn).map_err(|e| e.to_string())
    })
    .await
    {
        Ok(Ok(providers)) => providers,
        Ok(Err(error)) => {
            tracing::warn!(error = %error, "could not list providers for the balance check");
            return;
        }
        Err(error) => {
            tracing::warn!(error = %error, "the balance check could not read the provider list");
            return;
        }
    };

    for provider in providers {
        if provider.is_enabled == 0 || !supports_balance(&provider.provider_type) {
            continue;
        }
        let Some(api_key) = crate::agent::get_provider_api_key(&state.services.secrets, &provider.id) else {
            continue;
        };
        match fetch_balance(&provider.provider_type, &provider.base_url, &api_key).await {
            Ok(balance) => {
                if !balance.is_low(threshold) {
                    announced.remove(&provider.id);
                    continue;
                }
                if announced.contains(&provider.id) {
                    continue;
                }
                // Nothing is connected, so there is nobody to tell. Left as a
                // send-and-forget, this would mark the provider announced
                // against a broadcast that reached no sinks — and the alert
                // would then never be sent again until the balance recovered and
                // dropped a second time. The check runs every six hours, and QQ
                // being briefly disconnected across one of them is ordinary.
                if state.connected_clients.load(Ordering::Relaxed) == 0 {
                    tracing::info!(
                        provider_id = %provider.id,
                        "the account balance is low but no OneBot client is connected; will tell them next time"
                    );
                    continue;
                }
                tracing::info!(
                    provider_id = %provider.id,
                    available = balance.is_available,
                    "the account balance is low; telling the admins"
                );
                let text = alert_text(&provider.name, &balance);
                for admin in &state.config.admin_users {
                    let action = OneBotAction::send_private_msg(*admin, format::text_to_rich_segments(&text));
                    super::send_action_nowait(state, &action).await;
                }
                // Marked only once it has actually gone out.
                announced.insert(provider.id.clone());
            }
            Err(error) => {
                // A network blip must not be reported as an empty account: the
                // alert would be wrong and the user would learn to ignore it.
                tracing::warn!(
                    provider_id = %provider.id,
                    error = %error,
                    "could not read the account balance"
                );
            }
        }
    }
}

/// What the admins are actually sent.
///
/// The upstream's own verdict goes first when it is the reason, because "the
/// account has stopped working" and "the account is getting low" call for
/// different urgency and the numbers alone do not distinguish them.
fn alert_text(provider_name: &str, balance: &ProviderBalance) -> String {
    let mut text = if balance.is_available {
        format!("{provider_name} 余额偏低")
    } else {
        format!("{provider_name} 已停止服务：上游报告账户不可用")
    };
    for account in &balance.accounts {
        text.push_str(&format!("\n{} {}", account.currency, account.total_balance));
        // The split matters: a total propped up by expiring promotional credit
        // is closer to empty than it looks.
        if let (Some(topped_up), Some(granted)) = (&account.topped_up_balance, &account.granted_balance) {
            text.push_str(&format!("（充值 {topped_up} / 赠送 {granted}）"));
        }
    }
    if balance.accounts.is_empty() {
        text.push_str("\n上游没有给出余额明细。");
    }
    text.push_str("\n\n充值后本提醒会自动重置。");
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::balance::BalanceAccount;

    fn decimal(raw: &str) -> crate::decimal::Decimal {
        raw.parse().unwrap()
    }

    fn account(currency: &str, total: &str) -> BalanceAccount {
        let total = decimal(total);
        BalanceAccount {
            currency: currency.into(),
            total_balance: total.clone(),
            granted_balance: Some(decimal("10")),
            topped_up_balance: Some(total - decimal("10")),
        }
    }

    /// A refused account and a merely low one read differently, because they
    /// call for different urgency — one has already stopped working.
    #[test]
    fn the_alert_says_which_of_the_two_situations_this_is() {
        let low = ProviderBalance {
            is_available: true,
            accounts: vec![account("CNY", "12")],
        };
        let text = alert_text("DeepSeek", &low);
        assert!(text.contains("余额偏低"));
        assert!(text.contains("CNY 12"));
        assert!(text.contains("充值 2 / 赠送 10"), "{text}");

        let dead = ProviderBalance {
            is_available: false,
            accounts: vec![account("CNY", "0")],
        };
        assert!(alert_text("DeepSeek", &dead).contains("不可用"));
    }

    /// An upstream that refuses requests without itemising anything still has to
    /// produce a message that says something.
    #[test]
    fn an_alert_with_no_figures_still_reads_as_a_sentence() {
        let bare = ProviderBalance {
            is_available: false,
            accounts: vec![],
        };
        let text = alert_text("Acme", &bare);
        assert!(text.contains("Acme"));
        assert!(text.contains("没有给出余额明细"));
    }
}

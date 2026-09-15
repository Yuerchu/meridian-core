//! Telling the QQ admins about an alert.
//!
//! This used to be the watcher as well as the outlet: a six-hour poll of every
//! provider that publishes a balance, living inside the OneBot server. That put
//! the whole feature behind the bot being switched on — with QQ off, an account
//! could empty with nothing anywhere saying so — and it meant "is this balance
//! low" was answered here rather than in one place.
//!
//! The poll is `notify::balance` now. What remains is a sink: registered while
//! the server is running, unregistered when it stops, and one of several places
//! the same alert comes out. Everything the old module was careful about is
//! still true, and two of those cares moved rather than disappeared:
//!
//! - **Nothing connected means nothing was told.** The old loop checked
//!   `connected_clients` before marking a provider announced, because a
//!   broadcast that reaches no sink is not a notification. Here that is
//!   `deliver` returning `Err`, and `notify::raise_and_dispatch` not recording
//!   the alert as reported.
//! - **The admin list belongs to a generation.** A sink left registered after
//!   the server stopped would keep sending to the outgoing generation's admins
//!   over connections it no longer owns, which is why registration is undone in
//!   `stop()`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::SharedState;
use super::format;
use super::protocol::OneBotAction;
use crate::notify::{Alert, AlertSink};

pub struct OneBotAlertSink {
    state: Arc<SharedState>,
}

impl OneBotAlertSink {
    pub fn new(state: Arc<SharedState>) -> Self {
        Self { state }
    }
}

#[async_trait::async_trait]
impl AlertSink for OneBotAlertSink {
    fn name(&self) -> &'static str {
        "onebot"
    }

    async fn deliver(&self, alert: &Alert) -> Result<(), String> {
        if self.state.config.admin_users.is_empty() {
            return Err("no OneBot admin is configured; there is nobody to tell".into());
        }
        // A broadcast with no connections behind it succeeds and reaches
        // nobody. Reported as an error so the alert is not written down as
        // told — QQ being briefly disconnected across one check is ordinary,
        // and the next check has to be free to try again.
        if self.state.connected_clients.load(Ordering::Relaxed) == 0 {
            return Err("no OneBot client is connected".into());
        }

        let text = message_text(alert);
        for admin in &self.state.config.admin_users {
            let action = OneBotAction::send_private_msg(*admin, format::text_to_rich_segments(&text));
            super::send_action_nowait(&self.state, &action).await;
        }
        Ok(())
    }
}

/// What the admins actually read.
///
/// The alert's own `summary` is already written for a person — it is what a
/// webhook carries too — so this adds only the title, which a chat message has
/// no other way to show.
fn message_text(alert: &Alert) -> String {
    format!("{}\n{}", alert.title, alert.summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decimal::Decimal;
    use crate::notify::balance::build;
    use crate::provider::balance::{BalanceAccount, ProviderBalance};

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

    /// The message keeps the shape it had before this became a sink: the
    /// upstream's own verdict first when that is the reason, then the figures
    /// split into topped-up and granted.
    #[test]
    fn the_message_says_which_of_the_two_situations_this_is() {
        let low = ProviderBalance {
            is_available: true,
            accounts: vec![account("CNY", "12")],
        };
        let text = message_text(&build("p1", "DeepSeek", &low, &decimal("50")));
        assert!(text.contains("DeepSeek 余额偏低"));
        assert!(text.contains("CNY 12"));
        assert!(text.contains("充值 2 / 赠送 10"), "{text}");

        let dead = ProviderBalance {
            is_available: false,
            accounts: vec![account("CNY", "0")],
        };
        let text = message_text(&build("p1", "DeepSeek", &dead, &decimal("50")));
        assert!(text.contains("账户不可用"));
        assert!(text.contains("上游报告账户不可用"));
    }

    #[test]
    fn a_message_with_no_figures_still_reads_as_a_sentence() {
        let bare = ProviderBalance {
            is_available: false,
            accounts: vec![],
        };
        let text = message_text(&build("p1", "Acme", &bare, &decimal("1")));
        assert!(text.contains("Acme"));
        assert!(text.contains("没有给出余额明细"));
    }
}

//! What is left on the account, for the upstreams that will say.
//!
//! Almost none of them will. Anthropic and xAI publish nothing; OpenAI's
//! `/dashboard/billing` endpoints were withdrawn and the console is the only
//! answer there now. DeepSeek is the one that does, which is why this exists as
//! a `match` over provider types rather than a method on `ChatProvider`: a trait
//! method would put an unimplementable obligation on every adapter, and the
//! honest shape is a lookup that mostly answers "not here".
//!
//! Adding one is `supports_balance` plus an arm. Anything that reports its own
//! notion of "the account is usable" should map it to `is_available` rather than
//! be inferred from the number — a prepaid account at zero and a postpaid one at
//! zero are not the same situation.

use serde::{Deserialize, Serialize};

use super::ProviderError;
use super::dto::{ExtraIgnore, warn_extra_fields};
use crate::client::{HttpTransport, Request, ReqwestTransport};
use crate::decimal::Decimal;

/// One currency's worth of credit.
///
/// A list rather than a single figure because DeepSeek returns one entry per
/// currency, and adding them would produce a number in no currency at all.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BalanceAccount {
    pub currency: String,
    /// What can actually be spent, which is the only figure worth alerting on.
    pub total_balance: Decimal,
    /// Promotional credit, which typically expires. Shown, never used as the
    /// threshold: an account with 50 of expiring grant and nothing topped up is
    /// closer to empty than the total suggests.
    pub granted_balance: Option<Decimal>,
    pub topped_up_balance: Option<Decimal>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProviderBalance {
    /// The upstream's own verdict on whether requests will be served.
    ///
    /// Kept apart from the numbers because it is the more reliable signal: it
    /// accounts for postpaid arrangements, expired grants and holds, none of
    /// which are visible in a total.
    pub is_available: bool,
    pub accounts: Vec<BalanceAccount>,
}

impl ProviderBalance {
    /// Whether this is worth telling somebody about.
    ///
    /// Two conditions, and the first is not the threshold: an upstream that says
    /// it will not serve requests has already made the decision, whatever the
    /// numbers look like. The threshold is the early warning on top of that, and
    /// it is compared per currency rather than against a sum — one number cannot
    /// be a sensible floor for both CNY and USD at once, but somebody who holds
    /// balances in two currencies is better warned twice than not at all.
    pub fn is_low(&self, threshold: &Decimal) -> bool {
        !self.is_available
            || (!threshold.is_zero() && self.accounts.iter().any(|account| &account.total_balance < threshold))
    }
}

/// Whether asking is worth the request. Callers use it to decide whether to draw
/// the control at all — a button that always errors is worse than no button.
pub fn supports_balance(provider_type: &str) -> bool {
    matches!(provider_type, "deepseek")
}

pub async fn fetch_balance(
    provider_type: &str,
    base_url: &str,
    api_key: &str,
) -> Result<ProviderBalance, ProviderError> {
    match provider_type {
        "deepseek" => fetch_deepseek_balance(base_url, api_key).await,
        other => Err(ProviderError::NotImplemented(format!(
            "{other} does not publish an account balance"
        ))),
    }
}

/// The account endpoints sit beside the API root, not under its version.
///
/// DeepSeek accepts both `https://api.deepseek.com` and `.../v1` as a chat base
/// — the `v1` is there for OpenAI-compatible clients and means nothing to them —
/// but `/user/balance` exists only at the root. Left alone, a user who typed the
/// `v1` form gets a 404 from a button that works for everybody else.
fn account_root(base_url: &str) -> &str {
    base_url.trim_end_matches('/').trim_end_matches("/v1")
}

#[derive(Deserialize)]
struct DeepSeekBalanceDto {
    is_available: bool,
    #[serde(default)]
    balance_infos: Vec<DeepSeekBalanceInfoDto>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

/// Every amount is a **string** on this API. Parsed rather than trusted to be a
/// number, and a total that will not parse fails the whole lookup: read as zero
/// it would raise a false alarm, and skipped it would silence a real one.
#[derive(Deserialize)]
struct DeepSeekBalanceInfoDto {
    currency: String,
    total_balance: String,
    granted_balance: Option<String>,
    topped_up_balance: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

async fn fetch_deepseek_balance(base_url: &str, api_key: &str) -> Result<ProviderBalance, ProviderError> {
    let transport = ReqwestTransport::shared();
    let mut req = Request::new(http::Method::GET, format!("{}/user/balance", account_root(base_url)));
    req.headers.insert(
        http::header::AUTHORIZATION,
        super::auth_header_value(&format!("Bearer {api_key}")),
    );
    req.headers
        .insert(http::header::ACCEPT, "application/json".parse().unwrap());

    let resp = transport.execute(req).await.inspect_err(|error| {
        tracing::error!(api = "deepseek", error = %error, "could not fetch the account balance");
    })?;
    let parsed: DeepSeekBalanceDto = serde_json::from_slice(&resp.body).map_err(|error| {
        tracing::warn!(
            api = "deepseek",
            body_len = resp.body.len(),
            error = %error,
            "the balance response was not in the expected shape"
        );
        ProviderError::Parse(error.to_string())
    })?;
    warn_extra_fields("deepseek_balance", &parsed.extra);

    let mut accounts = Vec::with_capacity(parsed.balance_infos.len());
    for info in &parsed.balance_infos {
        warn_extra_fields("deepseek_balance_info", &info.extra);
        let total = info
            .total_balance
            .trim()
            .parse::<Decimal>()
            .map_err(|error| {
                tracing::warn!(
                    api = "deepseek",
                    currency = %info.currency,
                    error = %error,
                    "the balance was not a number"
                );
                ProviderError::Parse(format!("balance for {} is not a number", info.currency))
            })?
            .require_non_negative("total_balance")
            .map_err(|error| ProviderError::Parse(error.to_string()))?;
        accounts.push(BalanceAccount {
            currency: info.currency.clone(),
            total_balance: total,
            granted_balance: amount(info.granted_balance.as_deref(), "granted_balance")?,
            topped_up_balance: amount(info.topped_up_balance.as_deref(), "topped_up_balance")?,
        });
    }

    Ok(ProviderBalance {
        is_available: parsed.is_available,
        accounts,
    })
}

fn amount(raw: Option<&str>, field: &str) -> Result<Option<Decimal>, ProviderError> {
    raw.map(|raw| {
        raw.trim()
            .parse::<Decimal>()
            .map_err(|error| ProviderError::Parse(format!("{field} is not a decimal: {error}")))?
            .require_non_negative(field)
            .map_err(|error| ProviderError::Parse(error.to_string()))
    })
    .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decimal(raw: &str) -> Decimal {
        raw.parse().unwrap()
    }

    /// The mapping half of `fetch_deepseek_balance`, without the HTTP half.
    fn parse(json: &str) -> ProviderBalance {
        let dto: DeepSeekBalanceDto = serde_json::from_str(json).expect("a balance body");
        ProviderBalance {
            is_available: dto.is_available,
            accounts: dto
                .balance_infos
                .iter()
                .map(|info| BalanceAccount {
                    currency: info.currency.clone(),
                    total_balance: info.total_balance.trim().parse().expect("a numeric total"),
                    granted_balance: amount(info.granted_balance.as_deref(), "granted_balance").unwrap(),
                    topped_up_balance: amount(info.topped_up_balance.as_deref(), "topped_up_balance").unwrap(),
                })
                .collect(),
        }
    }

    /// The response from DeepSeek's own documentation, verbatim.
    #[test]
    fn the_documented_response_parses_with_its_string_amounts() {
        let balance = parse(
            r#"{"is_available":true,"balance_infos":[
                {"currency":"CNY","total_balance":"110.00",
                 "granted_balance":"10.00","topped_up_balance":"100.00"}]}"#,
        );
        assert!(balance.is_available);
        assert_eq!(balance.accounts.len(), 1);
        assert_eq!(balance.accounts[0].currency, "CNY");
        assert_eq!(balance.accounts[0].total_balance, decimal("110"));
        assert_eq!(balance.accounts[0].granted_balance, Some(decimal("10")));
    }

    /// The threshold is per currency. Summing them would compare a number in no
    /// currency at all against a floor quoted in one.
    #[test]
    fn a_low_currency_is_low_even_beside_a_healthy_one() {
        let balance = parse(
            r#"{"is_available":true,"balance_infos":[
                {"currency":"CNY","total_balance":"500.00"},
                {"currency":"USD","total_balance":"0.80"}]}"#,
        );
        assert!(balance.is_low(&decimal("5")));
        assert!(!balance.is_low(&decimal("0.5")));
    }

    /// The upstream's own verdict outranks the numbers: a postpaid account can
    /// be refused while showing a healthy figure, and a zero threshold means
    /// "only tell me when it actually stops working" rather than "never".
    #[test]
    fn an_unavailable_account_is_low_at_any_threshold() {
        let balance = parse(r#"{"is_available":false,"balance_infos":[{"currency":"CNY","total_balance":"999"}]}"#);
        assert!(balance.is_low(&decimal("0")));
        assert!(balance.is_low(&decimal("1")));

        let healthy = parse(r#"{"is_available":true,"balance_infos":[{"currency":"CNY","total_balance":"999"}]}"#);
        assert!(
            !healthy.is_low(&decimal("0")),
            "a zero threshold disables the early warning"
        );
    }

    /// Both spellings of the chat base URL have to reach the same account
    /// endpoint — `/v1` is an OpenAI-compatibility affordance and the account
    /// routes do not live under it.
    #[test]
    fn the_version_suffix_is_not_part_of_the_account_root() {
        assert_eq!(account_root("https://api.deepseek.com"), "https://api.deepseek.com");
        assert_eq!(account_root("https://api.deepseek.com/"), "https://api.deepseek.com");
        assert_eq!(account_root("https://api.deepseek.com/v1"), "https://api.deepseek.com");
        assert_eq!(account_root("https://api.deepseek.com/v1/"), "https://api.deepseek.com");
    }

    #[test]
    fn only_the_upstreams_that_publish_one_are_offered_the_control() {
        assert!(supports_balance("deepseek"));
        for other in ["openai", "anthropic", "xai", "google"] {
            assert!(!supports_balance(other), "{other} publishes no balance");
        }
    }
}

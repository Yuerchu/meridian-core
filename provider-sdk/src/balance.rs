//! Account balance contracts and exact-Decimal parsing helpers shared by
//! balance fetchers across vendors.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::client::{HttpTransport, Request, ReqwestTransport};
use crate::decimal::Decimal;
use crate::wire::auth_header_value;

/// One currency's worth of credit.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BalanceAccount {
    pub currency: String,
    /// What can actually be spent, which is the only figure worth alerting on.
    pub total_balance: Decimal,
    /// Promotional credit, which typically expires.
    pub granted_balance: Option<Decimal>,
    pub topped_up_balance: Option<Decimal>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProviderBalance {
    /// The upstream's own verdict on whether requests will be served.
    pub is_available: bool,
    pub accounts: Vec<BalanceAccount>,
}

impl ProviderBalance {
    /// Whether this balance should trigger a low-balance alert against `threshold`.
    pub fn is_low(&self, threshold: &Decimal) -> bool {
        !self.is_available
            || (!threshold.is_zero() && self.accounts.iter().any(|account| &account.total_balance < threshold))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BalanceError {
    #[error("transport: {0}")]
    Transport(#[from] crate::client::TransportError),
    #[error("parse: {0}")]
    Parse(String),
    #[error("not implemented: {0}")]
    NotImplemented(String),
}

/// Optional vendor capability: query remaining account balance.
#[async_trait]
pub trait BalanceFetcher: Send + Sync {
    fn vendor_id(&self) -> &'static str;

    async fn fetch_balance(&self, base_url: &str, api_key: &str) -> Result<ProviderBalance, BalanceError>;
}

/// Strip a trailing `/` and `/v1` segment so both `https://host` and
/// `https://host/v1` resolve to the same account root.
pub fn account_root(base_url: &str) -> &str {
    base_url.trim_end_matches('/').trim_end_matches("/v1")
}

/// The host, lowercased, with scheme, userinfo, port and path removed.
pub fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    authority.split(':').next().unwrap_or("").to_ascii_lowercase()
}

/// Infer currency from `.cn` vs international host when an upstream response
/// omits the currency field.
pub fn site_currency(base_url: &str) -> &'static str {
    if host_of(base_url).ends_with(".cn") {
        "CNY"
    } else {
        "USD"
    }
}

/// Perform one authenticated JSON GET against a vendor's account endpoint.
pub async fn get_json(api: &'static str, url: String, api_key: &str) -> Result<Vec<u8>, BalanceError> {
    let transport = ReqwestTransport::shared();
    let mut req = Request::new(http::Method::GET, url);
    req.headers.insert(
        http::header::AUTHORIZATION,
        auth_header_value(&format!("Bearer {api_key}")),
    );
    req.headers
        .insert(http::header::ACCEPT, "application/json".parse().unwrap());

    let resp = transport.execute(req).await.inspect_err(|error| {
        tracing::error!(api, error = %error, "could not fetch the account balance");
    })?;
    Ok(resp.body.to_vec())
}

pub fn parse_body<T: for<'de> Deserialize<'de>>(api: &'static str, body: &[u8]) -> Result<T, BalanceError> {
    serde_json::from_slice(body).map_err(|error| {
        tracing::warn!(
            api,
            body_len = body.len(),
            error = %error,
            "the balance response was not in the expected shape"
        );
        BalanceError::Parse(error.to_string())
    })
}

/// Parse one exact decimal amount (allowing negative overdrawn figures).
pub fn required_amount(raw: &str, field: &str) -> Result<Decimal, BalanceError> {
    let parsed = raw.trim().parse::<Decimal>().map_err(|error| {
        tracing::warn!(field, error = %error, "the balance is not a value this app can hold");
        BalanceError::Parse(format!("{field} is not a usable decimal: {error}"))
    })?;
    if parsed.is_negative() {
        tracing::warn!(field, "the account is overdrawn");
    }
    Ok(parsed)
}

pub fn amount(raw: Option<&str>, field: &str) -> Result<Option<Decimal>, BalanceError> {
    raw.map(|raw| required_amount(raw, field)).transpose()
}

/// Parse a raw JSON numeric literal directly into `Decimal` without passing
/// through `f64`.
pub fn json_number_amount(raw: &serde_json::value::RawValue, field: &str) -> Result<Decimal, BalanceError> {
    let text = raw.get().trim();
    if text.starts_with('"') {
        return Err(BalanceError::Parse(format!(
            "{field} arrived as a string, not a number"
        )));
    }
    required_amount(text, field)
}

pub fn optional_json_number_amount(
    raw: Option<&serde_json::value::RawValue>,
    field: &str,
) -> Result<Option<Decimal>, BalanceError> {
    raw.filter(|raw| raw.get().trim() != "null")
        .map(|raw| json_number_amount(raw, field))
        .transpose()
}

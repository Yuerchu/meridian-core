//! What is left on the account, for the upstreams that will say.
//!
//! Most will not. Anthropic and xAI publish nothing; OpenAI's
//! `/dashboard/billing` endpoints were withdrawn and the console is the only
//! answer there now; OpenRouter's account credits need a *management* key,
//! which is not the key a provider row holds — its inference key sees only that
//! key's own spending cap, which is a different figure and usually null. So
//! this is a lookup that mostly answers "not here", rather than a method on
//! `ChatProvider` that would put an unimplementable obligation on every adapter.
//!
//! **The lookup is keyed on the vendor, not on `provider_type`.**
//! `ProviderType` is a closed enum of five adapter families and every vendor
//! added since is OpenAI-compatible, so `provider_type` cannot tell Moonshot
//! from SiliconFlow from somebody's relay — they are all `openai`. `catalog_id`
//! is the thing that names a vendor, which is exactly what an account endpoint
//! belongs to. See [`balance_vendor`] for how a row that carries no
//! `catalog_id` is resolved.
//!
//! Adding one is a [`BalanceVendor`] variant, an arm, and `"balance": true` on
//! its catalog entry — which a test holds to agreement with the enum, so the
//! two cannot drift.
//!
//! Anything that reports its own notion of "the account is usable" maps to
//! `is_available` rather than being inferred from the number: a prepaid account
//! at zero and a postpaid one at zero are not the same situation. Where a
//! vendor *documents* that it refuses requests below a figure, reading that
//! figure is quoting the upstream rather than guessing, and the arm says so.

use serde::de::IgnoredAny;
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

/// A vendor whose account endpoint this app knows how to speak.
///
/// The variants are named by catalog id rather than by adapter, because that is
/// what an account endpoint belongs to: `/user/balance` is DeepSeek's, not
/// "the OpenAI dialect's".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BalanceVendor {
    DeepSeek,
    /// Moonshot AI, whose platform is now branded Kimi. The catalog id stays
    /// `moonshot` because that is the API host and what rows already hold.
    Moonshot,
    SiliconFlow,
}

impl BalanceVendor {
    /// The catalog id this vendor is known by, and the only spelling accepted.
    ///
    /// Deliberately not lenient: an alias would let a wrong id reach a real
    /// account endpoint, and the catalog is the one place these are authored.
    pub fn from_catalog_id(id: &str) -> Option<Self> {
        match id {
            "deepseek" => Some(Self::DeepSeek),
            "moonshot" => Some(Self::Moonshot),
            "siliconflow" => Some(Self::SiliconFlow),
            _ => None,
        }
    }

    pub fn catalog_id(self) -> &'static str {
        match self {
            Self::DeepSeek => "deepseek",
            Self::Moonshot => "moonshot",
            Self::SiliconFlow => "siliconflow",
        }
    }
}

/// The three things about a provider row that decide whose account endpoint to
/// ask. Grouped rather than passed as three `&str`s, which is an argument order
/// waiting to be got wrong — and getting it wrong here means sending a live API
/// key to the wrong host.
#[derive(Debug, Clone, Copy)]
pub struct ProviderIdentity<'a> {
    pub catalog_id: Option<&'a str>,
    pub provider_type: &'a str,
    pub base_url: &'a str,
}

impl<'a> ProviderIdentity<'a> {
    pub fn new(catalog_id: Option<&'a str>, provider_type: &'a str, base_url: &'a str) -> Self {
        Self {
            catalog_id,
            provider_type,
            base_url,
        }
    }
}

/// Which vendor's account endpoint a row should be asked at, if any.
///
/// Three sources, in this order, and each exists for a case the ones above it
/// do not reach.
///
/// 1. **`catalog_id`.** What the row says about itself, written when it was
///    created from the catalog. Authoritative whenever it is there.
/// 2. **`provider_type`**, for the types that *are* a vendor. `deepseek` is one
///    and is the reason this step exists: migration 40's backfill matches
///    `https://api.deepseek.com` verbatim, while `account_root` exists
///    precisely because people also write `.../v1` — so a working DeepSeek row
///    can perfectly well carry no `catalog_id`, and losing its balance to a
///    refactor would be a silent regression. Nothing new can arrive this way:
///    a vendor added today is `openai`-typed, and an `openai` row with no
///    catalog id is a relay we must not probe.
/// 3. **The address**, by the same rule the migration used. This is what keeps
///    a hand-made row at a vendor's own URL from being worse off than one
///    created through the picker; `identify` refuses anything ambiguous and
///    anything that is not a vendor's address verbatim, so it cannot promote a
///    relay.
///
/// `None` is the ordinary answer and not an error: it is what every upstream
/// that publishes nothing gets, and what a relay gets.
pub fn balance_vendor(identity: ProviderIdentity<'_>) -> Option<BalanceVendor> {
    identity
        .catalog_id
        .and_then(BalanceVendor::from_catalog_id)
        .or_else(|| BalanceVendor::from_catalog_id(identity.provider_type))
        .or_else(|| {
            super::catalog::identify(identity.provider_type, identity.base_url).and_then(BalanceVendor::from_catalog_id)
        })
}

/// Whether asking is worth the request. Callers use it to decide whether to draw
/// the control at all — a button that always errors is worse than no button.
pub fn supports_balance(identity: ProviderIdentity<'_>) -> bool {
    balance_vendor(identity).is_some()
}

pub async fn fetch_balance(identity: ProviderIdentity<'_>, api_key: &str) -> Result<ProviderBalance, ProviderError> {
    let Some(vendor) = balance_vendor(identity) else {
        return Err(ProviderError::NotImplemented(format!(
            "{} does not publish an account balance",
            identity.catalog_id.unwrap_or(identity.provider_type)
        )));
    };
    match vendor {
        BalanceVendor::DeepSeek => fetch_deepseek_balance(identity.base_url, api_key).await,
        BalanceVendor::Moonshot => fetch_moonshot_balance(identity.base_url, api_key).await,
        BalanceVendor::SiliconFlow => fetch_siliconflow_balance(identity.base_url, api_key).await,
    }
}

/// The account endpoints sit beside the API root, not under its version.
///
/// DeepSeek accepts both `https://api.deepseek.com` and `.../v1` as a chat base
/// — the `v1` is there for OpenAI-compatible clients and means nothing to them —
/// but `/user/balance` exists only at the root. Left alone, a user who typed the
/// `v1` form gets a 404 from a button that works for everybody else. Moonshot
/// and SiliconFlow put their account routes *under* `/v1`, so they append it
/// back; stripping first is what makes both spellings of the base URL work.
fn account_root(base_url: &str) -> &str {
    base_url.trim_end_matches('/').trim_end_matches("/v1")
}

/// The host, lowercased, with scheme, userinfo, port and path removed.
fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    authority.split(':').next().unwrap_or("").to_ascii_lowercase()
}

/// What currency the figures are in, which neither Moonshot nor SiliconFlow
/// says anywhere in its response.
///
/// Both run a mainland site and an international one, on separate accounts with
/// keys their own documentation says are not interchangeable, and each site
/// publishes its prices in one currency: `.cn` in CNY, the other host in USD.
/// So the host is not a guess about the account — a key that works against this
/// address belongs to that site by construction. It is a label either way:
/// `is_low` compares every account against the one threshold regardless of
/// currency, so getting this wrong would misname a figure in an alert rather
/// than change whether the alert fires.
fn site_currency(base_url: &str) -> &'static str {
    if host_of(base_url).ends_with(".cn") {
        "CNY"
    } else {
        "USD"
    }
}

/// One authenticated GET, which is the whole of every account endpoint here.
async fn get_json(api: &'static str, url: String, api_key: &str) -> Result<Vec<u8>, ProviderError> {
    let transport = ReqwestTransport::shared();
    let mut req = Request::new(http::Method::GET, url);
    req.headers.insert(
        http::header::AUTHORIZATION,
        super::auth_header_value(&format!("Bearer {api_key}")),
    );
    req.headers
        .insert(http::header::ACCEPT, "application/json".parse().unwrap());

    let resp = transport.execute(req).await.inspect_err(|error| {
        tracing::error!(api, error = %error, "could not fetch the account balance");
    })?;
    Ok(resp.body.to_vec())
}

fn parse_body<T: for<'de> Deserialize<'de>>(api: &'static str, body: &[u8]) -> Result<T, ProviderError> {
    serde_json::from_slice(body).map_err(|error| {
        tracing::warn!(
            api,
            body_len = body.len(),
            error = %error,
            "the balance response was not in the expected shape"
        );
        ProviderError::Parse(error.to_string())
    })
}

// ---------------------------------------------------------------- DeepSeek

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
    let body = get_json("deepseek", format!("{}/user/balance", account_root(base_url)), api_key).await?;
    let parsed: DeepSeekBalanceDto = parse_body("deepseek", &body)?;
    warn_extra_fields("deepseek_balance", &parsed.extra);

    let mut accounts = Vec::with_capacity(parsed.balance_infos.len());
    for info in &parsed.balance_infos {
        warn_extra_fields("deepseek_balance_info", &info.extra);
        accounts.push(BalanceAccount {
            currency: info.currency.clone(),
            total_balance: required_amount(&info.total_balance, "total_balance")?,
            granted_balance: amount(info.granted_balance.as_deref(), "granted_balance")?,
            topped_up_balance: amount(info.topped_up_balance.as_deref(), "topped_up_balance")?,
        });
    }

    Ok(ProviderBalance {
        is_available: parsed.is_available,
        accounts,
    })
}

// ---------------------------------------------------------------- Moonshot

/// `data` is present only when `code` is 0, so it is optional here and its
/// absence on a success is a parse failure rather than an empty balance.
#[derive(Deserialize)]
struct MoonshotBalanceDto {
    code: i64,
    data: Option<MoonshotBalanceDataDto>,
    /// The string spelling of `code` (`"0x0"`), and `status`, which mirrors it.
    /// Named so the extra-field warning keeps meaning "the wire moved"; not
    /// read, because requiring a second success indicator to agree can only
    /// turn a good reply into a failure.
    #[serde(default, rename = "scode")]
    _scode: IgnoredAny,
    #[serde(default, rename = "status")]
    _status: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

/// Every amount here is a JSON **number**, which is the one shape `Decimal`
/// will not take. Held as the literal text `RawValue` preserves, because
/// `serde_json` turns a number into an f64 before any visitor of ours is
/// called, and f64 is forbidden for money in this codebase.
#[derive(Deserialize)]
struct MoonshotBalanceDataDto {
    available_balance: Box<serde_json::value::RawValue>,
    voucher_balance: Option<Box<serde_json::value::RawValue>>,
    cash_balance: Option<Box<serde_json::value::RawValue>>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

async fn fetch_moonshot_balance(base_url: &str, api_key: &str) -> Result<ProviderBalance, ProviderError> {
    let body = get_json(
        "moonshot",
        format!("{}/v1/users/me/balance", account_root(base_url)),
        api_key,
    )
    .await?;
    let parsed: MoonshotBalanceDto = parse_body("moonshot", &body)?;
    warn_extra_fields("moonshot_balance", &parsed.extra);
    moonshot_balance(&parsed, site_currency(base_url))
}

fn moonshot_balance(parsed: &MoonshotBalanceDto, currency: &str) -> Result<ProviderBalance, ProviderError> {
    if parsed.code != 0 {
        return Err(ProviderError::Parse(format!(
            "the balance endpoint answered code {}",
            parsed.code
        )));
    }
    let data = parsed
        .data
        .as_ref()
        .ok_or_else(|| ProviderError::Parse("the balance reply carried no figures".into()))?;
    warn_extra_fields("moonshot_balance_data", &data.extra);

    let available = json_number_amount(&data.available_balance, "available_balance")?;
    Ok(ProviderBalance {
        // The upstream's rule, not ours: its own documentation says a request
        // is refused with `exceeded_current_quota_error` once this reaches or
        // falls below zero. Reading it is quoting that, which is what this
        // field is for — unlike inferring "empty means dead" from a figure,
        // which for a postpaid account would be wrong.
        is_available: !available.is_zero() && !available.is_negative(),
        accounts: vec![BalanceAccount {
            currency: currency.to_string(),
            total_balance: available,
            granted_balance: optional_json_number_amount(data.voucher_balance.as_deref(), "voucher_balance")?,
            topped_up_balance: optional_json_number_amount(data.cash_balance.as_deref(), "cash_balance")?,
        }],
    })
}

// ------------------------------------------------------------- SiliconFlow

/// The envelope carries its own verdict in `code`, the way the Chinese group
/// bots do; HTTP 200 is not the answer on its own.
#[derive(Deserialize)]
struct SiliconFlowUserDto {
    code: i64,
    message: Option<String>,
    data: Option<SiliconFlowUserDataDto>,
    #[serde(default, rename = "status")]
    _status: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

/// Amounts are **strings** here. The three are related — `totalBalance` is
/// documented as `balance` (granted) plus `chargeBalance` (topped up) — and the
/// total is the one that decides anything, so it is the only required field.
#[derive(Deserialize)]
struct SiliconFlowUserDataDto {
    #[serde(rename = "totalBalance")]
    total_balance: String,
    /// Granted credit, despite the bare name. `chargeBalance` is what was paid
    /// in, which is the opposite of what "balance" reads as.
    balance: Option<String>,
    #[serde(rename = "chargeBalance")]
    charge_balance: Option<String>,
    status: Option<String>,
    #[serde(default, rename = "id")]
    _id: IgnoredAny,
    #[serde(default, rename = "name")]
    _name: IgnoredAny,
    #[serde(default, rename = "image")]
    _image: IgnoredAny,
    #[serde(default, rename = "email")]
    _email: IgnoredAny,
    #[serde(default, rename = "isAdmin")]
    _is_admin: IgnoredAny,
    #[serde(default, rename = "introduction")]
    _introduction: IgnoredAny,
    #[serde(default, rename = "role")]
    _role: IgnoredAny,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

/// The one value the documentation shows. Anything else is treated as a reason
/// to say something rather than a reason to stay quiet — see below.
const SILICONFLOW_STATUS_OK: &str = "normal";

async fn fetch_siliconflow_balance(base_url: &str, api_key: &str) -> Result<ProviderBalance, ProviderError> {
    let body = get_json(
        "siliconflow",
        format!("{}/v1/user/info", account_root(base_url)),
        api_key,
    )
    .await?;
    let parsed: SiliconFlowUserDto = parse_body("siliconflow", &body)?;
    warn_extra_fields("siliconflow_user", &parsed.extra);
    siliconflow_balance(&parsed, site_currency(base_url))
}

fn siliconflow_balance(parsed: &SiliconFlowUserDto, currency: &str) -> Result<ProviderBalance, ProviderError> {
    if parsed.code != 20000 {
        return Err(ProviderError::Parse(format!(
            "the account endpoint answered code {}{}",
            parsed.code,
            parsed
                .message
                .as_deref()
                .filter(|message| !message.is_empty())
                .map(|message| format!(" ({message})"))
                .unwrap_or_default()
        )));
    }
    let data = parsed
        .data
        .as_ref()
        .ok_or_else(|| ProviderError::Parse("the account reply carried no figures".into()))?;
    warn_extra_fields("siliconflow_user_data", &data.extra);

    // An unrecognised status is reported as unavailable *and* warned about.
    // The two directions are not symmetric for a watcher: a spurious alert is
    // read and dismissed, while a status this app has never heard of quietly
    // read as healthy is an account that stops working with nothing said.
    let status = data.status.as_deref().unwrap_or(SILICONFLOW_STATUS_OK);
    if status != SILICONFLOW_STATUS_OK {
        tracing::warn!(api = "siliconflow", status, "the account is not in the normal state");
    }

    Ok(ProviderBalance {
        is_available: status == SILICONFLOW_STATUS_OK,
        accounts: vec![BalanceAccount {
            currency: currency.to_string(),
            total_balance: required_amount(&data.total_balance, "totalBalance")?,
            granted_balance: amount(data.balance.as_deref(), "balance")?,
            topped_up_balance: amount(data.charge_balance.as_deref(), "chargeBalance")?,
        }],
    })
}

// ------------------------------------------------------------------ shared

/// One amount, parsed exactly.
///
/// **A negative figure is reported, not refused**, and that is a correction
/// rather than an oversight. This used to run `require_non_negative`, which
/// turns an overdrawn account — the single most urgent thing a balance watcher
/// can find — into a failed lookup, and a failed lookup is
/// `BalanceOutcome::Unknown`, which is silence. Moonshot's own documentation is
/// what turned it up: it says requests are refused once the balance reaches
/// *or falls below* zero, so below zero is a state it expects to be in.
///
/// A value that will not parse at all is still refused, because that is
/// evidence the field does not hold what this reader thinks it does, and from
/// there both alerting directions are wrong.
fn required_amount(raw: &str, field: &str) -> Result<Decimal, ProviderError> {
    // The upstream's own words go into the message. `Decimal` refuses more than
    // non-numbers — exponent notation and anything past `NUMERIC(38,18)` are
    // outside the contract every monetary value in this app is stored under —
    // and "is not a number" for a figure that plainly is one sends whoever
    // reads the log looking in the wrong place.
    let parsed = raw.trim().parse::<Decimal>().map_err(|error| {
        tracing::warn!(field, error = %error, "the balance is not a value this app can hold");
        ProviderError::Parse(format!("{field} is not a usable decimal: {error}"))
    })?;
    if parsed.is_negative() {
        tracing::warn!(field, "the account is overdrawn");
    }
    Ok(parsed)
}

fn amount(raw: Option<&str>, field: &str) -> Result<Option<Decimal>, ProviderError> {
    raw.map(|raw| required_amount(raw, field)).transpose()
}

/// A JSON number read as an exact decimal, through its literal text.
///
/// `RawValue` is the source: it is the only thing `serde_json` will hand over
/// before the value has been through an f64. A string here is refused rather
/// than accepted as a second spelling — if the wire changes shape that is worth
/// finding out about, and `Decimal`'s own serde is string-only, so the two
/// stances agree.
fn json_number_amount(raw: &serde_json::value::RawValue, field: &str) -> Result<Decimal, ProviderError> {
    let text = raw.get().trim();
    if text.starts_with('"') {
        return Err(ProviderError::Parse(format!(
            "{field} arrived as a string, not a number"
        )));
    }
    required_amount(text, field)
}

fn optional_json_number_amount(
    raw: Option<&serde_json::value::RawValue>,
    field: &str,
) -> Result<Option<Decimal>, ProviderError> {
    raw.filter(|raw| raw.get().trim() != "null")
        .map(|raw| json_number_amount(raw, field))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decimal(raw: &str) -> Decimal {
        raw.parse().unwrap()
    }

    fn identity<'a>(catalog_id: Option<&'a str>, provider_type: &'a str, base_url: &'a str) -> ProviderIdentity<'a> {
        ProviderIdentity::new(catalog_id, provider_type, base_url)
    }

    /// `RawValue` cannot be written as a literal, so it is parsed.
    fn raw(json: &str) -> Box<serde_json::value::RawValue> {
        serde_json::value::RawValue::from_string(json.to_string()).expect("a JSON literal")
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
                    total_balance: required_amount(&info.total_balance, "total_balance").unwrap(),
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
    /// endpoint — `/v1` is an OpenAI-compatibility affordance and DeepSeek's
    /// account routes do not live under it, while Moonshot's and
    /// SiliconFlow's do and are appended back.
    #[test]
    fn the_version_suffix_is_not_part_of_the_account_root() {
        assert_eq!(account_root("https://api.deepseek.com"), "https://api.deepseek.com");
        assert_eq!(account_root("https://api.deepseek.com/"), "https://api.deepseek.com");
        assert_eq!(account_root("https://api.deepseek.com/v1"), "https://api.deepseek.com");
        assert_eq!(account_root("https://api.deepseek.com/v1/"), "https://api.deepseek.com");
        assert_eq!(account_root("https://api.moonshot.cn/v1"), "https://api.moonshot.cn");
        assert_eq!(
            account_root("https://api.siliconflow.cn/v1/"),
            "https://api.siliconflow.cn"
        );
    }

    // ------------------------------------------------------------ resolution

    /// `catalog_id` is what a row created through the picker carries, and it is
    /// the only thing that can tell two OpenAI-compatible vendors apart.
    #[test]
    fn the_catalog_id_names_the_vendor_that_the_adapter_family_cannot() {
        assert_eq!(
            balance_vendor(identity(Some("moonshot"), "openai", "https://api.moonshot.cn/v1")),
            Some(BalanceVendor::Moonshot)
        );
        assert_eq!(
            balance_vendor(identity(Some("siliconflow"), "openai", "https://api.siliconflow.cn/v1")),
            Some(BalanceVendor::SiliconFlow)
        );
        assert_eq!(
            balance_vendor(identity(Some("openai"), "openai", "https://api.openai.com/v1")),
            None,
            "OpenAI withdrew the endpoint; the catalog id must not promote it"
        );
    }

    /// A DeepSeek row can legitimately carry no `catalog_id`: migration 40's
    /// backfill matches the bare host verbatim, and `account_root` exists
    /// because people also write the `/v1` form. Losing that row's balance to
    /// the re-keying would be a silent regression, so `provider_type` is the
    /// second source.
    #[test]
    fn a_deepseek_row_the_backfill_missed_keeps_its_balance() {
        assert_eq!(
            balance_vendor(identity(None, "deepseek", "https://api.deepseek.com/v1")),
            Some(BalanceVendor::DeepSeek)
        );
        assert!(supports_balance(identity(
            None,
            "deepseek",
            "https://api.deepseek.com/v1"
        )));
    }

    /// The `provider_type` fallback must never reach a vendor added later: an
    /// `openai` row with no catalog id is a relay, and probing one means
    /// sending its key to an account endpoint its operator never published.
    #[test]
    fn an_unidentified_relay_is_never_probed() {
        for url in [
            "https://codex-api.foxline.cn",
            "https://api.openai.com/v1/proxy",
            "https://api.moonshot.cn.evil.example/v1",
        ] {
            assert_eq!(balance_vendor(identity(None, "openai", url)), None, "{url}");
        }
    }

    /// A row typed in by hand at a vendor's own address is not worse off than
    /// one created from the picker — the same rule migration 40 used.
    #[test]
    fn a_vendors_own_address_identifies_it_without_a_catalog_id() {
        assert_eq!(
            balance_vendor(identity(None, "openai", "https://api.moonshot.cn/v1")),
            Some(BalanceVendor::Moonshot)
        );
        assert_eq!(
            balance_vendor(identity(None, "openai", "https://api.siliconflow.cn/v1/")),
            Some(BalanceVendor::SiliconFlow)
        );
    }

    #[test]
    fn the_vendors_that_publish_nothing_are_not_offered_the_control() {
        for other in ["openai", "anthropic", "xai", "google"] {
            assert!(
                !supports_balance(identity(Some(other), other, "https://example.invalid")),
                "{other} publishes no balance"
            );
        }
    }

    #[test]
    fn every_vendor_round_trips_through_its_catalog_id() {
        for vendor in [
            BalanceVendor::DeepSeek,
            BalanceVendor::Moonshot,
            BalanceVendor::SiliconFlow,
        ] {
            assert_eq!(BalanceVendor::from_catalog_id(vendor.catalog_id()), Some(vendor));
        }
    }

    // -------------------------------------------------------------- currency

    #[test]
    fn the_site_decides_the_currency_because_the_reply_does_not() {
        assert_eq!(site_currency("https://api.moonshot.cn/v1"), "CNY");
        assert_eq!(site_currency("https://api.siliconflow.cn"), "CNY");
        assert_eq!(site_currency("https://api.moonshot.ai/v1"), "USD");
        assert_eq!(site_currency("https://api.siliconflow.com/v1"), "USD");
        // A port, a path and a capital letter do not change the host.
        assert_eq!(site_currency("HTTPS://API.MOONSHOT.CN:443/v1"), "CNY");
    }

    // -------------------------------------------------------------- Moonshot

    fn moonshot(json: &str, currency: &str) -> Result<ProviderBalance, ProviderError> {
        let dto: MoonshotBalanceDto = serde_json::from_str(json).expect("a balance body");
        moonshot_balance(&dto, currency)
    }

    /// The response from Moonshot's own documentation, verbatim.
    #[test]
    fn the_documented_moonshot_response_parses() {
        let balance = moonshot(
            r#"{"code":0,"data":{"available_balance":49.58894,"voucher_balance":46.58893,
                "cash_balance":3.00001},"scode":"0x0","status":true}"#,
            "CNY",
        )
        .expect("a balance");
        assert!(balance.is_available);
        assert_eq!(balance.accounts.len(), 1);
        assert_eq!(balance.accounts[0].currency, "CNY");
        assert_eq!(balance.accounts[0].total_balance, decimal("49.58894"));
        assert_eq!(balance.accounts[0].granted_balance, Some(decimal("46.58893")));
        assert_eq!(balance.accounts[0].topped_up_balance, Some(decimal("3.00001")));
    }

    /// The reason `RawValue` is in the wire DTO at all. Nineteen significant
    /// digits is past what an f64 can hold, so a reader that let `serde_json`
    /// produce one — which it does the instant it sees a number, before any
    /// visitor of ours runs — reports `1234567890.1234567` instead.
    ///
    /// Mutation check: typing the field `f64` and formatting it turns this red
    /// and leaves every other test in the file green.
    #[test]
    fn a_numeric_amount_keeps_every_digit_it_arrived_with() {
        let balance =
            moonshot(r#"{"code":0,"data":{"available_balance":1234567890.123456789}}"#, "USD").expect("a balance");
        assert_eq!(balance.accounts[0].total_balance, decimal("1234567890.123456789"));
        assert_ne!(
            balance.accounts[0].total_balance,
            decimal("1234567890.1234567"),
            "this is the figure an f64 round trip produces"
        );
    }

    /// `Decimal` is the `NUMERIC(38,18)` boundary every monetary value in this
    /// app crosses, and a figure outside it is refused rather than rounded —
    /// including exponent notation, which is a legal JSON number and not a
    /// legal decimal here. The message says which, because "is not a number"
    /// about something that plainly is one sends the reader the wrong way.
    #[test]
    fn a_figure_outside_the_decimal_contract_says_why() {
        for literal in ["4.958894e1", "0.1000000000000000055511151231257827"] {
            let error = json_number_amount(&raw(literal), "available_balance").expect_err("a refusal");
            match error {
                ProviderError::Parse(text) => assert!(
                    text.contains("available_balance is not a usable decimal"),
                    "{literal}: {text}"
                ),
                other => panic!("{other:?}"),
            }
        }
    }

    /// Zero is the documented point at which this upstream starts refusing
    /// requests, so it is an unavailable account rather than a low one — and
    /// that reading is the upstream's, quoted, not an inference from the figure.
    #[test]
    fn moonshot_at_zero_is_unavailable_rather_than_merely_low() {
        let flat = moonshot(r#"{"code":0,"data":{"available_balance":0}}"#, "CNY").expect("a balance");
        assert!(!flat.is_available);
        assert!(
            flat.is_low(&decimal("0")),
            "a zero threshold still reports a dead account"
        );

        let alive = moonshot(r#"{"code":0,"data":{"available_balance":0.01}}"#, "CNY").expect("a balance");
        assert!(alive.is_available);
        assert!(!alive.is_low(&decimal("0")));
    }

    /// HTTP 200 is not the answer: the envelope carries its own verdict, and a
    /// failure read as a balance is a zero nobody owes.
    #[test]
    fn a_moonshot_envelope_error_is_not_an_empty_account() {
        let error = moonshot(r#"{"code":1,"scode":"0x1","status":false}"#, "CNY").expect_err("an error");
        assert!(matches!(error, ProviderError::Parse(_)), "{error:?}");
        let empty = moonshot(r#"{"code":0,"status":true}"#, "CNY").expect_err("an error");
        assert!(matches!(empty, ProviderError::Parse(_)), "{empty:?}");
    }

    /// A number that turned into a string is the wire moving, and it fails
    /// rather than being quietly accepted as a second spelling.
    #[test]
    fn a_moonshot_amount_that_became_a_string_is_refused() {
        let error = json_number_amount(&raw(r#""1.00""#), "available_balance").expect_err("a refusal");
        assert!(matches!(error, ProviderError::Parse(_)), "{error:?}");
        assert_eq!(
            json_number_amount(&raw("1.00"), "available_balance").unwrap(),
            decimal("1")
        );
    }

    /// A null optional is absent, not a zero — the two mean different things
    /// in the split a summary prints.
    #[test]
    fn a_null_component_is_absent_rather_than_zero() {
        let balance = moonshot(
            r#"{"code":0,"data":{"available_balance":5,"voucher_balance":null,"cash_balance":5}}"#,
            "CNY",
        )
        .expect("a balance");
        assert_eq!(balance.accounts[0].granted_balance, None);
        assert_eq!(balance.accounts[0].topped_up_balance, Some(decimal("5")));
    }

    // ----------------------------------------------------------- SiliconFlow

    fn siliconflow(json: &str, currency: &str) -> Result<ProviderBalance, ProviderError> {
        let dto: SiliconFlowUserDto = serde_json::from_str(json).expect("a user body");
        siliconflow_balance(&dto, currency)
    }

    /// The response from SiliconFlow's own documentation, verbatim.
    #[test]
    fn the_documented_siliconflow_response_parses() {
        let balance = siliconflow(
            r#"{"code":20000,"message":"OK","status":true,"data":{"id":"userid","name":"username",
                "image":"user_avatar_image_url","email":"user_email_address","isAdmin":false,
                "balance":"0.88","status":"normal","introduction":"","role":"",
                "chargeBalance":"88.00","totalBalance":"88.88"}}"#,
            "CNY",
        )
        .expect("a balance");
        assert!(balance.is_available);
        assert_eq!(balance.accounts.len(), 1);
        assert_eq!(balance.accounts[0].currency, "CNY");
        assert_eq!(balance.accounts[0].total_balance, decimal("88.88"));
        // `balance` is the granted half despite its name, and `chargeBalance`
        // is what was paid in. Swapping them would report an account propped up
        // by expiring credit as a topped-up one.
        assert_eq!(balance.accounts[0].granted_balance, Some(decimal("0.88")));
        assert_eq!(balance.accounts[0].topped_up_balance, Some(decimal("88.00")));
    }

    /// A status this app has never heard of is reported as unavailable. The two
    /// directions are not symmetric: a spurious alert is read and dismissed,
    /// while an unknown status read as healthy is an account that stops working
    /// with nothing said.
    #[test]
    fn an_unknown_siliconflow_status_is_not_assumed_healthy() {
        let frozen = siliconflow(
            r#"{"code":20000,"status":true,"data":{"totalBalance":"88.88","status":"suspended"}}"#,
            "CNY",
        )
        .expect("a balance");
        assert!(!frozen.is_available);
        assert!(frozen.is_low(&decimal("0")));
    }

    #[test]
    fn a_siliconflow_envelope_error_is_not_an_empty_account() {
        let error =
            siliconflow(r#"{"code":40001,"message":"invalid token","status":false}"#, "CNY").expect_err("an error");
        match error {
            ProviderError::Parse(text) => assert!(text.contains("invalid token"), "{text}"),
            other => panic!("{other:?}"),
        }
    }

    /// An overdrawn account is the loudest thing this can find, so it is
    /// reported rather than refused. Refusing is `BalanceOutcome::Unknown`,
    /// which is silence — at the one moment silence is most expensive.
    ///
    /// Mutation check: restoring `require_non_negative` inside
    /// `required_amount` turns this and its Moonshot twin red.
    #[test]
    fn an_overdrawn_account_is_reported_rather_than_refused() {
        let owed = siliconflow(
            r#"{"code":20000,"status":true,"data":{"totalBalance":"-1.50","status":"normal"}}"#,
            "CNY",
        )
        .expect("a balance rather than a refusal");
        assert_eq!(owed.accounts[0].total_balance, decimal("-1.50"));
        assert!(owed.is_low(&decimal("0.01")));

        let moonshot_owed = moonshot(r#"{"code":0,"data":{"available_balance":-0.2}}"#, "CNY").expect("a balance");
        assert!(
            !moonshot_owed.is_available,
            "below zero is past the point this upstream stops serving"
        );
        assert!(moonshot_owed.is_low(&decimal("0")));
    }

    /// A value that is not a number at all is still refused: that is evidence
    /// the field does not hold what this reader thinks, and from there both
    /// alerting directions are wrong.
    #[test]
    fn a_non_numeric_total_is_refused() {
        let error = siliconflow(
            r#"{"code":20000,"status":true,"data":{"totalBalance":"unavailable","status":"normal"}}"#,
            "CNY",
        )
        .expect_err("a refusal");
        assert!(matches!(error, ProviderError::Parse(_)), "{error:?}");
    }
}

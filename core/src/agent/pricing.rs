use crate::db::models::model_config::ModelConfigRow;
use crate::decimal::Decimal;
use crate::provider::TokenUsage;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PricingError {
    #[error("unknown billing mode: {0}")]
    UnknownBillingMode(String),
    #[error("unknown transport profile: {0}")]
    UnknownTransportProfile(String),
    #[error("invalid price tier table: {0}")]
    InvalidTierTable(String),
}

/// Whether a request owes a per-request price at all.
///
/// It lives here, beside `compute_cost`, because it is a pricing rule and not a
/// reporting preference: it decides whether a rate is *owed*, which has to be
/// settled before anything goes looking for one. `db::ops::usage::resolve` falls
/// back to today's `model_configs` when a row carries no snapshotted rate, so a
/// subscription request under a provider that happens to have a price on file
/// would otherwise be billed at it — "no rate stored" and "no rate exists" are
/// indistinguishable without this.
///
/// Snapshotted onto the audit row rather than joined from the provider, for the
/// reason migration 30 gives about rates: this is a fact about the past.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingMode {
    /// A price is owed. A missing rate is a misconfiguration and still counts
    /// into `unpriced_messages`.
    #[default]
    Metered,
    /// Tokens are ours to count, but no per-request rate exists — the request
    /// draws on a plan bought elsewhere. Not unpriced; there is nothing to find.
    Subscription,
    /// The cost lands in someone else's ledger and never enters our totals.
    ///
    /// Live ACP transcripts use the ordinary audit path so their token counts
    /// remain visible, but the hosted process pays for those requests itself.
    External,
}

impl BillingMode {
    /// Whether a cost may be computed for this request.
    ///
    /// The one question every reporting path must ask before resolving rates.
    pub fn is_priced(self) -> bool {
        matches!(self, Self::Metered)
    }

    /// Whether a missing rate is worth telling the user about.
    ///
    /// False for the two modes where no rate was ever expected — counting those
    /// into `unpriced_messages` produces a warning nobody can act on.
    pub fn expects_a_price(self) -> bool {
        matches!(self, Self::Metered)
    }

    /// How a provider's transport is paid for.
    ///
    /// Keyed on `transport_profile` rather than on the credential: what decides
    /// this is *which service the request reaches*, not how we authenticated to
    /// it. Both ChatGPT logins — the CLI's session and one made in this app —
    /// draw on the same plan, and an API key reaching the same backend would
    /// too.
    ///
    /// Today only `standard` exists, so every row is `Metered` and this mapping
    /// is inert; `chatgpt_codex` arrives with the Codex transport.
    pub fn for_transport(transport_profile: &str) -> Result<Self, PricingError> {
        match transport_profile {
            "standard" => Ok(Self::Metered),
            "chatgpt_codex" => Ok(Self::Subscription),
            unknown => Err(PricingError::UnknownTransportProfile(unknown.to_owned())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Metered => "metered",
            Self::Subscription => "subscription",
            Self::External => "external",
        }
    }
}

impl std::str::FromStr for BillingMode {
    type Err = PricingError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "metered" => Ok(Self::Metered),
            "subscription" => Ok(Self::Subscription),
            "external" => Ok(Self::External),
            unknown => Err(PricingError::UnknownBillingMode(unknown.to_owned())),
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RequestCost {
    pub input_cost: Decimal,
    pub output_cost: Decimal,
    pub cache_cost: Decimal,
    /// What the provider's own tools charged, which is not a token cost.
    ///
    /// Its own slot rather than folded into the others because it does not
    /// divide by anything they do: a turn that cost $0.01 of tokens and $0.015
    /// of searching is a different decision from one that spent it all on
    /// tokens, and a single total cannot be taken apart again.
    pub tool_cost: Decimal,
    pub total_cost: Decimal,
}

/// A turn is many requests, and with tiered pricing they are not all priced the
/// same — so the costs add up, the tokens do not.
impl std::ops::AddAssign for RequestCost {
    fn add_assign(&mut self, other: Self) {
        self.input_cost += other.input_cost;
        self.output_cost += other.output_cost;
        self.cache_cost += other.cache_cost;
        self.tool_cost += other.tool_cost;
        self.total_cost += other.total_cost;
    }
}

/// What this turn's model costs, at every prompt size it might reach.
///
/// Carried into the turn loop because that loop is the only place that sees each
/// request's own prompt size. A turn of five 50k requests and a turn of one 250k
/// request have identical totals and different bills, and totals are all that
/// survives to the end — so a tier picked afterwards from `progress.input_tokens`
/// would put the first turn in the second one's bracket and overcharge it by the
/// whole premium.
///
/// Resolved once per turn rather than per round: the tier table is parsed here,
/// not on every usage event.
#[derive(Debug, Clone, Default)]
pub struct TurnPricing {
    base: Prices,
    tiers: Vec<PriceTier>,
}

impl TurnPricing {
    /// `None` when no part of a model's cost is known. A provider-tool rate is
    /// useful on its own: live progress can still show that known lower bound
    /// while the usage report flags the missing token rates.
    pub fn of(config: &ModelConfigRow) -> Result<Option<Self>, PricingError> {
        let base = Prices::of(config);
        if !base.known() && base.server_tool_price.is_none() {
            return Ok(None);
        }
        let tiers = if base.known() {
            parse_tiers(config.pricing_tiers.as_deref())?
        } else {
            Vec::new()
        };
        Ok(Some(Self {
            base,
            // A tier cannot turn blank base token rates into known ones. Keep
            // tool-only pricing tool-only at every prompt size.
            tiers,
        }))
    }

    /// The rates for one request of this size. See `Prices::for_prompt` — same
    /// rule, against an already-parsed table.
    pub fn for_prompt(&self, prompt_tokens: i64) -> Prices {
        with_tool_rate(tier_for(&self.tiers, prompt_tokens), self.base.clone())
    }
}

/// A tier's token rates, keeping the base's per-call tool rate.
///
/// Tiers describe what a *large prompt* costs. A tool invocation costs the same
/// whatever the prompt was, so a tier that omitted it — which is all of them —
/// would otherwise switch the charge off for exactly the long requests most
/// likely to have searched.
fn with_tool_rate(tier: Option<Prices>, base: Prices) -> Prices {
    match tier {
        Some(mut prices) => {
            prices.server_tool_price = base.server_tool_price;
            prices
        }
        None => base,
    }
}

/// The highest tier this prompt reaches, or `None` for the base rates.
///
/// One implementation, called from both places that choose a tier. They were
/// written twice and had already drifted: one of them applied a tier to a model
/// whose base rates nobody had filled in, which is how the same model came to be
/// "unpriced" on short requests and priced on long ones.
fn tier_for(tiers: &[PriceTier], prompt_tokens: i64) -> Option<Prices> {
    tiers
        .iter()
        .take_while(|tier| tier.min_prompt_tokens <= prompt_tokens)
        .last()
        .map(PriceTier::prices)
}

/// The four rates a bill needs, apart from wherever they were read.
///
/// A turn takes them off the model's configuration. A report takes them off the
/// audit row, where they were copied at the time — and those two disagree by
/// design, because a price edited last week must not reprice last month. Making
/// this a type rather than passing `&ModelConfigRow` around is what lets both feed
/// the same formula instead of growing a second one.
///
/// `None` on either cache rate means "priced like ordinary input", which is what
/// most upstreams do and what every row written before migration 30 recorded.
#[derive(Debug, Clone, Default)]
pub struct Prices {
    pub input_price: Option<Decimal>,
    pub output_price: Option<Decimal>,
    pub cache_read_price: Option<Decimal>,
    pub cache_write_price: Option<Decimal>,
    /// What one provider-side tool invocation costs, per **thousand** calls —
    /// the unit the upstreams publish it in. `None` means nobody has said, which
    /// is not zero: a searching turn priced at nothing is under-reported, not
    /// free. Never carried by a tier, because a tier is about prompt size and
    /// this charge does not vary with it.
    pub server_tool_price: Option<Decimal>,
}

/// A rate set that takes over once the prompt is large enough.
///
/// Three upstreams price this way and all three do the same surprising thing:
/// the threshold is measured against the **whole prompt**, cached part
/// included, and crossing it re-prices the *entire* request rather than the
/// excess. A 201k-token prompt on `grok-4.6` costs double on all 201k, not
/// double on the last thousand. Written down here because the intuitive reading
/// — a bracket, like income tax — is the wrong one, and a formula built on it
/// would be quietly cheap by almost exactly the base rate.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PriceTier {
    /// The prompt size at which this tier starts applying, inclusive.
    pub min_prompt_tokens: i64,
    pub input_price: Decimal,
    pub output_price: Decimal,
    pub cache_read_price: Option<Decimal>,
    pub cache_write_price: Option<Decimal>,
}

impl<'de> serde::Deserialize<'de> for PriceTier {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // `Option<T>` normally treats a missing field as `None`. Stored tier
        // documents do not: nullable prices must be present as an explicit JSON
        // null so schema drift cannot masquerade as an intentionally blank rate.
        struct RequiredNullableDecimal(Option<Decimal>);

        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum RequiredNullableDecimalValue {
            Value(Decimal),
            Null(()),
        }

        impl<'de> serde::Deserialize<'de> for RequiredNullableDecimal {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                Ok(
                    match <RequiredNullableDecimalValue as serde::Deserialize>::deserialize(deserializer)? {
                        RequiredNullableDecimalValue::Value(value) => Self(Some(value)),
                        RequiredNullableDecimalValue::Null(()) => Self(None),
                    },
                )
            }
        }

        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct PriceTierDocument {
            min_prompt_tokens: i64,
            input_price: Decimal,
            output_price: Decimal,
            cache_read_price: RequiredNullableDecimal,
            cache_write_price: RequiredNullableDecimal,
        }

        let document = <PriceTierDocument as serde::Deserialize>::deserialize(deserializer)?;
        Ok(Self {
            min_prompt_tokens: document.min_prompt_tokens,
            input_price: document.input_price,
            output_price: document.output_price,
            cache_read_price: document.cache_read_price.0,
            cache_write_price: document.cache_write_price.0,
        })
    }
}

impl PriceTier {
    /// A tier states token rates and nothing else. `server_tool_price` is filled in by
    /// the caller from the base rates — a per-call charge does not vary with how
    /// long the prompt was, and no upstream prices it that way.
    fn prices(&self) -> Prices {
        Prices {
            input_price: Some(self.input_price.clone()),
            output_price: Some(self.output_price.clone()),
            cache_read_price: self.cache_read_price.clone(),
            cache_write_price: self.cache_write_price.clone(),
            server_tool_price: None,
        }
    }
}

/// Read and validate the stored tier table, returning it in ascending order.
///
/// The column is hand-editable and reaches here from a form, so malformed JSON,
/// unknown fields, non-positive or duplicate thresholds, and invalid monetary
/// values are errors. Sorting is normalization, not compatibility: order carries
/// no meaning in the contract, while each threshold remains unique and explicit.
pub fn parse_tiers(raw: Option<&str>) -> Result<Vec<PriceTier>, PricingError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(PricingError::InvalidTierTable(
            "empty string is not a tier table".into(),
        ));
    }
    let tiers = serde_json::from_str::<Vec<PriceTier>>(raw)
        .map_err(|error| PricingError::InvalidTierTable(error.to_string()))?;
    validate_tiers(tiers)
}

pub fn validate_tiers(mut tiers: Vec<PriceTier>) -> Result<Vec<PriceTier>, PricingError> {
    for tier in &tiers {
        if tier.min_prompt_tokens <= 0 {
            return Err(PricingError::InvalidTierTable(format!(
                "min_prompt_tokens must be positive, got {}",
                tier.min_prompt_tokens
            )));
        }
        for (field, value) in [
            ("input_price", Some(&tier.input_price)),
            ("output_price", Some(&tier.output_price)),
            ("cache_read_price", tier.cache_read_price.as_ref()),
            ("cache_write_price", tier.cache_write_price.as_ref()),
        ] {
            if value.is_some_and(Decimal::is_negative) {
                return Err(PricingError::InvalidTierTable(format!("{field} must be non-negative")));
            }
        }
    }
    tiers.sort_by_key(|tier| tier.min_prompt_tokens);
    if tiers
        .windows(2)
        .any(|pair| pair[0].min_prompt_tokens == pair[1].min_prompt_tokens)
    {
        return Err(PricingError::InvalidTierTable("duplicate min_prompt_tokens".into()));
    }
    Ok(tiers)
}

impl Prices {
    pub fn of(config: &ModelConfigRow) -> Self {
        Self {
            input_price: config.input_price.clone(),
            output_price: config.output_price.clone(),
            cache_read_price: config.cache_read_price.clone(),
            cache_write_price: config.cache_write_price.clone(),
            server_tool_price: config.server_tool_price.clone(),
        }
    }

    /// The rates that apply to a request with this prompt size.
    ///
    /// The highest tier whose threshold the prompt reaches, or the base rates
    /// when it reaches none — which is every model that does not price this way
    /// and every row written before the column existed.
    ///
    /// `prompt_tokens` is the whole prompt including the cached part, because
    /// that is what the upstreams measure. Passing the uncached remainder
    /// instead would put a heavily-cached 400k conversation back in the cheap
    /// tier, which is the case most likely to arise and most expensive to get
    /// wrong.
    /// A model whose base rates are blank stays unpriced at *every* size, tiers
    /// or no tiers. Filling in only the long-context row — easy to do, since it
    /// is the row that surprises people — otherwise made one model report as two
    /// things at once: its short requests counted into `unpriced_messages` while
    /// its long ones were billed, and the turn showed no cost either way.
    pub fn for_prompt(config: &ModelConfigRow, prompt_tokens: i64) -> Result<Self, PricingError> {
        let base = Self::of(config);
        if !base.known() {
            return Ok(base);
        }
        Ok(with_tool_rate(
            tier_for(&parse_tiers(config.pricing_tiers.as_deref())?, prompt_tokens),
            base,
        ))
    }

    /// Whether anyone has actually said what this model costs.
    ///
    /// `None` is unknown and an explicit Decimal zero is free. Keeping those two
    /// states separate is what lets reports distinguish missing configuration
    /// from a provider that genuinely charges nothing.
    pub fn known(&self) -> bool {
        self.input_price.is_some() && self.output_price.is_some()
    }
}

/// What one request cost, in whatever currency the model's prices are quoted in.
///
/// The prompt is split three ways because the three parts bill at three rates,
/// and every prompt token belongs to exactly one of them:
///
/// ```text
/// uncached = prompt_tokens - cache_read_tokens - cache_write_tokens
/// ```
///
/// The previous formula charged `input_price` on the *whole* prompt and then
/// `cache_read_price` on the cached part on top, so a cached token was billed twice —
/// once of them at full rate. It stayed dormant only because the single caller
/// hard-coded the cache fields to `None`; with the adapters filling them in, a
/// DeepSeek turn at a 90% hit rate would have reported nearly six times its real
/// cost, and that number is user-facing — it ships in the stop event's
/// `cost_breakdown`.
pub fn compute_cost(usage: &TokenUsage, prices: &Prices) -> RequestCost {
    cost_of(
        &BilledTokens {
            uncached_input: usage.uncached_prompt_tokens() as i64,
            cache_read: usage.cache_read_tokens.unwrap_or(0) as i64,
            cache_write: usage.cache_write_tokens.unwrap_or(0) as i64,
            output: usage.completion_tokens.unwrap_or(0) as i64,
            server_tool_calls: usage.billable_tool_calls.unwrap_or(0) as i64,
        },
        prices,
    )
}

/// The prompt already split into the three parts that bill at three rates.
///
/// Wider than `TokenUsage`, which reports one request and so fits in `i32`. A
/// month of requests does not — two billion tokens is a fortnight for a busy
/// deployment — and a report that had to squeeze its groups back through `i32`
/// would wrap into a negative bill rather than fail. So the formula is written
/// over `i64` and `compute_cost` is the narrow door into it.
#[derive(Debug, Clone, Copy, Default)]
pub struct BilledTokens {
    pub uncached_input: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub output: i64,
    /// Billable provider-side tool invocations. Not a token count; see
    /// `TokenUsage::billable_tool_calls`.
    pub server_tool_calls: i64,
}

impl BilledTokens {
    /// The same split `TokenUsage::uncached_prompt_tokens` makes, over totals
    /// rather than one request — including its saturation, because a provider
    /// that over-reports its cache does so in the aggregate too and a negative
    /// token count would print a negative price.
    pub fn from_totals(prompt: i64, output: i64, cache_read: i64, cache_write: i64, server_tool_calls: i64) -> Self {
        Self {
            uncached_input: prompt.saturating_sub(cache_read).saturating_sub(cache_write).max(0),
            cache_read,
            cache_write,
            output,
            server_tool_calls,
        }
    }
}

pub fn cost_of(tokens: &BilledTokens, prices: &Prices) -> RequestCost {
    let uncached = Decimal::from(tokens.uncached_input);
    let cache_read = Decimal::from(tokens.cache_read);
    let cache_write = Decimal::from(tokens.cache_write);
    let output = Decimal::from(tokens.output);
    let per_million: Decimal = "0.000001".parse().expect("constant decimal");
    let per_thousand: Decimal = "0.001".parse().expect("constant decimal");

    // A blank cache price means "this model prices a cache read like ordinary
    // input". That reading can only ever over-report, which is the safe
    // direction for a number someone makes spending decisions on.
    let input_price = prices.input_price.clone().unwrap_or_default();
    let output_price = prices.output_price.clone().unwrap_or_default();
    let read_price = prices.cache_read_price.clone().unwrap_or_else(|| input_price.clone());
    // Anthropic charges 1.25x input for a five-minute cache entry and 2x for an
    // hour. A blank column means this upstream charges no premium, which is true
    // of every provider except that one — and the same reading every row written
    // before migration 30 already had.
    let write_price = prices.cache_write_price.clone().unwrap_or_else(|| input_price.clone());

    let input_cost = uncached * input_price * per_million.clone();
    // Both cache legs report under one heading because the breakdown has three
    // slots and "input" there means the part that paid full price. Splitting
    // writes out would need a fourth slot to say something nobody can act on.
    let cache_cost = (cache_read * read_price + cache_write * write_price) * per_million;
    let output_cost = output * output_price * "0.000001".parse::<Decimal>().expect("constant decimal");
    // Per *thousand*, not per million: that is the unit the upstreams publish an
    // invocation charge in, and the column stores it that way so nobody has to
    // convert while copying it off a pricing page. An unpriced rate contributes
    // nothing — the calls still happened, and `Prices::known` is what decides
    // whether the whole row counts as unpriced.
    let tool_cost =
        Decimal::from(tokens.server_tool_calls) * prices.server_tool_price.clone().unwrap_or_default() * per_thousand;

    let total_cost = input_cost.clone() + output_cost.clone() + cache_cost.clone() + tool_cost.clone();

    RequestCost {
        input_cost,
        output_cost,
        cache_cost,
        tool_cost,
        total_cost,
    }
}

pub fn has_pricing(config: &ModelConfigRow) -> bool {
    Prices::of(config).known()
}

#[cfg(test)]
mod pricing_tests {
    use super::*;

    fn decimal(raw: &str) -> Decimal {
        raw.parse().unwrap()
    }

    fn mock_config(input: Option<&str>, output: Option<&str>, cache: Option<&str>) -> ModelConfigRow {
        priced(input, output, cache, None)
    }

    fn priced(
        input: Option<&str>,
        output: Option<&str>,
        cache: Option<&str>,
        cache_write: Option<&str>,
    ) -> ModelConfigRow {
        ModelConfigRow {
            id: "test".into(),
            provider_id: "p".into(),
            model_id: "m".into(),
            display_name: None,
            context_window: 128000,
            compact_threshold: 100000,
            max_output_tokens: None,
            input_price: input.map(decimal),
            output_price: output.map(decimal),
            cache_read_price: cache.map(decimal),
            cache_write_price: cache_write.map(decimal),
            created_at: 0,
            updated_at: 0,
            capability_overrides: None,
            pricing_tiers: None,
            server_tools: None,
            server_tool_price: None,
        }
    }

    /// A provider that reports no caching bills the whole prompt as input.
    #[test]
    fn test_basic_cost() {
        let usage = TokenUsage {
            prompt_tokens: Some(1_000_000),
            completion_tokens: Some(1_000_000),
            total_tokens: Some(2_000_000),
            cache_read_tokens: None,
            cache_write_tokens: None,
            billable_tool_calls: None,
        };
        let config = mock_config(Some("15"), Some("60"), None);
        let cost = compute_cost(&usage, &Prices::of(&config));
        assert_eq!(cost.input_cost, decimal("15"));
        assert_eq!(cost.output_cost, decimal("60"));
        assert_eq!(cost.total_cost, decimal("75"));
        assert_eq!(serde_json::to_value(&cost).unwrap()["total_cost"], "75");
    }

    /// The cached part is billed at the cache rate *instead of*, not in addition
    /// to, the input rate.
    ///
    /// The old formula charged the whole prompt at `input_price` and then added
    /// the cached part again at `cache_read_price`, which on these numbers came to
    /// 16.35 against a real cost of 2.85. The explicit `< 15.0` below is there to
    /// fail loudly if anyone reintroduces that shape: 15.0 is what the full
    /// prompt alone would cost, so a total at or above it means the discount was
    /// not applied at all.
    #[test]
    fn a_cached_token_is_billed_once_not_twice() {
        let usage = TokenUsage {
            prompt_tokens: Some(1_000_000),
            completion_tokens: None,
            total_tokens: None,
            cache_read_tokens: Some(900_000),
            cache_write_tokens: None,
            billable_tool_calls: None,
        };
        let config = mock_config(Some("15"), Some("60"), Some("1.5"));
        let cost = compute_cost(&usage, &Prices::of(&config));
        assert_eq!(cost.input_cost, decimal("1.5"), "100k uncached at 15/M");
        assert_eq!(cost.cache_cost, decimal("1.35"), "900k read at 1.5/M");
        assert_eq!(cost.total_cost, decimal("2.85"));
        assert!(cost.total_cost < decimal("15"), "the old formula gave 16.35 here");
    }

    fn wrote_100k() -> TokenUsage {
        TokenUsage {
            prompt_tokens: Some(100_000),
            completion_tokens: None,
            total_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: Some(100_000),
            billable_tool_calls: None,
        }
    }

    /// A blank write price means the upstream charges no premium, which is what
    /// every provider except Anthropic does — and what every row written before
    /// migration 30 was already billed at.
    #[test]
    fn a_cache_write_with_no_price_of_its_own_costs_what_input_costs() {
        let config = mock_config(Some("15"), Some("60"), Some("1.5"));
        let cost = compute_cost(&wrote_100k(), &Prices::of(&config));
        assert_eq!(cost.input_cost, Decimal::zero(), "nothing was uncached");
        assert_eq!(cost.cache_cost, decimal("1.5"), "100k written at the input rate");
    }

    /// The premium the column exists for. Anthropic's five-minute entry is
    /// 1.25x input; billing it as ordinary input understates the run by exactly
    /// the difference, which is what this asserts is no longer happening.
    #[test]
    fn a_cache_write_is_billed_at_its_own_price_when_there_is_one() {
        let config = priced(Some("15"), Some("60"), Some("1.5"), Some("18.75"));
        let cost = compute_cost(&wrote_100k(), &Prices::of(&config));
        assert_eq!(cost.cache_cost, decimal("1.875"), "100k written at 18.75/M");
        assert!(
            cost.cache_cost > decimal("1.5"),
            "the premium is what distinguishes this from input"
        );
    }

    /// A provider contradicting itself must not produce a negative bill.
    #[test]
    fn an_over_reported_cache_count_cannot_produce_a_negative_bill() {
        let usage = TokenUsage {
            prompt_tokens: Some(100),
            completion_tokens: None,
            total_tokens: None,
            cache_read_tokens: Some(9_999),
            cache_write_tokens: None,
            billable_tool_calls: None,
        };
        let config = mock_config(Some("15"), Some("60"), Some("1.5"));
        let cost = compute_cost(&usage, &Prices::of(&config));
        assert!(cost.input_cost >= Decimal::zero());
        assert!(cost.total_cost >= Decimal::zero());
    }

    #[test]
    fn explicit_zero_is_known_free_pricing() {
        let config = mock_config(Some("0"), Some("0"), None);
        assert!(has_pricing(&config));
        assert!(!has_pricing(&mock_config(None, None, None)));
    }

    // --- tiered pricing ---

    /// xAI's actual `grok-4.6` table: everything doubles above a 200k prompt.
    fn grok() -> ModelConfigRow {
        let mut config = priced(Some("2"), Some("6"), Some("0.5"), None);
        config.pricing_tiers = Some(
            r#"[{"min_prompt_tokens":200000,"input_price":"4","output_price":"12","cache_read_price":"1","cache_write_price":null}]"#.into(),
        );
        config
    }

    /// The whole point, and the part that is not obvious: crossing the threshold
    /// re-prices the *entire* prompt, not the part above it. A bracket reading
    /// would bill this at roughly the base rate and look completely reasonable.
    #[test]
    fn crossing_the_threshold_reprices_the_whole_request() {
        let config = grok();
        let usage = TokenUsage {
            prompt_tokens: Some(201_000),
            completion_tokens: Some(1_000),
            ..Default::default()
        };
        let prices = Prices::for_prompt(&config, 201_000).unwrap();
        let cost = compute_cost(&usage, &prices);

        // 201k at 4/M, not 200k at 2/M plus 1k at 4/M.
        assert_eq!(cost.input_cost, decimal("0.804"));
        assert_eq!(cost.output_cost, decimal("0.012"));
        let as_a_bracket = decimal("0.404");
        assert!(cost.input_cost > as_a_bracket, "a bracket would give {as_a_bracket}");
    }

    /// One token below, and nothing has changed.
    #[test]
    fn a_prompt_under_the_threshold_pays_the_base_rate() {
        let prices = Prices::for_prompt(&grok(), 199_999).unwrap();
        assert_eq!(prices.input_price, Some(decimal("2")));
        assert_eq!(prices.output_price, Some(decimal("6")));
        assert_eq!(prices.cache_read_price, Some(decimal("0.5")));
    }

    /// The threshold counts the cached part too. Measuring the uncached
    /// remainder instead would drop a heavily-cached 400k conversation back into
    /// the cheap tier — the most likely case, and the most expensive to miss.
    #[test]
    fn the_cached_part_still_counts_towards_the_threshold() {
        let usage = TokenUsage {
            prompt_tokens: Some(400_000),
            completion_tokens: Some(0),
            cache_read_tokens: Some(390_000),
            ..Default::default()
        };
        let prices = Prices::for_prompt(&grok(), usage.prompt_tokens.unwrap() as i64).unwrap();
        assert_eq!(
            prices.cache_read_price,
            Some(decimal("1")),
            "the long-context cache rate"
        );
        let cost = compute_cost(&usage, &prices);
        // 10k uncached at 4/M + 390k read at 1/M.
        assert_eq!(cost.input_cost, decimal("0.04"));
        assert_eq!(cost.cache_cost, decimal("0.39"));
    }

    /// Several tiers, and the highest one that applies wins.
    #[test]
    fn the_highest_applicable_tier_is_the_one_that_applies() {
        let mut config = priced(Some("1"), Some("2"), None, None);
        config.pricing_tiers = Some(
            r#"[{"min_prompt_tokens":1000000,"input_price":"9","output_price":"9","cache_read_price":null,"cache_write_price":null},
                {"min_prompt_tokens":200000,"input_price":"3","output_price":"3","cache_read_price":null,"cache_write_price":null}]"#
                .into(),
        );
        // Stored out of order on purpose: the reader sorts rather than trusting.
        assert_eq!(
            Prices::for_prompt(&config, 100).unwrap().input_price,
            Some(decimal("1"))
        );
        assert_eq!(
            Prices::for_prompt(&config, 200_000).unwrap().input_price,
            Some(decimal("3"))
        );
        assert_eq!(
            Prices::for_prompt(&config, 999_999).unwrap().input_price,
            Some(decimal("3"))
        );
        assert_eq!(
            Prices::for_prompt(&config, 2_000_000).unwrap().input_price,
            Some(decimal("9"))
        );
    }

    /// A tier that omits a cache rate means what a blank column has always
    /// meant — priced like that tier's input — rather than inheriting the base
    /// tier's cheaper one, which would understate every long request.
    #[test]
    fn a_tier_without_a_cache_rate_prices_reads_at_its_own_input() {
        let mut config = priced(Some("2"), Some("6"), Some("0.5"), None);
        config.pricing_tiers = Some(
            r#"[{"min_prompt_tokens":200000,"input_price":"4","output_price":"12","cache_read_price":null,"cache_write_price":null}]"#.into(),
        );
        let usage = TokenUsage {
            prompt_tokens: Some(300_000),
            cache_read_tokens: Some(300_000),
            ..Default::default()
        };
        let prices = Prices::for_prompt(&config, 300_000).unwrap();
        let cost = compute_cost(&usage, &prices);
        assert_eq!(cost.cache_cost, decimal("1.2"), "300k at the tier's 4/M");
    }

    #[test]
    fn nullable_tier_prices_must_be_explicit() {
        let complete = serde_json::json!([{
            "min_prompt_tokens": 200_000,
            "input_price": "4",
            "output_price": "12",
            "cache_read_price": null,
            "cache_write_price": null
        }]);
        assert!(parse_tiers(Some(&complete.to_string())).is_ok());

        for key in ["cache_read_price", "cache_write_price"] {
            let mut missing = complete.clone();
            missing[0].as_object_mut().unwrap().remove(key);
            assert!(
                parse_tiers(Some(&missing.to_string())).is_err(),
                "{key} must be present"
            );
        }
    }

    /// Malformed tier data is a broken contract, never a signal to bill at a
    /// different rate. Both the direct parser and the model pricing path must
    /// reject it rather than falling back to the base price.
    #[test]
    fn a_malformed_tier_table_is_rejected() {
        for raw in [
            "",
            "   ",
            "not json",
            "{}",
            r#"[{"min_prompt_tokens":0,"input_price":"99","output_price":"99"}]"#,
            r#"[{"min_prompt_tokens":-5,"input_price":"99","output_price":"99"}]"#,
            r#"[{"min_prompt_tokens":1,"input_price":99,"output_price":"99"}]"#,
            r#"[{"min_prompt_tokens":1,"input_price":"99","output_price":"99","future":true}]"#,
            r#"[{"min_prompt_tokens":1,"input_price":"4","output_price":"12"},{"min_prompt_tokens":1,"input_price":"5","output_price":"13"}]"#,
            r#"[{"min_prompt_tokens":1,"input":"99","output":"99"}]"#,
        ] {
            let mut config = priced(Some("2"), Some("6"), None, None);
            config.pricing_tiers = Some(raw.into());
            assert!(parse_tiers(Some(raw)).is_err(), "{raw:?} must be rejected");
            assert!(
                Prices::for_prompt(&config, 10_000_000).is_err(),
                "{raw:?} must fail through the model pricing path"
            );
        }
        let mut config = priced(Some("2"), Some("6"), None, None);
        config.pricing_tiers = None;
        assert_eq!(
            Prices::for_prompt(&config, 10_000_000).unwrap().input_price,
            Some(decimal("2"))
        );
    }

    /// A turn is many requests. Five small ones and one large one leave the same
    /// totals behind and are billed differently, which is why the loop prices
    /// each round rather than the sum.
    #[test]
    fn five_small_requests_are_not_one_large_one() {
        let pricing = TurnPricing::of(&grok()).unwrap().expect("base token prices are known");
        let round = |prompt: i64| {
            let usage = TokenUsage {
                prompt_tokens: Some(prompt as i32),
                completion_tokens: Some(0),
                ..Default::default()
            };
            compute_cost(&usage, &pricing.for_prompt(prompt))
        };

        let mut split = RequestCost::default();
        for _ in 0..5 {
            split += round(50_000);
        }
        let whole = round(250_000);

        assert_eq!(split.total_cost, decimal("0.5"), "250k at the base 2/M");
        assert_eq!(whole.total_cost, decimal("1"), "250k at the long 4/M");
        assert!(
            whole.total_cost > split.total_cost,
            "identical token totals, different bills — pricing from the sum picks one of these at random",
        );
    }

    /// The reply that made this necessary, priced end to end.
    ///
    /// A live `grok-4.6` answer with one web search: 4551 prompt tokens (1152 of
    /// them cached), 74 output, and `web_search_calls: 1`. xAI's own
    /// `cost_in_usd_ticks` said $0.012818 — of which half a cent was the search,
    /// on top of the tokens. Counting only the tokens reports two thirds of it.
    #[test]
    fn a_search_is_billed_on_top_of_the_tokens() {
        let mut config = priced(Some("2"), Some("6"), Some("0.5"), None);
        config.server_tool_price = Some(decimal("5")); // $5 per 1000 calls
        let usage = TokenUsage {
            prompt_tokens: Some(4551),
            completion_tokens: Some(74),
            total_tokens: Some(4625),
            cache_read_tokens: Some(1152),
            cache_write_tokens: None,
            billable_tool_calls: Some(1),
        };

        let cost = compute_cost(&usage, &Prices::of(&config));
        assert_eq!(cost.tool_cost, decimal("0.005"), "one search at $5/1k");
        assert_eq!(cost.total_cost, decimal("0.012818"));
        // The figure the old formula would have produced, kept explicit so
        // reintroducing it fails here rather than in a month's invoice.
        let tokens_only = cost.input_cost + cost.output_cost + cost.cache_cost;
        assert_eq!(tokens_only, decimal("0.007818"));
    }

    /// A model nobody has given a tool rate still ran the searches. The calls are
    /// counted and contribute nothing, rather than being priced at a rate that
    /// was never configured.
    #[test]
    fn an_unpriced_tool_rate_adds_nothing_rather_than_guessing() {
        let config = priced(Some("2"), Some("6"), None, None);
        let usage = TokenUsage {
            prompt_tokens: Some(1_000),
            completion_tokens: Some(0),
            billable_tool_calls: Some(4),
            ..Default::default()
        };
        let cost = compute_cost(&usage, &Prices::of(&config));
        assert_eq!(cost.tool_cost, Decimal::zero());
        assert_eq!(cost.total_cost, decimal("0.002"), "the tokens still bill");
    }

    /// The per-call rate does not vary with prompt size, so a tier must not
    /// switch it off — and every tier omits it, since no upstream prices it that
    /// way. Long requests are the ones most likely to have searched.
    #[test]
    fn a_tier_keeps_the_base_tool_rate() {
        let mut config = priced(Some("2"), Some("6"), Some("0.5"), None);
        config.server_tool_price = Some(decimal("5"));
        config.pricing_tiers = Some(
            r#"[{"min_prompt_tokens":200000,"input_price":"4","output_price":"12","cache_read_price":null,"cache_write_price":null}]"#.into(),
        );

        let long = Prices::for_prompt(&config, 250_000).unwrap();
        assert_eq!(long.input_price, Some(decimal("4")), "the tier applied");
        assert_eq!(
            long.server_tool_price,
            Some(decimal("5")),
            "and did not take the tool rate with it"
        );

        let pricing = TurnPricing::of(&config).unwrap().expect("priced");
        assert_eq!(pricing.for_prompt(250_000).server_tool_price, Some(decimal("5")));
    }

    /// An unpriced model has no tier table worth reading, and has to stay
    /// distinguishable from one priced at zero.
    #[test]
    fn an_unpriced_model_has_no_turn_pricing() {
        assert!(TurnPricing::of(&mock_config(None, None, None)).unwrap().is_none());
        assert!(
            TurnPricing::of(&mock_config(Some("0"), Some("0"), None))
                .unwrap()
                .is_some(),
            "explicit zero is a known free rate"
        );
        assert!(
            TurnPricing::of(&mock_config(Some("2"), None, None)).unwrap().is_none(),
            "a partial token rate remains unknown"
        );
    }

    #[test]
    fn a_tool_rate_alone_keeps_the_known_live_cost() {
        let mut config = mock_config(None, None, None);
        config.server_tool_price = Some(decimal("15"));
        config.pricing_tiers = Some(
            r#"[{"min_prompt_tokens":1,"input_price":"99","output_price":"99","cache_read_price":null,"cache_write_price":null}]"#.into(),
        );

        let pricing = TurnPricing::of(&config)
            .unwrap()
            .expect("the provider-tool rate is known");
        let prices = pricing.for_prompt(1_000_000);
        assert!(
            prices.input_price.is_none() && prices.output_price.is_none(),
            "a tier must not invent token pricing"
        );
        assert_eq!(prices.server_tool_price, Some(decimal("15")));

        let usage = TokenUsage {
            prompt_tokens: Some(100),
            completion_tokens: Some(50),
            billable_tool_calls: Some(2),
            ..Default::default()
        };
        let cost = compute_cost(&usage, &prices);
        assert_eq!(cost.input_cost, Decimal::zero());
        assert_eq!(cost.output_cost, Decimal::zero());
        assert_eq!(cost.tool_cost, decimal("0.03"));
        assert_eq!(cost.total_cost, decimal("0.03"));
    }

    /// Filling in only the long-context row is an easy mistake — it is the row
    /// that surprises people — and it used to make one model report as two
    /// things at once: short requests counted as unpriced, long ones billed, and
    /// the turn showing no cost either way. Both paths now agree that a model
    /// without base rates is unpriced at every size.
    #[test]
    fn tiers_alone_do_not_price_a_model_nobody_has_priced() {
        let mut config = priced(None, None, None, None);
        config.pricing_tiers = Some(
            r#"[{"min_prompt_tokens":200000,"input_price":"4","output_price":"12","cache_read_price":null,"cache_write_price":null}]"#.into(),
        );

        let prices = Prices::for_prompt(&config, 500_000).unwrap();
        assert_eq!(prices.input_price, None, "still unpriced");
        assert!(!prices.known());
        assert!(TurnPricing::of(&config).unwrap().is_none(), "and the turn agrees");
    }

    /// A negative rate is invalid data. It must fail instead of being dropped or
    /// silently replaced with the base rate.
    #[test]
    fn a_negative_tier_rate_is_rejected() {
        for raw in [
            r#"[{"min_prompt_tokens":200000,"input_price":"-4","output_price":"12","cache_read_price":null,"cache_write_price":null}]"#,
            r#"[{"min_prompt_tokens":200000,"input_price":"4","output_price":"-12","cache_read_price":null,"cache_write_price":null}]"#,
            r#"[{"min_prompt_tokens":200000,"input_price":"4","output_price":"12","cache_read_price":"-1","cache_write_price":null}]"#,
        ] {
            let mut config = priced(Some("2"), Some("6"), None, None);
            config.pricing_tiers = Some(raw.into());
            assert!(parse_tiers(Some(raw)).is_err(), "{raw} must be rejected");
            assert!(Prices::for_prompt(&config, 500_000).is_err());
        }
    }
}

#[cfg(test)]
mod billing_mode_tests {
    use super::BillingMode;

    /// The round trip has to hold: `as_str` is what lands in the column and
    /// `from_str` is what reads it back, so a mismatch between them would make
    /// every subscription row read as metered from the moment it was written.
    #[test]
    fn every_mode_survives_the_column() {
        for mode in [BillingMode::Metered, BillingMode::Subscription, BillingMode::External] {
            assert_eq!(mode.as_str().parse::<BillingMode>(), Ok(mode));
        }
    }

    /// Billing mode is a closed contract. Unknown spelling cannot be interpreted
    /// as metered because that would hide a producer/schema mismatch.
    #[test]
    fn an_unknown_mode_is_rejected() {
        assert!("".parse::<BillingMode>().is_err());
        assert!("Subscription".parse::<BillingMode>().is_err(), "match is exact");
        assert!("whatever".parse::<BillingMode>().is_err());
        assert_eq!(BillingMode::default(), BillingMode::Metered);
    }

    /// Only metered traffic owes a price, and only metered traffic can be
    /// reported as missing one. These two travel together on purpose: a mode
    /// that is priced but not expected to have a rate, or the reverse, would
    /// either bill a plan twice or hide a real gap.
    #[test]
    fn only_metered_traffic_owes_a_price() {
        assert!(BillingMode::Metered.is_priced());
        assert!(BillingMode::Metered.expects_a_price());
        for mode in [BillingMode::Subscription, BillingMode::External] {
            assert!(!mode.is_priced(), "{mode:?} must not be priced");
            assert!(!mode.expects_a_price(), "{mode:?} has no rate to be missing");
        }
    }

    /// The mapping is keyed on the transport, not on the credential: what
    /// decides how a request is paid for is which service it reaches. Today only
    /// `standard` exists, so this is inert until the Codex transport lands.
    #[test]
    fn the_transport_decides_how_a_request_is_paid_for() {
        assert_eq!(BillingMode::for_transport("standard"), Ok(BillingMode::Metered));
        assert_eq!(
            BillingMode::for_transport("chatgpt_codex"),
            Ok(BillingMode::Subscription)
        );
        assert!(BillingMode::for_transport("something_new").is_err());
    }
}

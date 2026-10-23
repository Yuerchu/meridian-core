//! Prices from [models.dev](https://models.dev), applied to model profiles.
//!
//! A profile describes a model — its window, its capability patch and, usually,
//! its prices. Nobody types a price for a model whose vendor publishes one, so
//! this module reads models.dev and fills the blanks. It runs in the core, on a
//! timer and once after a save, because pricing is the backend's answer and not
//! something a form asks for.
//!
//! Three things it deliberately does not do.
//!
//! It does not touch a profile somebody has priced. "All four rates are NULL"
//! is the one state that means *nobody has said*: `Some(0)` is an explicit free
//! model, and any other number is a decision. A row carrying a number is never
//! rewritten — including a row this module wrote, unless its four columns still
//! equal what models.dev says today. That is what makes a manual edit final
//! without needing a `pricing_source` column to remember it.
//!
//! It does not price a relay. models.dev lists what a vendor charges; a relay
//! charges what it likes, and `model_configs.overrides_pricing` is where that
//! goes. A provider the vendor catalog cannot identify is skipped.
//!
//! It does not decide capabilities. `provider/model_catalog.json` carries the
//! request shape — `thinking_style` is the field that produces a hard 400 when
//! wrong — and models.dev's `reasoning` flag is far too coarse to replace it.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::client::{HttpTransport, Request, ReqwestTransport};
use crate::db::entity::{model_profile, provider};
use crate::db::sea::cap::Db;
use crate::db::sea::ops;
use crate::db::types::EpochMs;
use crate::decimal::Decimal;
use crate::provider::catalog;
use crate::util::now_ms;

pub const API_URL: &str = "https://models.dev/api.json";
/// `true` (the shipped default) is "fill prices nobody has typed".
pub const SYNC_ENABLED_KEY: &str = "models_dev.price_sync";
/// When the last successful sync finished, as epoch milliseconds. Recorded for
/// the person reading the logs; the timer does not depend on it.
pub const SYNC_AT_KEY: &str = "models_dev.price_sync_at";

const SYNC_PERIOD: Duration = Duration::from_secs(24 * 60 * 60);
/// Long enough for startup to finish touching the database first.
const FIRST_SYNC_DELAY: Duration = Duration::from_secs(30);
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// What models.dev says about one model, in the units the database stores.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ModelFacts {
    pub input_price: Option<Decimal>,
    pub output_price: Option<Decimal>,
    pub cache_read_price: Option<Decimal>,
    pub cache_write_price: Option<Decimal>,
}

impl ModelFacts {
    /// Whether anything was said at all. A model with no rates is not a price.
    fn is_empty(&self) -> bool {
        self.input_price.is_none()
            && self.output_price.is_none()
            && self.cache_read_price.is_none()
            && self.cache_write_price.is_none()
    }
}

/// models.dev as of one fetch, indexed by the id we normalise to.
#[derive(Debug)]
pub struct Snapshot {
    fetched_at: EpochMs,
    /// provider id → normalised model id → rates.
    providers: HashMap<String, HashMap<String, ModelFacts>>,
}

impl Snapshot {
    /// Answers for a model reached through one provider.
    ///
    /// The provider is looked up by the catalog id the row carries, falling
    /// back to `catalog::identify` for rows written before that column existed.
    /// The model is looked up under the normalised id, because the same model
    /// is spelled `claude-sonnet-4`, `anthropic/claude-sonnet-4` and
    /// `claude-sonnet-4:beta` depending on who is writing it down.
    pub(crate) fn facts(&self, catalog_id: &str, base_url: &str, model_id: &str) -> Option<&ModelFacts> {
        let normalized = normalize_model_id(model_id);
        for id in models_dev_provider_ids(catalog_id, base_url) {
            if let Some(facts) = self.providers.get(id).and_then(|models| models.get(&normalized)) {
                return Some(facts);
            }
        }
        None
    }
}

/// The one snapshot this process holds.
///
/// A whole models.dev document is a few megabytes and is read on every save, so
/// it is kept behind an `Arc` rather than cloned per lookup.
#[derive(Default)]
pub struct ModelsDevCatalog {
    snapshot: RwLock<Option<Arc<Snapshot>>>,
}

impl ModelsDevCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    fn current(&self) -> Option<Arc<Snapshot>> {
        self.snapshot
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn store(&self, snapshot: Snapshot) {
        *self.snapshot.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::new(snapshot));
    }
}

/// What one pass changed, for the log line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Profiles whose four rates were written.
    pub filled: usize,
    /// Profiles kept because somebody had already priced them.
    pub kept: usize,
    /// Profiles skipped because two providers disagreed about the price.
    pub ambiguous: usize,
    /// Profiles no model of which models.dev knows.
    pub unmatched: usize,
}

/// Fetches and parses the document.
///
/// `async fn fetch` on the trait rather than a body-first helper: this is the
/// one request in the app with no credential, no dialect and no retry of its
/// own, so it uses the transport directly.
pub async fn fetch(transport: &impl HttpTransport) -> Result<Snapshot, String> {
    let mut request = Request::new(http::Method::GET, API_URL.to_owned());
    request.timeout = Some(FETCH_TIMEOUT);
    let response = transport
        .execute(request)
        .await
        .map_err(|error| format!("models.dev: {error}"))?;
    let raw: HashMap<String, ApiProvider> = serde_json::from_slice(&response.body)
        .map_err(|error| format!("models.dev returned an unexpected document: {error}"))?;
    Ok(Snapshot::from_api(raw, now_ms()))
}

impl Snapshot {
    /// A snapshot built without going through the document's spelling rules,
    /// for tests that are about what happens to a row rather than what the
    /// document says.
    #[cfg(test)]
    pub(crate) fn from_facts(providers: HashMap<String, HashMap<String, ModelFacts>>) -> Self {
        Self {
            fetched_at: 0,
            providers,
        }
    }

    fn from_api(raw: HashMap<String, ApiProvider>, fetched_at: EpochMs) -> Self {
        let mut providers = HashMap::with_capacity(raw.len());
        for (provider_id, provider) in raw {
            // A newer release wins a collision: two spellings of one model at
            // different prices is a model that got cheaper, and the price a
            // person is billed today is the newer one. `release_date` is ISO
            // 8601, so comparing the strings orders the dates.
            let mut models: HashMap<String, (String, ModelFacts)> = HashMap::new();
            for (model_id, model) in provider.models.unwrap_or_default() {
                if !is_a_priced_chat_model(&model) {
                    continue;
                }
                let facts = facts_of(&model);
                if facts.is_empty() {
                    continue;
                }
                let normalized = normalize_model_id(model.id.as_deref().unwrap_or(&model_id));
                if normalized.is_empty() {
                    continue;
                }
                let release = model.release_date.clone().unwrap_or_default();
                match models.get(&normalized) {
                    Some((existing, _)) if *existing >= release => {}
                    _ => {
                        models.insert(normalized, (release, facts));
                    }
                }
            }
            providers.insert(
                provider_id,
                models.into_iter().map(|(id, (_, facts))| (id, facts)).collect(),
            );
        }
        Self { fetched_at, providers }
    }
}

/// models.dev's shape, kept forgiving on purpose.
///
/// This is a third party's document that gains fields without asking. An
/// unknown field, a null, or a rate that stopped being a number must not take
/// the whole sync down; the fields below are the ones that matter and the rest
/// is ignored.
#[derive(Debug, serde::Deserialize)]
struct ApiProvider {
    #[serde(default)]
    models: Option<HashMap<String, ApiModel>>,
}

#[derive(Debug, serde::Deserialize)]
struct ApiModel {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    release_date: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    cost: Option<ApiCost>,
    #[serde(default)]
    modalities: Option<ApiModalities>,
}

#[derive(Debug, serde::Deserialize)]
struct ApiCost {
    #[serde(default)]
    input: Option<serde_json::Number>,
    #[serde(default)]
    output: Option<serde_json::Number>,
    #[serde(default)]
    cache_read: Option<serde_json::Number>,
    #[serde(default)]
    cache_write: Option<serde_json::Number>,
}

#[derive(Debug, serde::Deserialize)]
struct ApiModalities {
    #[serde(default)]
    output: Option<Vec<String>>,
}

const NON_TEXT_OUTPUT_MODALITIES: [&str; 3] = ["audio", "image", "video"];

/// Whether a model is chat/coding rather than an embedding, a speech model or a
/// retired one. Pricing an embedding model would put its per-token rate on a
/// profile nobody can talk to.
fn is_a_priced_chat_model(model: &ApiModel) -> bool {
    if model
        .status
        .as_deref()
        .is_some_and(|status| status.eq_ignore_ascii_case("deprecated"))
    {
        return false;
    }
    let Some(output) = model
        .modalities
        .as_ref()
        .and_then(|modalities| modalities.output.as_ref())
    else {
        return true;
    };
    output.is_empty()
        || (output.iter().any(|modality| modality.eq_ignore_ascii_case("text"))
            && !output.iter().any(|modality| {
                NON_TEXT_OUTPUT_MODALITIES
                    .iter()
                    .any(|known| modality.eq_ignore_ascii_case(known))
            }))
}

fn facts_of(model: &ApiModel) -> ModelFacts {
    let cost = model.cost.as_ref();
    ModelFacts {
        input_price: cost.and_then(|cost| price(cost.input.as_ref())),
        output_price: cost.and_then(|cost| price(cost.output.as_ref())),
        cache_read_price: cost.and_then(|cost| price(cost.cache_read.as_ref())),
        cache_write_price: cost.and_then(|cost| price(cost.cache_write.as_ref())),
    }
}

/// A JSON number to an exact decimal string.
///
/// `Number`'s `Display` is the shortest representation that round-trips, so
/// `0.1` stays `0.1` and `1` stays `1`. The value never becomes an `f64` of
/// ours and never takes part in arithmetic — the database stores it and
/// `agent::pricing` does the only multiplication that matters.
fn price(number: Option<&serde_json::Number>) -> Option<Decimal> {
    Decimal::from_str(&number?.to_string()).ok()
}

/// The spellings a model id appears in, reduced to one.
///
/// `openai/gpt-5`, `gpt-5:fast` and `gpt-5[1m]` are the same model reached
/// through different doors; a profile prices the model, not the door.
pub fn normalize_model_id(raw: &str) -> String {
    let after_slash = raw.rsplit('/').next().unwrap_or(raw);
    let before_variant = after_slash.split(':').next().unwrap_or(after_slash);
    let mut normalized = before_variant.trim().replace('@', "-").to_lowercase();
    if let Some(stripped) = normalized.strip_suffix("[1m]") {
        normalized = stripped.trim().to_owned();
    }
    normalized
}

/// Which models.dev providers describe a row in ours.
///
/// A vendor with a `.cn` endpoint prices in a separate entry for the same
/// models, so the address picks between the two; the other is a fallback for a
/// row whose address was typed without the region.
fn models_dev_provider_ids(catalog_id: &str, base_url: &str) -> Vec<&'static str> {
    let cn = base_url.to_ascii_lowercase().contains(".cn");
    match catalog_id {
        "openai" => vec!["openai"],
        "anthropic" => vec!["anthropic"],
        "deepseek" => vec!["deepseek"],
        "xai" => vec!["xai"],
        "google" => vec!["google", "google-vertex"],
        "moonshot" if cn => vec!["moonshotai-cn", "moonshotai"],
        "moonshot" => vec!["moonshotai", "moonshotai-cn"],
        "siliconflow" if cn => vec!["siliconflow-cn", "siliconflow"],
        "siliconflow" => vec!["siliconflow", "siliconflow-cn"],
        _ => Vec::new(),
    }
}

pub(crate) fn catalog_of(row: &provider::Model) -> Option<String> {
    if let Some(id) = &row.catalog_id {
        return Some(id.clone());
    }
    catalog::identify(row.provider_type.as_str(), &row.base_url)
        .or_else(|| {
            // A row written before `catalog_id` existed may carry a version
            // segment its vendor's preset does not (`…:443/v1`). The vendor is
            // still the same one; `identify` still has to answer uniquely.
            strip_version_segment(&row.base_url).and_then(|base| catalog::identify(row.provider_type.as_str(), base))
        })
        .map(str::to_owned)
}

/// `https://api.anthropic.com/v1` → `https://api.anthropic.com`, and nothing
/// else. Only a trailing `v<digits>[beta…]` is removed, because that is the one
/// spelling difference between a preset and the same address typed by hand.
fn strip_version_segment(url: &str) -> Option<&str> {
    let trimmed = url.trim().trim_end_matches('/');
    let (head, last) = trimmed.rsplit_once('/')?;
    let digits = last
        .strip_prefix('v')?
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .count();
    if digits == 0 {
        return None;
    }
    let tail = &last[1 + digits..];
    (tail.is_empty() || tail.to_ascii_lowercase().starts_with("beta")).then_some(head)
}

/// Whether this profile's four rates may be written.
///
/// Two states qualify: nobody has priced it (all NULL), or it already holds
/// exactly what models.dev says (which is what we wrote last time, and writing
/// it again changes nothing). Anything else is a decision this module did not
/// make.
pub(crate) fn may_write(profile: &model_profile::Model, facts: &ModelFacts) -> bool {
    let already_says_the_same = profile.input_price == facts.input_price
        && profile.output_price == facts.output_price
        && profile.cache_read_price == facts.cache_read_price
        && profile.cache_write_price == facts.cache_write_price;
    already_says_the_same
        || (profile.input_price.is_none()
            && profile.output_price.is_none()
            && profile.cache_read_price.is_none()
            && profile.cache_write_price.is_none())
}

/// One pass over every profile.
///
/// Reads and writes in one transaction: the decision depends on the row it then
/// writes, and a save landing between the two would otherwise be overwritten.
/// The document itself is fetched outside it — a network wait inside
/// `BEGIN IMMEDIATE` would hold the database for as long as models.dev took.
pub async fn sync_once(db: &Db, catalog: &ModelsDevCatalog, force: bool) -> Result<SyncReport, String> {
    let enabled = ops::preference::parse_bool_preference(
        SYNC_ENABLED_KEY,
        ops::preference::get_preference(db, SYNC_ENABLED_KEY)
            .await
            .map_err(|error| error.to_string())?
            .as_deref(),
        true,
    )?;
    if !enabled {
        debug!("models.dev price sync is off");
        return Ok(SyncReport::default());
    }

    let snapshot = match catalog.current() {
        Some(existing) if !force && now_ms() - existing.fetched_at < SYNC_PERIOD.as_millis() as i64 => existing,
        _ => {
            let fetched = fetch(&ReqwestTransport::shared()).await?;
            catalog.store(fetched);
            catalog
                .current()
                .ok_or_else(|| "models.dev snapshot vanished after storing".to_owned())?
        }
    };

    let report = apply(db, &snapshot).await?;
    let now = now_ms();
    db.write(async |tx| ops::preference::set_preference(tx, SYNC_AT_KEY, &now.to_string(), now).await)
        .await
        .map_err(|error| error.to_string())?;
    Ok(report)
}

/// Fills every profile the snapshot knows a price for.
///
/// The reads and the write share one transaction: the decision depends on the
/// row being written, and a save landing between them would otherwise be
/// overwritten. The document is fetched outside it — a network wait inside
/// `BEGIN IMMEDIATE` would hold the database for as long as models.dev took.
async fn apply(db: &Db, snapshot: &Snapshot) -> Result<SyncReport, String> {
    db.write(async |tx| ops::model_price::apply(tx, snapshot).await)
        .await
        .map_err(|error| error.to_string())
}

/// Fills one model's profile, used right after a save.
///
/// A save is the moment a person is most likely to look at the prices, and
/// waiting up to a day for the timer would show them a blank they just asked
/// about. The snapshot is in memory by then; if it is not (the app started less
/// than thirty seconds ago) this does nothing, and the timer gets it.
pub async fn fill_for_config(db: &Db, catalog: &ModelsDevCatalog, provider_id: &str, model_id: &str) {
    let Some(snapshot) = catalog.current() else {
        return;
    };
    if let Err(error) = fill_one(db, &snapshot, provider_id, model_id).await {
        warn!(error = %error, "models.dev could not fill the prices of a saved model");
    }
}

async fn fill_one(db: &Db, snapshot: &Snapshot, provider_id: &str, model_id: &str) -> Result<(), String> {
    db.write(async |tx| ops::model_price::fill_one(tx, snapshot, provider_id, model_id).await)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Starts the timer. One pass thirty seconds after startup, then once a day.
///
/// There is no shutdown signal: the process ending is the only thing that stops
/// it, and a failure in one pass says so in the log and leaves the previous
/// snapshot — and every price already written — in place.
pub fn spawn_sync_loop(db: Db, catalog: Arc<ModelsDevCatalog>) {
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_SYNC_DELAY).await;
        loop {
            match sync_once(&db, &catalog, true).await {
                Ok(report) => info!(
                    filled = report.filled,
                    kept = report.kept,
                    ambiguous = report.ambiguous,
                    unmatched = report.unmatched,
                    "models.dev price sync finished"
                ),
                Err(error) => warn!(error = %error, "models.dev price sync failed; prices are unchanged"),
            }
            tokio::time::sleep(SYNC_PERIOD).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::types::SqlBool;

    fn snapshot(json: &str) -> Snapshot {
        let raw: HashMap<String, ApiProvider> = serde_json::from_str(json).expect("the fixture must parse");
        Snapshot::from_api(raw, 0)
    }

    fn price_of(raw: &str) -> Option<Decimal> {
        Decimal::from_str(raw).ok()
    }

    const DOCUMENT: &str = r#"{
        "anthropic": {
            "models": {
                "claude-sonnet-4": {
                    "id": "claude-sonnet-4",
                    "release_date": "2025-05-22",
                    "cost": {"input": 3, "output": 15, "cache_read": 0.3, "cache_write": 3.75}
                },
                "anthropic/claude-sonnet-4": {
                    "id": "anthropic/claude-sonnet-4",
                    "release_date": "2025-01-01",
                    "cost": {"input": 99, "output": 99}
                },
                "claude-retired": {
                    "status": "deprecated",
                    "cost": {"input": 1, "output": 2}
                },
                "text-embedding-4": {
                    "modalities": {"output": ["embedding"]},
                    "cost": {"input": 0.1}
                },
                "claude-no-price": {"name": "No price"}
            }
        },
        "moonshotai-cn": {
            "models": {
                "kimi-k3": {
                    "id": "kimi-k3",
                    "cost": {"input": 0.6, "output": 2.5}
                }
            }
        }
    }"#;

    #[test]
    fn a_model_is_found_under_every_spelling_of_its_id() {
        let snapshot = snapshot(DOCUMENT);
        let facts = snapshot
            .facts("anthropic", "https://api.anthropic.com/v1", "claude-sonnet-4")
            .expect("the model is in the document");
        assert_eq!(facts.input_price, price_of("3"));
        assert_eq!(facts.output_price, price_of("15"));
        assert_eq!(facts.cache_read_price, price_of("0.3"));
        assert_eq!(facts.cache_write_price, price_of("3.75"));

        // The prefixed spelling normalises to the same entry, and the newer
        // release won the collision rather than the one that happened to be
        // read second.
        for spelling in [
            "anthropic/claude-sonnet-4",
            "claude-sonnet-4:beta",
            "claude-sonnet-4[1m]",
        ] {
            let facts = snapshot
                .facts("anthropic", "https://api.anthropic.com/v1", spelling)
                .expect(spelling);
            assert_eq!(facts.input_price, price_of("3"), "{spelling}");
        }
    }

    #[test]
    fn a_retired_or_non_chat_model_is_not_a_price() {
        let snapshot = snapshot(DOCUMENT);
        assert!(
            snapshot
                .facts("anthropic", "https://api.anthropic.com", "claude-retired")
                .is_none()
        );
        assert!(
            snapshot
                .facts("anthropic", "https://api.anthropic.com", "text-embedding-4")
                .is_none()
        );
        assert!(
            snapshot
                .facts("anthropic", "https://api.anthropic.com", "claude-no-price")
                .is_none()
        );
    }

    #[test]
    fn the_region_of_the_address_picks_the_provider_entry() {
        let snapshot = snapshot(DOCUMENT);
        assert_eq!(
            models_dev_provider_ids("moonshot", "https://api.moonshot.cn/v1")[0],
            "moonshotai-cn"
        );
        assert_eq!(
            models_dev_provider_ids("moonshot", "https://api.moonshot.ai/v1")[0],
            "moonshotai"
        );
        // A vendor the catalog describes but models.dev does not price.
        assert!(models_dev_provider_ids("litert_lm", "http://localhost").is_empty());
        // The documented address reaches the same entry.
        assert!(
            snapshot
                .facts("moonshot", "https://api.moonshot.cn/v1", "kimi-k3")
                .is_some()
        );
    }

    #[test]
    fn a_model_id_normalises_to_one_identity_across_doors() {
        assert_eq!(normalize_model_id("openai/gpt-5"), "gpt-5");
        assert_eq!(normalize_model_id("gpt-5:fast"), "gpt-5");
        assert_eq!(normalize_model_id("gpt-5[1m]"), "gpt-5");
        assert_eq!(
            normalize_model_id("Claude-Sonnet-4@20250514"),
            "claude-sonnet-4-20250514"
        );
        assert_eq!(normalize_model_id("  gpt-5  "), "gpt-5");
    }

    fn profile(
        input: Option<&str>,
        output: Option<&str>,
        cache_read: Option<&str>,
        cache_write: Option<&str>,
    ) -> model_profile::Model {
        model_profile::Model {
            id: "profile".into(),
            name: "A model".into(),
            context_window: 200_000,
            compact_threshold: 150_000,
            max_output_tokens: Some(64_000),
            input_price: input.and_then(price_of),
            output_price: output.and_then(price_of),
            cache_read_price: cache_read.and_then(price_of),
            cache_write_price: cache_write.and_then(price_of),
            pricing_tiers: None,
            capability_overrides: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn facts() -> ModelFacts {
        ModelFacts {
            input_price: price_of("3"),
            output_price: price_of("15"),
            cache_read_price: price_of("0.3"),
            cache_write_price: price_of("3.75"),
        }
    }

    #[test]
    fn an_unpriced_profile_is_written() {
        assert!(may_write(&profile(None, None, None, None), &facts()));
    }

    #[test]
    fn a_profile_somebody_priced_is_left_alone() {
        assert!(!may_write(&profile(Some("1"), Some("2"), None, None), &facts()));
        // A partial price is still a decision, not a blank column.
        assert!(!may_write(&profile(Some("3"), None, None, None), &facts()));
        // Explicitly free is a price.
        assert!(!may_write(
            &profile(Some("0"), Some("0"), Some("0"), Some("0")),
            &facts()
        ));
    }

    #[test]
    fn a_profile_that_already_says_what_models_dev_says_stays_writable() {
        assert!(may_write(
            &profile(Some("3"), Some("15"), Some("0.3"), Some("3.75")),
            &facts()
        ));
    }

    #[test]
    fn the_rule_reads_prices_and_nothing_else() {
        // A window or a capability patch edited by hand must not make the
        // profile look priced.
        let mut row = profile(None, None, None, None);
        row.context_window = 1;
        row.capability_overrides = Some(r#"{"supports_fast":true}"#.into());
        assert!(may_write(&row, &facts()));
    }

    #[test]
    fn a_provider_row_without_a_catalog_id_is_identified_from_its_address() {
        let row = provider::Model {
            id: "p".into(),
            name: "Anthropic".into(),
            provider_type: crate::provider::registry::ProviderType::Anthropic,
            base_url: "https://api.anthropic.com/v1".into(),
            is_enabled: SqlBool::TRUE,
            sort_order: 0,
            created_at: 0,
            updated_at: 0,
            api_format: crate::provider::registry::ApiFormat::ChatCompletions,
            catalog_id: None,
            credential_kind: crate::provider::registry::CredentialKind::ApiKey,
            transport_profile: crate::provider::registry::TransportProfile::Standard,
            icon: None,
            codex_request_shape: SqlBool::FALSE,
            local_backend: None,
        };
        assert_eq!(catalog_of(&row).as_deref(), Some("anthropic"));
    }
}

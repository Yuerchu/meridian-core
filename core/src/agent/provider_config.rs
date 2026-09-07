use crate::db::models::assistant::AssistantRow;
use crate::db::models::model_config::ModelConfigRow;
use crate::db::{self, DbPool};
use crate::provider::{self, ChatParams, ProviderCapabilities, ServerToolKind};
use crate::secrets::{SecretName, SecretScope, SecretsManager};
use crate::util::get_conn;

pub fn provider_secret_name(provider_id: &str) -> String {
    format!("PROVIDER_{}_KEY", provider_id.replace('-', "_").to_uppercase())
}

pub fn get_provider_api_key(secrets: &SecretsManager, provider_id: &str) -> Option<String> {
    let key = provider_secret_name(provider_id);
    match secrets.get(&SecretScope::Global, &SecretName::new(&key).unwrap()) {
        Ok(value) => value,
        Err(e) => {
            // Callers turn a None into "API Key not set", which sends a user who
            // definitely set one to enter it again — and re-entering rewrites the
            // store under a fresh passphrase, taking the other providers' keys
            // with it. The distinction between "absent" and "unreadable" only
            // exists here.
            tracing::error!(
                provider_id = %provider_id,
                secret_name = %key,
                error = %e,
                "stored API key could not be read; it will look as though none was set"
            );
            None
        }
    }
}

/// Secrets exposed to tool executors (web_search provider selection + service
/// API keys). Shared by the desktop chat loop and the OneBot headless agent.
pub fn build_tool_secrets(secrets: &SecretsManager, pool: &DbPool) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    if let Ok(mut conn) = pool.get()
        && let Ok(Some(sp)) = db::ops::preference::get_preference(&mut conn, "search_provider")
    {
        map.insert("SEARCH_PROVIDER".to_string(), sp);
    }
    for key in ["SERVICE_TAVILY_KEY", "SERVICE_ZHIPU_SEARCH_KEY"] {
        if let Ok(name) = SecretName::new(key)
            && let Ok(Some(val)) = secrets.get(&SecretScope::Global, &name)
        {
            map.insert(key.to_string(), val);
        }
    }
    map
}

pub fn resolve_provider_config(
    secrets: &SecretsManager,
    pool: &DbPool,
    assistant: Option<&AssistantRow>,
) -> Result<ResolvedProvider, String> {
    if let Some(provider_id) = assistant.and_then(|a| a.provider_id.as_deref()) {
        let mut conn = get_conn(pool)?;
        let provider =
            db::ops::provider::get_provider(&mut conn, provider_id).map_err(|e| format!("Provider not found: {e}"))?;
        let credential = resolve_credential(secrets, &provider)?;
        let model = assistant
            .and_then(|a| a.model_id.clone())
            .ok_or("No model configured. Go to Settings → Assistant to set a model.")?;
        let base_url = provider.base_url.trim_end_matches('/').to_string();
        return Ok(ResolvedProvider {
            provider_id: provider.id,
            provider_name: provider.name,
            provider_type: provider.provider_type,
            base_url,
            credential,
            model,
            api_format: provider.api_format,
            transport_profile: provider.transport_profile,
        });
    }

    // Fallback: first enabled provider
    let mut conn = get_conn(pool)?;
    // Still only the first enabled provider, and still all-or-nothing on it.
    // A damaged row or unreadable credential is reported as itself rather than
    // being relabelled "no provider configured" or silently moving to a
    // different endpoint.
    let providers = db::ops::provider::list_providers(&mut conn)
        .map_err(|error| format!("could not read configured providers: {error}"))?;
    let p = providers
        .into_iter()
        .find(|p| p.is_enabled != 0)
        .ok_or("No provider configured. Go to Settings → Provider to add one.")?;
    let credential = resolve_credential(secrets, &p)?;
    let model = assistant
        .and_then(|a| a.model_id.clone())
        .ok_or("No model configured. Go to Settings → Assistant to set a model.")?;
    let base_url = p.base_url.trim_end_matches('/').to_string();
    Ok(ResolvedProvider {
        provider_id: p.id,
        provider_name: p.name,
        provider_type: p.provider_type,
        base_url,
        credential,
        model,
        api_format: p.api_format,
        transport_profile: p.transport_profile,
    })
}

/// Where a request is going, once the caller's overrides have had their say.
pub struct ResolvedProvider {
    /// Which configured provider row this resolved to, and what it was called.
    ///
    /// Carried out rather than discarded, because the rows a turn writes record
    /// it: `messages.provider_id` has existed since migration 1 and was never
    /// filled in on a reply, so no report could say which upstream produced
    /// what. The fallback branch made that worse — it picks the first enabled
    /// provider and used to return only its settings, so a turn that took that
    /// path had no id to record even in principle.
    ///
    /// The name travels beside the id because `messages.provider_id` is
    /// `ON DELETE SET NULL`: deleting a provider silently un-attributes every
    /// reply it ever produced.
    pub provider_id: String,
    pub provider_name: String,
    pub provider_type: String,
    pub base_url: String,
    pub credential: provider::Credential,
    pub model: String,
    pub api_format: String,
    /// Which wire this row speaks. Travels beside `api_format` because the two
    /// together pick the adapter — the format alone cannot separate OpenAI's
    /// Responses API from ChatGPT's Codex backend, which are both `responses`.
    pub transport_profile: String,
}

/// The credential for a provider row, by whatever route its login uses.
///
/// One place rather than three, because the "API Key not set" message it can
/// produce has to keep appearing for every provider that does need one. Folding
/// the bypass into each call site is how a login that needs no key ends up
/// letting a misconfigured API-key provider through as an anonymous request.
fn resolve_credential(
    secrets: &SecretsManager,
    provider: &db::models::provider::ProviderRow,
) -> Result<provider::Credential, String> {
    provider::registry::validate_stored_contract(
        &provider.provider_type,
        &provider.api_format,
        &provider.transport_profile,
        &provider.credential_kind,
    )?;
    match provider.credential_kind.as_str() {
        "api_key" => get_provider_api_key(secrets, &provider.id)
            .map(provider::Credential::ApiKey)
            .ok_or_else(|| format!("API Key not set for provider '{}'", provider.name)),
        "codex_cli" => {
            let home = crate::codex_auth::storage::find_codex_home()
                .ok_or("Could not work out where the Codex CLI keeps its login (no home directory).")?;
            // Resolved, not validated: whether the login is present, usable, or
            // needs renewing is decided when a request actually needs a token.
            // Reading the file here would put a disk hit on every turn setup and
            // would report "not logged in" for a session that a refresh could
            // have saved.
            Ok(provider::Credential::ChatGpt(
                crate::codex_auth::registry().get(crate::codex_auth::StoreId::CodexCli { home }),
            ))
        }
        // Reserved: the in-app login writes to a store this app owns. Nothing
        // creates such a row yet, and the manager refuses it with a message
        // rather than pretending.
        "chatgpt_oauth" => Ok(provider::Credential::ChatGpt(crate::codex_auth::registry().get(
            crate::codex_auth::StoreId::MeridianOwned {
                provider_id: provider.id.clone(),
                slot: "default".into(),
            },
        ))),
        other => Err(format!("unknown provider credential kind `{other}`")),
    }
}

/// The assistant's provider and model, with a caller's choices layered on top.
///
/// Two overrides that do not compose the way they look like they might. A model
/// override replaces only the name — the endpoint and the key stay whatever the
/// assistant resolved. A provider override replaces the endpoint, the key and
/// the wire format together, because a key is meaningless against a different
/// host, and it leaves the model name alone. So "same model, different endpoint"
/// and "same endpoint, different model" are both expressible, which is what the
/// model picker in the composer actually offers.
///
/// Synchronous: every read here is a pooled connection or the OS credential
/// store, both of which block. Callers put it in `spawn_blocking`.
pub fn resolve_with_overrides(
    secrets: &SecretsManager,
    pool: &DbPool,
    assistant: Option<&AssistantRow>,
    model_override: Option<String>,
    provider_override: Option<&str>,
) -> Result<ResolvedProvider, String> {
    if let Some(pid) = provider_override {
        // Both overrides together name a complete destination, so the
        // assistant is not consulted at all. Resolving it first anyway is not
        // just wasted work — its gaps still fail the call. The auto reviewer
        // passes no assistant on purpose, and used to be answered "No model
        // configured" on every review while the pair the user had picked sat
        // unread in the overrides.
        if let Some(model) = model_override {
            return resolve_named(secrets, pool, pid, model);
        }
        // The identity moves with the endpoint. A row attributed to the
        // assistant's standing choice while the request went somewhere else
        // would be worse than no attribution at all — it would look measured.
        let resolved = resolve_provider_config(secrets, pool, assistant)?;
        return resolve_named(secrets, pool, pid, resolved.model);
    }

    let mut resolved = resolve_provider_config(secrets, pool, assistant)?;
    if let Some(m) = model_override {
        resolved.model = m;
    }
    Ok(resolved)
}

/// A provider row plus a model name — everything a request needs, with no
/// assistant in the picture. This being total is what lets the pair of
/// overrides above skip the assistant resolution entirely.
fn resolve_named(
    secrets: &SecretsManager,
    pool: &DbPool,
    provider_id: &str,
    model: String,
) -> Result<ResolvedProvider, String> {
    let mut conn = get_conn(pool)?;
    let p = db::ops::provider::get_provider(&mut conn, provider_id).map_err(|e| e.to_string())?;
    let credential = resolve_credential(secrets, &p)?;
    Ok(ResolvedProvider {
        provider_id: p.id,
        provider_name: p.name,
        provider_type: p.provider_type,
        base_url: p.base_url.trim_end_matches('/').to_string(),
        credential,
        model,
        api_format: p.api_format,
        transport_profile: p.transport_profile,
    })
}

/// For the passes that only read a conversation and write prose about it —
/// summarisation, extraction, titles, automatic review.
///
/// Thinking comes off so an inherited budget cannot meet or exceed max_tokens
/// (Anthropic 400s when budget_tokens >= max_tokens), and so a throwaway pass is
/// not paid for in reasoning tokens.
///
/// **The provider-side tools come off with it, and that is not merely thrift.**
/// A summariser handed `web_search` is a background request that can go out to
/// the open internet, on a query the model composed out of whatever it was
/// summarising, billed per call and reported to nobody. For `auto_review` it is
/// worse than that: it is shown a projection built from untrusted tool output,
/// its verdict goes back into the chat, and it needs no approval to search — the
/// same exfiltration path the `FileAccess` rule for that module exists to close,
/// reopened through a different door. These passes write prose; none of them has
/// any use for a tool.
///
/// The cache key goes too. It names the transcript's prefix, and none of these
/// requests sends that prefix — pinning them to the server holding it buys
/// nothing and makes the claim in the architecture notes ("only the two real
/// loops set one") false.
pub(crate) fn without_thinking(params: ChatParams) -> ChatParams {
    ChatParams {
        thinking_enabled: false,
        thinking_budget: None,
        thinking_effort: None,
        server_tools: Vec::new(),
        cache_key: None,
        ..params
    }
}

/// Everything a request needs beyond the messages: the wire parameters plus the
/// limits the token budget is derived from. AssistantRow settings, the per-model
/// config row and the catalog are layered here once, so no call site invents a
/// value of its own — a summarisation request is filtered against the same
/// capabilities as the turn it summarises.
#[derive(Debug)]
pub struct TurnParams {
    pub params: ChatParams,
    pub caps: ProviderCapabilities,
    pub context_limit: usize,
    pub max_output: usize,
    /// `None` leaves the budget free to derive its own threshold.
    pub compact_threshold: Option<usize>,
    /// Handed back so callers that also need pricing don't query it twice.
    pub model_config: Option<ModelConfigRow>,
}

pub struct TurnParamsResolveRequest<'a> {
    pub assistant: Option<&'a AssistantRow>,
    /// The provider actually used this turn, which a per-request override may
    /// have moved away from the assistant's own.
    pub provider_id: Option<&'a str>,
    pub provider_type: &'a str,
    pub api_format: &'a str,
    /// Which wire this turn will really go out on, so the capabilities reflect
    /// what can actually be sent rather than what the family supports in general.
    pub transport_profile: &'a str,
    pub model: &'a str,
    pub thinking_level: Option<&'a str>,
    pub fast: bool,
}

/// Which provider-side tools this turn actually asks for.
///
/// What the user switched on, after proving every stored name belongs to this
/// model's closed capability list. The stored list outlives the thing it names:
/// switching a model from Responses to chat-completions can leave
/// `["web_search"]` behind. Treating that as empty looks exactly like the tool
/// was deliberately switched off, so stale names and malformed JSON fail the
/// turn and point back to the damaged setting.
fn enabled_server_tools(
    model_config: Option<&ModelConfigRow>,
    caps: &ProviderCapabilities,
) -> Result<Vec<ServerToolKind>, String> {
    let Some(raw) = model_config.and_then(|mc| mc.server_tools.as_deref()) else {
        return Ok(Vec::new());
    };
    let requested = serde_json::from_str::<Vec<ServerToolKind>>(raw)
        .map_err(|error| format!("stored model server_tools is invalid: {error}"))?;
    let unsupported: Vec<ServerToolKind> = requested
        .iter()
        .copied()
        .filter(|tool| !caps.server_tools.contains(tool))
        .collect();
    if !unsupported.is_empty() {
        return Err(format!(
            "stored model server_tools contains unsupported tools: {}",
            unsupported
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(requested)
}

pub fn resolve_turn_params(pool: &DbPool, input: TurnParamsResolveRequest<'_>) -> Result<TurnParams, String> {
    let TurnParamsResolveRequest {
        assistant,
        provider_id,
        provider_type,
        api_format,
        transport_profile,
        model,
        thinking_level,
        fast,
    } = input;

    // Not a silent fallback. A pool timeout here used to be indistinguishable
    // from "this model has no config row": the turn would drop through to the
    // catalog defaults and run with a different context limit, output ceiling
    // and capability set than the user configured. Failing visibly beats
    // quietly changing the parameters of the request.
    let model_config = match provider_id {
        Some(pid) => {
            let mut conn = get_conn(pool)?;
            db::ops::model_config::get_by_provider_and_model(&mut conn, pid, model)
                .map_err(|e| format!("could not read the stored config for '{model}': {e}"))?
        }
        None => None,
    };

    let mut caps = provider::registry::get_capabilities(provider_type, api_format, transport_profile, model)?;
    provider::capabilities::apply_overrides(
        &mut caps,
        model_config.as_ref().and_then(|mc| mc.capability_overrides.as_deref()),
    )?;

    let context_limit = assistant
        .filter(|a| a.context_limit > 0)
        .map(|a| a.context_limit as usize)
        .or_else(|| model_config.as_ref().map(|mc| mc.context_window as usize))
        .or_else(|| caps.max_context_tokens.map(|t| t as usize))
        .ok_or_else(|| {
            format!("No context window known for '{model}'. Go to Settings → Provider → Model to set one.")
        })?;
    let max_output = model_config
        .as_ref()
        .and_then(|mc| mc.max_output_tokens.map(|t| t as usize))
        .or_else(|| caps.max_output_tokens.map(|t| t as usize))
        .ok_or_else(|| {
            format!("No max output tokens known for '{model}'. Go to Settings → Provider → Model to set one.")
        })?;

    let server_tools = enabled_server_tools(model_config.as_ref(), &caps)?;

    let (thinking_enabled, thinking_budget, thinking_effort) = provider::capabilities::resolve_thinking(
        assistant.map(|a| a.thinking_enabled != 0).unwrap_or(false),
        assistant.and_then(|a| a.thinking_budget),
        thinking_level,
    )?;

    let mut params = ChatParams {
        model: model.to_string(),
        temperature: assistant.and_then(|a| a.temperature.map(|t| t as f64)),
        top_p: assistant.and_then(|a| a.top_p.map(|t| t as f64)),
        max_tokens: assistant.and_then(|a| a.max_tokens).or(Some(max_output as i32)),
        thinking_enabled,
        thinking_budget,
        thinking_effort,
        fast,
        server_tools,
        // thinking_style and verbosity are derived from the catalog by
        // filter_params below, not supplied by the caller.
        ..Default::default()
    };
    provider::capabilities::filter_params(&mut params, &caps)?;

    Ok(TurnParams {
        params,
        caps,
        context_limit,
        max_output,
        compact_threshold: model_config.as_ref().map(|mc| mc.compact_threshold as usize),
        model_config,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decimal(raw: &str) -> crate::decimal::Decimal {
        raw.parse().unwrap()
    }

    #[test]
    fn test_provider_secret_name() {
        assert_eq!(provider_secret_name("my-provider-1"), "PROVIDER_MY_PROVIDER_1_KEY");
    }

    fn assistant_with(temperature: Option<f32>) -> AssistantRow {
        AssistantRow {
            id: "a1".into(),
            name: "A".into(),
            description: None,
            avatar: None,
            system_prompt: String::new(),
            provider_id: None,
            model_id: None,
            temperature,
            top_p: None,
            max_tokens: None,
            is_default: 0,
            sort_order: 0,
            created_at: 0,
            updated_at: 0,
            context_limit: 0,
            compact_keep_recent: 10,
            enabled_tools: None,
            thinking_enabled: 1,
            thinking_budget: Some(4096),
            tool_preset_id: None,
            auto_compact_enabled: 1,
        }
    }

    fn resolve_for(pool: &DbPool, model: &str, assistant: &AssistantRow) -> TurnParams {
        resolve_turn_params(
            pool,
            TurnParamsResolveRequest {
                assistant: Some(assistant),
                provider_id: None,
                provider_type: "openai",
                api_format: "responses",
                transport_profile: "standard",
                model,
                thinking_level: None,
                fast: false,
            },
        )
        .unwrap()
    }

    #[test]
    fn a_model_that_rejects_temperature_never_sees_one() {
        let pool = crate::db::test_db();
        let assistant = assistant_with(Some(0.7));

        let turn = resolve_for(&pool, "o3", &assistant);

        assert_eq!(turn.params.temperature, None);
    }

    #[test]
    fn dropping_thinking_leaves_the_rest_of_the_turn_alone() {
        let pool = crate::db::test_db();
        let assistant = assistant_with(Some(0.7));

        let turn = resolve_for(&pool, "gpt-4o", &assistant);
        let summarising = without_thinking(turn.params.clone());

        assert_eq!(summarising.temperature, turn.params.temperature);
        assert_eq!(summarising.max_tokens, turn.params.max_tokens);
        assert_eq!(summarising.model, turn.params.model);
        assert!(!summarising.thinking_enabled);
        assert_eq!(summarising.thinking_budget, None);
        assert_eq!(summarising.thinking_effort, None);
    }

    /// What decides whether this turn offers tools at all. The headless side
    /// used to answer it from `capabilities::resolve` directly, which does not
    /// apply overrides — so a model the user had told us takes no tools was
    /// still sent them, while every other parameter of the same request came
    /// from here and did honour the override.
    #[test]
    fn a_capability_override_reaches_the_turns_capabilities() {
        let pool = crate::db::test_db();
        {
            let mut conn = pool.get().unwrap();
            db::ops::provider::create_provider(
                &mut conn,
                &db::models::provider::ProviderInsert {
                    id: "p1",
                    name: "P",
                    provider_type: "openai",
                    base_url: "https://example.invalid",
                    is_enabled: 1,
                    sort_order: 0,
                    created_at: 0,
                    updated_at: 0,
                    api_format: "chat_completions",
                    catalog_id: None,
                    credential_kind: "api_key",
                    transport_profile: "standard",
                },
            )
            .unwrap();
            db::ops::model_config::upsert(
                &mut conn,
                &db::models::model_config::ModelConfigInsert {
                    id: "mc1",
                    provider_id: "p1",
                    model_id: "gpt-4o",
                    display_name: None,
                    context_window: 128_000,
                    compact_threshold: 0,
                    max_output_tokens: Some(16_384),
                    input_price: None,
                    output_price: None,
                    cache_read_price: None,
                    cache_write_price: None,
                    created_at: 0,
                    updated_at: 0,
                    capability_overrides: Some(r#"{"supports_tools": false}"#),
                    pricing_tiers: None,
                    server_tools: None,
                    server_tool_price: None,
                },
            )
            .unwrap();
        }
        let assistant = assistant_with(None);

        let with_provider = resolve_turn_params(
            &pool,
            TurnParamsResolveRequest {
                assistant: Some(&assistant),
                provider_id: Some("p1"),
                provider_type: "openai",
                api_format: "chat_completions",
                transport_profile: "standard",
                model: "gpt-4o",
                thinking_level: None,
                fast: false,
            },
        )
        .unwrap();
        assert!(!with_provider.caps.supports_tools);

        // And it is the row that says so, not the catalog.
        assert!(resolve_for(&pool, "gpt-4o", &assistant).caps.supports_tools);
    }

    /// The stored list outlives what it names. Moving a model from the Responses
    /// API to chat-completions leaves `["web_search"]` behind in the row; that
    /// stale first-party value must be reported rather than impersonating an
    /// intentionally empty list.
    #[test]
    fn a_server_tool_the_model_no_longer_supports_is_rejected() {
        let caps_responses = provider::capabilities::resolve("xai", Some("responses"), "grok-4.6");
        let caps_chat = provider::capabilities::resolve("xai", Some("chat_completions"), "grok-4.6");

        let mut config = configured_with(Some(r#"["web_search","x_search"]"#));
        assert_eq!(
            enabled_server_tools(Some(&config), &caps_responses).unwrap(),
            vec![ServerToolKind::WebSearch, ServerToolKind::XSearch],
        );
        let error = enabled_server_tools(Some(&config), &caps_chat).unwrap_err();
        assert!(error.contains("web_search") && error.contains("x_search"), "{error}");

        // And one the user switched on that this model never had.
        config.server_tools = Some(r#"["web_search","image_generation"]"#.into());
        let error = enabled_server_tools(Some(&config), &caps_responses).unwrap_err();
        assert!(error.contains("image_generation"), "{error}");
    }

    /// End to end, the way the desktop actually reaches it: a stored row, a
    /// Responses-format provider, and a turn that comes out asking the upstream
    /// to search.
    ///
    /// Worth its own test because the chain has two places it silently produces
    /// nothing — the row is only read when a `provider_id` is passed, and the
    /// list is then intersected with capabilities that depend on `api_format`.
    /// Either one failing looks identical from the outside: the model quietly
    /// goes on using the built-in `web_search`.
    #[test]
    fn a_configured_server_tool_reaches_the_turn() {
        let pool = crate::db::test_db();
        {
            let mut conn = pool.get().unwrap();
            db::ops::provider::create_provider(
                &mut conn,
                &db::models::provider::ProviderInsert {
                    id: "p1",
                    name: "xAI",
                    provider_type: "xai",
                    base_url: "https://api.x.ai/v1",
                    is_enabled: 1,
                    sort_order: 0,
                    created_at: 0,
                    updated_at: 0,
                    api_format: "responses",
                    catalog_id: None,
                    credential_kind: "api_key",
                    transport_profile: "standard",
                },
            )
            .unwrap();
            db::ops::model_config::upsert(
                &mut conn,
                &db::models::model_config::ModelConfigInsert {
                    id: "mc1",
                    provider_id: "p1",
                    model_id: "grok-4.6",
                    display_name: None,
                    context_window: 500_000,
                    compact_threshold: 400_000,
                    max_output_tokens: Some(64_000),
                    input_price: Some(decimal("2")),
                    output_price: Some(decimal("6")),
                    cache_read_price: Some(decimal("0.5")),
                    cache_write_price: None,
                    created_at: 0,
                    updated_at: 0,
                    capability_overrides: None,
                    pricing_tiers: None,
                    server_tools: Some(r#"["web_search"]"#),
                    server_tool_price: None,
                },
            )
            .unwrap();
        }
        let assistant = assistant_with(None);

        let turn = resolve_turn_params(
            &pool,
            TurnParamsResolveRequest {
                assistant: Some(&assistant),
                provider_id: Some("p1"),
                provider_type: "xai",
                api_format: "responses",
                transport_profile: "standard",
                model: "grok-4.6",
                thinking_level: None,
                fast: false,
            },
        )
        .unwrap();
        assert_eq!(turn.params.server_tools, vec![ServerToolKind::WebSearch]);

        // The same row, reached over the dialect that has no such thing.
        let error = resolve_turn_params(
            &pool,
            TurnParamsResolveRequest {
                assistant: Some(&assistant),
                provider_id: Some("p1"),
                provider_type: "xai",
                api_format: "chat_completions",
                transport_profile: "standard",
                model: "grok-4.6",
                thinking_level: None,
                fast: false,
            },
        )
        .unwrap_err();
        assert!(error.contains("web_search"), "{error}");
    }

    /// Only absence and an explicit empty array mean "ask for none". Broken
    /// JSON and wrong element types are damaged persisted contracts.
    #[test]
    fn only_an_absent_or_explicitly_empty_server_tool_list_asks_for_none() {
        let caps = provider::capabilities::resolve("xai", Some("responses"), "grok-4.6");
        assert!(enabled_server_tools(None, &caps).unwrap().is_empty());
        assert!(
            enabled_server_tools(Some(&configured_with(None)), &caps)
                .unwrap()
                .is_empty()
        );
        assert!(
            enabled_server_tools(Some(&configured_with(Some("[]"))), &caps)
                .unwrap()
                .is_empty()
        );

        for raw in ["not json", "{}", r#"[1,2]"#, r#"["web_search",1]"#] {
            let config = configured_with(Some(raw));
            let error =
                enabled_server_tools(Some(&config), &caps).expect_err("malformed stored server_tools must fail");
            assert!(error.contains("server_tools"), "{raw:?}: {error}");
        }
    }

    fn seed_provider_with(pool: &DbPool, api_format: &str, credential_kind: &str, transport_profile: &str) {
        let mut conn = pool.get().unwrap();
        db::ops::provider::create_provider(
            &mut conn,
            &db::models::provider::ProviderInsert {
                id: "p1",
                name: "Deepseek",
                provider_type: "deepseek",
                base_url: "https://api.deepseek.com/v1/",
                is_enabled: 1,
                sort_order: 0,
                created_at: 0,
                updated_at: 0,
                api_format,
                catalog_id: None,
                credential_kind,
                transport_profile,
            },
        )
        .unwrap();
    }

    fn seed_provider(pool: &DbPool) {
        seed_provider_with(pool, "chat_completions", "api_key", "standard");
    }

    fn mock_secrets(dir: &std::path::Path) -> SecretsManager {
        SecretsManager::new_with_keyring_store(
            dir.to_path_buf(),
            std::sync::Arc::new(crate::keyring::test_support::MockKeyringStore::new()),
        )
    }

    #[test]
    fn an_unknown_stored_credential_kind_is_reported_through_fallback_resolution() {
        let pool = crate::db::test_db();
        seed_provider_with(&pool, "chat_completions", "future_login", "standard");
        let dir = tempfile::tempdir().unwrap();
        let secrets = mock_secrets(dir.path());
        let mut assistant = assistant_with(None);
        assistant.model_id = Some("deepseek-chat".into());

        let error = resolve_provider_config(&secrets, &pool, Some(&assistant))
            .err()
            .expect("an unknown credential kind must not become an API-key provider");

        assert!(error.contains("future_login"), "{error}");
        assert!(error.contains("credential"), "{error}");
        assert!(!error.contains("No provider configured"), "{error}");
    }

    /// How the auto reviewer resolves its model: a bare `provider:model` pair
    /// and no assistant at all. The base resolution used to run first anyway
    /// and fail on the assistant it did not have — so every review, however
    /// the feature was configured, came back "No model configured" while the
    /// pair the user had saved sat unread in the overrides.
    #[test]
    fn a_full_override_pair_needs_no_assistant() {
        let pool = crate::db::test_db();
        seed_provider(&pool);
        let dir = tempfile::tempdir().unwrap();
        let secrets = mock_secrets(dir.path());
        secrets
            .set(
                &SecretScope::Global,
                &SecretName::new("PROVIDER_P1_KEY").unwrap(),
                "sk-test",
            )
            .unwrap();

        let resolved = resolve_with_overrides(&secrets, &pool, None, Some("deepseek-v4-flash".into()), Some("p1"))
            .expect("a complete pair of overrides is a complete destination");

        assert_eq!(resolved.provider_id, "p1");
        assert_eq!(resolved.provider_name, "Deepseek");
        assert_eq!(resolved.model, "deepseek-v4-flash");
        assert_eq!(
            resolved.base_url, "https://api.deepseek.com/v1",
            "trailing slash trimmed"
        );
        assert_eq!(resolved.credential.api_key(), "sk-test");
        assert_eq!(resolved.api_format, "chat_completions");
    }

    /// And when the pair cannot resolve, the error is about the pair — the
    /// named provider's missing key — never about the assistant nobody passed.
    #[test]
    fn a_full_override_pair_fails_about_itself() {
        let pool = crate::db::test_db();
        seed_provider(&pool);
        let dir = tempfile::tempdir().unwrap();
        let secrets = mock_secrets(dir.path());

        let Err(err) = resolve_with_overrides(&secrets, &pool, None, Some("deepseek-v4-flash".into()), Some("p1"))
        else {
            panic!("no key was stored, so this cannot resolve");
        };

        assert!(err.contains("API Key not set"), "{err}");
        assert!(!err.contains("No model configured"), "{err}");
    }

    fn configured_with(server_tools: Option<&str>) -> ModelConfigRow {
        ModelConfigRow {
            id: "mc".into(),
            provider_id: "p".into(),
            model_id: "grok-4.6".into(),
            display_name: None,
            context_window: 500_000,
            compact_threshold: 400_000,
            max_output_tokens: None,
            input_price: Some(decimal("2")),
            output_price: Some(decimal("6")),
            cache_read_price: None,
            cache_write_price: None,
            created_at: 0,
            updated_at: 0,
            capability_overrides: None,
            pricing_tiers: None,
            server_tools: server_tools.map(str::to_string),
            server_tool_price: None,
        }
    }

    #[test]
    fn an_unknown_model_asks_the_user_to_configure_it() {
        let pool = crate::db::test_db();
        let assistant = assistant_with(None);

        let err = resolve_turn_params(
            &pool,
            TurnParamsResolveRequest {
                assistant: Some(&assistant),
                provider_id: None,
                provider_type: "openai",
                api_format: "chat_completions",
                transport_profile: "standard",
                model: "some-model-nobody-catalogued",
                thinking_level: None,
                fast: false,
            },
        )
        .unwrap_err();

        assert!(err.contains("Settings"), "{err}");
    }
}

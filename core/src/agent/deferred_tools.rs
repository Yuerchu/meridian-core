//! Cross-protocol Progressive Tool Disclosure (Deferred Tool Loading).
//!
//! Coordinates native deferred tool loading across Anthropic Messages, OpenAI /
//! DeepSeek Responses, and Google Gemini Interactions while providing a
//! universal client dual-tool gateway (`search_tools` + `call_tool`) for
//! protocols and models without native deferred loading (Chat Completions,
//! `deepseek-v4-pro`, local models, or DeepSeek with web search enabled).
//!
//! All discovered tool snapshots are frozen into [`FrozenToolSearchResult`]
//! rows (`schema_version = TOOL_SEARCH_RESULT_SCHEMA_VERSION`) so the Prompt
//! Cache prefix stays byte-for-byte invariant across replays and mid-conversation
//! model switches never produce upstream 400 errors.

use std::collections::{HashMap, HashSet};

use crate::provider::{
    AnthropicToolSearchStrategy, ChatParams, DeferredToolMode, FrozenToolSearchResult, TOOL_SEARCH_CALL_SENTINEL,
    ToolDefinition,
};

/// Client-side deferred tool search function name.
pub const SEARCH_TOOLS_NAME: &str = "search_tools";

/// Client-side deferred tool invocation gateway name (used only under
/// `DisclosureMode::ClientGateway`).
pub const CALL_TOOL_NAME: &str = "call_tool";

/// `messages.schema_version` written exclusively by the turn loop for a
/// persisted [`FrozenToolSearchResult`] row.
pub const TOOL_SEARCH_RESULT_SCHEMA_VERSION: i32 = 4;

/// Preference key selecting the Anthropic deferred search strategy
/// (`"server_bm25"` default | `"server_regex"` | `"client_tool_reference"`).
pub const ANTHROPIC_SEARCH_STRATEGY_PREF: &str = "tools.anthropic_search_strategy";

const DEFAULT_SEARCH_LIMIT: usize = 10;
const MAX_SEARCH_LIMIT: usize = 25;

/// Pluggable local tool catalog search index (default: [`Bm25ToolIndex`]).
pub trait DeferredToolIndex: Send + Sync {
    fn search(&self, query: &str, limit: usize, allowed: &dyn Fn(&str) -> bool) -> Vec<ToolDefinition>;
}

/// Deterministic BM25 index over deferred tool names, descriptions, and
/// top-level parameter schemas.
#[derive(Debug, Clone, Default)]
pub struct Bm25ToolIndex {
    entries: Vec<IndexedTool>,
    df: HashMap<String, usize>,
    avg_len: f64,
}

#[derive(Debug, Clone)]
struct IndexedTool {
    def: ToolDefinition,
    name_lower: String,
    tf: HashMap<String, usize>,
    doc_len: usize,
}

impl Bm25ToolIndex {
    pub fn build(defs: Vec<ToolDefinition>) -> Self {
        let mut entries = Vec::with_capacity(defs.len());
        let mut df: HashMap<String, usize> = HashMap::new();
        let mut total_len = 0usize;

        for def in defs {
            let mut text = String::new();
            text.push_str(&def.name);
            text.push(' ');
            text.push_str(&def.name);
            text.push(' ');
            text.push_str(&def.description);
            if let Some(props) = def.parameters.get("properties").and_then(|v| v.as_object()) {
                for (key, val) in props {
                    text.push(' ');
                    text.push_str(key);
                    if let Some(desc) = val.get("description").and_then(|d| d.as_str()) {
                        text.push(' ');
                        text.push_str(desc);
                    }
                }
            }
            let tokens = tokenize(&text);
            let doc_len = tokens.len();
            total_len += doc_len;
            let mut tf: HashMap<String, usize> = HashMap::new();
            for tok in tokens {
                *tf.entry(tok).or_insert(0) += 1;
            }
            for tok in tf.keys() {
                *df.entry(tok.clone()).or_insert(0) += 1;
            }
            entries.push(IndexedTool {
                name_lower: def.name.to_ascii_lowercase(),
                def,
                tf,
                doc_len,
            });
        }
        entries.sort_by(|a, b| a.def.name.cmp(&b.def.name));
        let avg_len = if entries.is_empty() {
            1.0
        } else {
            (total_len as f64 / entries.len() as f64).max(1.0)
        };
        Self { entries, df, avg_len }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Compact summary of deferred tool names / server groups for the search
    /// tool's description (stays small and deterministic for Prompt Caching).
    pub fn catalog_hint(&self) -> String {
        if self.entries.is_empty() {
            return String::new();
        }
        let mut groups: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for entry in &self.entries {
            let group = if let Some(rest) = entry.def.name.strip_prefix("mcp__") {
                rest.split("__").next().unwrap_or("mcp").to_string()
            } else {
                "other".to_string()
            };
            *groups.entry(group).or_insert(0) += 1;
        }
        let summary: Vec<String> = groups
            .into_iter()
            .map(|(group, count)| format!("{group} ({count})"))
            .collect();
        format!("Available deferred tool groups: {}.", summary.join(", "))
    }
}

impl DeferredToolIndex for Bm25ToolIndex {
    fn search(&self, query: &str, limit: usize, allowed: &dyn Fn(&str) -> bool) -> Vec<ToolDefinition> {
        let limit = limit.clamp(1, MAX_SEARCH_LIMIT);
        let q_trimmed = query.trim().to_ascii_lowercase();
        let q_tokens = tokenize(&q_trimmed);

        if q_tokens.is_empty() {
            return self
                .entries
                .iter()
                .filter(|e| allowed(&e.def.name))
                .take(limit)
                .map(|e| e.def.clone())
                .collect();
        }

        let n = self.entries.len() as f64;
        const K1: f64 = 1.5;
        const B: f64 = 0.75;

        let mut scored: Vec<(f64, &IndexedTool)> = Vec::new();
        for entry in &self.entries {
            if !allowed(&entry.def.name) {
                continue;
            }
            let mut score = 0.0f64;
            if entry.name_lower == q_trimmed {
                score += 6.0;
            } else if !q_trimmed.is_empty() && entry.name_lower.contains(&q_trimmed) {
                score += 3.0;
            }
            for tok in &q_tokens {
                if let Some(&freq) = entry.tf.get(tok) {
                    let df = *self.df.get(tok).unwrap_or(&1) as f64;
                    let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln().max(0.01);
                    let tf_f = freq as f64;
                    let denom = tf_f + K1 * (1.0 - B + B * (entry.doc_len as f64 / self.avg_len));
                    score += idf * ((tf_f * (K1 + 1.0)) / denom);
                } else if entry.name_lower.contains(tok.as_str()) {
                    score += 0.5;
                }
            }
            if score > 0.0 {
                scored.push((score, entry));
            }
        }
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.def.name.cmp(&b.1.def.name))
        });
        scored
            .into_iter()
            .take(limit)
            .map(|(_, entry)| entry.def.clone())
            .collect()
    }
}

fn tokenize(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in input.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                current.push(lc);
            }
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Active progressive disclosure strategy for one turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisclosureMode {
    /// No deferred tools in this turn.
    None,
    /// Anthropic native server-side search (`tool_search_tool_bm25` / `regex`).
    AnthropicServerSearch,
    /// Anthropic / DeepSeek client-side `search_tools` returning `tool_reference` blocks.
    AnthropicClientReference,
    /// OpenAI / DeepSeek Responses native `tool_search` (`execution: "client"`).
    ResponsesClientSearch,
    /// Google Gemini Interactions native server-side `tool_search`.
    GeminiServerSearch,
    /// Universal client dual-tool gateway (`search_tools` + `call_tool`).
    ClientGateway,
}

/// Per-turn progressive disclosure state shared by the turn loop.
#[derive(Debug, Clone)]
pub struct DeferredToolSession {
    mode: DisclosureMode,
    index: Bm25ToolIndex,
    deferred_names: HashSet<String>,
}

impl DeferredToolSession {
    pub fn disabled() -> Self {
        Self {
            mode: DisclosureMode::None,
            index: Bm25ToolIndex::default(),
            deferred_names: HashSet::new(),
        }
    }

    pub fn mode(&self) -> DisclosureMode {
        self.mode
    }

    /// Configure `tool_defs` and `params` for this turn's deferred tool set.
    ///
    /// - When `deferred_tools` is empty (or none exist in `tool_defs`), leaves
    ///   everything untouched (`DisclosureMode::None`).
    /// - Mutates `params.deferred_tool_names` and `params.client_tool_search_description`
    ///   deterministically so the prompt cache prefix stays byte-stable.
    pub fn configure(
        tool_defs: &mut Vec<ToolDefinition>,
        deferred_tools: &HashSet<String>,
        params: &mut ChatParams,
    ) -> Self {
        params.deferred_tool_names.clear();
        params.client_tool_search_description = None;

        if deferred_tools.is_empty() {
            return Self::disabled();
        }

        let deferred_defs: Vec<ToolDefinition> = tool_defs
            .iter()
            .filter(|t| deferred_tools.contains(&t.name))
            .cloned()
            .collect();
        if deferred_defs.is_empty() {
            return Self::disabled();
        }

        let deferred_names: HashSet<String> = deferred_defs.iter().map(|t| t.name.clone()).collect();
        let mut sorted_deferred: Vec<String> = deferred_names.iter().cloned().collect();
        sorted_deferred.sort();

        let index = Bm25ToolIndex::build(deferred_defs);
        let hint = index.catalog_hint();

        let mode = match params.deferred_tool_mode {
            DeferredToolMode::AnthropicDeferred => {
                let is_deepseek = params.model.to_ascii_lowercase().starts_with("deepseek");
                if is_deepseek
                    || params.anthropic_tool_search_strategy == AnthropicToolSearchStrategy::ClientToolReference
                {
                    DisclosureMode::AnthropicClientReference
                } else {
                    DisclosureMode::AnthropicServerSearch
                }
            }
            DeferredToolMode::ResponsesClientSearch => DisclosureMode::ResponsesClientSearch,
            DeferredToolMode::GeminiServerSearch => DisclosureMode::GeminiServerSearch,
            DeferredToolMode::None => DisclosureMode::ClientGateway,
        };

        match mode {
            DisclosureMode::AnthropicServerSearch | DisclosureMode::GeminiServerSearch => {
                // Keep all tools in `tool_defs`; protocol adapter tags
                // deferred tools with `defer_loading: true` and injects native
                // server search tool.
                params.deferred_tool_names = sorted_deferred;
            }
            DisclosureMode::AnthropicClientReference => {
                // Keep deferred tools in `tool_defs` (tagged `defer_loading: true`
                // by `anthropic_messages.rs`) and add eager client `search_tools`.
                params.deferred_tool_names = sorted_deferred;
                tool_defs.push(search_tools_definition(&hint, false));
            }
            DisclosureMode::ResponsesClientSearch => {
                // Adapter omits deferred tools from top-level `tools` and emits
                // `{"type":"tool_search","execution":"client"}`.
                params.deferred_tool_names = sorted_deferred;
                params.client_tool_search_description = Some(format!(
                    "Search deferred external tools by keyword to load their schemas on demand. {hint}"
                ));
            }
            DisclosureMode::ClientGateway => {
                // Strip deferred tools from top-level `tools` and inject eager
                // `search_tools` + `call_tool` gateway.
                tool_defs.retain(|t| !deferred_names.contains(&t.name));
                tool_defs.push(search_tools_definition(&hint, true));
                tool_defs.push(call_tool_definition());
            }
            DisclosureMode::None => {}
        }

        Self {
            mode,
            index,
            deferred_names,
        }
    }

    pub fn is_search_call(&self, name: &str) -> bool {
        match self.mode {
            DisclosureMode::ResponsesClientSearch => name == TOOL_SEARCH_CALL_SENTINEL || name == SEARCH_TOOLS_NAME,
            DisclosureMode::AnthropicClientReference | DisclosureMode::ClientGateway => name == SEARCH_TOOLS_NAME,
            DisclosureMode::None | DisclosureMode::AnthropicServerSearch | DisclosureMode::GeminiServerSearch => false,
        }
    }

    pub fn is_gateway_call(&self, name: &str) -> bool {
        self.mode == DisclosureMode::ClientGateway && name == CALL_TOOL_NAME
    }

    pub fn is_deferred(&self, name: &str) -> bool {
        self.deferred_names.contains(name)
    }

    /// Execute a local deferred tool search and freeze the result into a
    /// [`FrozenToolSearchResult`] envelope.
    pub fn execute_search(
        &self,
        arguments: &str,
        allowed: &dyn Fn(&str) -> bool,
    ) -> Result<FrozenToolSearchResult, String> {
        let parsed: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("invalid search_tools arguments JSON: {e}"))?;
        let obj = parsed
            .as_object()
            .ok_or_else(|| "search_tools arguments must be a JSON object".to_string())?;
        let query = obj
            .get("query")
            .or_else(|| obj.get("pattern"))
            .or_else(|| obj.get("keywords"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let limit = obj
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_SEARCH_LIMIT);

        let matched = self.index.search(query, limit, allowed);
        let tool_names: Vec<String> = matched.iter().map(|t| t.name.clone()).collect();
        let format = match self.mode {
            DisclosureMode::ResponsesClientSearch => "responses_client",
            DisclosureMode::AnthropicClientReference => "anthropic_client_ref",
            DisclosureMode::ClientGateway => "client_gateway",
            _ => "client_gateway",
        }
        .to_string();

        let summary = if matched.is_empty() {
            format!("No deferred tools matched query `{query}`.")
        } else if self.mode == DisclosureMode::ClientGateway {
            let mut lines = vec![format!(
                "Found {} deferred tool(s). Invoke any of them using `call_tool` with `{{\"name\": \"...\", \"arguments\": {{...}}}}`:",
                matched.len()
            )];
            for tool in &matched {
                let schema = serde_json::to_string(&tool.parameters).unwrap_or_else(|_| "{}".into());
                lines.push(format!(
                    "- `{}`: {}\n  parameters: {}",
                    tool.name, tool.description, schema
                ));
            }
            lines.join("\n")
        } else {
            let names_list: Vec<String> = matched
                .iter()
                .map(|t| {
                    let first_line = t.description.lines().next().unwrap_or("").trim();
                    if first_line.is_empty() {
                        format!("- `{}`", t.name)
                    } else {
                        format!("- `{}`: {first_line}", t.name)
                    }
                })
                .collect();
            format!(
                "Loaded {} deferred tool(s) into active context:\n{}",
                matched.len(),
                names_list.join("\n")
            )
        };

        Ok(FrozenToolSearchResult {
            format,
            tool_names,
            tools: matched,
            summary,
        })
    }
}

/// Parse arguments for the universal `call_tool` gateway.
pub fn parse_gateway_call(arguments: &str) -> Result<(String, String), String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct GatewayArgs {
        name: String,
        #[serde(default)]
        arguments: Option<serde_json::Value>,
    }
    let parsed: GatewayArgs =
        serde_json::from_str(arguments).map_err(|e| format!("invalid call_tool arguments: {e}"))?;
    if parsed.name.trim().is_empty() {
        return Err("call_tool requires a non-empty `name`".into());
    }
    let args_val = parsed.arguments.unwrap_or_else(|| serde_json::json!({}));
    if !args_val.is_object() {
        return Err("call_tool `arguments` must be a JSON object".into());
    }
    let encoded = serde_json::to_string(&args_val).map_err(|e| format!("could not encode target arguments: {e}"))?;
    Ok((parsed.name, encoded))
}

fn search_tools_definition(hint: &str, with_gateway: bool) -> ToolDefinition {
    let mut description = if with_gateway {
        "Search deferred external tools (such as MCP integrations) by keyword to discover their names and parameter schemas, then execute them via `call_tool`.".to_string()
    } else {
        "Search deferred external tools (such as MCP integrations) by keyword to load their full definitions into this conversation so you can call them directly.".to_string()
    };
    if !hint.is_empty() {
        description.push(' ');
        description.push_str(hint);
    }
    ToolDefinition {
        name: SEARCH_TOOLS_NAME.to_string(),
        description,
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords describing the integration or action you want to find (leave empty to list available deferred tools)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of matching tools to return (default 10, max 25)."
                }
            },
            "required": ["query"]
        }),
    }
}

fn call_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: CALL_TOOL_NAME.to_string(),
        description: "Invoke a deferred tool discovered via `search_tools`, passing its exact `name` and JSON `arguments` object. Subject to the same authorization and approval rules as direct tool calls.".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Exact qualified name of the deferred tool returned by `search_tools`."
                },
                "arguments": {
                    "type": "object",
                    "description": "JSON object of arguments matching the tool's parameter schema."
                }
            },
            "required": ["name", "arguments"]
        }),
    }
}

/// Read the user's configured [`AnthropicToolSearchStrategy`] from `preferences`.
pub async fn read_anthropic_search_strategy(
    db: &impl crate::db::sea::cap::Read,
) -> Result<AnthropicToolSearchStrategy, String> {
    let raw = crate::db::sea::ops::preference::get(db, ANTHROPIC_SEARCH_STRATEGY_PREF)
        .await
        .map_err(|e| format!("failed to read `{ANTHROPIC_SEARCH_STRATEGY_PREF}`: {e}"))?;
    match raw.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(AnthropicToolSearchStrategy::default()),
        Some(value) => AnthropicToolSearchStrategy::parse(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str, desc: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: desc.to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "item": { "type": "string", "description": "item name" }
                }
            }),
        }
    }

    #[test]
    fn bm25_ranks_matching_mcp_tool_first_and_respects_authorizer() {
        let index = Bm25ToolIndex::build(vec![
            def("mcp__github__create_issue", "Create a GitHub issue in a repository"),
            def(
                "mcp__luckin__order_coffee",
                "Order a cup of Luckin coffee with sugar level",
            ),
            def("mcp__fetch__get_url", "Fetch the contents of a web URL"),
        ]);

        let hits = index.search("luckin coffee", 5, &|_| true);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].name, "mcp__luckin__order_coffee");

        // Authorizer denies luckin -> must not leak in search results.
        let filtered = index.search("luckin coffee", 5, &|name| !name.contains("luckin"));
        assert!(filtered.iter().all(|t| !t.name.contains("luckin")));
    }

    #[test]
    fn configure_selects_expected_disclosure_mode_per_protocol() {
        let deferred: HashSet<String> = ["mcp__luckin__order_coffee".to_string()].into_iter().collect();

        // 1. Anthropic server BM25 (default for Claude)
        let mut defs = vec![
            def("read_file", "Read a file"),
            def("mcp__luckin__order_coffee", "Order coffee"),
        ];
        let mut params = ChatParams {
            model: "claude-sonnet-4-6".into(),
            deferred_tool_mode: DeferredToolMode::AnthropicDeferred,
            anthropic_tool_search_strategy: AnthropicToolSearchStrategy::ServerBm25,
            ..Default::default()
        };
        let session = DeferredToolSession::configure(&mut defs, &deferred, &mut params);
        assert_eq!(session.mode(), DisclosureMode::AnthropicServerSearch);
        assert_eq!(params.deferred_tool_names, vec!["mcp__luckin__order_coffee"]);
        assert!(!defs.iter().any(|d| d.name == SEARCH_TOOLS_NAME));

        // 2. Anthropic client tool_reference (DeepSeek Anthropic or user override)
        let mut defs = vec![
            def("read_file", "Read a file"),
            def("mcp__luckin__order_coffee", "Order coffee"),
        ];
        let mut params = ChatParams {
            model: "deepseek-v4-flash".into(),
            deferred_tool_mode: DeferredToolMode::AnthropicDeferred,
            anthropic_tool_search_strategy: AnthropicToolSearchStrategy::ServerBm25,
            ..Default::default()
        };
        let session = DeferredToolSession::configure(&mut defs, &deferred, &mut params);
        assert_eq!(session.mode(), DisclosureMode::AnthropicClientReference);
        assert!(defs.iter().any(|d| d.name == SEARCH_TOOLS_NAME));
        assert!(!defs.iter().any(|d| d.name == CALL_TOOL_NAME));

        // 3. Responses client search
        let mut defs = vec![
            def("read_file", "Read a file"),
            def("mcp__luckin__order_coffee", "Order coffee"),
        ];
        let mut params = ChatParams {
            model: "gpt-5.4".into(),
            deferred_tool_mode: DeferredToolMode::ResponsesClientSearch,
            ..Default::default()
        };
        let session = DeferredToolSession::configure(&mut defs, &deferred, &mut params);
        assert_eq!(session.mode(), DisclosureMode::ResponsesClientSearch);
        assert!(params.client_tool_search_description.is_some());
        assert!(session.is_search_call(TOOL_SEARCH_CALL_SENTINEL));

        // 4. Fallback universal client dual-tool gateway (Chat Completions / deepseek-v4-pro)
        let mut defs = vec![
            def("read_file", "Read a file"),
            def("mcp__luckin__order_coffee", "Order coffee"),
        ];
        let mut params = ChatParams {
            model: "deepseek-v4-pro".into(),
            deferred_tool_mode: DeferredToolMode::None,
            ..Default::default()
        };
        let session = DeferredToolSession::configure(&mut defs, &deferred, &mut params);
        assert_eq!(session.mode(), DisclosureMode::ClientGateway);
        assert!(!defs.iter().any(|d| d.name == "mcp__luckin__order_coffee"));
        assert!(defs.iter().any(|d| d.name == SEARCH_TOOLS_NAME));
        assert!(defs.iter().any(|d| d.name == CALL_TOOL_NAME));

        let result = session.execute_search(r#"{"query":"coffee"}"#, &|_| true).unwrap();
        assert_eq!(result.tool_names, vec!["mcp__luckin__order_coffee"]);
        assert!(result.summary.contains("call_tool"));
    }

    #[test]
    fn parse_gateway_call_validates_strict_shape() {
        let (name, args) =
            parse_gateway_call(r#"{"name":"mcp__fetch__get","arguments":{"url":"https://a.test"}}"#).unwrap();
        assert_eq!(name, "mcp__fetch__get");
        assert_eq!(args, r#"{"url":"https://a.test"}"#);

        assert!(parse_gateway_call(r#"{"name":"","arguments":{}}"#).is_err());
        assert!(parse_gateway_call(r#"{"name":"x","arguments":[1,2]}"#).is_err());
        assert!(parse_gateway_call(r#"{"name":"x","arguments":{},"extra":1}"#).is_err());
    }
}

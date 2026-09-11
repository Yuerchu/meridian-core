pub mod builtin;
pub mod rule;

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use diesel::SqliteConnection;

use crate::db::models::redaction_rule::RuleCategory;
use crate::db::ops::redaction_rule as ops;
use crate::provider::ChatMessage;

use self::builtin::BUILTIN_RULES;
use self::rule::{CompiledRule, RuleKind, compile};

pub const MODE_PREFERENCE: &str = "redaction.mode";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactionMode {
    Off,
    Secrets,
    Standard,
    Strict,
}

impl RedactionMode {
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw {
            None => Ok(Self::Standard),
            Some("off") => Ok(Self::Off),
            Some("secrets") => Ok(Self::Secrets),
            Some("standard") => Ok(Self::Standard),
            Some("strict") => Ok(Self::Strict),
            Some(other) => Err(format!(
                "unknown redaction mode `{other}`; valid values: off, secrets, standard, strict"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Secrets => "secrets",
            Self::Standard => "standard",
            Self::Strict => "strict",
        }
    }

    pub fn is_enabled(self) -> bool {
        self != Self::Off
    }

    pub fn includes(self, category: RuleCategory) -> bool {
        match self {
            Self::Off => false,
            Self::Secrets => matches!(category, RuleCategory::Secret),
            Self::Standard => matches!(category, RuleCategory::Secret | RuleCategory::Pii),
            Self::Strict => true,
        }
    }
}

struct RuleSet {
    mode: RedactionMode,
    builtin: Vec<CompiledRule>,
    global: Vec<CompiledRule>,
    by_project: HashMap<String, Vec<CompiledRule>>,
}

#[derive(Debug, Clone)]
pub struct RuleHit {
    pub name: String,
    pub count: usize,
}

pub struct RedactResult<'a> {
    pub text: Cow<'a, str>,
    pub hits: Vec<RuleHit>,
}

#[derive(Debug, Default)]
pub struct RedactionReport {
    pub hits: Vec<RuleHit>,
    pub messages_touched: usize,
}

impl RedactionReport {
    fn merge_hit(&mut self, name: &str, count: usize) {
        if count == 0 {
            return;
        }
        if let Some(h) = self.hits.iter_mut().find(|h| h.name == name) {
            h.count += count;
        } else {
            self.hits.push(RuleHit {
                name: name.to_string(),
                count,
            });
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuleSummary {
    pub name: String,
    pub kind: String,
    pub category: String,
    pub description: String,
    pub active: bool,
}

pub struct RedactionEngine {
    compiled: RwLock<Arc<RuleSet>>,
}

impl RedactionEngine {
    pub fn new() -> Self {
        let mode = RedactionMode::Standard;
        let builtin = compile_builtins();
        Self {
            compiled: RwLock::new(Arc::new(RuleSet {
                mode,
                builtin,
                global: Vec::new(),
                by_project: HashMap::new(),
            })),
        }
    }

    pub fn disabled() -> Self {
        Self {
            compiled: RwLock::new(Arc::new(RuleSet {
                mode: RedactionMode::Off,
                builtin: Vec::new(),
                global: Vec::new(),
                by_project: HashMap::new(),
            })),
        }
    }

    pub fn reload(&self, conn: &mut SqliteConnection) -> Result<(), String> {
        use crate::db::ops::preference::get_preference;

        let mode_raw = get_preference(conn, MODE_PREFERENCE).map_err(|e| e.to_string())?;
        let mode = RedactionMode::parse(mode_raw.as_deref())?;

        let builtin = compile_builtins();
        let mut global = Vec::new();
        let mut by_project: HashMap<String, Vec<CompiledRule>> = HashMap::new();

        if mode.is_enabled() {
            let rows = ops::list_enabled_rules(conn).map_err(|e| e.to_string())?;
            for row in &rows {
                let category = match row.category() {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!(rule_id = %row.id, name = %row.name, "skipping rule with bad category: {e}");
                        continue;
                    }
                };
                let regex = match compile(&row.pattern) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!(rule_id = %row.id, name = %row.name, "skipping rule that fails to compile: {e}");
                        continue;
                    }
                };
                let kind = match row.scope_type.as_str() {
                    "global" => RuleKind::Global,
                    "project" => RuleKind::Project(row.scope_id.clone()),
                    other => {
                        tracing::error!(rule_id = %row.id, "skipping rule with unknown scope_type: {other}");
                        continue;
                    }
                };
                let compiled = CompiledRule::new(row.name.clone(), kind.clone(), category, regex, None);

                match kind {
                    RuleKind::Global | RuleKind::Builtin => global.push(compiled),
                    RuleKind::Project(ref pid) => by_project.entry(pid.clone()).or_default().push(compiled),
                }
            }
        }

        let global_count = global.len();
        let project_count = by_project.len();
        let new_set = Arc::new(RuleSet {
            mode,
            builtin,
            global,
            by_project,
        });
        *self.compiled.write().unwrap() = new_set;

        tracing::info!(
            mode = mode.as_str(),
            global_custom = global_count,
            projects = project_count,
            "redaction engine reloaded"
        );
        Ok(())
    }

    pub fn mode(&self) -> RedactionMode {
        self.compiled.read().unwrap().mode
    }

    pub fn is_enabled(&self) -> bool {
        self.mode().is_enabled()
    }

    pub fn redact<'a>(&self, text: &'a str, project_id: Option<&str>) -> RedactResult<'a> {
        let set = self.compiled.read().unwrap().clone();
        if !set.mode.is_enabled() {
            return RedactResult {
                text: Cow::Borrowed(text),
                hits: Vec::new(),
            };
        }

        let mut current: Cow<'a, str> = Cow::Borrowed(text);
        let mut hits = Vec::new();

        for rule in &set.builtin {
            if !set.mode.includes(rule.category) {
                continue;
            }
            let (result, count) = rule.apply(&current);
            if count > 0 {
                hits.push(RuleHit {
                    name: rule.name.clone(),
                    count,
                });
                current = Cow::Owned(result.into_owned());
            }
        }

        for rule in &set.global {
            let (result, count) = rule.apply(&current);
            if count > 0 {
                hits.push(RuleHit {
                    name: rule.name.clone(),
                    count,
                });
                current = Cow::Owned(result.into_owned());
            }
        }

        if let Some(pid) = project_id
            && let Some(project_rules) = set.by_project.get(pid)
        {
            for rule in project_rules {
                let (result, count) = rule.apply(&current);
                if count > 0 {
                    hits.push(RuleHit {
                        name: rule.name.clone(),
                        count,
                    });
                    current = Cow::Owned(result.into_owned());
                }
            }
        }

        RedactResult { text: current, hits }
    }

    pub fn redact_messages(&self, messages: &mut [ChatMessage], project_id: Option<&str>) -> RedactionReport {
        let mut report = RedactionReport::default();

        if !self.is_enabled() {
            return report;
        }

        for msg in messages.iter_mut() {
            if !should_scan_role(&msg.role) {
                continue;
            }
            if msg.content.is_empty() {
                continue;
            }

            if let Some(redacted) = self.redact_content(&msg.content, project_id, &mut report) {
                msg.content = redacted;
            }
        }

        report
    }

    fn redact_content(&self, content: &str, project_id: Option<&str>, report: &mut RedactionReport) -> Option<String> {
        if content.starts_with("[{") {
            self.redact_multimodal(content, project_id, report)
        } else {
            let result = self.redact(content, project_id);
            if result.hits.is_empty() {
                None
            } else {
                report.messages_touched += 1;
                for h in &result.hits {
                    report.merge_hit(&h.name, h.count);
                }
                Some(result.text.into_owned())
            }
        }
    }

    fn redact_multimodal(
        &self,
        content: &str,
        project_id: Option<&str>,
        report: &mut RedactionReport,
    ) -> Option<String> {
        let Ok(mut parts) = serde_json::from_str::<Vec<serde_json::Value>>(content) else {
            return None;
        };

        let mut changed = false;
        for part in &mut parts {
            let obj = match part.as_object_mut() {
                Some(o) => o,
                None => continue,
            };
            let part_type = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if part_type != "text" {
                continue;
            }
            let text = match obj.get("text").and_then(|t| t.as_str()) {
                Some(t) => t.to_string(),
                None => continue,
            };
            let result = self.redact(&text, project_id);
            if !result.hits.is_empty() {
                changed = true;
                report.messages_touched += 1;
                for h in &result.hits {
                    report.merge_hit(&h.name, h.count);
                }
                obj.insert("text".into(), serde_json::Value::String(result.text.into_owned()));
            }
        }

        if changed {
            serde_json::to_string(&parts).ok()
        } else {
            None
        }
    }

    pub fn active_rules(&self, project_id: Option<&str>) -> Vec<RuleSummary> {
        let set = self.compiled.read().unwrap().clone();
        let mut out = Vec::new();

        for rule in &set.builtin {
            out.push(RuleSummary {
                name: rule.name.clone(),
                kind: "builtin".into(),
                category: rule.category.as_str().into(),
                description: BUILTIN_RULES
                    .iter()
                    .find(|b| b.name == rule.name)
                    .map(|b| b.description.to_string())
                    .unwrap_or_default(),
                active: set.mode.includes(rule.category),
            });
        }

        for rule in &set.global {
            out.push(RuleSummary {
                name: rule.name.clone(),
                kind: "global".into(),
                category: rule.category.as_str().into(),
                description: String::new(),
                active: true,
            });
        }

        if let Some(pid) = project_id
            && let Some(project_rules) = set.by_project.get(pid)
        {
            for rule in project_rules {
                out.push(RuleSummary {
                    name: rule.name.clone(),
                    kind: "project".into(),
                    category: rule.category.as_str().into(),
                    description: String::new(),
                    active: true,
                });
            }
        }

        out
    }
}

// ── Per-conversation reverse mapping ────────────────────────────────────────

/// Per-conversation mapping between original values and numbered placeholders.
/// Lives in memory only — never persisted, lost on restart.
#[derive(Debug, Default)]
pub struct ConversationMappings {
    forward: HashMap<String, String>,
    reverse: HashMap<String, String>,
    counters: HashMap<String, usize>,
}

impl ConversationMappings {
    fn get_or_assign(&mut self, rule_name: &str, original: &str) -> String {
        if let Some(existing) = self.forward.get(original) {
            return existing.clone();
        }
        let counter = self.counters.entry(rule_name.to_string()).or_insert(0);
        *counter += 1;
        let placeholder = format!("[REDACTED:{}:{}]", rule_name, *counter);
        self.forward.insert(original.to_string(), placeholder.clone());
        self.reverse.insert(placeholder.clone(), original.to_string());
        placeholder
    }

    pub fn restore<'a>(&self, text: &'a str) -> Cow<'a, str> {
        if self.reverse.is_empty() || !text.contains("[REDACTED:") {
            return Cow::Borrowed(text);
        }
        let mut result = text.to_string();
        for (placeholder, original) in &self.reverse {
            result = result.replace(placeholder, original);
        }
        if result == text {
            Cow::Borrowed(text)
        } else {
            Cow::Owned(result)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }

    pub fn count(&self) -> usize {
        self.forward.len()
    }
}

/// Process-level store keyed by conversation id.
pub struct RedactionMappings {
    inner: std::sync::Mutex<HashMap<String, ConversationMappings>>,
}

impl RedactionMappings {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn restore_in_text<'a>(&self, conversation_id: &str, text: &'a str) -> Cow<'a, str> {
        let guard = self.inner.lock().unwrap();
        match guard.get(conversation_id) {
            Some(m) => {
                let restored = m.restore(text);
                match restored {
                    Cow::Borrowed(_) => Cow::Borrowed(text),
                    Cow::Owned(s) => Cow::Owned(s),
                }
            }
            None => Cow::Borrowed(text),
        }
    }

    pub fn restore_in_json(&self, conversation_id: &str, value: &mut serde_json::Value) -> bool {
        let guard = self.inner.lock().unwrap();
        let Some(mappings) = guard.get(conversation_id) else {
            return false;
        };
        if mappings.is_empty() {
            return false;
        }
        restore_json_value(mappings, value)
    }

    pub fn clear_conversation(&self, conversation_id: &str) {
        self.inner.lock().unwrap().remove(conversation_id);
    }

    pub fn conversation_count(&self, conversation_id: &str) -> usize {
        self.inner.lock().unwrap().get(conversation_id).map_or(0, |m| m.count())
    }
}

fn restore_json_value(mappings: &ConversationMappings, value: &mut serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(s) => {
            let restored = mappings.restore(s);
            if let Cow::Owned(new) = restored {
                *s = new;
                return true;
            }
            false
        }
        serde_json::Value::Array(arr) => {
            let mut changed = false;
            for v in arr {
                changed |= restore_json_value(mappings, v);
            }
            changed
        }
        serde_json::Value::Object(obj) => {
            let mut changed = false;
            for v in obj.values_mut() {
                changed |= restore_json_value(mappings, v);
            }
            changed
        }
        _ => false,
    }
}

impl RedactionEngine {
    /// Redact messages using numbered placeholders with per-conversation mapping.
    /// The mapping accumulates across turns: same value always gets the same placeholder.
    pub fn scrub_for_conversation(
        &self,
        messages: &mut [ChatMessage],
        project_id: Option<&str>,
        conversation_id: &str,
        mappings: &RedactionMappings,
    ) -> RedactionReport {
        let mut report = RedactionReport::default();
        if !self.is_enabled() {
            return report;
        }

        let set = self.compiled.read().unwrap().clone();
        let mut conv_map = {
            let mut guard = mappings.inner.lock().unwrap();
            guard.remove(conversation_id).unwrap_or_default()
        };

        for msg in messages.iter_mut() {
            if !should_scan_role(&msg.role) {
                continue;
            }
            if msg.content.is_empty() {
                continue;
            }

            if msg.content.starts_with("[{") {
                if let Some(redacted) =
                    self.redact_multimodal_mapped(&msg.content, project_id, &set, &mut conv_map, &mut report)
                {
                    msg.content = redacted;
                }
            } else {
                let (redacted, hits) = self.redact_text_mapped(&msg.content, project_id, &set, &mut conv_map);
                if !hits.is_empty() {
                    report.messages_touched += 1;
                    for h in &hits {
                        report.merge_hit(&h.name, h.count);
                    }
                    msg.content = redacted;
                }
            }
        }

        {
            let mut guard = mappings.inner.lock().unwrap();
            guard.insert(conversation_id.to_string(), conv_map);
        }

        report
    }

    fn redact_text_mapped(
        &self,
        text: &str,
        project_id: Option<&str>,
        set: &RuleSet,
        conv_map: &mut ConversationMappings,
    ) -> (String, Vec<RuleHit>) {
        let mut current = text.to_string();
        let mut hits = Vec::new();

        let rules = self.collect_active_rules(set, project_id);
        for rule in &rules {
            let (result, count) = apply_with_mappings(rule, &current, conv_map);
            if count > 0 {
                hits.push(RuleHit {
                    name: rule.name.clone(),
                    count,
                });
                current = result;
            }
        }

        (current, hits)
    }

    fn redact_multimodal_mapped(
        &self,
        content: &str,
        project_id: Option<&str>,
        set: &RuleSet,
        conv_map: &mut ConversationMappings,
        report: &mut RedactionReport,
    ) -> Option<String> {
        let Ok(mut parts) = serde_json::from_str::<Vec<serde_json::Value>>(content) else {
            return None;
        };

        let mut changed = false;
        for part in &mut parts {
            let obj = match part.as_object_mut() {
                Some(o) => o,
                None => continue,
            };
            let part_type = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if part_type != "text" {
                continue;
            }
            let text = match obj.get("text").and_then(|t| t.as_str()) {
                Some(t) => t.to_string(),
                None => continue,
            };
            let (redacted, hits) = self.redact_text_mapped(&text, project_id, set, conv_map);
            if !hits.is_empty() {
                changed = true;
                report.messages_touched += 1;
                for h in &hits {
                    report.merge_hit(&h.name, h.count);
                }
                obj.insert("text".into(), serde_json::Value::String(redacted));
            }
        }

        if changed {
            serde_json::to_string(&parts).ok()
        } else {
            None
        }
    }

    fn collect_active_rules<'a>(&self, set: &'a RuleSet, project_id: Option<&str>) -> Vec<&'a CompiledRule> {
        let mut rules: Vec<&CompiledRule> = Vec::new();
        for r in &set.builtin {
            if set.mode.includes(r.category) {
                rules.push(r);
            }
        }
        for r in &set.global {
            rules.push(r);
        }
        if let Some(pid) = project_id
            && let Some(project_rules) = set.by_project.get(pid)
        {
            for r in project_rules {
                rules.push(r);
            }
        }
        rules
    }
}

/// Apply a rule with mapping-aware replacement: each unique matched value gets
/// a numbered placeholder like `[REDACTED:rule_name:N]`.
fn apply_with_mappings(rule: &CompiledRule, text: &str, conv_map: &mut ConversationMappings) -> (String, usize) {
    let mut result = String::new();
    let mut last_end = 0;
    let mut count = 0usize;

    if rule.secret_group {
        for caps in rule.regex.captures_iter(text) {
            if let Some(m) = caps.name("secret") {
                let matched = m.as_str();
                if matched.is_empty() || !rule.verify.is_none_or(|v| v(matched)) {
                    continue;
                }
                let placeholder = conv_map.get_or_assign(&rule.name, matched);
                result.push_str(&text[last_end..m.start()]);
                result.push_str(&placeholder);
                last_end = m.end();
                count += 1;
            }
        }
    } else {
        for m in rule.regex.find_iter(text) {
            let matched = m.as_str();
            if matched.is_empty() || !rule.verify.is_none_or(|v| v(matched)) {
                continue;
            }
            let placeholder = conv_map.get_or_assign(&rule.name, matched);
            result.push_str(&text[last_end..m.start()]);
            result.push_str(&placeholder);
            last_end = m.end();
            count += 1;
        }
    }

    if count == 0 {
        return (text.to_string(), 0);
    }
    result.push_str(&text[last_end..]);
    (result, count)
}

fn should_scan_role(role: &str) -> bool {
    matches!(role, "user" | "tool" | "system" | "context")
}

fn compile_builtins() -> Vec<CompiledRule> {
    BUILTIN_RULES
        .iter()
        .map(|def| {
            let regex = compile(def.pattern).unwrap_or_else(|e| {
                panic!("builtin rule `{}` failed to compile: {e}", def.name);
            });
            CompiledRule::new(def.name.into(), RuleKind::Builtin, def.category, regex, def.verify)
        })
        .collect()
}

impl Clone for RuleSet {
    fn clone(&self) -> Self {
        Self {
            mode: self.mode,
            builtin: self.builtin.clone(),
            global: self.global.clone(),
            by_project: self.by_project.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_engine_does_nothing() {
        let engine = RedactionEngine::disabled();
        let result = engine.redact("sk-1234567890abcdefghijklmno", None);
        assert!(result.hits.is_empty());
        assert_eq!(result.text, "sk-1234567890abcdefghijklmno");
    }

    #[test]
    fn standard_mode_catches_secrets_and_pii() {
        let engine = RedactionEngine::new();
        let result = engine.redact(
            "my key is sk-1234567890abcdefghijklmno and email is user@example.com",
            None,
        );
        assert!(!result.hits.is_empty());
        assert!(result.text.contains("[REDACTED:openai_key]"));
        assert!(result.text.contains("[REDACTED:email]"));
    }

    #[test]
    fn secrets_mode_skips_pii() {
        let engine = RedactionEngine::new();
        {
            let mut set = engine.compiled.write().unwrap();
            let mut new_set = (**set).clone();
            new_set.mode = RedactionMode::Secrets;
            *set = Arc::new(new_set);
        }
        let result = engine.redact("email is user@example.com", None);
        assert!(result.hits.is_empty());
    }

    #[test]
    fn no_allocation_on_clean_text() {
        let engine = RedactionEngine::new();
        let result = engine.redact("just a normal sentence", None);
        assert!(result.hits.is_empty());
        assert!(matches!(result.text, Cow::Borrowed(_)));
    }

    #[test]
    fn numbered_placeholders_distinguish_values() {
        let mut mappings = ConversationMappings::default();
        let engine = RedactionEngine::new();
        let set = engine.compiled.read().unwrap().clone();
        let rules = engine.collect_active_rules(&set, None);

        let text = "key1: sk-aaaabbbbccccddddeeeefffff key2: sk-11112222333344445555gggg";
        let mut current = text.to_string();
        for rule in &rules {
            let (result, _count) = apply_with_mappings(rule, &current, &mut mappings);
            current = result;
        }
        assert!(current.contains("[REDACTED:openai_key:1]"), "got: {current}");
        assert!(current.contains("[REDACTED:openai_key:2]"), "got: {current}");
        assert!(!current.contains("sk-"), "secret leaked: {current}");
    }

    #[test]
    fn same_value_gets_same_placeholder() {
        let mut mappings = ConversationMappings::default();
        let engine = RedactionEngine::new();
        let set = engine.compiled.read().unwrap().clone();
        let rules = engine.collect_active_rules(&set, None);

        let text = "first: sk-aaaabbbbccccddddeeeefffff second: sk-aaaabbbbccccddddeeeefffff";
        let mut current = text.to_string();
        for rule in &rules {
            let (result, _) = apply_with_mappings(rule, &current, &mut mappings);
            current = result;
        }
        assert_eq!(
            current.matches("[REDACTED:openai_key:1]").count(),
            2,
            "same value should get same placeholder: {current}"
        );
    }

    #[test]
    fn reverse_mapping_restores_values() {
        let mut mappings = ConversationMappings::default();
        mappings.get_or_assign("openai_key", "sk-real-secret-key-12345678");

        let text = "curl -H 'Authorization: Bearer [REDACTED:openai_key:1]'";
        let restored = mappings.restore(text);
        assert_eq!(restored, "curl -H 'Authorization: Bearer sk-real-secret-key-12345678'");
    }

    #[test]
    fn restore_in_json_walks_nested_values() {
        let mappings = RedactionMappings::new();
        {
            let mut guard = mappings.inner.lock().unwrap();
            let mut conv = ConversationMappings::default();
            conv.get_or_assign("openai_key", "sk-the-real-key-1234567890");
            guard.insert("conv1".to_string(), conv);
        }

        let mut value = serde_json::json!({
            "command": "curl -H 'Bearer [REDACTED:openai_key:1]' https://api.example.com",
            "nested": { "key": "[REDACTED:openai_key:1]" }
        });

        let changed = mappings.restore_in_json("conv1", &mut value);
        assert!(changed);
        assert_eq!(value["nested"]["key"].as_str().unwrap(), "sk-the-real-key-1234567890");
    }

    #[test]
    fn mappings_persist_across_calls() {
        let mappings = RedactionMappings::new();
        let engine = RedactionEngine::new();

        let mut msgs1 = vec![ChatMessage {
            role: "user".into(),
            content: "my key is sk-aaaabbbbccccddddeeeefffff".into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            tool_error: false,
            provider_state: None,
            origin: crate::provider::MessageOrigin::LegacyUser,
        }];
        engine.scrub_for_conversation(&mut msgs1, None, "conv1", &mappings);

        let mut msgs2 = vec![ChatMessage {
            role: "user".into(),
            content: "reminder: sk-aaaabbbbccccddddeeeefffff is the key".into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            tool_error: false,
            provider_state: None,
            origin: crate::provider::MessageOrigin::LegacyUser,
        }];
        engine.scrub_for_conversation(&mut msgs2, None, "conv1", &mappings);

        assert!(msgs2[0].content.contains("[REDACTED:openai_key:1]"));
        assert!(!msgs2[0].content.contains("[REDACTED:openai_key:2]"));
    }

    #[test]
    fn assistant_role_not_scanned() {
        assert!(!should_scan_role("assistant"));
    }

    #[test]
    fn user_role_scanned() {
        assert!(should_scan_role("user"));
        assert!(should_scan_role("tool"));
        assert!(should_scan_role("system"));
        assert!(should_scan_role("context"));
    }
}

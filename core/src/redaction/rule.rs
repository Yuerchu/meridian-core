use std::borrow::Cow;

use regex::Regex;

use crate::db::models::redaction_rule::{
    MAX_EXAMPLE_LEN, MAX_EXAMPLES, MAX_PATTERN_LEN, RedactionExample, RuleCategory,
};

#[derive(Debug, Clone)]
pub struct RuleSpec {
    pub name: String,
    pub description: String,
    pub pattern: String,
    pub category: RuleCategory,
    pub examples: Vec<RedactionExample>,
}

#[derive(Debug, Clone)]
pub enum RuleKind {
    Builtin,
    Global,
    Project(String),
}

#[derive(Debug, Clone)]
pub struct CompiledRule {
    pub name: String,
    pub kind: RuleKind,
    pub category: RuleCategory,
    pub(crate) regex: Regex,
    pub(crate) secret_group: bool,
    pub(crate) verify: Option<fn(&str) -> bool>,
}

impl CompiledRule {
    pub fn new(
        name: String,
        kind: RuleKind,
        category: RuleCategory,
        regex: Regex,
        verify: Option<fn(&str) -> bool>,
    ) -> Self {
        let secret_group = regex.capture_names().any(|n| n == Some("secret"));
        Self {
            name,
            kind,
            category,
            regex,
            secret_group,
            verify,
        }
    }

    pub fn placeholder(&self) -> String {
        format!("[REDACTED:{}]", self.name)
    }

    pub fn finds(&self, text: &str) -> bool {
        if self.secret_group {
            for caps in self.regex.captures_iter(text) {
                if let Some(m) = caps.name("secret") {
                    let matched = m.as_str();
                    if !matched.is_empty() && self.verify.is_none_or(|v| v(matched)) {
                        return true;
                    }
                }
            }
            false
        } else {
            for m in self.regex.find_iter(text) {
                if !m.as_str().is_empty() && self.verify.is_none_or(|v| v(m.as_str())) {
                    return true;
                }
            }
            false
        }
    }

    pub fn apply<'a>(&self, text: &'a str) -> (Cow<'a, str>, usize) {
        let placeholder = self.placeholder();
        let mut count = 0usize;

        if self.secret_group {
            let mut result = String::new();
            let mut last_end = 0;
            for caps in self.regex.captures_iter(text) {
                if let Some(m) = caps.name("secret") {
                    let matched = m.as_str();
                    if matched.is_empty() || !self.verify.is_none_or(|v| v(matched)) {
                        continue;
                    }
                    result.push_str(&text[last_end..m.start()]);
                    result.push_str(&placeholder);
                    last_end = m.end();
                    count += 1;
                }
            }
            if count == 0 {
                return (Cow::Borrowed(text), 0);
            }
            result.push_str(&text[last_end..]);
            (Cow::Owned(result), count)
        } else if self.verify.is_some() {
            let verify = self.verify.unwrap();
            let mut result = String::new();
            let mut last_end = 0;
            for m in self.regex.find_iter(text) {
                if !verify(m.as_str()) {
                    continue;
                }
                result.push_str(&text[last_end..m.start()]);
                result.push_str(&placeholder);
                last_end = m.end();
                count += 1;
            }
            if count == 0 {
                return (Cow::Borrowed(text), 0);
            }
            result.push_str(&text[last_end..]);
            (Cow::Owned(result), count)
        } else {
            let replaced = self.regex.replace_all(text, placeholder.as_str());
            if let Cow::Owned(ref s) = replaced {
                count = self.regex.find_iter(text).count();
                if count == 0 {
                    return (Cow::Borrowed(text), 0);
                }
                (Cow::Owned(s.clone()), count)
            } else {
                (Cow::Borrowed(text), 0)
            }
        }
    }
}

static NAME_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9_]{2,39}$").unwrap());

pub fn compile(pattern: &str) -> Result<Regex, String> {
    regex::RegexBuilder::new(pattern)
        .size_limit(2 << 20)
        .dfa_size_limit(1 << 20)
        .build()
        .map_err(|e| format!("invalid regex: {e}"))
}

pub const BENIGN_CORPUS: &[&str] = &[
    "Hello, this is a normal English sentence.",
    "你好，这是一段普通的中文文本。",
    "The quick brown fox jumps over the lazy dog.",
    "fn main() { println!(\"Hello, world!\"); }",
    "SELECT * FROM users WHERE id = 42;",
    "https://example.com/path?query=value&other=123",
    "git commit -m \"fix: resolve parsing issue\"",
    "2026-09-11T14:30:00Z",
    "v1.2.3-beta.4",
    "550e8400-e29b-41d4-a716-446655440000",
    "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2",
    "/usr/local/bin/python3",
    "C:\\Users\\Administrator\\Documents\\Code\\",
    "{\"key\": \"value\", \"count\": 42}",
    "[\"apple\", \"banana\", \"cherry\"]",
    "The file size is 1234567890 bytes.",
    "Error code: 0x1A2B3C4D",
    "Build #20260911.1 completed successfully.",
    "Temperature: 36.5°C, Humidity: 65%",
    "Phone model: iPhone 15 Pro Max 256GB",
    "Version 12.0.1 (build 24A348)",
    "npm install @heroui/react@3.0.0",
    "cargo test --workspace --target-dir ../target",
    "docker run -it --rm ubuntu:24.04 bash",
    "export PATH=$HOME/.local/bin:$PATH",
    "pip install requests==2.31.0",
    "2024年12月25日 星期三",
    "订单号: ORD-2026-0911-001234",
    "The result is 3.14159265358979",
    "Total: ¥1,234.56 (tax included)",
    "Meeting at 10:30 AM in Room 42B",
    "README.md LICENSE.txt CHANGELOG.md",
    "192.168.1.1 is a private network address",
    "The SHA-256 hash is e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    "[REDACTED:example]",
    "user@host:~$ ls -la",
    "Content-Type: application/json; charset=utf-8",
    "rgba(255, 128, 0, 0.5)",
    "font-family: 'PingFang SC', sans-serif;",
    "The population of Tokyo is approximately 14 million.",
];

pub fn validate_spec(spec: &RuleSpec) -> Result<CompiledRule, String> {
    if !NAME_RE.is_match(&spec.name) {
        return Err(format!(
            "name `{}` must be 3-40 chars, lowercase letters/digits/underscores, starting with a letter",
            spec.name
        ));
    }

    use crate::redaction::builtin::BUILTIN_RULES;
    if BUILTIN_RULES.iter().any(|b| b.name == spec.name) {
        return Err(format!("`{}` is a built-in rule name and cannot be used", spec.name));
    }

    if spec.description.is_empty() || spec.description.len() > 200 {
        return Err("description must be 1-200 characters".into());
    }

    if spec.pattern.len() > MAX_PATTERN_LEN {
        return Err(format!("pattern exceeds {MAX_PATTERN_LEN} characters"));
    }

    let regex = compile(&spec.pattern)?;

    let capture_names: Vec<_> = regex.capture_names().flatten().collect();
    if capture_names.iter().any(|n| *n != "secret") {
        return Err("only the named group `secret` is allowed in the pattern".into());
    }

    if regex.is_match("") {
        return Err("pattern must not match the empty string".into());
    }

    if spec.examples.is_empty() || spec.examples.len() > MAX_EXAMPLES {
        return Err(format!("provide 1-{MAX_EXAMPLES} examples"));
    }

    for ex in &spec.examples {
        if ex.text.len() > MAX_EXAMPLE_LEN {
            return Err(format!("example text exceeds {MAX_EXAMPLE_LEN} characters"));
        }
    }

    let has_positive = spec.examples.iter().any(|e| e.should_match);
    let has_negative = spec.examples.iter().any(|e| !e.should_match);
    if !has_positive {
        return Err("at least one example must have should_match: true".into());
    }
    if !has_negative {
        return Err("at least one example must have should_match: false".into());
    }

    let rule = CompiledRule::new(spec.name.clone(), RuleKind::Global, spec.category, regex, None);

    for ex in &spec.examples {
        let found = rule.finds(&ex.text);
        if ex.should_match && !found {
            return Err(format!("positive example not matched: {:?}", ex.text));
        }
        if !ex.should_match && found {
            return Err(format!("negative example unexpectedly matched: {:?}", ex.text));
        }
    }

    for (i, line) in BENIGN_CORPUS.iter().enumerate() {
        if rule.finds(line) {
            return Err(format!(
                "pattern is too broad: it matches benign corpus line {}: {:?}",
                i + 1,
                line
            ));
        }
    }

    let placeholder = rule.placeholder();
    if rule.finds(&placeholder) {
        return Err("pattern matches its own placeholder — redaction would not be idempotent".into());
    }

    for ex in spec.examples.iter().filter(|e| e.should_match) {
        let (once, _) = rule.apply(&ex.text);
        let (twice, _) = rule.apply(&once);
        if once != twice {
            return Err(format!(
                "redaction is not idempotent on example {:?}: first pass → {:?}, second pass → {:?}",
                ex.text, once, twice
            ));
        }
    }

    Ok(rule)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_name_accepted() {
        assert!(NAME_RE.is_match("openai_key"));
        assert!(NAME_RE.is_match("abc"));
    }

    #[test]
    fn invalid_name_rejected() {
        assert!(!NAME_RE.is_match("AB"));
        assert!(!NAME_RE.is_match("1abc"));
        assert!(!NAME_RE.is_match("a-b"));
        assert!(!NAME_RE.is_match(""));
    }

    #[test]
    fn empty_match_rejected() {
        let spec = RuleSpec {
            name: "bad_empty".into(),
            description: "matches empty".into(),
            pattern: ".*".into(),
            category: RuleCategory::Secret,
            examples: vec![
                RedactionExample {
                    text: "anything".into(),
                    should_match: true,
                },
                RedactionExample {
                    text: "".into(),
                    should_match: false,
                },
            ],
        };
        assert!(validate_spec(&spec).is_err());
    }

    #[test]
    fn benign_corpus_rejects_broad_pattern() {
        let spec = RuleSpec {
            name: "too_broad".into(),
            description: "catches everything".into(),
            pattern: r"[A-Za-z0-9]{8,}".into(),
            category: RuleCategory::Secret,
            examples: vec![
                RedactionExample {
                    text: "ABCDEFGH12345678".into(),
                    should_match: true,
                },
                RedactionExample {
                    text: "a".into(),
                    should_match: false,
                },
            ],
        };
        assert!(validate_spec(&spec).is_err());
    }

    #[test]
    fn named_group_replaces_only_value() {
        let regex = compile(r"(?i)api_key\s*=\s*(?P<secret>[A-Za-z0-9]+)").unwrap();
        let rule = CompiledRule::new("test_key".into(), RuleKind::Global, RuleCategory::Secret, regex, None);
        let (result, count) = rule.apply("api_key = MySecret123");
        assert_eq!(count, 1);
        assert_eq!(result, "api_key = [REDACTED:test_key]");
    }

    #[test]
    fn idempotent() {
        let regex = compile(r"(?-u:\b)sk-[A-Za-z0-9_-]{20,}").unwrap();
        let rule = CompiledRule::new(
            "openai_key".into(),
            RuleKind::Builtin,
            RuleCategory::Secret,
            regex,
            None,
        );
        let text = "my key is sk-1234567890abcdefghijklmno";
        let (once, _) = rule.apply(text);
        let (twice, _) = rule.apply(&once);
        assert_eq!(once, twice);
    }
}

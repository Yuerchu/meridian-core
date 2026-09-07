// Ported from codex-rs/secrets/sanitizer.rs (Apache-2.0, OpenAI)

use regex::Regex;
use std::sync::LazyLock;

static OPENAI_KEY_REGEX: LazyLock<Regex> = LazyLock::new(|| compile_regex(r"sk-[A-Za-z0-9]{20,}"));
static AWS_ACCESS_KEY_ID_REGEX: LazyLock<Regex> = LazyLock::new(|| compile_regex(r"\bAKIA[0-9A-Z]{16}\b"));
static BEARER_TOKEN_REGEX: LazyLock<Regex> = LazyLock::new(|| compile_regex(r"(?i)\bBearer\s+[A-Za-z0-9._\-]{16,}\b"));
static SECRET_ASSIGNMENT_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r#"(?i)\b(api[_-]?key|token|secret|password)\b(\s*[:=]\s*)(["']?)[^\s"']{8,}"#));

pub fn redact_secrets(input: String) -> String {
    let redacted = OPENAI_KEY_REGEX.replace_all(&input, "[REDACTED_SECRET]");
    let redacted = AWS_ACCESS_KEY_ID_REGEX.replace_all(&redacted, "[REDACTED_SECRET]");
    let redacted = BEARER_TOKEN_REGEX.replace_all(&redacted, "Bearer [REDACTED_SECRET]");
    let redacted = SECRET_ASSIGNMENT_REGEX.replace_all(&redacted, "$1$2$3[REDACTED_SECRET]");
    redacted.to_string()
}

fn compile_regex(pattern: &str) -> Regex {
    match Regex::new(pattern) {
        Ok(regex) => regex,
        Err(err) => panic!("invalid regex pattern `{pattern}`: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_regex_compiles() {
        redact_secrets("harmless text".into());
    }

    #[test]
    fn test_openai_key_redacted() {
        let input = "my key is sk-ABCDEFGHIJKLMNOPQRSTUVWXYZ".into();
        let output = redact_secrets(input);
        assert!(!output.contains("sk-"));
        assert!(output.contains("[REDACTED_SECRET]"));
    }

    #[test]
    fn test_aws_key_redacted() {
        let input = "key is AKIAIOSFODNN7EXAMPLE".into();
        let output = redact_secrets(input);
        assert!(!output.contains("AKIA"));
        assert!(output.contains("[REDACTED_SECRET]"));
    }

    #[test]
    fn test_bearer_token_redacted() {
        let input = "Authorization: Bearer abc123def456ghi789.jkl012".into();
        let output = redact_secrets(input);
        assert!(!output.contains("abc123"));
        assert!(output.contains("Bearer [REDACTED_SECRET]"));
    }

    #[test]
    fn test_secret_assignment_redacted() {
        let input = r#"api_key = "sk_live_testing12345678""#.into();
        let output = redact_secrets(input);
        assert!(!output.contains("sk_live"));
        assert!(output.contains("api_key"));
        assert!(output.contains("[REDACTED_SECRET]"));
    }

    #[test]
    fn test_no_false_positive() {
        let input = "This is a normal sentence with no secrets.".to_string();
        let output = redact_secrets(input.clone());
        assert_eq!(output, input);
    }
}

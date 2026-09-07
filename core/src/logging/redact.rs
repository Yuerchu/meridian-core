//! Keeping credentials out of the log file.
//!
//! Two layers, because they fail differently. The field-name blacklist catches a
//! secret whatever shape its value has, but only if whoever wrote the call site
//! named the field recognisably. The value-level regexes in
//! `secrets::sanitizer` catch a key pasted into the middle of a sentence, but
//! only if it matches a known vendor format. Neither alone is enough.

use serde_json::Value;

/// Matched as a lowercased substring of the field name, so `provider_api_key`
/// and `X-Api-Key` are both covered.
///
/// `sign` also matches `assignment`, and `secret` matches `secretary`. Redacting
/// something harmless costs a field in a log nobody was reading; missing a real
/// credential costs the credential. The false positives stay.
const SENSITIVE_FIELDS: &[&str] = &[
    "api_key",
    "apikey",
    "api-key",
    "authorization",
    "auth_header",
    "bearer",
    "token",
    "credential",
    "signature",
    "provider_state",
    "encrypted_content",
    "sign",
    "secret",
    "password",
    "passwd",
    "passphrase",
    "cookie",
    "session_id",
    "private_key",
];

fn is_sensitive(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SENSITIVE_FIELDS.iter().any(|k| lower.contains(k))
}

/// Redact one structured field, keyed on its name.
///
/// The replacement keeps the length. "Is the key empty, or truncated, or does it
/// have a stray newline" is a question the log has to be able to answer — that
/// is exactly the class of bug someone reads the log for.
pub(crate) fn scrub_field(name: &str, value: Value) -> Value {
    if is_sensitive(name) {
        let len = match &value {
            Value::String(s) => s.chars().count(),
            Value::Null => 0,
            other => other.to_string().chars().count(),
        };
        return Value::String(format!("[REDACTED len={len}]"));
    }
    match value {
        // The name looked innocent, so fall back to matching the value itself.
        Value::String(s) => Value::String(scrub_text(&s)),
        Value::Array(items) => Value::Array(items.into_iter().map(|v| scrub_field(name, v)).collect()),
        other => other,
    }
}

/// Redact free text — a log message, or a value under an innocent-looking name.
pub(crate) fn scrub_text(text: &str) -> String {
    crate::secrets::sanitizer::redact_secrets(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scrub(name: &str, value: Value) -> String {
        scrub_field(name, value).to_string()
    }

    #[test]
    fn sensitive_names_are_replaced_whatever_the_case_or_prefix() {
        for name in [
            "api_key",
            "API_KEY",
            "provider_api_key",
            "Authorization",
            "x-api-key",
            "db_password",
            "refresh_token",
            "session_id",
            "provider_state",
            "encrypted_content",
        ] {
            let out = scrub(name, json!("hunter2hunter2hunter2"));
            assert!(out.contains("REDACTED"), "{name} was not redacted: {out}");
            assert!(!out.contains("hunter2"), "{name} leaked its value: {out}");
        }
    }

    #[test]
    fn redaction_keeps_the_length_so_an_empty_key_is_still_diagnosable() {
        assert_eq!(scrub_field("api_key", json!("")), json!("[REDACTED len=0]"));
        assert_eq!(scrub_field("api_key", json!("abcde")), json!("[REDACTED len=5]"));
    }

    #[test]
    fn ordinary_fields_pass_through_untouched() {
        assert_eq!(scrub_field("conversation_id", json!("c-123")), json!("c-123"));
        assert_eq!(scrub_field("status", json!(401)), json!(401));
        assert_eq!(scrub_field("enabled", json!(true)), json!(true));
        assert_eq!(scrub_field("count", json!(3)), json!(3));
    }

    #[test]
    fn an_innocent_name_still_gets_its_value_scanned() {
        // The name says nothing, but the value is unmistakably a key.
        let out = scrub(
            "detail",
            json!("request failed with sk-abcdefghijklmnopqrstuvwxyz012345"),
        );
        assert!(!out.contains("sk-abcdefghijklmnop"), "{out}");
        assert!(out.contains("REDACTED"), "{out}");
    }

    #[test]
    fn error_chains_are_scanned_element_by_element() {
        let chain = json!(["outer failed", "Bearer abcdefghijklmnopqrstuvwxyz"]);
        let out = scrub_field("error", chain);
        assert!(!out.to_string().contains("abcdefghijklmnopqrstuvwxyz"), "{out}");
    }

    #[test]
    fn messages_are_scanned_as_free_text() {
        let out = scrub_text("using key sk-abcdefghijklmnopqrstuvwxyz012345 for the call");
        assert!(!out.contains("sk-abcdefghijklmnop"), "{out}");
        assert!(out.starts_with("using key "), "{out}");
    }
}

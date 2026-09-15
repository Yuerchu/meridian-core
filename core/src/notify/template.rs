//! Shaping an alert into somebody else's schema.
//!
//! A company's own alert pipe has a contract its operations team wrote down,
//! and no amount of formats shipped here will match it. So the body is a JSON
//! document the operator supplies, with placeholders where the alert's values
//! go.
//!
//! **Substitution is structural, not textual.** The template is parsed as JSON
//! first and placeholders are replaced at the *value* level, so a value
//! containing a quote, a brace or a newline cannot change the document's shape.
//! Rendering by string interpolation — the obvious implementation — would make
//! every provider name and every upstream error message an injection into the
//! receiver's parser.
//!
//! Two forms follow from that:
//!
//! - A string that is **exactly** one placeholder takes that value's own type.
//!   `"{{balance.total}}"` becomes the decimal string `"3.2"`, and
//!   `"{{balance.total:number}}"` becomes the JSON number `3.2`.
//! - A string that merely **contains** placeholders is interpolated, and the
//!   result is always a string. `"balance is {{balance.total}}"` is text.
//!
//! Anything that is not a string — a number, a bool, a nested object — is a
//! constant and passes through untouched.

use serde_json::{Map, Value};

use super::alert::{Alert, AlertDetail};

/// Every placeholder this app will substitute.
///
/// A closed list, checked when the template is stored rather than when an alert
/// fires: a typo that is only caught at delivery time is caught by nobody,
/// because the delivery it breaks is the one nobody is watching for.
///
/// The names are grouped by the alert they describe. A template naming a
/// balance field renders `null` there for a usage alert, which is why an
/// endpoint usually subscribes to one family of events — but a `null` is a
/// document the receiver can still parse, and refusing to send would lose an
/// alert over a field the receiver may not even read.
pub const PLACEHOLDERS: &[&str] = &[
    // Every alert.
    "event",
    "alert_key",
    "title",
    "summary",
    "delivery_id",
    "raised_at_ms",
    "raised_at_iso",
    // Balance alerts. The account fields describe the one that triggered the
    // alert — see `triggering_account`.
    "balance.provider_id",
    "balance.provider_name",
    "balance.is_available",
    "balance.threshold",
    "balance.currency",
    "balance.total",
    "balance.granted",
    "balance.topped_up",
    "balance.accounts",
    // Usage alerts.
    "usage.window_hours",
    "usage.baseline_days",
    "usage.multiplier",
    "usage.window_cost",
    "usage.baseline_cost",
    "usage.baseline_windows",
    "usage.is_lower_bound",
    "usage.top_provider",
    "usage.top_conversation",
];

/// The `:number` suffix, and the only placeholders it may be applied to.
///
/// Money crosses every other boundary in this app as an exact decimal string,
/// and that rule is not negotiable internally. This is the same concession the
/// vendor formats already make: outbound, the receiver's schema wins. An
/// operator asking for a number is choosing it, and choosing the precision loss
/// their receiver's JSON parser may impose.
const NUMERIC_SUFFIX: &str = ":number";

fn is_numeric_capable(name: &str) -> bool {
    matches!(
        name,
        "raised_at_ms"
            | "balance.threshold"
            | "balance.total"
            | "balance.granted"
            | "balance.topped_up"
            | "usage.window_hours"
            | "usage.baseline_days"
            | "usage.multiplier"
            | "usage.window_cost"
            | "usage.baseline_cost"
            | "usage.baseline_windows"
    )
}

/// Check a template before it is stored.
///
/// Walks every string in the document and refuses an unknown placeholder, or a
/// `:number` on something that is not a number. Both are typos, and both would
/// otherwise surface as a malformed alert at the one moment nobody is reading
/// carefully.
pub fn validate(template: &Value) -> Result<(), String> {
    match template {
        Value::String(text) => {
            for (name, numeric) in placeholders_in(text) {
                if !PLACEHOLDERS.contains(&name.as_str()) {
                    return Err(format!(
                        "`{{{{{name}}}}}` is not a placeholder this app substitutes; \
                         see the list in the documentation"
                    ));
                }
                if numeric && !is_numeric_capable(&name) {
                    return Err(format!("`{name}` is not a number, so `:number` cannot apply to it"));
                }
            }
            Ok(())
        }
        Value::Array(items) => items.iter().try_for_each(validate),
        Value::Object(map) => map.values().try_for_each(validate),
        _ => Ok(()),
    }
}

/// Every `{{name}}` or `{{name:number}}` in a string, in order.
fn placeholders_in(text: &str) -> Vec<(String, bool)> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            break;
        };
        let raw = after[..end].trim();
        match raw.strip_suffix(NUMERIC_SUFFIX) {
            Some(name) => found.push((name.trim().to_string(), true)),
            None => found.push((raw.to_string(), false)),
        }
        rest = &after[end + 2..];
    }
    found
}

/// If the whole string is one placeholder, its name and whether `:number` was
/// asked for. This is what decides between a typed substitution and text.
fn sole_placeholder(text: &str) -> Option<(String, bool)> {
    let trimmed = text.trim();
    let inner = trimmed.strip_prefix("{{")?.strip_suffix("}}")?;
    if inner.contains("{{") || inner.contains("}}") {
        return None;
    }
    let inner = inner.trim();
    match inner.strip_suffix(NUMERIC_SUFFIX) {
        Some(name) => Some((name.trim().to_string(), true)),
        None => Some((inner.to_string(), false)),
    }
}

/// Fill a validated template in for one alert.
pub fn render(template: &Value, alert: &Alert, delivery_id: &str) -> Value {
    let values = Bindings::new(alert, delivery_id);
    fill(template, &values)
}

fn fill(node: &Value, values: &Bindings) -> Value {
    match node {
        Value::String(text) => {
            if let Some((name, numeric)) = sole_placeholder(text) {
                return values.typed(&name, numeric);
            }
            let mut out = String::with_capacity(text.len());
            let mut rest = text.as_str();
            while let Some(start) = rest.find("{{") {
                let after = &rest[start + 2..];
                let Some(end) = after.find("}}") else {
                    break;
                };
                out.push_str(&rest[..start]);
                let raw = after[..end].trim();
                let name = raw.strip_suffix(NUMERIC_SUFFIX).unwrap_or(raw).trim();
                out.push_str(&values.text(name));
                rest = &after[end + 2..];
            }
            out.push_str(rest);
            Value::String(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(|item| fill(item, values)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), fill(value, values)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// What a placeholder resolves to, for one alert.
struct Bindings(Map<String, Value>);

impl Bindings {
    fn new(alert: &Alert, delivery_id: &str) -> Self {
        let mut map = Map::new();
        map.insert("event".into(), Value::String(alert.event.as_str().into()));
        map.insert("alert_key".into(), Value::String(alert.alert_key.clone()));
        map.insert("title".into(), Value::String(alert.title.clone()));
        map.insert("summary".into(), Value::String(alert.summary.clone()));
        map.insert("delivery_id".into(), Value::String(delivery_id.into()));
        map.insert("raised_at_ms".into(), Value::from(alert.raised_at));
        map.insert("raised_at_iso".into(), Value::String(iso8601(alert.raised_at)));

        match &alert.detail {
            AlertDetail::Balance(balance) => {
                map.insert("balance.provider_id".into(), Value::String(balance.provider_id.clone()));
                map.insert(
                    "balance.provider_name".into(),
                    Value::String(balance.provider_name.clone()),
                );
                map.insert("balance.is_available".into(), Value::Bool(balance.is_available));
                map.insert("balance.threshold".into(), Value::String(balance.threshold.to_string()));
                map.insert(
                    "balance.accounts".into(),
                    serde_json::to_value(&balance.accounts).unwrap_or(Value::Null),
                );
                if let Some(account) = triggering_account(balance) {
                    map.insert("balance.currency".into(), Value::String(account.currency.clone()));
                    map.insert("balance.total".into(), Value::String(account.total_balance.to_string()));
                    if let Some(granted) = &account.granted_balance {
                        map.insert("balance.granted".into(), Value::String(granted.to_string()));
                    }
                    if let Some(topped_up) = &account.topped_up_balance {
                        map.insert("balance.topped_up".into(), Value::String(topped_up.to_string()));
                    }
                }
            }
            AlertDetail::Usage(usage) => {
                map.insert("usage.window_hours".into(), Value::from(usage.window_hours));
                map.insert("usage.baseline_days".into(), Value::from(usage.baseline_days));
                map.insert("usage.multiplier".into(), Value::String(usage.multiplier.to_string()));
                map.insert("usage.window_cost".into(), Value::String(usage.window_cost.to_string()));
                map.insert(
                    "usage.baseline_cost".into(),
                    Value::String(usage.baseline_cost.to_string()),
                );
                map.insert("usage.baseline_windows".into(), Value::from(usage.baseline_windows));
                map.insert("usage.is_lower_bound".into(), Value::Bool(usage.is_lower_bound));
                if let Some(slice) = usage.top_providers.first() {
                    map.insert("usage.top_provider".into(), Value::String(slice.key.clone()));
                }
                if let Some(slice) = usage.top_conversations.first() {
                    map.insert(
                        "usage.top_conversation".into(),
                        Value::String(slice.label.clone().unwrap_or_else(|| slice.key.clone())),
                    );
                }
            }
            AlertDetail::Test(_) => {}
        }
        Self(map)
    }

    /// The value for a sole placeholder, with its own type.
    ///
    /// A name that is valid but does not apply to this alert — a balance field
    /// on a usage alert — is `null` rather than an error. The alternative is
    /// losing an alert over a field the receiver may not read, and an alert not
    /// sent is the failure this whole subsystem exists to prevent.
    fn typed(&self, name: &str, numeric: bool) -> Value {
        let value = self.0.get(name).cloned().unwrap_or(Value::Null);
        if !numeric {
            return value;
        }
        match &value {
            // A decimal crosses as a string everywhere inside this app; here the
            // receiver's schema decides, and it asked for a number.
            Value::String(text) => serde_json::from_str::<serde_json::Number>(text)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            Value::Number(_) => value,
            _ => Value::Null,
        }
    }

    /// The value for a placeholder inside a larger string.
    ///
    /// Absent renders as empty rather than the four characters `null`, which in
    /// a sentence reads as a bug in the sender.
    fn text(&self, name: &str) -> String {
        match self.0.get(name) {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
        }
    }
}

/// The account the alert is actually about.
///
/// A balance alert can carry several currencies and the receiving schema
/// usually has room for one. The first under the floor is the one worth
/// naming; with none under it — which is what an upstream reporting the account
/// unusable looks like — the first is as good as any.
fn triggering_account(balance: &super::alert::BalanceAlert) -> Option<&crate::provider::balance::BalanceAccount> {
    balance
        .accounts
        .iter()
        .find(|account| account.total_balance < balance.threshold)
        .or_else(|| balance.accounts.first())
}

/// Milliseconds since the epoch, as RFC 3339 in UTC.
///
/// Public because `meridiand --status` prints timestamps too, and a daemon
/// reporting a time in a different shape from the one it sends is a small way
/// to make two logs impossible to line up.
pub fn format_timestamp(ms: i64) -> String {
    iso8601(ms)
}

fn iso8601(ms: i64) -> String {
    use chrono::TimeZone;
    match chrono::Utc.timestamp_millis_opt(ms).single() {
        Some(time) => time.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        // A timestamp this app produced is always representable; a stored one
        // from a corrupted row is not worth failing a delivery over.
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::notification::NotificationEventKind;
    use crate::notify::alert::{BalanceAlert, TestAlert, UsageAlert, UsageSlice};
    use crate::provider::balance::BalanceAccount;

    fn decimal(raw: &str) -> crate::decimal::Decimal {
        raw.parse().unwrap()
    }

    fn balance_alert() -> Alert {
        Alert {
            event: NotificationEventKind::BalanceLow,
            raised_at: 1_757_764_800_000,
            alert_key: "balance:ds".into(),
            title: "DeepSeek 余额偏低".into(),
            summary: "余额 3.2".into(),
            detail: AlertDetail::Balance(BalanceAlert {
                provider_id: "ds".into(),
                provider_name: "DeepSeek prod".into(),
                is_available: true,
                threshold: decimal("100"),
                accounts: vec![
                    BalanceAccount {
                        currency: "USD".into(),
                        total_balance: decimal("900"),
                        granted_balance: None,
                        topped_up_balance: None,
                    },
                    BalanceAccount {
                        currency: "CNY".into(),
                        total_balance: decimal("12.5"),
                        granted_balance: Some(decimal("0")),
                        topped_up_balance: Some(decimal("12.5")),
                    },
                ],
            }),
        }
    }

    fn render_str(template: &str, alert: &Alert) -> Value {
        let parsed: Value = serde_json::from_str(template).expect("a template");
        validate(&parsed).expect("a valid template");
        render(&parsed, alert, "delivery-1")
    }

    /// The worked example from a real deployment's contract.
    #[test]
    fn it_fills_a_receivers_own_schema() {
        let rendered = render_str(
            r#"{
                "title": "{{title}}",
                "message": "{{summary}}",
                "service": "billing",
                "environment": "prod",
                "requestId": "{{delivery_id}}",
                "timestamp": "{{raised_at_iso}}",
                "data": {
                    "account": "{{balance.provider_name}}",
                    "balance": "{{balance.total:number}}",
                    "threshold": "{{balance.threshold:number}}",
                    "currency": "{{balance.currency}}"
                }
            }"#,
            &balance_alert(),
        );

        assert_eq!(rendered["title"], "DeepSeek 余额偏低");
        assert_eq!(rendered["service"], "billing", "constants pass through");
        assert_eq!(rendered["requestId"], "delivery-1");
        assert_eq!(rendered["timestamp"], "2025-09-13T12:00:00.000Z");
        // A number, not a string: the receiver's schema wins outbound.
        assert_eq!(rendered["data"]["balance"], serde_json::json!(12.5));
        assert!(rendered["data"]["balance"].is_number());
        assert_eq!(rendered["data"]["threshold"], serde_json::json!(100));
        assert_eq!(rendered["data"]["currency"], "CNY");
    }

    /// The account named is the one under the floor, not the first one listed.
    /// Reported the other way round, an alert about an empty CNY balance would
    /// name a healthy USD one.
    #[test]
    fn the_account_named_is_the_one_that_triggered_the_alert() {
        let rendered = render_str(
            r#"{"c": "{{balance.currency}}", "t": "{{balance.total}}"}"#,
            &balance_alert(),
        );
        assert_eq!(rendered["c"], "CNY");
        assert_eq!(rendered["t"], "12.5");
    }

    /// What "structural" buys, stated precisely.
    ///
    /// It is **parse-then-substitute** that protects this, not the typed
    /// substitution above: even rendering every placeholder as text into a
    /// parsed tree is safe, because `serde_json` escapes a `Value::String` on
    /// the way out. Measured — replacing the typed path with plain
    /// interpolation turns four tests red and leaves this one green.
    ///
    /// The implementation this refuses is substituting into the template's
    /// *serialized text* and re-parsing, which is the shape a templating
    /// library would hand you. Mutated that way, this test fails: a provider
    /// name is user-supplied and an upstream error message is not ours at all,
    /// and either could close the object and append its own keys.
    #[test]
    fn a_value_cannot_break_out_of_the_document() {
        let mut alert = balance_alert();
        alert.title = r#"", "injected": true, "x": ""#.into();
        alert.summary = "line\nbreak \"quoted\" \\ backslash".into();

        let rendered = render_str(r#"{"title": "{{title}}", "message": "{{summary}}"}"#, &alert);
        assert!(rendered.get("injected").is_none(), "{rendered}");
        assert_eq!(rendered.as_object().unwrap().len(), 2);
        assert_eq!(rendered["title"], r#"", "injected": true, "x": ""#);
        assert_eq!(rendered["message"], "line\nbreak \"quoted\" \\ backslash");
    }

    /// Exactly one placeholder keeps the value's type; anything around it makes
    /// the result text.
    #[test]
    fn a_sole_placeholder_is_typed_and_a_mixed_one_is_text() {
        let alert = balance_alert();
        let rendered = render_str(
            r#"{"sole": "{{balance.is_available}}", "mixed": "available: {{balance.is_available}}",
                "num": "{{balance.total:number}}", "mixed_num": "{{balance.total}} {{balance.currency}}"}"#,
            &alert,
        );
        assert_eq!(rendered["sole"], serde_json::json!(true));
        assert_eq!(rendered["mixed"], "available: true");
        assert!(rendered["num"].is_number());
        assert_eq!(rendered["mixed_num"], "12.5 CNY");
    }

    /// A template written for balance alerts still produces a parseable
    /// document for a usage one. Refusing to send would lose the alert over a
    /// field the receiver may not even read.
    #[test]
    fn a_placeholder_that_does_not_apply_renders_null_rather_than_failing() {
        let usage = Alert {
            event: NotificationEventKind::UsageSurge,
            raised_at: 1_757_764_800_000,
            alert_key: "usage_surge".into(),
            title: "用量异常".into(),
            summary: "超出基线".into(),
            detail: AlertDetail::Usage(UsageAlert {
                window_hours: 1,
                baseline_days: 7,
                multiplier: decimal("3"),
                window_start_ms: 0,
                window_cost: decimal("9.5"),
                baseline_cost: decimal("1"),
                baseline_windows: 168,
                is_lower_bound: false,
                top_providers: vec![UsageSlice {
                    key: "Acme".into(),
                    label: None,
                    cost: decimal("9"),
                    messages: 3,
                }],
                top_conversations: vec![],
            }),
        };
        let rendered = render_str(
            r#"{"currency": "{{balance.currency}}", "cost": "{{usage.window_cost:number}}",
                "who": "{{usage.top_provider}}", "note": "in {{balance.currency}}."}"#,
            &usage,
        );
        assert_eq!(rendered["currency"], Value::Null);
        assert_eq!(rendered["cost"], serde_json::json!(9.5));
        assert_eq!(rendered["who"], "Acme");
        // Inside a sentence, absent is empty rather than the word "null".
        assert_eq!(rendered["note"], "in .");
    }

    /// Both are typos, and both would otherwise surface as a malformed alert at
    /// the one moment nobody is reading carefully.
    #[test]
    fn a_typo_is_refused_when_the_template_is_stored() {
        let unknown: Value = serde_json::from_str(r#"{"x": "{{balance.totl}}"}"#).unwrap();
        let error = validate(&unknown).unwrap_err();
        assert!(error.contains("balance.totl"), "{error}");

        let nested: Value = serde_json::from_str(r#"{"a": {"b": ["{{nope}}"]}}"#).unwrap();
        assert!(validate(&nested).is_err(), "it must walk the whole document");

        let not_a_number: Value = serde_json::from_str(r#"{"x": "{{balance.currency:number}}"}"#).unwrap();
        let error = validate(&not_a_number).unwrap_err();
        assert!(error.contains(":number"), "{error}");

        let fine: Value = serde_json::from_str(r#"{"a": {"b": ["{{title}}", 1, true, null]}}"#).unwrap();
        assert!(validate(&fine).is_ok());
    }

    /// A `{{` with no closing pair is text, not an error and not a panic — the
    /// template is somebody's JSON and a stray brace should not cost an alert.
    #[test]
    fn an_unclosed_placeholder_is_left_alone() {
        let alert = balance_alert();
        let rendered = render_str(r#"{"x": "{{ unterminated", "y": "}} stray"}"#, &alert);
        assert_eq!(rendered["x"], "{{ unterminated");
        assert_eq!(rendered["y"], "}} stray");
    }

    #[test]
    fn a_test_alert_fills_only_the_fields_every_alert_has() {
        let alert = Alert {
            event: NotificationEventKind::Test,
            raised_at: 1_757_764_800_000,
            alert_key: "test:ops".into(),
            title: "测试".into(),
            summary: "这是一条测试".into(),
            detail: AlertDetail::Test(TestAlert {
                format: crate::db::models::notification::NotificationFormat::Custom,
            }),
        };
        let rendered = render_str(
            r#"{"title": "{{title}}", "event": "{{event}}", "when": "{{raised_at_iso}}",
                "balance": "{{balance.total}}"}"#,
            &alert,
        );
        assert_eq!(rendered["title"], "测试");
        assert_eq!(rendered["event"], "test");
        assert_eq!(rendered["when"], "2025-09-13T12:00:00.000Z");
        assert_eq!(rendered["balance"], Value::Null);
    }
}

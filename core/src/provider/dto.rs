use std::collections::{BTreeMap, HashSet};
use std::sync::{Mutex, OnceLock};

use serde::Deserialize;

pub(crate) type ExtraIgnore = BTreeMap<String, serde_json::Value>;

#[derive(Deserialize)]
struct UpstreamErrorEnvelope {
    error: Option<UpstreamErrorBody>,
    message: Option<String>,
    msg: Option<String>,
    code: Option<serde_json::Value>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum UpstreamErrorBody {
    Text(String),
    Detail(UpstreamErrorDetail),
}

#[derive(Deserialize)]
struct UpstreamErrorDetail {
    message: Option<String>,
    msg: Option<String>,
    code: Option<serde_json::Value>,
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default, flatten)]
    extra: ExtraIgnore,
}

/// Some relays return an OpenAI-shaped error object inside an HTTP 200 SSE
/// event. It is a different wire DTO from a success response; recognizing it
/// after strict success parsing fails keeps the real upstream diagnosis.
pub(crate) fn embedded_upstream_error(raw: &[u8]) -> Option<String> {
    let envelope: UpstreamErrorEnvelope = serde_json::from_slice(raw).ok()?;
    let has_error = envelope.error.is_some() || envelope.message.is_some() || envelope.msg.is_some();
    if !has_error {
        return None;
    }
    warn_extra_fields("upstream_error_envelope", &envelope.extra);

    let (message, kind, code) = match envelope.error {
        Some(UpstreamErrorBody::Text(message)) => (Some(message), None, envelope.code),
        Some(UpstreamErrorBody::Detail(detail)) => {
            warn_extra_fields("upstream_error_detail", &detail.extra);
            (
                detail.message.or(detail.msg).or(envelope.message).or(envelope.msg),
                detail.kind,
                detail.code.or(envelope.code),
            )
        }
        None => (envelope.message.or(envelope.msg), None, envelope.code),
    };

    let mut summary = message.unwrap_or_else(|| "upstream returned an error response".into());
    if let Some(kind) = kind.filter(|value| !value.is_empty()) {
        summary = format!("{kind}: {summary}");
    }
    if let Some(code) = error_code_text(code.as_ref())
        && !summary.contains(&code)
    {
        summary.push_str(&format!(" (code: {code})"));
    }
    Some(summary)
}

fn error_code_text(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(value) if !value.is_empty() => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

/// Record unknown wire fields without accepting them into the domain model.
/// Streaming responses repeat the same envelope for every token, so each DTO
/// shape is warned once per process. Values are deliberately never logged.
pub(crate) fn warn_extra_fields(dto: &'static str, extra: &ExtraIgnore) {
    if extra.is_empty() {
        return;
    }

    let fields = extra.keys().cloned().collect::<Vec<_>>().join(",");
    if warn_once(format!("{dto}:{fields}")) {
        tracing::warn!(
            dto = dto,
            ignored_fields = %fields,
            "upstream response contained ignored fields"
        );
    }
}

/// A stream event or delta type this adapter has no branch for.
///
/// The same once-per-process rule as the fields above, and for the same
/// reason: the point is to learn that the wire moved, not to write a line per
/// token until it is fixed. Events the spec documents and this app has decided
/// to ignore do not come through here — they are named in the adapter, so this
/// only fires for something genuinely new.
pub(crate) fn warn_unknown_event(dto: &'static str, event: &str) {
    if warn_once(format!("{dto}:event:{event}")) {
        tracing::warn!(dto = dto, event = event, "upstream stream carried an unknown event");
    }
}

fn warn_once(fingerprint: String) -> bool {
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    WARNED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .map(|mut warned| {
            if warned.len() >= 256 {
                false
            } else {
                warned.insert(fingerprint)
            }
        })
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::embedded_upstream_error;

    #[test]
    fn extracts_newapi_error_from_a_success_status_body() {
        let raw = br#"{
            "error": {
                "message": "request parameters are invalid",
                "type": "invalid_request_error",
                "code": "bad_arguments",
                "param": null
            }
        }"#;
        assert_eq!(
            embedded_upstream_error(raw).as_deref(),
            Some("invalid_request_error: request parameters are invalid (code: bad_arguments)")
        );
    }

    #[test]
    fn does_not_turn_an_unrelated_malformed_success_body_into_an_api_error() {
        assert_eq!(embedded_upstream_error(br#"{"usage":{}}"#), None);
    }
}

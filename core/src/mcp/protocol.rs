//! The MCP wire, both ways round.
//!
//! Most of this file is the **client** half — what this app sends to an MCP
//! server it dialled, and what it reads back. [`Incoming`] and [`Outgoing`] at
//! the bottom are the **server** half, which exists for `acp::bridge`: there
//! this app *is* an MCP server, and a hosted agent is the one calling.
//!
//! The two directions are separate types on purpose. The tempting version makes
//! [`JsonRpcRequest`] bidirectional and widens its `id` to a `Value` so it can
//! also stand for an inbound request — but that decides the type of an id *we*
//! choose based on what somebody else might send. Ours is a counter and the
//! `u64` says so; theirs is whatever they picked and must come back untouched.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    pub fn new(id: u64, method: &str, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct JsonRpcResponse {
    #[allow(dead_code)]
    pub id: Option<u64>,
    pub result: Option<serde_json::Value>,
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct McpToolInfo {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct McpToolsListResult {
    pub tools: Vec<McpToolInfo>,
}

#[derive(Debug, Deserialize)]
pub struct McpCallToolResult {
    #[serde(default)]
    pub content: Vec<McpContent>,
    #[serde(default, rename = "isError")]
    pub is_error: bool,
}

#[derive(Debug, Deserialize)]
pub struct McpContent {
    #[serde(rename = "type")]
    pub content_type: String,
    #[serde(default)]
    pub text: Option<String>,
}

// ------------------------------------------------------------ the server half

/// A line arriving from a client, when this app is the MCP server.
///
/// Everything is held as loosely as the protocol permits, because the point is
/// to answer rather than to police: an unknown method gets a JSON-RPC error
/// naming it, which is a far better failure than a parse error that says only
/// that the frame was wrong.
///
/// `jsonrpc` is read and not checked. A client that omits it is out of spec and
/// perfectly answerable, and refusing over the version string would break a
/// working session to make a point.
#[derive(Debug, Deserialize)]
pub struct Incoming {
    /// **Doubly optional, for the reason `acp::protocol::Incoming::result`
    /// spells out**: a plain `Option<Value>` reads an explicit `null` as
    /// `None`, which is exactly how an absent member reads — and here those two
    /// mean opposite things. Absent is a *notification*, which must not be
    /// answered at all; `"id": null` is a malformed request, which must be.
    /// Collapsed, a client sending null would be met with silence and wait for
    /// a reply that is never coming.
    ///
    /// Measured against `claude-agent-acp` 0.70.0, which sends numbers and
    /// omits the member for notifications — never null. So this is the case
    /// that does not arise today and would be untraceable if it ever did.
    #[serde(default, deserialize_with = "present")]
    pub id: Option<Option<serde_json::Value>>,
    pub method: String,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

/// Records that a member was *there*, whatever it said.
///
/// serde calls this only when the member exists, which is itself the answer the
/// plain `Option` cannot express. The same two-line trick as `acp::protocol`,
/// duplicated rather than shared: one is about a `result` and one about an
/// `id`, and a helper spanning both modules would tie the ACP client's parsing
/// to the MCP server's.
fn present<'de, D>(deserializer: D) -> Result<Option<Option<serde_json::Value>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<serde_json::Value>::deserialize(deserializer).map(Some)
}

/// What a line turned out to be.
pub enum Inbound {
    /// Owes a reply carrying `id` back unchanged.
    Request {
        id: serde_json::Value,
        method: String,
        params: serde_json::Value,
    },
    /// Owes no reply. `notifications/initialized` is the one that matters.
    Notification { method: String },
    /// A request with an explicit null id. Owes an error, and the error's own
    /// id is null — which is what JSON-RPC prescribes for a request that could
    /// not be attributed.
    Malformed,
}

impl Incoming {
    pub fn classify(self) -> Inbound {
        match self.id {
            Some(Some(id)) => Inbound::Request {
                id,
                method: self.method,
                params: self.params.unwrap_or(serde_json::Value::Null),
            },
            None => Inbound::Notification { method: self.method },
            Some(None) => Inbound::Malformed,
        }
    }
}

/// A reply going back to a client, when this app is the MCP server.
///
/// The id is a `Value` and is never inspected: whatever came in goes back out,
/// which is the one thing a JSON-RPC server owes unconditionally.
#[derive(Debug, Serialize)]
pub struct Outgoing {
    pub jsonrpc: &'static str,
    pub id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<OutgoingError>,
}

#[derive(Debug, Serialize)]
pub struct OutgoingError {
    pub code: i32,
    pub message: String,
}

/// JSON-RPC's own codes, for the two things that can go wrong here.
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_REQUEST: i32 = -32600;

impl Outgoing {
    pub fn result(id: serde_json::Value, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: serde_json::Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(OutgoingError {
                code,
                message: message.into(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn classify(raw: serde_json::Value) -> Inbound {
        serde_json::from_value::<Incoming>(raw).unwrap().classify()
    }

    /// The three id shapes the spec allows, all of which must come back
    /// unchanged. Measured: the adapter sends numbers.
    #[test]
    fn every_id_shape_survives_the_round_trip() {
        for id in [json!(0), json!(42), json!("abc"), json!("0")] {
            let Inbound::Request { id: got, .. } = classify(json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/list"
            })) else {
                panic!("{id} was not read as a request");
            };
            assert_eq!(got, id);
            let encoded = serde_json::to_value(Outgoing::result(got, json!({}))).unwrap();
            assert_eq!(encoded["id"], id, "the reply changed the id");
        }
    }

    /// The distinction the doubly-optional field exists for. Collapsed, the
    /// second of these would be answered with silence.
    #[test]
    fn an_absent_id_is_a_notification_and_a_null_one_is_not() {
        assert!(matches!(
            classify(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })),
            Inbound::Notification { .. }
        ));
        assert!(matches!(
            classify(json!({ "jsonrpc": "2.0", "id": null, "method": "tools/list" })),
            Inbound::Malformed
        ));
    }

    /// Absent params are an empty object's worth of nothing, not a parse error:
    /// `tools/list` legitimately carries none.
    #[test]
    fn a_request_without_params_still_parses() {
        let Inbound::Request { params, .. } = classify(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list"
        })) else {
            panic!("not a request");
        };
        assert_eq!(params, serde_json::Value::Null);
    }

    /// A client that leaves the version off is answerable, and refusing it
    /// would break a working session over a string nobody reads.
    #[test]
    fn a_missing_jsonrpc_member_is_not_fatal() {
        assert!(matches!(
            classify(json!({ "id": 1, "method": "tools/list" })),
            Inbound::Request { .. }
        ));
    }

    /// Exactly one of the two members, so a client cannot read a reply as both
    /// a success and a failure.
    #[test]
    fn a_reply_carries_a_result_or_an_error_and_never_both() {
        let ok = serde_json::to_value(Outgoing::result(json!(1), json!({ "tools": [] }))).unwrap();
        assert!(ok.get("result").is_some() && ok.get("error").is_none(), "{ok}");

        let bad = serde_json::to_value(Outgoing::error(json!(1), METHOD_NOT_FOUND, "nope")).unwrap();
        assert!(bad.get("error").is_some() && bad.get("result").is_none(), "{bad}");
        assert_eq!(bad["error"]["code"], METHOD_NOT_FOUND);
    }
}

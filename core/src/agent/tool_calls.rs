use serde::{Deserialize, Serialize};

use crate::provider;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenAiToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: OpenAiToolCallKind,
    function: OpenAiFunction,
}

#[derive(Deserialize)]
enum OpenAiToolCallKind {
    #[serde(rename = "function")]
    Function,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenAiFunction {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyToolCallBlock {
    #[serde(rename = "type")]
    kind: LegacyToolCallKind,
    data: LegacyToolCallData,
}

#[derive(Deserialize)]
enum LegacyToolCallKind {
    #[serde(rename = "tool_call")]
    ToolCall,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyToolCallData {
    call_id: String,
    tool_name: String,
    arguments: String,
}

fn validate_call(call: provider::ToolCall, location: &str) -> Result<provider::ToolCall, String> {
    if call.id.is_empty() {
        return Err(format!("{location}.id must not be empty"));
    }
    if call.name.is_empty() {
        return Err(format!("{location}.function.name must not be empty"));
    }
    Ok(call)
}

pub fn extract_tool_calls_from_blocks(blocks_json: &str) -> Result<Vec<provider::ToolCall>, String> {
    let blocks: Vec<serde_json::Value> =
        serde_json::from_str(blocks_json).map_err(|error| format!("invalid legacy tool_calls JSON: {error}"))?;
    let mut calls = Vec::new();
    for (index, block) in blocks.into_iter().enumerate() {
        let object = block
            .as_object()
            .ok_or_else(|| format!("legacy tool_calls[{index}] must be a JSON object"))?;
        let kind = object
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("legacy tool_calls[{index}].type must be a string"))?;
        if kind != "tool_call" {
            continue;
        }
        let block: LegacyToolCallBlock = serde_json::from_value(block)
            .map_err(|error| format!("invalid legacy tool_calls[{index}] object: {error}"))?;
        let LegacyToolCallKind::ToolCall = block.kind;
        calls.push(validate_call(
            provider::ToolCall {
                id: block.data.call_id,
                name: block.data.tool_name,
                arguments: block.data.arguments,
            },
            &format!("legacy tool_calls[{index}]"),
        )?);
    }
    Ok(calls)
}

pub(crate) fn parse_openai_tool_calls(json: Option<&str>) -> Result<Vec<provider::ToolCall>, String> {
    let Some(json) = json else { return Ok(Vec::new()) };
    let calls: Vec<OpenAiToolCall> =
        serde_json::from_str(json).map_err(|error| format!("invalid tool_calls JSON: {error}"))?;
    calls
        .into_iter()
        .enumerate()
        .map(|(index, call)| {
            let OpenAiToolCallKind::Function = call.kind;
            validate_call(
                provider::ToolCall {
                    id: call.id,
                    name: call.function.name,
                    arguments: call.function.arguments,
                },
                &format!("tool_calls[{index}]"),
            )
        })
        .collect()
}

pub fn parse_stored_tool_calls(schema_version: i32, json: Option<&str>) -> Result<Vec<provider::ToolCall>, String> {
    match schema_version {
        1 => json
            .map(extract_tool_calls_from_blocks)
            .transpose()
            .map(Option::unwrap_or_default),
        2 => parse_openai_tool_calls(json),
        version => Err(format!("unsupported persisted message schema_version {version}")),
    }
}

#[derive(Serialize)]
struct StoredOpenAiToolCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: StoredOpenAiFunction<'a>,
}

#[derive(Serialize)]
struct StoredOpenAiFunction<'a> {
    name: &'a str,
    arguments: &'a str,
}

pub(crate) fn serialize_tool_calls_openai(tool_calls: &[provider::ToolCall]) -> String {
    let calls = tool_calls
        .iter()
        .map(|call| StoredOpenAiToolCall {
            id: &call.id,
            kind: "function",
            function: StoredOpenAiFunction {
                name: &call.name,
                arguments: &call.arguments,
            },
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&calls).expect("serializing tool calls cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str =
        r#"[{"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.rs\"}"}}]"#;

    #[test]
    fn openai_tool_calls_require_the_exact_persisted_shape() {
        let calls = parse_openai_tool_calls(Some(VALID)).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");

        assert!(parse_openai_tool_calls(Some("not-json")).is_err());
        assert!(parse_openai_tool_calls(Some(r#"[{"id":"call_1"}]"#)).is_err());
        assert!(
            parse_openai_tool_calls(Some(
                r#"[{"id":"call_1","type":"future","function":{"name":"read_file","arguments":"{}"}}]"#,
            ))
            .is_err()
        );
        assert!(
            parse_openai_tool_calls(Some(
                r#"[{"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{}","future":true}}]"#,
            ))
            .is_err()
        );
    }

    #[test]
    fn stored_arguments_are_preserved_verbatim() {
        let calls = parse_openai_tool_calls(Some(
            r#"[{"id":"call_1","type":"function","function":{"name":"run_command","arguments":"nope"}}]"#,
        ))
        .unwrap();
        assert_eq!(calls[0].arguments, "nope", "truncated arguments preserved");

        let calls = parse_openai_tool_calls(Some(
            r#"[{"id":"call_1","type":"function","function":{"name":"run_command","arguments":"[]"}}]"#,
        ))
        .unwrap();
        assert_eq!(calls[0].arguments, "[]", "non-object arguments preserved");
    }

    #[test]
    fn legacy_tool_call_blocks_do_not_drop_malformed_entries() {
        let calls = extract_tool_calls_from_blocks(
            r#"[{"type":"text","data":{"text":"hi"}},{"type":"tool_call","data":{"call_id":"c","tool_name":"read_file","arguments":"{}"}}]"#,
        )
        .unwrap();
        assert_eq!(calls.len(), 1);
        assert!(extract_tool_calls_from_blocks(r#"[{"type":"tool_call","data":{}}]"#).is_err());
        assert!(extract_tool_calls_from_blocks(r#"[null]"#).is_err());
    }

    #[test]
    fn serializer_round_trips_the_strict_contract() {
        let calls = vec![provider::ToolCall {
            id: "c".into(),
            name: "read_file".into(),
            arguments: r#"{"path":"a.rs"}"#.into(),
        }];
        let stored = serialize_tool_calls_openai(&calls);
        let parsed = parse_openai_tool_calls(Some(&stored)).unwrap();
        assert_eq!(parsed[0].id, "c");
        assert_eq!(parsed[0].arguments, r#"{"path":"a.rs"}"#);
    }

    #[test]
    fn future_message_schema_versions_are_rejected() {
        assert!(parse_stored_tool_calls(0, None).is_err());
        assert!(parse_stored_tool_calls(3, Some("[]")).is_err());
    }
}

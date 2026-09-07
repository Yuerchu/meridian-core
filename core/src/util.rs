use std::time::{SystemTime, UNIX_EPOCH};

use crate::db::{self, DbPool};

pub fn take_bytes_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = 0;
    for (i, ch) in s.char_indices() {
        let next = i + ch.len_utf8();
        if next > max_bytes {
            break;
        }
        end = next;
    }
    &s[..end]
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

pub fn get_conn(pool: &DbPool) -> Result<db::PooledConn, String> {
    pool.get().map_err(|e| format!("db connection error: {e}"))
}

/// The balanced `{...}` starting at `start`, or `None` if it never closes.
fn balanced_object_at(text: &str, start: usize) -> Option<String> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if escaped {
            escaped = false;
            continue;
        }
        match b {
            b'\\' if in_string => escaped = true,
            b'"' => in_string = !in_string,
            b'{' if !in_string => depth += 1,
            b'}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Pull the first balanced `{...}` out of a reply, tolerating fenced code blocks
/// and the odd sentence of preamble.
pub(crate) fn extract_json_object(text: &str) -> Option<String> {
    balanced_object_at(text, text.find('{')?)
}

/// The *last* balanced `{...}`, for protocols that put the answer at the end.
///
/// A reviewer asked to finish with a verdict object will happily quote another
/// JSON fragment earlier as evidence — a snippet of the config it is objecting
/// to, an example of the shape it wants. Taking the first `{` gets that one.
/// Scanning candidate openings from the back and keeping the first that parses
/// is what makes "the verdict is the last thing you write" enforceable.
///
/// `accept` decides whether a candidate is the object being looked for, so the
/// caller's own deserialiser is the test rather than mere well-formedness. A
/// candidate that closes but fails the predicate is passed over rather than
/// returned: handing it back would make "found the wrong object" indistinguish-
/// able from "found the right one" at the call site.
pub(crate) fn extract_last_json_object(text: &str, accept: impl Fn(&str) -> bool) -> Option<String> {
    let mut openings: Vec<usize> = text.match_indices('{').map(|(i, _)| i).collect();
    openings.reverse();
    openings
        .into_iter()
        .filter_map(|start| balanced_object_at(text, start))
        .find(|candidate| accept(candidate))
}

pub fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(de).map(Some)
}

/// Read a response body, refusing to buffer more than `max` bytes.
///
/// Streamed rather than `bytes()` because `Content-Length` is a claim by the
/// far end: a server that lies about it, or omits it, would otherwise decide
/// how much memory this process spends. Shared by every caller that fetches
/// something from outside — a limit implemented twice is a limit that will
/// eventually be two different numbers.
pub async fn read_body_capped(resp: reqwest::Response, max: usize, what: &str) -> Result<Vec<u8>, String> {
    use futures::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("failed to read {what}: {e}"))?;
        if buf.len() + chunk.len() > max {
            return Err(format!("{what} exceeded its size limit"));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_take_bytes_ascii() {
        assert_eq!(take_bytes_at_char_boundary("hello world", 5), "hello");
    }

    #[test]
    fn test_take_bytes_short_string() {
        assert_eq!(take_bytes_at_char_boundary("hi", 10), "hi");
    }

    #[test]
    fn test_take_bytes_multibyte() {
        let s = "hello世界";
        assert_eq!(take_bytes_at_char_boundary(s, 5), "hello");
        assert_eq!(take_bytes_at_char_boundary(s, 6), "hello");
        assert_eq!(take_bytes_at_char_boundary(s, 7), "hello");
        assert_eq!(take_bytes_at_char_boundary(s, 8), "hello世");
        assert_eq!(take_bytes_at_char_boundary(s, 11), "hello世界");
    }

    #[test]
    fn test_take_bytes_zero() {
        assert_eq!(take_bytes_at_char_boundary("hello", 0), "");
    }

    #[test]
    fn extracts_object_past_preamble_and_fence() {
        let text = "Sure, here it is:\n```json\n{\"a\": 1}\n```\n";
        assert_eq!(extract_json_object(text).as_deref(), Some("{\"a\": 1}"));
    }

    #[test]
    fn braces_inside_strings_do_not_close_the_object() {
        let text = r#"{"a": "} not the end {", "b": 2}"#;
        assert_eq!(extract_json_object(text).as_deref(), Some(text));
    }

    #[test]
    fn unclosed_object_is_not_an_object() {
        assert!(extract_json_object("{\"a\": 1").is_none());
        assert!(extract_json_object("no braces here").is_none());
    }

    /// The whole reason `extract_last_json_object` exists: a reviewer quotes a
    /// fragment as evidence and then states its verdict.
    #[test]
    fn last_object_wins_over_a_quoted_example() {
        let text = concat!(
            "The config you wrote is `{\"verdict\": \"approve\"}` which is not\n",
            "what the schema says. My own answer:\n",
            "```json\n{\"verdict\": \"revise\", \"summary\": \"schema mismatch\"}\n```\n",
        );
        let found = extract_last_json_object(text, |c| c.contains("summary")).unwrap();
        assert!(found.contains("revise"), "{found}");
        assert!(found.contains("schema mismatch"), "{found}");
    }

    #[test]
    fn nested_objects_do_not_confuse_the_scan() {
        let text = "{\"outer\": {\"inner\": 1}, \"verdict\": \"approve\"}";
        let found = extract_last_json_object(text, |c| c.contains("verdict")).unwrap();
        assert_eq!(found, text);
    }

    #[test]
    fn a_candidate_that_fails_the_predicate_is_not_returned() {
        let text = "{\"unrelated\": 1}";
        assert!(extract_last_json_object(text, |c| c.contains("verdict")).is_none());
    }
}

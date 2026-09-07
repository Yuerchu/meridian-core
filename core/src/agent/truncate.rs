// Ported from codex-rs/utils/string/src/truncate.rs, codex-rs/utils/output-truncation/src/lib.rs,
// and TruncationPolicy from codex-rs/protocol/src/protocol.rs (Apache-2.0, OpenAI)
// NOTICE: This file contains code derived from the OpenAI Codex project.
// Changes: merged into one module; dropped FunctionCallOutputContentItem/audio helpers,
// serde/schema derives, and the Mul<f64> impl; visibility reduced to pub(crate).

//! Utilities for truncating large chunks of output while preserving a prefix
//! and suffix on UTF-8 boundaries.

const APPROX_BYTES_PER_TOKEN: usize = 4;

/// Default budget applied to tool outputs before they reach the UI, DB, and
/// model context (~40KB).
pub(crate) const TOOL_OUTPUT_TRUNCATION: TruncationPolicy = TruncationPolicy::Tokens(10_000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TruncationPolicy {
    #[allow(dead_code)]
    Bytes(usize),
    Tokens(usize),
}

impl TruncationPolicy {
    #[allow(dead_code)]
    pub(crate) fn token_budget(&self) -> usize {
        match self {
            TruncationPolicy::Bytes(bytes) => {
                usize::try_from(approx_tokens_from_byte_count(*bytes)).unwrap_or(usize::MAX)
            }
            TruncationPolicy::Tokens(tokens) => *tokens,
        }
    }

    pub(crate) fn byte_budget(&self) -> usize {
        match self {
            TruncationPolicy::Bytes(bytes) => *bytes,
            TruncationPolicy::Tokens(tokens) => approx_bytes_for_tokens(*tokens),
        }
    }
}

pub(crate) fn formatted_truncate_text(content: &str, policy: TruncationPolicy) -> String {
    if content.len() <= policy.byte_budget() {
        return content.to_string();
    }

    let original_token_count = approx_token_count(content);
    let total_lines = content.lines().count();
    let result = truncate_text(content, policy);
    format!(
        "Warning: truncated output (original token count: {original_token_count})\nTotal output lines: {total_lines}\n\n{result}"
    )
}

pub(crate) fn truncate_text(content: &str, policy: TruncationPolicy) -> String {
    match policy {
        TruncationPolicy::Bytes(bytes) => truncate_middle_chars(content, bytes),
        TruncationPolicy::Tokens(tokens) => truncate_middle_with_token_budget(content, tokens).0,
    }
}

/// Truncate a string to `max_bytes` using a character-count marker.
pub(crate) fn truncate_middle_chars(s: &str, max_bytes: usize) -> String {
    truncate_with_byte_estimate(s, max_bytes, /*use_tokens*/ false)
}

/// Truncate the middle of a UTF-8 string to at most `max_tokens` approximate
/// tokens, preserving the beginning and the end. Returns the possibly
/// truncated string and `Some(original_token_count)` if truncation occurred;
/// otherwise returns the original string and `None`.
pub(crate) fn truncate_middle_with_token_budget(s: &str, max_tokens: usize) -> (String, Option<u64>) {
    if s.is_empty() {
        return (String::new(), None);
    }

    if max_tokens > 0 && s.len() <= approx_bytes_for_tokens(max_tokens) {
        return (s.to_string(), None);
    }

    let truncated = truncate_with_byte_estimate(s, approx_bytes_for_tokens(max_tokens), /*use_tokens*/ true);
    let total_tokens = u64::try_from(approx_token_count(s)).unwrap_or(u64::MAX);

    if truncated == s {
        (truncated, None)
    } else {
        (truncated, Some(total_tokens))
    }
}

fn truncate_with_byte_estimate(s: &str, max_bytes: usize, use_tokens: bool) -> String {
    if s.is_empty() {
        return String::new();
    }

    let total_chars = s.chars().count();

    if max_bytes == 0 {
        return format_truncation_marker(use_tokens, removed_units(use_tokens, s.len(), total_chars));
    }

    if s.len() <= max_bytes {
        return s.to_string();
    }

    let total_bytes = s.len();
    let (left_budget, right_budget) = split_budget(max_bytes);
    let (removed_chars, left, right) = split_string(s, left_budget, right_budget);
    let marker = format_truncation_marker(
        use_tokens,
        removed_units(use_tokens, total_bytes.saturating_sub(max_bytes), removed_chars),
    );

    assemble_truncated_output(left, right, &marker)
}

pub(crate) fn approx_token_count(text: &str) -> usize {
    let len = text.len();
    len.saturating_add(APPROX_BYTES_PER_TOKEN.saturating_sub(1)) / APPROX_BYTES_PER_TOKEN
}

pub(crate) fn approx_bytes_for_tokens(tokens: usize) -> usize {
    tokens.saturating_mul(APPROX_BYTES_PER_TOKEN)
}

pub(crate) fn approx_tokens_from_byte_count(bytes: usize) -> u64 {
    let bytes_u64 = bytes as u64;
    bytes_u64.saturating_add((APPROX_BYTES_PER_TOKEN as u64).saturating_sub(1)) / (APPROX_BYTES_PER_TOKEN as u64)
}

fn split_string(s: &str, beginning_bytes: usize, end_bytes: usize) -> (usize, &str, &str) {
    if s.is_empty() {
        return (0, "", "");
    }

    let len = s.len();
    let tail_start_target = len.saturating_sub(end_bytes);
    let mut prefix_end = 0usize;
    let mut suffix_start = len;
    let mut removed_chars = 0usize;
    let mut suffix_started = false;

    for (idx, ch) in s.char_indices() {
        let char_end = idx + ch.len_utf8();
        if char_end <= beginning_bytes {
            prefix_end = char_end;
            continue;
        }

        if idx >= tail_start_target {
            if !suffix_started {
                suffix_start = idx;
                suffix_started = true;
            }
            continue;
        }

        removed_chars = removed_chars.saturating_add(1);
    }

    if suffix_start < prefix_end {
        suffix_start = prefix_end;
    }

    let before = &s[..prefix_end];
    let after = &s[suffix_start..];

    (removed_chars, before, after)
}

fn split_budget(budget: usize) -> (usize, usize) {
    let left = budget / 2;
    (left, budget - left)
}

fn format_truncation_marker(use_tokens: bool, removed_count: u64) -> String {
    if use_tokens {
        format!("…{removed_count} tokens truncated…")
    } else {
        format!("…{removed_count} chars truncated…")
    }
}

fn removed_units(use_tokens: bool, removed_bytes: usize, removed_chars: usize) -> u64 {
    if use_tokens {
        approx_tokens_from_byte_count(removed_bytes)
    } else {
        u64::try_from(removed_chars).unwrap_or(u64::MAX)
    }
}

fn assemble_truncated_output(prefix: &str, suffix: &str, marker: &str) -> String {
    let mut out = String::with_capacity(prefix.len() + marker.len() + suffix.len() + 1);
    out.push_str(prefix);
    out.push_str(marker);
    out.push_str(suffix);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_string_works() {
        assert_eq!(
            split_string("hello world", /*beginning_bytes*/ 5, /*end_bytes*/ 5),
            (1, "hello", "world")
        );
        assert_eq!(split_string("abc", /*beginning_bytes*/ 0, /*end_bytes*/ 0), (3, "", ""));
    }

    #[test]
    fn split_string_handles_empty_string() {
        assert_eq!(split_string("", /*beginning_bytes*/ 4, /*end_bytes*/ 4), (0, "", ""));
    }

    #[test]
    fn split_string_only_keeps_prefix_when_tail_budget_is_zero() {
        assert_eq!(
            split_string("abcdef", /*beginning_bytes*/ 3, /*end_bytes*/ 0),
            (3, "abc", "")
        );
    }

    #[test]
    fn split_string_only_keeps_suffix_when_prefix_budget_is_zero() {
        assert_eq!(
            split_string("abcdef", /*beginning_bytes*/ 0, /*end_bytes*/ 3),
            (3, "", "def")
        );
    }

    #[test]
    fn split_string_handles_overlapping_budgets_without_removal() {
        assert_eq!(
            split_string("abcdef", /*beginning_bytes*/ 4, /*end_bytes*/ 4),
            (0, "abcd", "ef")
        );
    }

    #[test]
    fn split_string_respects_utf8_boundaries() {
        assert_eq!(
            split_string("😀abc😀", /*beginning_bytes*/ 5, /*end_bytes*/ 5),
            (1, "😀a", "c😀")
        );
        assert_eq!(
            split_string("😀😀😀😀😀", /*beginning_bytes*/ 1, /*end_bytes*/ 1),
            (5, "", "")
        );
        assert_eq!(
            split_string("😀😀😀😀😀", /*beginning_bytes*/ 7, /*end_bytes*/ 7),
            (3, "😀", "😀")
        );
        assert_eq!(
            split_string("😀😀😀😀😀", /*beginning_bytes*/ 8, /*end_bytes*/ 8),
            (1, "😀😀", "😀😀")
        );
    }

    #[test]
    fn truncate_with_token_budget_returns_original_when_under_limit() {
        let s = "short output";
        let limit = 100;
        let (out, original) = truncate_middle_with_token_budget(s, limit);
        assert_eq!(out, s);
        assert_eq!(original, None);
    }

    #[test]
    fn truncate_with_token_budget_reports_truncation_at_zero_limit() {
        let s = "abcdef";
        let (out, original) = truncate_middle_with_token_budget(s, /*max_tokens*/ 0);
        assert_eq!(out, "…2 tokens truncated…");
        assert_eq!(original, Some(2));
    }

    #[test]
    fn truncate_middle_tokens_handles_utf8_content() {
        let s = "😀😀😀😀😀😀😀😀😀😀\nsecond line with text\n";
        let (out, tokens) = truncate_middle_with_token_budget(s, /*max_tokens*/ 8);
        assert_eq!(out, "😀😀😀😀…8 tokens truncated… line with text\n");
        assert_eq!(tokens, Some(16));
    }

    #[test]
    fn truncate_middle_bytes_handles_utf8_content() {
        let s = "😀😀😀😀😀😀😀😀😀😀\nsecond line with text\n";
        let out = truncate_middle_chars(s, /*max_bytes*/ 20);
        assert_eq!(out, "😀😀…21 chars truncated…with text\n");
    }

    #[test]
    fn formatted_truncate_returns_original_when_under_budget() {
        let s = "short output";
        assert_eq!(formatted_truncate_text(s, TruncationPolicy::Tokens(100)), s);
    }

    #[test]
    fn formatted_truncate_adds_warning_header() {
        let s = "line\n".repeat(100);
        let out = formatted_truncate_text(&s, TruncationPolicy::Tokens(10));
        assert!(out.starts_with("Warning: truncated output (original token count: 125)\nTotal output lines: 100\n\n"));
        assert!(out.contains("tokens truncated…"));
    }
}

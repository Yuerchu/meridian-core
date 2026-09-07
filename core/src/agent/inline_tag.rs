// Ported from codex-rs/utils/stream-parser/src/stream_text.rs and
// src/inline_hidden_tag.rs (Apache-2.0, OpenAI)
// NOTICE: This file contains code derived from the OpenAI Codex project.
// Changes: merged into one module; added a streaming mode (new_streaming) that
// emits extracted tag content incrementally as deltas instead of buffering
// until the close tag; visibility reduced to pub(crate).

/// Incremental parser result for one pushed chunk (or final flush).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StreamTextChunk<T> {
    /// Text safe to render immediately.
    pub(crate) visible_text: String,
    /// Hidden payloads extracted from the chunk.
    pub(crate) extracted: Vec<T>,
}

impl<T> Default for StreamTextChunk<T> {
    fn default() -> Self {
        Self {
            visible_text: String::new(),
            extracted: Vec::new(),
        }
    }
}

/// One hidden inline tag extracted by [`InlineHiddenTagParser`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExtractedInlineTag<T> {
    pub(crate) tag: T,
    pub(crate) content: String,
}

/// Literal tag specification used by [`InlineHiddenTagParser`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InlineTagSpec<T> {
    pub(crate) tag: T,
    pub(crate) open: &'static str,
    pub(crate) close: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveTag<T> {
    tag: T,
    close: &'static str,
    content: String,
}

/// Generic streaming parser that hides configured inline tags and extracts their contents.
///
/// Example:
/// - input: `hello <think>reasoning</think> world`
/// - visible output: `hello  world`
/// - extracted: `["reasoning"]`
///
/// Matching is literal and non-nested. If EOF is reached while a tag is still open, the parser
/// auto-closes it and returns the buffered content as extracted data.
///
/// In streaming mode ([`InlineHiddenTagParser::new_streaming`]) the content of an active tag is
/// emitted incrementally as extracted deltas rather than buffered until the close tag, so
/// consumers can render it live (e.g. a reasoning pane).
#[derive(Debug)]
pub(crate) struct InlineHiddenTagParser<T>
where
    T: Clone + Eq,
{
    specs: Vec<InlineTagSpec<T>>,
    pending: String,
    active: Option<ActiveTag<T>>,
    stream_content: bool,
}

impl<T> InlineHiddenTagParser<T>
where
    T: Clone + Eq,
{
    /// Create a parser for one or more hidden inline tags.
    pub(crate) fn new(specs: Vec<InlineTagSpec<T>>) -> Self {
        assert!(
            !specs.is_empty(),
            "InlineHiddenTagParser requires at least one tag spec"
        );
        for spec in &specs {
            assert!(
                !spec.open.is_empty(),
                "InlineHiddenTagParser requires non-empty open delimiters"
            );
            assert!(
                !spec.close.is_empty(),
                "InlineHiddenTagParser requires non-empty close delimiters"
            );
        }
        Self {
            specs,
            pending: String::new(),
            active: None,
            stream_content: false,
        }
    }

    /// Like [`InlineHiddenTagParser::new`], but emits active-tag content incrementally.
    pub(crate) fn new_streaming(specs: Vec<InlineTagSpec<T>>) -> Self {
        let mut parser = Self::new(specs);
        parser.stream_content = true;
        parser
    }

    fn find_next_open(&self) -> Option<(usize, usize)> {
        self.specs
            .iter()
            .enumerate()
            .filter_map(|(idx, spec)| self.pending.find(spec.open).map(|pos| (pos, spec.open.len(), idx)))
            .min_by(|(pos_a, len_a, idx_a), (pos_b, len_b, idx_b)| {
                pos_a
                    .cmp(pos_b)
                    .then_with(|| len_b.cmp(len_a))
                    .then_with(|| idx_a.cmp(idx_b))
            })
            .map(|(pos, _len, idx)| (pos, idx))
    }

    fn max_open_prefix_suffix_len(&self) -> usize {
        self.specs
            .iter()
            .map(|spec| longest_suffix_prefix_len(&self.pending, spec.open))
            .max()
            .unwrap_or(0)
    }

    fn push_visible_prefix(out: &mut StreamTextChunk<ExtractedInlineTag<T>>, pending: &str) {
        if !pending.is_empty() {
            out.visible_text.push_str(pending);
        }
    }

    fn drain_visible_to_suffix_match(
        &mut self,
        out: &mut StreamTextChunk<ExtractedInlineTag<T>>,
        keep_suffix_len: usize,
    ) {
        let take = self.pending.len().saturating_sub(keep_suffix_len);
        if take == 0 {
            return;
        }
        Self::push_visible_prefix(out, &self.pending[..take]);
        self.pending.drain(..take);
    }

    /// Feed a new text chunk.
    pub(crate) fn push_str(&mut self, chunk: &str) -> StreamTextChunk<ExtractedInlineTag<T>> {
        self.pending.push_str(chunk);
        let mut out = StreamTextChunk::default();

        loop {
            if let Some(close) = self.active.as_ref().map(|active| active.close) {
                if let Some(close_idx) = self.pending.find(close) {
                    let Some(mut active) = self.active.take() else {
                        continue;
                    };
                    if self.stream_content {
                        if close_idx > 0 {
                            out.extracted.push(ExtractedInlineTag {
                                tag: active.tag,
                                content: self.pending[..close_idx].to_string(),
                            });
                        }
                    } else {
                        active.content.push_str(&self.pending[..close_idx]);
                        out.extracted.push(ExtractedInlineTag {
                            tag: active.tag,
                            content: active.content,
                        });
                    }
                    let close_len = close.len();
                    self.pending.drain(..close_idx + close_len);
                    continue;
                }

                let keep = longest_suffix_prefix_len(&self.pending, close);
                let take = self.pending.len().saturating_sub(keep);
                if take > 0 {
                    if self.stream_content {
                        if let Some(active) = self.active.as_ref() {
                            out.extracted.push(ExtractedInlineTag {
                                tag: active.tag.clone(),
                                content: self.pending[..take].to_string(),
                            });
                        }
                    } else if let Some(active) = self.active.as_mut() {
                        active.content.push_str(&self.pending[..take]);
                    }
                    self.pending.drain(..take);
                }
                break;
            }

            if let Some((open_idx, spec_idx)) = self.find_next_open() {
                Self::push_visible_prefix(&mut out, &self.pending[..open_idx]);
                let spec = &self.specs[spec_idx];
                let open_len = spec.open.len();
                self.pending.drain(..open_idx + open_len);
                self.active = Some(ActiveTag {
                    tag: spec.tag.clone(),
                    close: spec.close,
                    content: String::new(),
                });
                continue;
            }

            let keep = self.max_open_prefix_suffix_len();
            self.drain_visible_to_suffix_match(&mut out, keep);
            break;
        }

        out
    }

    /// Flush any buffered state at end-of-stream.
    pub(crate) fn finish(&mut self) -> StreamTextChunk<ExtractedInlineTag<T>> {
        let mut out = StreamTextChunk::default();

        if let Some(mut active) = self.active.take() {
            if !self.pending.is_empty() {
                active.content.push_str(&self.pending);
                self.pending.clear();
            }
            if !self.stream_content || !active.content.is_empty() {
                out.extracted.push(ExtractedInlineTag {
                    tag: active.tag,
                    content: active.content,
                });
            }
            return out;
        }

        if !self.pending.is_empty() {
            out.visible_text.push_str(&self.pending);
            self.pending.clear();
        }

        out
    }
}

fn longest_suffix_prefix_len(s: &str, needle: &str) -> usize {
    let max = s.len().min(needle.len().saturating_sub(1));
    for k in (1..=max).rev() {
        if needle.is_char_boundary(k) && s.ends_with(&needle[..k]) {
            return k;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Tag {
        A,
        B,
    }

    fn collect_chunks<T: Clone + Eq>(
        parser: &mut InlineHiddenTagParser<T>,
        chunks: &[&str],
    ) -> StreamTextChunk<ExtractedInlineTag<T>> {
        let mut all = StreamTextChunk::default();
        for chunk in chunks {
            let next = parser.push_str(chunk);
            all.visible_text.push_str(&next.visible_text);
            all.extracted.extend(next.extracted);
        }
        let tail = parser.finish();
        all.visible_text.push_str(&tail.visible_text);
        all.extracted.extend(tail.extracted);
        all
    }

    #[test]
    fn generic_inline_parser_supports_multiple_tag_types() {
        let mut parser = InlineHiddenTagParser::new(vec![
            InlineTagSpec {
                tag: Tag::A,
                open: "<a>",
                close: "</a>",
            },
            InlineTagSpec {
                tag: Tag::B,
                open: "<b>",
                close: "</b>",
            },
        ]);

        let out = collect_chunks(&mut parser, &["1<a>x</a>2<b>y</b>3"]);

        assert_eq!(out.visible_text, "123");
        assert_eq!(out.extracted.len(), 2);
        assert_eq!(out.extracted[0].tag, Tag::A);
        assert_eq!(out.extracted[0].content, "x");
        assert_eq!(out.extracted[1].tag, Tag::B);
        assert_eq!(out.extracted[1].content, "y");
    }

    #[test]
    fn generic_inline_parser_supports_non_ascii_tag_delimiters() {
        let mut parser = InlineHiddenTagParser::new(vec![InlineTagSpec {
            tag: Tag::A,
            open: "<é>",
            close: "</é>",
        }]);

        let out = collect_chunks(&mut parser, &["a<", "é>中</", "é>b"]);

        assert_eq!(out.visible_text, "ab");
        assert_eq!(out.extracted.len(), 1);
        assert_eq!(out.extracted[0].tag, Tag::A);
        assert_eq!(out.extracted[0].content, "中");
    }

    #[test]
    fn generic_inline_parser_prefers_longest_opener_at_same_offset() {
        let mut parser = InlineHiddenTagParser::new(vec![
            InlineTagSpec {
                tag: Tag::A,
                open: "<a>",
                close: "</a>",
            },
            InlineTagSpec {
                tag: Tag::B,
                open: "<ab>",
                close: "</ab>",
            },
        ]);

        let out = collect_chunks(&mut parser, &["x<ab>y</ab>z"]);

        assert_eq!(out.visible_text, "xz");
        assert_eq!(out.extracted.len(), 1);
        assert_eq!(out.extracted[0].tag, Tag::B);
        assert_eq!(out.extracted[0].content, "y");
    }

    #[test]
    #[should_panic(expected = "non-empty open delimiters")]
    fn generic_inline_parser_rejects_empty_open_delimiter() {
        let _ = InlineHiddenTagParser::new(vec![InlineTagSpec {
            tag: Tag::A,
            open: "",
            close: "</a>",
        }]);
    }

    #[test]
    #[should_panic(expected = "non-empty close delimiters")]
    fn generic_inline_parser_rejects_empty_close_delimiter() {
        let _ = InlineHiddenTagParser::new(vec![InlineTagSpec {
            tag: Tag::A,
            open: "<a>",
            close: "",
        }]);
    }

    #[test]
    fn streaming_mode_emits_deltas_incrementally() {
        let mut parser = InlineHiddenTagParser::new_streaming(vec![InlineTagSpec {
            tag: (),
            open: "<think>",
            close: "</think>",
        }]);

        let first = parser.push_str("a<think>reason");
        assert_eq!(first.visible_text, "a");
        assert_eq!(first.extracted.len(), 1);
        assert_eq!(first.extracted[0].content, "reason");

        let second = parser.push_str("ing</think>b");
        assert_eq!(second.visible_text, "b");
        assert_eq!(second.extracted.len(), 1);
        assert_eq!(second.extracted[0].content, "ing");

        let tail = parser.finish();
        assert_eq!(tail.visible_text, "");
        assert!(tail.extracted.is_empty());
    }

    #[test]
    fn streaming_mode_handles_tag_split_across_chunks() {
        let mut parser = InlineHiddenTagParser::new_streaming(vec![InlineTagSpec {
            tag: (),
            open: "<think>",
            close: "</think>",
        }]);

        let out = collect_chunks(&mut parser, &["a<th", "ink>x", "y</th", "ink>b"]);
        assert_eq!(out.visible_text, "ab");
        let reasoning: String = out.extracted.iter().map(|e| e.content.as_str()).collect();
        assert_eq!(reasoning, "xy");
    }

    #[test]
    fn streaming_mode_holds_close_tag_prefix_at_chunk_boundary() {
        let mut parser = InlineHiddenTagParser::new_streaming(vec![InlineTagSpec {
            tag: (),
            open: "<think>",
            close: "</think>",
        }]);

        // "</th" could be the start of "</think>", so it must not leak into
        // the extracted deltas until disambiguated.
        let first = parser.push_str("<think>x</th");
        let reasoning: String = first.extracted.iter().map(|e| e.content.as_str()).collect();
        assert_eq!(reasoning, "x");

        // It was a false alarm: "</thud" is tag content after all.
        let second = parser.push_str("ud y");
        let reasoning: String = second.extracted.iter().map(|e| e.content.as_str()).collect();
        assert_eq!(reasoning, "</thud y");

        let out = parser.push_str("</think>done");
        assert_eq!(out.visible_text, "done");
    }

    #[test]
    fn streaming_mode_flushes_unclosed_tag_at_eof() {
        let mut parser = InlineHiddenTagParser::new_streaming(vec![InlineTagSpec {
            tag: (),
            open: "<think>",
            close: "</think>",
        }]);

        let first = parser.push_str("<think>partial");
        let reasoning: String = first.extracted.iter().map(|e| e.content.as_str()).collect();
        assert_eq!(reasoning, "partial");

        let tail = parser.finish();
        assert!(tail.visible_text.is_empty());
        assert!(tail.extracted.is_empty());
    }
}

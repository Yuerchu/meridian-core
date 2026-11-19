//! Resource guards before body collection and before the existing SSE parser.
//! Framing is left to eventsource-stream. Only its broken initial UTF-8 BOM
//! handling is shimmed: the optional three-byte marker is removed once.
use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

use super::TransportError;
use super::transport::ByteStream;

pub(crate) const BODY_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const ERROR_BYTES: usize = 64 * 1024;
pub(crate) const MCP_LINE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MCP_TOTAL_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(crate) struct SseLimits {
    pub line: usize,
    pub event: usize,
    pub total: usize,
}

pub(crate) const SSE_LIMITS: SseLimits = SseLimits {
    line: 8 * 1024 * 1024,
    event: 16 * 1024 * 1024,
    total: 128 * 1024 * 1024,
};

pub(crate) async fn collect<S>(stream: S, limit: usize) -> Result<Bytes, TransportError>
where
    S: Stream<Item = Result<Bytes, TransportError>>,
{
    futures::pin_mut!(stream);
    let mut body = BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(TransportError::Limit("response body"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

pub(crate) async fn body(response: reqwest::Response, limit: usize) -> Result<Bytes, TransportError> {
    if response.content_length().is_some_and(|length| length > limit as u64) {
        return Err(TransportError::Limit("response body"));
    }
    collect(
        response
            .bytes_stream()
            .map(|r| r.map_err(super::ReqwestTransport::map_error)),
        limit,
    )
    .await
}

/// An HTTP error status is authoritative even if its optional diagnostic body
/// is oversized, truncated, or unreadable. Never replace it with a body error.
pub(crate) async fn error_body(response: reqwest::Response, limit: usize) -> Option<String> {
    body(response, limit)
        .await
        .ok()
        .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok())
}

#[derive(Default)]
struct SseBudget {
    total: usize,
    event: usize,
    line: usize,
    after_cr: bool,
    pending_reset: bool,
}

impl SseBudget {
    fn check(&mut self, bytes: &[u8], limits: SseLimits) -> Result<(), TransportError> {
        if bytes.len() > limits.total.saturating_sub(self.total) {
            return Err(TransportError::Limit("SSE total"));
        }
        self.total += bytes.len();
        for &byte in bytes {
            // A blank CR ends the event, but a following LF belongs to that
            // same delimiter. Defer the reset across a possible chunk boundary.
            if self.pending_reset && byte != b'\n' {
                self.event = 0;
                self.pending_reset = false;
            }
            if self.event == limits.event {
                return Err(TransportError::Limit("SSE event"));
            }
            self.event += 1;
            if self.after_cr && byte == b'\n' {
                self.after_cr = false;
                if self.pending_reset {
                    self.event = 0;
                    self.pending_reset = false;
                }
                continue;
            }
            self.after_cr = byte == b'\r';
            if byte == b'\r' || byte == b'\n' {
                if self.line == 0 {
                    if byte == b'\r' {
                        self.pending_reset = true;
                    } else {
                        self.event = 0;
                    }
                }
                self.line = 0;
            } else {
                if self.line == limits.line {
                    return Err(TransportError::Limit("SSE line"));
                }
                self.line += 1;
            }
        }
        Ok(())
    }
}

/// Fail once and terminate. A rejected chunk is never handed to eventsource,
/// so neither an unterminated line nor many small data lines can grow its buffer.
pub(crate) fn sse(stream: ByteStream, limits: SseLimits) -> ByteStream {
    Box::pin(async_stream::try_stream! {
        let mut stream = stream;
        let mut budget = SseBudget::default();
        let mut prefix = Vec::with_capacity(3);
        let mut started = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            budget.check(&chunk, limits)?;
            if started {
                yield chunk;
                continue;
            }
            let mut consumed = 0;
            for &byte in chunk.iter() {
                prefix.push(byte);
                consumed += 1;
                if !b"\xef\xbb\xbf".starts_with(&prefix) || prefix.len() == 3 {
                    started = true;
                    break;
                }
            }
            if started {
                if prefix != b"\xef\xbb\xbf" {
                    yield Bytes::from(std::mem::take(&mut prefix));
                } else {
                    // eventsource-stream 0.2.3 slices a leading BOM at byte 1.
                    // Prime its decoder with a no-op comment, so a second BOM
                    // (legitimate content, not another marker) cannot panic it.
                    yield Bytes::from_static(b":\n");
                }
                if consumed < chunk.len() {
                    yield chunk.slice(consumed..);
                }
            }
        }
        if !started && !prefix.is_empty() {
            yield Bytes::from(prefix);
        }
    })
}

/// Bounded newline framing for MCP. On error the caller must retire the pipe;
/// continuing would treat the remainder of an oversized frame as a new one.
pub(crate) async fn line<R: AsyncBufRead + Unpin>(reader: &mut R, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    loop {
        let input = reader.fill_buf().await?;
        if input.is_empty() {
            return Ok(output);
        }
        let take = input.iter().position(|&b| b == b'\n').map_or(input.len(), |i| i + 1);
        if take > limit.saturating_sub(output.len()) {
            return Err(std::io::Error::other("MCP line limit exceeded"));
        }
        let ended = input[take - 1] == b'\n';
        output.extend_from_slice(&input[..take]);
        reader.consume(take);
        if ended {
            return Ok(output);
        }
    }
}

/// Account every stdout frame, including notifications and non-JSON noise.
pub(crate) async fn frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line_limit: usize,
    remaining: &mut usize,
) -> std::io::Result<Vec<u8>> {
    let bytes = line(reader, line_limit.min(*remaining)).await?;
    *remaining -= bytes.len();
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eventsource_stream::Eventsource;

    fn chunks(parts: Vec<Vec<u8>>) -> ByteStream {
        Box::pin(futures::stream::iter(parts.into_iter().map(|b| Ok(Bytes::from(b)))))
    }

    #[tokio::test]
    async fn collection_checks_actual_chunks_at_the_boundary() {
        assert_eq!(collect(chunks(vec![vec![1; 3], vec![2; 2]]), 5).await.unwrap().len(), 5);
        assert!(matches!(
            collect(chunks(vec![vec![1; 3], vec![2; 3]]), 5).await,
            Err(TransportError::Limit(_))
        ));
    }

    async fn response(wire: &'static [u8]) -> reqwest::Response {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 2048];
            let _ = socket.read(&mut request).await.unwrap();
            socket.write_all(wire).await.unwrap();
        });
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}"))
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn optional_error_body_discards_overflow_and_truncation() {
        for wire in [
            &b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 6\r\n\r\nabcdef"[..],
            &b"HTTP/1.1 503 Unavailable\r\nContent-Length: 5\r\n\r\nab"[..],
        ] {
            assert!(error_body(response(wire).await, 5).await.is_none());
        }
        assert_eq!(
            error_body(
                response(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 3\r\n\r\nabc").await,
                5
            )
            .await
            .as_deref(),
            Some("abc")
        );
    }

    #[tokio::test]
    async fn fixed_and_chunked_http_bodies_have_the_same_small_budget() {
        for wire in [
            &b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nabcdef"[..],
            &b"HTTP/1.1 400 Bad Request\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n3\r\ndef\r\n0\r\n\r\n"[..],
            // A claimed length alone is enough to refuse, even with no body.
            &b"HTTP/1.1 200 OK\r\nContent-Length: 999\r\n\r\n"[..],
        ] {
            assert!(matches!(
                body(response(wire).await, 5).await,
                Err(TransportError::Limit(_))
            ));
        }
        let exact = response(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabcde").await;
        assert_eq!(body(exact, 5).await.unwrap(), &b"abcde"[..]);
    }

    #[tokio::test]
    async fn sse_guards_each_budget_before_parsing() {
        for (wire, limits, expected) in [
            (
                "data: abc",
                SseLimits {
                    line: 8,
                    event: 30,
                    total: 40,
                },
                "SSE line",
            ),
            (
                "data:a\ndata:b\n",
                SseLimits {
                    line: 8,
                    event: 12,
                    total: 40,
                },
                "SSE event",
            ),
            (
                ":\n\n:\n\n",
                SseLimits {
                    line: 8,
                    event: 12,
                    total: 5,
                },
                "SSE total",
            ),
        ] {
            let mut guarded = sse(chunks(wire.bytes().map(|b| vec![b]).collect()), limits);
            let mut error = None;
            while let Some(item) = guarded.next().await {
                if let Err(TransportError::Limit(kind)) = item {
                    error = Some(kind);
                }
            }
            assert_eq!(error, Some(expected));
        }
    }

    #[tokio::test]
    async fn sse_parser_semantics_survive_every_chunk_split() {
        let wire = "\u{feff}data: 你好\r\n:comment\r\nid: 7\r\nevent: custom\r\ndata: second\r\n\r\ndata: next\r\rdata: last\n\n";
        let expected = vec![
            ("你好\nsecond".to_string(), "7".to_string(), "custom".to_string()),
            ("next".to_string(), "7".to_string(), "message".to_string()),
            ("last".to_string(), "7".to_string(), "message".to_string()),
        ];
        for split in 0..=wire.len() {
            let raw = chunks(vec![
                wire.as_bytes()[..split].to_vec(),
                wire.as_bytes()[split..].to_vec(),
            ]);
            let actual: Vec<_> = sse(
                raw,
                SseLimits {
                    line: 32,
                    event: 128,
                    total: 256,
                },
            )
            .eventsource()
            .collect()
            .await;
            let actual: Vec<_> = actual
                .into_iter()
                .map(|event| {
                    let event = event.expect("valid SSE must parse");
                    (event.data, event.id, event.event)
                })
                .collect();
            assert_eq!(actual, expected, "split {split}");
        }
    }

    #[tokio::test]
    async fn only_one_initial_bom_is_removed_without_parser_panics() {
        let wire = "\u{feff}\u{feff}data: ignored\ndata: retained\n\n";
        for split in 0..=wire.len() {
            let raw = chunks(vec![
                wire.as_bytes()[..split].to_vec(),
                wire.as_bytes()[split..].to_vec(),
            ]);
            let events: Vec<_> = sse(raw, SSE_LIMITS).eventsource().collect().await;
            assert_eq!(events.len(), 1, "split {split}");
            assert_eq!(events[0].as_ref().unwrap().data, "retained", "split {split}");
        }
        for incomplete in [vec![0xef], vec![0xef, 0xbb]] {
            let raw = chunks(vec![incomplete.clone()]);
            assert_eq!(collect(sse(raw, SSE_LIMITS), 3).await.unwrap(), Bytes::from(incomplete));
        }
    }

    #[tokio::test]
    async fn stdio_total_budget_includes_skipped_frames() {
        let mut reader = tokio::io::BufReader::with_capacity(2, &b"noise\n\n{}\n"[..]);
        let mut remaining = 9;
        assert_eq!(frame(&mut reader, 6, &mut remaining).await.unwrap(), b"noise\n");
        assert_eq!(frame(&mut reader, 6, &mut remaining).await.unwrap(), b"\n");
        assert_eq!(remaining, 2);
        assert!(frame(&mut reader, 6, &mut remaining).await.is_err());
    }

    #[tokio::test]
    async fn crlf_event_boundary_is_exact_and_limit_errors_are_terminal() {
        let wire = b"data:a\r\n\r\ndata:b\r\n\r\n";
        let limits = SseLimits {
            line: 6,
            event: 10,
            total: 20,
        };
        let results: Vec<_> = sse(chunks(wire.iter().map(|&b| vec![b]).collect()), limits)
            .eventsource()
            .collect()
            .await;
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(Result::is_ok));
        let mut guarded = sse(chunks(vec![b"1234567".to_vec(), b"data:a\n\n".to_vec()]), limits);
        assert!(matches!(
            guarded.next().await,
            Some(Err(TransportError::Limit("SSE line")))
        ));
        assert!(guarded.next().await.is_none());
    }

    #[tokio::test]
    async fn bounded_lines_accept_exact_limit_and_reject_unterminated_input() {
        let mut reader = tokio::io::BufReader::with_capacity(2, &b"abc\nnext"[..]);
        assert_eq!(line(&mut reader, 4).await.unwrap(), b"abc\n");
        assert!(line(&mut reader, 3).await.is_err());
    }
}

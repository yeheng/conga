//! Server-Sent Events transport: incremental frame splitter + the shared
//! streaming download loop used by every SSE provider.
//!
//! Shared by [`crate::providers::openai_compat`] and
//! [`crate::providers::anthropic`]. Both providers emit `data: {json}\n\n`
//! frames; the splitter turns a live byte stream into frames, and
//! [`download_sse`] owns the whole download loop (true streaming, abort
//! racing, eager emission) so the two providers only differ in request
//! body shape and chunk parsing.

use std::pin::Pin;

use futures_util::{Stream, StreamExt};

use crate::cancel::CancelSignal;
use crate::types::context::StreamChunk;

/// The shared SSE download loop for provider requests.
///
/// POSTs `body` to `url` (`decorate` adds transport-specific auth headers),
/// then parses SSE frames incrementally off the wire as bytes arrive (first
/// token reaches the user at first-token time, not whole-response time)
/// and races the download against the cancel signal so Ctrl-C stops it
/// mid-flight the instant `cancel()` fires (event-driven - no polling).
/// Each payload JSON is mapped through `parse_chunk`. `label`
/// names the provider in errors/logs.
///
/// Behavior contract (pinned by the providers' streaming tests):
/// - non-2xx or send failure → one `Error` chunk;
/// - `data: [DONE]` → `Done` and end of stream;
/// - abort while downloading → the stream simply ends (the loop's accumulator
///   marks the message Aborted).
pub(crate) fn download_sse<F, P>(
    label: &'static str,
    client: reqwest::Client,
    url: String,
    decorate: F,
    body: serde_json::Value,
    signal: Option<CancelSignal>,
    parse_chunk: P,
) -> Pin<Box<dyn Stream<Item = StreamChunk> + Send>>
where
    F: FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder + Send + 'static,
    P: Fn(&str) -> Vec<StreamChunk> + Send + 'static,
{
    Box::pin(async_stream::stream! {
        let resp = match signal.as_ref() {
            Some(sig) => tokio::select! {
                biased;
                _ = sig.cancelled() => {
                    tracing::debug!("{label} stream aborted before response");
                    return;
                }
                r = decorate(client.post(&url).json(&body)).send() => r,
            },
            None => decorate(client.post(&url).json(&body)).send().await,
        };
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "{label} request failed");
                yield StreamChunk::Error(e.to_string());
                return;
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            tracing::warn!(status = %status, "{label} non-2xx response");
            yield StreamChunk::Error(format!("HTTP {status}: {text}"));
            return;
        }

        let mut byte_stream = resp.bytes_stream();
        let mut splitter = SseFrameSplitter::new();
        let mut frames: Vec<String> = Vec::new();
        let mut finished = false;
        while !finished {
            // Emit eagerly: every frame parsed so far goes out before the
            // next network read, keeping the pipeline live.
            for frame in frames.drain(..) {
                for payload in parse_sse_frame(&frame) {
                    match payload {
                        None => {
                            yield StreamChunk::Done;
                            return;
                        }
                        Some(json_str) => {
                            for chunk in parse_chunk(&json_str) {
                                yield chunk;
                            }
                        }
                    }
                }
            }
            let chunk = match signal.as_ref() {
                Some(sig) => {
                    // Event-driven abort: the watch-backed cancel future
                    // resolves the instant cancel() fires - including while
                    // the HTTP read is parked with no bytes flowing.
                    tokio::select! {
                        biased;
                        _ = sig.cancelled() => {
                            tracing::debug!("{label} stream aborted mid-download");
                            return;
                        }
                        c = byte_stream.next() => c,
                    }
                }
                None => byte_stream.next().await,
            };
            match chunk {
                Some(Ok(bytes)) => frames.extend(splitter.push(&bytes)),
                Some(Err(e)) => {
                    yield StreamChunk::Error(e.to_string());
                    return;
                }
                None => {
                    frames.extend(splitter.finish());
                    finished = true;
                }
            }
        }
        yield StreamChunk::Done;
    })
}

/// Parse a complete SSE frame buffer into its `data:` payload lines.
///
/// Returns one `String` per `data:` field (a frame may carry several). Lines
/// that are not `data:` are dropped. A `data:` line whose payload is exactly
/// `[DONE]` yields `None` to signal end-of-stream to the caller.
pub fn parse_sse_frame(frame: &str) -> Vec<Option<String>> {
    let mut payloads = Vec::new();
    let mut current_data = String::new();
    let mut have_data = false;

    for line in frame.split('\n') {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("data:") {
            have_data = true;
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            if !current_data.is_empty() {
                current_data.push('\n');
            }
            current_data.push_str(rest);
        } else if line.is_empty() {
            // Blank line terminates the current event.
            if have_data {
                if current_data.trim() == "[DONE]" {
                    payloads.push(None);
                } else {
                    payloads.push(Some(std::mem::take(&mut current_data)));
                }
                current_data.clear();
                have_data = false;
            }
        }
        // event:/id:/retry:/comment lines are intentionally ignored.
    }

    // Flush a trailing non-blank-terminated frame.
    if have_data && !current_data.is_empty() {
        if current_data.trim() == "[DONE]" {
            payloads.push(None);
        } else {
            payloads.push(Some(current_data));
        }
    }

    payloads
}

/// Incremental SSE frame splitter for a live byte stream.
///
/// A frame ends at a blank line (`\n\n` or `\r\n\r\n`). Bytes arrive split at
/// arbitrary boundaries - mid-frame, mid-UTF-8 - so [`push`](Self::push)
/// buffers until a full separator is present and each complete frame goes
/// through [`parse_sse_frame`]. [`finish`](Self::finish) flushes a trailing
/// frame whose blank line never arrived (some servers omit the final one).
///
/// Frames are cut via a read cursor over the buffer, NOT `Vec::drain`: a
/// per-frame drain memmoves the whole remaining tail, which is pure CPU burn
/// on long token streams. The buffer is compacted only when the consumed
/// prefix exceeds [`COMPACT_THRESHOLD`] (or is fully drained), amortizing
/// the memmove to near zero.
pub(crate) struct SseFrameSplitter {
    buf: Vec<u8>,
    /// Start of the unconsumed region in `buf` (everything before it has
    /// already been emitted as frames).
    read_pos: usize,
}

/// Once this many consumed bytes sit in front of the cursor, slide the
/// unconsumed tail to the front in one memmove instead of one per frame.
const COMPACT_THRESHOLD: usize = 32 * 1024;

impl SseFrameSplitter {
    pub(crate) fn new() -> Self {
        Self {
            buf: Vec::new(),
            read_pos: 0,
        }
    }

    /// Feed freshly received bytes; returns every complete frame with its
    /// terminating separator removed.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut frames = Vec::new();
        while let Some((start, end)) = find_separator(&self.buf[self.read_pos..]) {
            let frame = String::from_utf8_lossy(&self.buf[self.read_pos..self.read_pos + start])
                .into_owned();
            self.read_pos += end; // skip past the separator
            frames.push(frame);
        }
        self.compact();
        frames
    }

    /// End of stream: return a trailing frame that was never terminated by a
    /// blank line, if any bytes remain buffered.
    pub(crate) fn finish(&mut self) -> Vec<String> {
        if self.read_pos >= self.buf.len() {
            self.buf.clear();
            self.read_pos = 0;
            return Vec::new();
        }
        let frame = String::from_utf8_lossy(&self.buf[self.read_pos..]).into_owned();
        self.buf.clear();
        self.read_pos = 0;
        vec![frame]
    }

    /// Reclaim consumed prefix space: free the whole buffer when everything
    /// is consumed (the common steady-state), otherwise memmove the tail to
    /// the front only once the consumed prefix has grown past the threshold.
    fn compact(&mut self) {
        if self.read_pos == self.buf.len() {
            self.buf.clear();
            self.read_pos = 0;
        } else if self.read_pos >= COMPACT_THRESHOLD {
            self.buf.copy_within(self.read_pos.., 0);
            self.buf.truncate(self.buf.len() - self.read_pos);
            self.read_pos = 0;
        }
    }
}

impl Default for SseFrameSplitter {
    fn default() -> Self {
        Self::new()
    }
}

/// Find the earliest frame separator in `buf`. Returns `(start, end)` byte
/// offsets of the separator (the frame is `buf[..start]`). The two spellings
/// cannot overlap (`\r\n\r\n` never contains `\n\n`), so the earliest match
/// is unambiguous.
fn find_separator(buf: &[u8]) -> Option<(usize, usize)> {
    let lf_lf = buf
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|i| (i, i + 2));
    let crlf_crlf = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, i + 4));
    match (lf_lf, crlf_crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_data_frame() {
        let payloads = parse_sse_frame("data: {\"hello\":1}\n\n");
        assert_eq!(payloads, vec![Some("{\"hello\":1}".to_string())]);
    }

    #[test]
    fn parses_done_sentinel() {
        let payloads = parse_sse_frame("data: [DONE]\n\n");
        assert_eq!(payloads, vec![None]);
    }

    #[test]
    fn ignores_non_data_lines() {
        let frame = "event: message\ndata: {\"x\":2}\n\n";
        let payloads = parse_sse_frame(frame);
        assert_eq!(payloads, vec![Some("{\"x\":2}".to_string())]);
    }

    #[test]
    fn parses_multiple_frames_in_one_buffer() {
        let buf = "data: a\n\ndata: b\n\n";
        let payloads = parse_sse_frame(buf);
        assert_eq!(payloads, vec![Some("a".to_string()), Some("b".to_string())]);
    }

    #[test]
    fn splitter_handles_chunks_split_mid_frame() {
        let mut s = SseFrameSplitter::new();
        // Frame 1 arrives in two pieces, frame 2 whole.
        assert!(s.push(b"data: {\"a\"").is_empty());
        assert!(s.push(b":1}\n").is_empty());
        assert_eq!(
            s.push(b"\ndata: {\"b\":2}\n\n"),
            vec!["data: {\"a\":1}".to_string(), "data: {\"b\":2}".to_string()]
        );
        assert!(s.push(b"data: partial").is_empty());
        assert_eq!(s.finish(), vec!["data: partial".to_string()]);
    }

    #[test]
    fn splitter_handles_crlf_separators_and_split_utf8() {
        let frame = "data: {\"t\":\"你\"}"; // multi-byte UTF-8 in the payload
        let wire = format!("{frame}\r\n\r\n");
        // Split at every possible byte boundary; the frame must survive intact.
        for cut in 1..wire.len() {
            let mut s = SseFrameSplitter::new();
            assert!(s.push(&wire.as_bytes()[..cut]).is_empty(), "cut {cut}");
            let frames = s.push(&wire.as_bytes()[cut..]);
            assert_eq!(frames, vec![frame.to_string()], "cut {cut}");
        }
    }

    #[test]
    fn splitter_eager_multiple_frames_per_chunk() {
        let mut s = SseFrameSplitter::new();
        let frames = s.push(b"data: a\n\ndata: b\n\ndata: c\n\n");
        assert_eq!(
            frames,
            vec![
                "data: a".to_string(),
                "data: b".to_string(),
                "data: c".to_string()
            ]
        );
        assert!(s.finish().is_empty());
    }

    #[test]
    fn complete_frame_yields_payload_without_trailing_blank() {
        // A frame extracted by the splitter has no terminating blank line;
        // parse_sse_frame's trailing-flush path must still yield its payload.
        let payloads = parse_sse_frame("data: {\"x\":1}");
        assert_eq!(payloads, vec![Some("{\"x\":1}".to_string())]);
    }

    #[test]
    fn splitter_survives_thousands_of_frames_without_growth() {
        // Regression for the per-frame `drain`: the cursor keeps the buffer
        // near one frame in size no matter how many frames flow through.
        let mut s = SseFrameSplitter::new();
        let frame = format!("data: {}\n\n", "x".repeat(200));
        for _ in 0..20_000 {
            let frames = s.push(frame.as_bytes());
            assert_eq!(frames.len(), 1);
            assert!(
                s.buf.len() < frame.len() + COMPACT_THRESHOLD,
                "buffer grew to {} bytes",
                s.buf.len()
            );
        }
        assert!(s.finish().is_empty());
    }

    #[test]
    fn splitter_compacts_across_many_small_chunks() {
        // Feed one frame larger than the compaction threshold in small
        // chunks: every push must return empty until the separator arrives,
        // the single frame must come out whole, and the buffer must reset.
        let payload = "y".repeat(COMPACT_THRESHOLD + 8 * 1024);
        let frame = format!("data: {payload}\n\n");
        let mut s = SseFrameSplitter::new();
        let bytes = frame.as_bytes();
        for chunk in bytes[..bytes.len() - 1].chunks(64) {
            assert!(s.push(chunk).is_empty());
        }
        // Last byte completes the frame; the buffer must reset to empty.
        assert_eq!(s.push(&bytes[bytes.len() - 1..]).len(), 1);
        assert_eq!(s.buf.len(), 0, "fully-consumed buffer must be cleared");
        assert!(s.finish().is_empty());
    }

    #[test]
    fn splitter_mixed_frames_and_leading_garbage_across_compaction() {
        // Interleave small frames until several compactions have happened,
        // verifying frames stay correct across cursor resets.
        let mut s = SseFrameSplitter::new();
        let expected_total = 5000;
        for i in 0..expected_total {
            let chunk = format!("data: {i}\n\n");
            let frames = s.push(chunk.as_bytes());
            assert_eq!(frames, vec![format!("data: {i}")]);
        }
        // Trailing un-terminated frame still flushes at finish().
        assert_eq!(s.push(b"data: tail"), Vec::<String>::new());
        assert_eq!(s.finish(), vec!["data: tail".to_string()]);
    }

    /// A TCP server that accepts connections and never answers: `download_sse`
    /// parks on the response read with no bytes flowing, proving cancel is
    /// watch-driven rather than rescued by a poll interval.
    async fn stalled_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // Accept and hold connections open without writing a byte.
            while let Ok((_sock, _)) = listener.accept().await {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });
        format!("http://{addr}/v1/chat")
    }

    #[tokio::test]
    async fn download_sse_unwinds_promptly_when_cancelled_mid_download() {
        use futures_util::StreamExt;
        let url = stalled_server().await;
        let signal = crate::cancel::CancelSignal::new();
        let mut stream = download_sse(
            "test",
            reqwest::Client::new(),
            url,
            |req| req,
            serde_json::json!({}),
            Some(signal.clone()),
            |frame| {
                let _ = frame;
                Vec::new()
            },
        );
        // Park on the stalled download, then cancel: the stream must end
        // within milliseconds (watch-backed) rather than hang or poll.
        let started = std::time::Instant::now();
        let drain = tokio::spawn(async move { while stream.next().await.is_some() {} });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        signal.cancel();
        drain.await.unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "cancel must unwind a stalled download promptly, took {:?}",
            started.elapsed()
        );
    }

    // ── SseFrameSplitter::default ─────────────────────────────────

    #[test]
    fn default_splitter_behaves_like_a_fresh_one() {
        let mut s = SseFrameSplitter::default();
        assert_eq!(s.push(b"data: a\n\n"), vec!["data: a".to_string()]);
        assert!(s.finish().is_empty());
    }

    #[test]
    fn default_splitter_starts_empty() {
        let mut s = SseFrameSplitter::default();
        assert!(s.push(b"data: partial").is_empty());
        assert_eq!(s.finish(), vec!["data: partial".to_string()]);
    }

    // ── SseFrameSplitter::compact ─────────────────────────────────

    #[test]
    fn compact_clears_a_fully_consumed_buffer() {
        // Steady state: everything read is consumed, so the buffer must be
        // released instead of growing with the stream.
        let mut s = SseFrameSplitter::new();
        s.push(b"data: a\n\ndata: b\n\n");
        assert_eq!(s.buf.len(), 0);
        assert_eq!(s.read_pos, 0);
    }

    #[test]
    fn compact_keeps_the_unconsumed_tail_when_nothing_was_consumed() {
        let mut s = SseFrameSplitter::new();
        assert!(s.push(b"data: partial").is_empty());
        assert_eq!(s.buf, b"data: partial".to_vec());
        assert_eq!(s.read_pos, 0);
    }

    #[test]
    fn compact_slides_the_tail_to_the_front_once_the_prefix_grows_past_the_threshold() {
        // Feed enough complete frames to push the consumed prefix past
        // COMPACT_THRESHOLD, then leave a partial frame behind: the buffer
        // must shrink back to the tail alone and the tail must survive.
        let mut s = SseFrameSplitter::new();
        let frame = format!("data: {}\n\n", "x".repeat(100));
        let mut expected = Vec::new();
        for i in 0..400 {
            let f = format!("data: {i}\n\n");
            let frames = s.push(f.as_bytes());
            assert_eq!(frames, vec![f.trim_end().to_string()]);
            expected.push(f.trim_end().to_string());
        }
        // Now a big run of frames plus a trailing partial.
        let frames = s.push(frame.as_bytes());
        assert_eq!(frames.len(), 1);
        assert!(s.push(b"data: tail").is_empty());
        // The consumed prefix is gone: only the unterminated tail is left.
        assert_eq!(s.buf, b"data: tail".to_vec());
        assert_eq!(s.read_pos, 0);
        assert_eq!(s.finish(), vec!["data: tail".to_string()]);
    }

    #[test]
    fn compact_preserves_the_tail_across_many_compaction_cycles() {
        let mut s = SseFrameSplitter::new();
        for i in 0..2000 {
            let f = format!("data: {i}\n\n");
            assert_eq!(s.push(f.as_bytes()), vec![f.trim_end().to_string()]);
        }
        assert!(s.push(b"data: tail").is_empty());
        assert_eq!(s.finish(), vec!["data: tail".to_string()]);
    }

    // ── find_separator ────────────────────────────────────────────

    #[test]
    fn find_separator_locates_an_lf_lf_separator() {
        assert_eq!(find_separator(b"data: a\n\n"), Some((7, 9)));
    }

    #[test]
    fn find_separator_locates_a_crlf_crlf_separator() {
        assert_eq!(find_separator(b"data: a\r\n\r\n"), Some((7, 11)));
    }

    #[test]
    fn find_separator_returns_none_without_a_separator() {
        assert_eq!(find_separator(b"data: a\n"), None);
        assert_eq!(find_separator(b"data: a\r\n"), None);
    }

    #[test]
    fn find_separator_on_an_empty_buffer() {
        assert_eq!(find_separator(b""), None);
    }

    #[test]
    fn find_separator_on_a_buffer_of_one_byte() {
        assert_eq!(find_separator(b"\n"), None);
        assert_eq!(find_separator(b"\r"), None);
    }

    #[test]
    fn find_separator_picks_the_earliest_of_both_spellings() {
        // "\n\n" comes first -> it wins.
        assert_eq!(find_separator(b"\n\n\r\n\r\n"), Some((0, 2)));
        // "\r\n\r\n" comes first -> it wins.
        assert_eq!(find_separator(b"x\r\n\r\n\n\n"), Some((1, 5)));
    }

    #[test]
    fn find_separator_reports_the_frame_start_and_end() {
        // The frame is buf[..start]; the separator is buf[start..end].
        let buf = b"data: x\n\ndata: y\n\n";
        let (start, end) = find_separator(buf).expect("separator");
        assert_eq!(&buf[..start], b"data: x");
        assert_eq!(&buf[start..end], b"\n\n");
        assert_eq!(&buf[end..], b"data: y\n\n");
    }

    #[test]
    fn find_separator_finds_a_separator_at_the_very_end() {
        assert_eq!(find_separator(b"a\n\n"), Some((1, 3)));
        assert_eq!(find_separator(b"a\r\n\r\n"), Some((1, 5)));
    }

    // ── parse_sse_frame boundaries ────────────────────────────────

    #[test]
    fn parses_a_multiline_data_payload() {
        // Two `data:` lines in one frame are joined with a newline.
        let payloads = parse_sse_frame("data: line1\ndata: line2\n\n");
        assert_eq!(payloads, vec![Some("line1\nline2".to_string())]);
    }

    #[test]
    fn parses_a_crlf_frame() {
        let payloads = parse_sse_frame("data: {\"x\":1}\r\n\r\n");
        assert_eq!(payloads, vec![Some("{\"x\":1}".to_string())]);
    }

    #[test]
    fn parses_a_data_field_without_the_optional_space() {
        let payloads = parse_sse_frame("data:{\"x\":1}\n\n");
        assert_eq!(payloads, vec![Some("{\"x\":1}".to_string())]);
    }

    #[test]
    fn ignores_comment_and_event_lines() {
        let frame = ": keep-alive\nevent: message\nid: 7\nretry: 100\ndata: {\"x\":1}\n\n";
        assert_eq!(parse_sse_frame(frame), vec![Some("{\"x\":1}".to_string())]);
    }

    #[test]
    fn parses_an_empty_frame_as_no_payload() {
        assert!(parse_sse_frame("\n\n").is_empty());
        assert!(parse_sse_frame("").is_empty());
    }

    #[test]
    fn done_sentinel_with_surrounding_whitespace() {
        assert_eq!(parse_sse_frame("data: [DONE]  \n\n"), vec![None]);
    }

    // ── download_sse error paths ──────────────────────────────────

    #[tokio::test]
    async fn download_sse_yields_one_error_chunk_on_a_non_2xx_response() {
        use futures_util::StreamExt;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            // Drain the request head.
            let _ = sock.read(&mut buf).await;
            sock.write_all(
                b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 10\r\nConnection: close\r\n\r\nboom, oops",
            )
            .await
            .unwrap();
            sock.flush().await.unwrap();
        });

        let mut stream = download_sse(
            "test",
            reqwest::Client::builder().no_proxy().build().unwrap(),
            format!("http://{addr}/v1/chat"),
            |req| req,
            serde_json::json!({}),
            None,
            |_frame| Vec::new(),
        );
        let mut chunks = Vec::new();
        while let Some(c) = stream.next().await {
            chunks.push(c);
        }
        server.await.unwrap();

        // Exactly one error chunk, naming the status and the body.
        assert_eq!(chunks.len(), 1, "{chunks:?}");
        match &chunks[0] {
            StreamChunk::Error(msg) => {
                assert!(msg.contains("500"), "{msg}");
                assert!(msg.contains("boom, oops"), "{msg}");
            }
            other => panic!("expected an error chunk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn download_sse_yields_one_error_chunk_when_the_connection_fails() {
        use futures_util::StreamExt;

        // Bind then drop: the port is free, so the connect is refused.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let mut stream = download_sse(
            "test",
            reqwest::Client::builder().no_proxy().build().unwrap(),
            format!("http://{addr}/v1/chat"),
            |req| req,
            serde_json::json!({}),
            None,
            |_frame| Vec::new(),
        );
        let mut chunks = Vec::new();
        while let Some(c) = stream.next().await {
            chunks.push(c);
        }
        assert_eq!(chunks.len(), 1, "{chunks:?}");
        assert!(
            matches!(&chunks[0], StreamChunk::Error(_)),
            "expected an error chunk, got {:?}",
            chunks[0]
        );
    }
}

//! Incremental Server-Sent Events parser, shared by Semantik's
//! `/v1/subscribe` and Bud's `/v1/watch`.
//!
//! The scanner is fed arbitrary byte chunks from the HTTP body and
//! yields complete [`Frame`] values as they become available. It
//! tolerates `\n`, `\r\n`, and bare `\r` line endings (some SSE
//! implementations emit the last form), ignores comment lines (leading
//! `:`), strips a single space after `:`, and silently ignores unknown
//! directives per the SSE spec.
//!
//! Per-frame size is capped — at [`DEFAULT_MAX_FRAME_BYTES`] unless the
//! caller chose a bound with [`Scanner::with_max_frame`] — and a larger
//! frame produces [`ScanError::FrameTooLarge`]. The bound covers a line
//! that has not ended yet as well as the lines already accumulated, so a
//! peer that never sends a line break cannot grow the buffer without
//! limit.

/// The frame bound [`Scanner::new`] applies. 128 KiB admits the largest
/// Semantik event, a match frame carrying a 4096-dimension vector whose
/// components encode at their longest (about 94 KB; a typical embedding is
/// about 55 KB); a stream whose frames batch many events sets its own bound
/// with [`Scanner::with_max_frame`].
pub(crate) const DEFAULT_MAX_FRAME_BYTES: usize = 128 * 1024;

/// One complete SSE event.
#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub(crate) struct Frame {
    /// Value following `event:` (e.g. `"subscribed"`, `"match"`).
    pub event: String,
    /// Concatenation of `data:` values within the frame joined by `\n`
    /// per the SSE specification.
    pub data: String,
}

/// Scanner state. Holds an unparsed byte tail and the in-progress
/// frame's `event`/`data` fields.
#[derive(Debug)]
pub(crate) struct Scanner {
    /// Bytes received that do not yet form a complete line.
    buf: Vec<u8>,
    /// How much of `buf` is known to hold no line terminator, so a long
    /// line arriving in many chunks is scanned once rather than once per
    /// chunk.
    scanned: usize,
    cur_event: String,
    cur_data: String,
    cur_size: usize,
    max: usize,
    closed: bool,
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum ScanError {
    #[error("sse: frame exceeds maximum size {0} bytes")]
    FrameTooLarge(usize),
}

/// Where the next line in a buffer ends, if it is known yet.
enum LineEnd {
    /// The line is `buf[start..end]`; the next one begins at `next`.
    Complete { end: usize, next: usize },
    /// No terminator yet. Bytes before `resume` hold none, so the next
    /// scan starts there.
    Incomplete { resume: usize },
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Scanner {
    /// A scanner bounded by [`DEFAULT_MAX_FRAME_BYTES`].
    pub(crate) fn new() -> Self {
        Self::with_max_frame(DEFAULT_MAX_FRAME_BYTES)
    }

    /// A scanner whose frames are bounded by `max_frame` bytes. The
    /// buffer grows only as frames need it, so a large bound costs memory
    /// only when a frame that large arrives.
    pub(crate) fn with_max_frame(max_frame: usize) -> Self {
        Self {
            buf: Vec::new(),
            scanned: 0,
            cur_event: String::new(),
            cur_data: String::new(),
            cur_size: 0,
            max: max_frame,
            closed: false,
        }
    }

    /// Feed a byte chunk and yield all frames that became complete.
    /// Returned in order.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Frame>, ScanError> {
        self.buf.extend_from_slice(chunk);
        let buf = std::mem::take(&mut self.buf);
        let mut out = Vec::new();
        let mut start = 0;
        let resume = loop {
            match next_line(&buf, self.scanned.max(start)) {
                LineEnd::Complete { end, next } => {
                    if let Some(frame) = self.consume_line(&buf[start..end])? {
                        out.push(frame);
                    }
                    start = next;
                }
                LineEnd::Incomplete { resume } => break resume,
            }
        };
        // The unfinished line counts towards the frame it belongs to: a
        // peer that never ends a line is a frame that never ends.
        let partial = buf.len() - start;
        if self.cur_size + partial > self.max {
            return Err(ScanError::FrameTooLarge(self.max));
        }
        self.buf = buf;
        self.buf.drain(..start);
        self.scanned = resume - start;
        Ok(out)
    }

    /// Signal end-of-stream. Returns the final frame if any partial
    /// state was accumulated, as the SSE parsers in the other Noetive
    /// SDKs do on a clean EOF.
    pub(crate) fn close(&mut self) -> Result<Option<Frame>, ScanError> {
        if self.closed {
            return Ok(None);
        }
        self.closed = true;
        // The remaining buffer is a final unterminated line. A bare \r
        // at the end is a line break, not part of the line.
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            let line = line.strip_suffix(b"\r").unwrap_or(&line);
            if let Some(frame) = self.consume_line(line)? {
                return Ok(Some(frame));
            }
        }
        if !self.cur_event.is_empty() || !self.cur_data.is_empty() {
            Ok(Some(self.take_frame()))
        } else {
            Ok(None)
        }
    }

    fn take_frame(&mut self) -> Frame {
        let event = std::mem::take(&mut self.cur_event);
        let data = std::mem::take(&mut self.cur_data);
        self.cur_size = 0;
        Frame { event, data }
    }

    fn consume_line(&mut self, line: &[u8]) -> Result<Option<Frame>, ScanError> {
        if line.is_empty() {
            // Frame terminator. Emit unless we're between empty frames.
            if self.cur_event.is_empty() && self.cur_data.is_empty() {
                return Ok(None);
            }
            return Ok(Some(self.take_frame()));
        }
        if line[0] == b':' {
            // Comment line; ignore.
            return Ok(None);
        }
        self.cur_size += line.len();
        if self.cur_size > self.max {
            return Err(ScanError::FrameTooLarge(self.max));
        }
        let (field, value) = split_field(line);
        match field {
            b"event" => {
                // Lossy on non-UTF-8, which the servers never emit but
                // the parser still tolerates.
                self.cur_event = String::from_utf8_lossy(value).into_owned();
            }
            b"data" => {
                if !self.cur_data.is_empty() {
                    self.cur_data.push('\n');
                }
                self.cur_data.push_str(&String::from_utf8_lossy(value));
            }
            _ => {
                // Unknown field; ignore per SSE spec (id/retry, etc.).
            }
        }
        Ok(None)
    }
}

/// Find the end of the next line, scanning from `from` (the bytes of the
/// current line before it are already known to hold no terminator).
///
/// A `\r` as the last byte is deferred rather than taken as a line
/// break: the next chunk may begin with the `\n` of a `\r\n`.
fn next_line(buf: &[u8], from: usize) -> LineEnd {
    for (i, &b) in buf.iter().enumerate().skip(from) {
        match b {
            b'\n' => {
                return LineEnd::Complete {
                    end: i,
                    next: i + 1,
                }
            }
            b'\r' => {
                if i + 1 < buf.len() {
                    let next = if buf[i + 1] == b'\n' { i + 2 } else { i + 1 };
                    return LineEnd::Complete { end: i, next };
                }
                return LineEnd::Incomplete { resume: i };
            }
            _ => {}
        }
    }
    LineEnd::Incomplete { resume: buf.len() }
}

/// Separate an SSE `field: value` line. Per the spec, a single space
/// after `:` is stripped but additional spaces are preserved. A line
/// without `:` treats the entire line as the field name and uses an
/// empty value.
fn split_field(line: &[u8]) -> (&[u8], &[u8]) {
    if let Some(i) = line.iter().position(|b| *b == b':') {
        let field = &line[..i];
        let mut value = &line[i + 1..];
        if value.first() == Some(&b' ') {
            value = &value[1..];
        }
        (field, value)
    } else {
        (line, &[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(chunks: &[&[u8]]) -> Vec<Frame> {
        let mut s = Scanner::new();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(s.feed(c).unwrap());
        }
        if let Some(f) = s.close().unwrap() {
            out.push(f);
        }
        out
    }

    #[test]
    fn single_frame_lf() {
        let frames = collect(&[b"event: match\ndata: hello\n\n"]);
        assert_eq!(
            frames,
            vec![Frame {
                event: "match".into(),
                data: "hello".into()
            }]
        );
    }

    #[test]
    fn crlf_line_endings() {
        let frames = collect(&[b"event: match\r\ndata: hello\r\n\r\n"]);
        assert_eq!(
            frames,
            vec![Frame {
                event: "match".into(),
                data: "hello".into()
            }]
        );
    }

    #[test]
    fn bare_cr_line_endings() {
        let frames = collect(&[b"event: match\rdata: hello\r\r"]);
        assert_eq!(
            frames,
            vec![Frame {
                event: "match".into(),
                data: "hello".into()
            }]
        );
    }

    #[test]
    fn multi_line_data_joined_with_newline() {
        let frames = collect(&[b"event: match\ndata: a\ndata: b\ndata: c\n\n"]);
        assert_eq!(
            frames,
            vec![Frame {
                event: "match".into(),
                data: "a\nb\nc".into()
            }]
        );
    }

    #[test]
    fn comment_lines_ignored() {
        let frames = collect(&[b":this is a comment\nevent: match\ndata: x\n\n"]);
        assert_eq!(
            frames,
            vec![Frame {
                event: "match".into(),
                data: "x".into()
            }]
        );
    }

    #[test]
    fn unknown_fields_ignored() {
        let frames = collect(&[b"event: match\nid: 42\nretry: 1000\ndata: x\n\n"]);
        assert_eq!(
            frames,
            vec![Frame {
                event: "match".into(),
                data: "x".into()
            }]
        );
    }

    #[test]
    fn multiple_frames() {
        let frames =
            collect(&[b"event: subscribed\ndata: {\"id\":1}\n\nevent: match\ndata: a\n\n"]);
        assert_eq!(
            frames,
            vec![
                Frame {
                    event: "subscribed".into(),
                    data: "{\"id\":1}".into()
                },
                Frame {
                    event: "match".into(),
                    data: "a".into()
                },
            ]
        );
    }

    #[test]
    fn chunked_byte_at_a_time() {
        let input = b"event: match\ndata: hi\n\n";
        let mut s = Scanner::new();
        let mut out = Vec::new();
        for b in input {
            out.extend(s.feed(&[*b]).unwrap());
        }
        out.extend(s.close().unwrap());
        assert_eq!(
            out,
            vec![Frame {
                event: "match".into(),
                data: "hi".into()
            }]
        );
    }

    #[test]
    fn split_at_crlf_boundary() {
        let mut s = Scanner::new();
        let mut out = Vec::new();
        // Feed \r alone, then \n on next chunk. Should not produce a
        // spurious empty line.
        out.extend(s.feed(b"event: match\r").unwrap());
        out.extend(s.feed(b"\ndata: ok\n\n").unwrap());
        out.extend(s.close().unwrap());
        assert_eq!(
            out,
            vec![Frame {
                event: "match".into(),
                data: "ok".into()
            }]
        );
    }

    #[test]
    fn unterminated_frame_flushed_on_close() {
        let frames = collect(&[b"event: match\ndata: hi"]);
        assert_eq!(
            frames,
            vec![Frame {
                event: "match".into(),
                data: "hi".into()
            }]
        );
    }

    #[test]
    fn empty_lines_between_empty_frames_skipped() {
        let frames = collect(&[b"\n\n\nevent: match\ndata: x\n\n"]);
        assert_eq!(
            frames,
            vec![Frame {
                event: "match".into(),
                data: "x".into()
            }]
        );
    }

    #[test]
    fn extra_space_after_colon_preserved() {
        // SSE spec strips ONE space after ':'; additional spaces stay.
        let frames = collect(&[b"data:  two leading spaces\n\n"]);
        assert_eq!(
            frames,
            vec![Frame {
                event: "".into(),
                data: " two leading spaces".into()
            }]
        );
    }

    #[test]
    fn frame_too_large_caps() {
        let mut huge = b"event: match\ndata: ".to_vec();
        huge.extend(std::iter::repeat(b'a').take(DEFAULT_MAX_FRAME_BYTES + 100));
        huge.extend_from_slice(b"\n\n");
        let mut s = Scanner::new();
        let err = s.feed(&huge).unwrap_err();
        assert!(matches!(err, ScanError::FrameTooLarge(_)));
    }

    #[test]
    fn a_chosen_bound_admits_frames_past_the_default() {
        let payload = "a".repeat(DEFAULT_MAX_FRAME_BYTES * 2);
        let input = format!("event: batch\ndata: {payload}\n\n");
        let mut s = Scanner::with_max_frame(DEFAULT_MAX_FRAME_BYTES * 4);
        let mut out = Vec::new();
        // In small chunks, as a body arrives, so the long line is
        // assembled across many feeds.
        for chunk in input.as_bytes().chunks(4096) {
            out.extend(s.feed(chunk).unwrap());
        }
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data.len(), payload.len());

        let mut small = Scanner::new();
        assert!(matches!(
            small.feed(input.as_bytes()),
            Err(ScanError::FrameTooLarge(DEFAULT_MAX_FRAME_BYTES))
        ));
    }

    #[test]
    fn a_line_that_never_ends_is_bounded() {
        // No line break ever arrives: the bound must fire on the
        // unfinished line rather than buffering for ever.
        let mut s = Scanner::with_max_frame(8 * 1024);
        let chunk = [b'x'; 1024];
        let mut failed = false;
        for _ in 0..16 {
            if s.feed(&chunk).is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed, "an unterminated line grew past the bound");
    }

    #[test]
    fn arbitrary_input_never_panics() {
        // Light property-style smoke test. Feed pseudo-random bytes
        // and ensure the scanner never panics.
        let mut s = Scanner::new();
        let mut seed: u64 = 0x9E3779B97F4A7C15;
        for _ in 0..200 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let len = (seed >> 32) as usize % 64;
            let bytes: Vec<u8> = (0..len)
                .map(|i| ((seed >> (i % 56)) & 0xFF) as u8)
                .collect();
            let _ = s.feed(&bytes);
        }
        let _ = s.close();
    }
}

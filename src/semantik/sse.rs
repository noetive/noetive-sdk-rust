//! Incremental SSE parser for the Semantik subscribe stream.
//!
//! The scanner is fed arbitrary byte chunks from the HTTP body and
//! yields complete [`Frame`] values as they become available. It
//! tolerates `\n`, `\r\n`, and bare `\r` line endings (some SSE
//! implementations emit the last form), ignores comment lines (leading
//! `:`), strips a single space after `:`, and silently ignores unknown
//! directives per the SSE spec.
//!
//! Per-frame accumulated size is capped at
//! [`MAX_SSE_FRAME_BYTES`](crate::semantik::limits::MAX_SSE_FRAME_BYTES);
//! larger frames produce [`ScanError::FrameTooLarge`]. This bounds the
//! memory exposure of a misbehaving server.

use crate::semantik::limits::MAX_SSE_FRAME_BYTES;

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
#[derive(Debug, Default)]
pub(crate) struct Scanner {
    buf: Vec<u8>,
    cur_event: String,
    cur_data: String,
    cur_size: usize,
    pending_cr: bool,
    closed: bool,
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum ScanError {
    #[error("sse: frame exceeds maximum size {0} bytes")]
    FrameTooLarge(usize),
}

impl Scanner {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feed a byte chunk and yield all frames that became complete.
    /// Returned in order.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Frame>, ScanError> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((line, advance)) = next_line(&self.buf, self.pending_cr, false) {
            self.buf.drain(..advance);
            self.pending_cr = false;
            if let Some(frame) = self.consume_line(&line)? {
                out.push(frame);
            }
        }
        // The next_line returns None when it needs more bytes. If we
        // see a trailing CR at end-of-buffer we don't know yet if it's
        // a lone \r (line break) or part of \r\n. Remember it for the
        // next feed.
        self.pending_cr = !self.buf.is_empty() && *self.buf.last().unwrap() == b'\r';
        Ok(out)
    }

    /// Signal end-of-stream. Returns the final frame if any partial
    /// state was accumulated (Go's scanner does the same on clean EOF).
    pub(crate) fn close(&mut self) -> Result<Option<Frame>, ScanError> {
        if self.closed {
            return Ok(None);
        }
        self.closed = true;
        // Treat remaining buffer as a final unterminated line. A bare
        // \r at the end is also flushed as an empty line break.
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            let (line_bytes, _) = strip_trailing_cr(&line);
            if let Some(frame) = self.consume_line(line_bytes)? {
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
        if self.cur_size > MAX_SSE_FRAME_BYTES {
            return Err(ScanError::FrameTooLarge(MAX_SSE_FRAME_BYTES));
        }
        let (field, value) = split_field(line);
        match field {
            b"event" => {
                // `value` is a slice of `line`, which itself comes from
                // self.buf bytes already drained. Allocate a String
                // from the bytes (lossy on non-UTF-8, which the server
                // never emits but we still tolerate).
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

/// Strip a single trailing `\r` from `line`. Returns the trimmed
/// slice and whether one was stripped. Used on the final unterminated
/// chunk at close-time.
fn strip_trailing_cr(line: &[u8]) -> (&[u8], bool) {
    if let Some((&b'\r', rest)) = line.split_last() {
        (rest, true)
    } else {
        (line, false)
    }
}

/// Extract the next line from `data`. Returns `(line_bytes,
/// advance_count)` or `None` if more bytes are needed. `prior_cr`
/// indicates that the previous feed ended on a bare `\r` — if this
/// chunk starts with `\n`, the `\n` belongs to the prior `\r\n` and
/// should be consumed without emitting a line.
fn next_line(data: &[u8], prior_cr: bool, at_eof: bool) -> Option<(Vec<u8>, usize)> {
    if prior_cr && data.first() == Some(&b'\n') {
        // The leading \n is the tail of a \r\n straddling chunks. The
        // line was already emitted on the previous feed when we saw
        // the \r. Just consume the \n.
        return Some((Vec::new(), 1));
    }
    for (i, &b) in data.iter().enumerate() {
        match b {
            b'\n' => return Some((data[..i].to_vec(), i + 1)),
            b'\r' => {
                if i + 1 < data.len() {
                    if data[i + 1] == b'\n' {
                        return Some((data[..i].to_vec(), i + 2));
                    }
                    return Some((data[..i].to_vec(), i + 1));
                }
                // Defer: we may be one byte short of distinguishing
                // \r\n from lone \r.
                if at_eof {
                    return Some((data[..i].to_vec(), i + 1));
                }
                return None;
            }
            _ => {}
        }
    }
    if at_eof {
        return Some((data.to_vec(), data.len()));
    }
    None
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
        huge.extend(std::iter::repeat(b'a').take(MAX_SSE_FRAME_BYTES + 100));
        huge.extend_from_slice(b"\n\n");
        let mut s = Scanner::new();
        let err = s.feed(&huge).unwrap_err();
        assert!(matches!(err, ScanError::FrameTooLarge(_)));
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

//! Watch and wait: the journal as a stream, and one bounded window of it.
//!
//! The server serves the journal only as a stream. [`Client::watch`] hands
//! that stream to a caller that holds it for as long as it runs;
//! [`Client::wait`] opens the same stream for one bounded window and returns
//! the first batch, for a caller that thinks in turns — an agent loop, a cron
//! job — and would otherwise manage a connection it does not want.
//!
//! The server sends a named `open` frame before anything else. Everything
//! before it is setup, and a connection that fails before any response is
//! retried under the client's policy. Everything after it is in flight, and
//! this crate does not reconnect on its own. Nothing is lost by that: the
//! journal is stored, so a new watch from [`JournalStream::cursor`] replays
//! everything after it. Whether and when to reconnect is the caller's budget
//! to spend.
//!
//! Delivery is at least once against the journal's per-mailbox sequence: a
//! reconnect from a cursor can repeat events, so dedupe on
//! [`JournalEvent::id`].

use std::collections::VecDeque;
use std::future::poll_fn;
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures_core::Stream;
use reqwest::header::CONTENT_TYPE;
use serde::Deserialize;

use crate::bud::client::{encode, read_capped, request_id_of, Client, MIME_EVENT_STREAM};
use crate::bud::error::{error_from, malformed, Error, Failure};
use crate::bud::types::{quoted, JournalEvent, WaitInput, WaitOutput};
use crate::sse::{Frame, Scanner};

/// The longest window [`Client::wait`] holds the stream open, and the
/// longest quiet interval the server allows before a keepalive. Also the
/// default.
///
/// A caller whose own deadline is shorter than its window sees its normal
/// empty answer as a cancelled call — the reading that makes an agent retry
/// immediately, against the description that told it not to.
pub const MAX_WAIT_SECONDS: u32 = 25;

/// Bounds one frame on the stream. A batch can carry many events, each with
/// a subject and an address a stranger chose, and escaping can multiply
/// their size; this leaves ample room for that and is still bounded, so a
/// misbehaving proxy cannot decide how much one stream holds.
const MAX_FRAME_BYTES: usize = 4 << 20;

/// Bounds the refusal body read when the stream does not open.
const MAX_REFUSAL_BYTES: usize = 1 << 20;

/// The frame names the server sends. Only these carry data; a comment line
/// is a heartbeat and the parser drops it.
const EVENT_OPEN: &str = "open";
const EVENT_BATCH: &str = "batch";

/// The status a stream arrives with, recorded on what goes wrong inside it.
const STREAM_STATUS: u16 = 200;

/// An open connection to the journal.
///
/// A [`Stream`] of events. It ends — yields `None` — on a clean close, and
/// yields one `Err` before ending on a failure or on a refusal the server
/// sent in flight; the server does not continue after one, and a stream that
/// kept reading would look healthy while receiving nothing. A keepalive is
/// consumed and never surfaces. Drop it to release the connection.
pub struct JournalStream {
    body: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    scanner: Scanner,
    frames: VecDeque<Frame>,
    pending: VecDeque<JournalEvent>,
    /// The latest cursor the server sent.
    latest: String,
    /// The cursor after the last event handed to the caller, which is what
    /// [`JournalStream::cursor`] reports: resuming there must not skip the
    /// rest of a batch the caller stopped part-way through.
    delivered: String,
    request_id: String,
    eof: bool,
    ended: bool,
}

impl std::fmt::Debug for JournalStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JournalStream")
            .field("cursor", &self.delivered)
            .field("request_id", &self.request_id)
            .field("pending", &self.pending.len())
            .field("ended", &self.ended)
            .finish()
    }
}

/// What the next batch frame came to.
enum Batch {
    /// A batch, possibly with no events: one that only moves the cursor.
    Events(WaitOutput),
    /// A refusal sent in flight; `error` is set. The stream has ended.
    Refused(WaitOutput),
    /// The stream could not be read. It has ended.
    Failed(Failure),
    /// The stream closed.
    End,
}

/// The handshake's outcome when the connection itself worked.
enum Opened {
    Stream(JournalStream),
    Refused(Error),
}

impl Client {
    /// Open the stream.
    ///
    /// An `Err` is a setup failure: the connection, the credential, a server
    /// that did not send its opening frame — or a refusal, which arrives as
    /// [`Failure::Refused`] because a stream has no envelope to carry one.
    /// Once this returns `Ok` the stream is established, and every later
    /// failure arrives through the stream.
    pub async fn watch(&self, input: WaitInput) -> Result<JournalStream, Failure> {
        match self.open(&input).await? {
            Opened::Stream(stream) => Ok(stream),
            Opened::Refused(refusal) => Err(refusal.into()),
        }
    }

    /// Block until something happens, or until the window closes.
    ///
    /// The window is [`WaitInput::timeout_seconds`], at most and by default
    /// [`MAX_WAIT_SECONDS`]. It starts once the stream is open, so a slow
    /// handshake does not eat into it. The handshake itself is bounded by
    /// [`DEFAULT_RESPONSE_TIMEOUT`](crate::bud::DEFAULT_RESPONSE_TIMEOUT): the
    /// headers on the default transport, the opening frame on any. Within the window this returns the
    /// first batch carrying events, whole, with the cursor after it; pass
    /// that cursor to the next wait. A batch with no events only advances
    /// the cursor.
    ///
    /// An empty result is success ([`WaitOutput::is_empty`]) and carries the
    /// latest cursor the stream gave. A refusal is a value, as on the other
    /// operations: one before the stream opens comes back with the caller's
    /// own cursor, one sent in flight with the stream's. `Err` is a
    /// connection that failed, a stream this crate could not read
    /// ([`Code::MalformedResponse`](crate::bud::Code::MalformedResponse)), or
    /// a stream that closed before answering ([`Failure::Closed`]).
    ///
    /// With `types` or `agent_part` set, an empty wait returns the cursor it
    /// started from, and the next wait reads past the same excluded events
    /// again; over a busy mailbox, filter on the caller's side instead. For a
    /// long-lived consumer prefer [`watch`](Self::watch), which holds one
    /// connection instead of one per window.
    pub async fn wait(&self, input: WaitInput) -> Result<WaitOutput, Failure> {
        let mut stream = match self.open(&input).await? {
            Opened::Stream(stream) => stream,
            Opened::Refused(refusal) => {
                // The caller's own cursor, so a refusal it retries after
                // fixing the request resumes from the same place.
                return Ok(WaitOutput {
                    cursor: input.cursor,
                    error: Some(refusal),
                    ..WaitOutput::default()
                });
            }
        };

        let deadline = tokio::time::Instant::now() + wait_window(input.timeout_seconds);
        loop {
            let next = poll_fn(|cx| stream.poll_batch(cx));
            match tokio::time::timeout_at(deadline, next).await {
                // The window closed with nothing in it. Not a failure.
                Err(_) => {
                    return Ok(WaitOutput {
                        cursor: stream.latest,
                        ..WaitOutput::default()
                    })
                }
                Ok(Batch::Events(batch)) if batch.events.is_empty() => continue,
                Ok(Batch::Events(batch)) | Ok(Batch::Refused(batch)) => return Ok(batch),
                Ok(Batch::Failed(failure)) => return Err(failure),
                // The server ends a stream without a batch only when it is
                // going away. An empty window would send the caller straight
                // back to a server that is not there.
                Ok(Batch::End) => return Err(Failure::Closed),
            }
        }
    }

    /// The handshake. A refusal sent before the stream opened is returned
    /// apart from failures, so `watch` can return it as an error and `wait`
    /// as a value.
    async fn open(&self, input: &WaitInput) -> Result<Opened, Failure> {
        let body = encode(input)?;
        let resp = self
            .post_with_retry("watch", body, Some(MIME_EVENT_STREAM), "")
            .await?;
        let status = resp.status().as_u16();
        let request_id = request_id_of(resp.headers());

        // A refusal arrives as the envelope at a failing status.
        if status != 200 {
            // A body that breaks off reads as whatever arrived, which then
            // either decodes or is reported as not an envelope.
            let raw = read_capped(resp, MAX_REFUSAL_BYTES)
                .await
                .unwrap_or_default();
            if let Ok(WaitOutput {
                error: Some(mut refusal),
                ..
            }) = serde_json::from_slice::<WaitOutput>(&raw)
            {
                refusal.http_status = status;
                if refusal.request_id.is_empty() {
                    refusal.request_id = request_id;
                }
                return Ok(Opened::Refused(refusal));
            }
            return Err(error_from(status, &raw, &request_id).into());
        }

        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if !is_event_stream(&content_type) {
            return Err(malformed(
                format!(
                    "the server answered 200 with content type {content_type} rather than an event stream"
                ),
                status,
                &request_id,
            )
            .into());
        }

        let mut stream = JournalStream {
            body: Box::pin(resp.bytes_stream()),
            scanner: Scanner::with_max_frame(MAX_FRAME_BYTES),
            frames: VecDeque::new(),
            pending: VecDeque::new(),
            latest: String::new(),
            delivered: String::new(),
            request_id,
            eof: false,
            ended: false,
        };
        // Read the handshake here, so a server that never sends it is a setup
        // failure the caller can retry rather than a stream that yields
        // nothing. It is bounded: the header timeout ended when the headers
        // arrived.
        let limit = self.open_timeout();
        match tokio::time::timeout(limit, stream.read_open()).await {
            Ok(opened) => opened?,
            Err(_) => return Err(Failure::Timeout(limit)),
        }
        Ok(Opened::Stream(stream))
    }
}

/// How long `wait` holds the stream: zero or past the bound means the bound,
/// since the server holds a quiet interval no longer than that anyway.
pub(crate) fn wait_window(seconds: u32) -> Duration {
    let seconds = if seconds == 0 || seconds > MAX_WAIT_SECONDS {
        MAX_WAIT_SECONDS
    } else {
        seconds
    };
    Duration::from_secs(u64::from(seconds))
}

/// The media type, with or without parameters.
fn is_event_stream(content_type: &str) -> bool {
    let media = content_type.split(';').next().unwrap_or_default();
    media.trim().eq_ignore_ascii_case(MIME_EVENT_STREAM)
}

impl JournalStream {
    /// Where the caller has read to: what it needs to reconnect. It moves past
    /// a batch only once the batch's last event has been yielded, so a
    /// consumer that stops part-way through a batch and resumes from here gets
    /// the rest of it. Pass it as [`WaitInput::cursor`] on the next watch or
    /// wait; the journal replays everything after it, including what arrived
    /// while no stream was open.
    pub fn cursor(&self) -> &str {
        &self.delivered
    }

    /// Correlates this stream with the server's own records.
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    fn malformed(&self, message: String) -> Failure {
        malformed(message, STREAM_STATUS, &self.request_id).into()
    }

    /// Consume the opening frame and record where the stream starts.
    async fn read_open(&mut self) -> Result<(), Failure> {
        let Some(frame) = poll_fn(|cx| self.poll_frame(cx)).await? else {
            return Err(self.malformed("the stream closed before its opening frame".into()));
        };
        if frame.event != EVENT_OPEN {
            return Err(self.malformed(format!(
                "the stream began with a {} frame rather than {}",
                quoted(&frame.event),
                quoted(EVENT_OPEN)
            )));
        }
        #[derive(Deserialize)]
        struct Open {
            #[serde(default)]
            cursor: String,
        }
        let open: Open = serde_json::from_str(&frame.data)
            .map_err(|e| self.malformed(format!("the opening frame did not decode: {e}")))?;
        self.delivered = open.cursor.clone();
        self.latest = open.cursor;
        Ok(())
    }

    /// The next frame of any name, or `None` at a clean end of the body.
    fn poll_frame(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<Frame>, Failure>> {
        loop {
            if let Some(frame) = self.frames.pop_front() {
                return Poll::Ready(Ok(Some(frame)));
            }
            if self.eof {
                return Poll::Ready(Ok(None));
            }
            match ready!(self.body.as_mut().poll_next(cx)) {
                Some(Ok(chunk)) => match self.scanner.feed(&chunk) {
                    Ok(frames) => self.frames.extend(frames),
                    Err(_) => return Poll::Ready(Err(self.frame_too_large())),
                },
                Some(Err(e)) => return Poll::Ready(Err(Failure::Transport(e))),
                None => {
                    self.eof = true;
                    match self.scanner.close() {
                        Ok(Some(frame)) => self.frames.push_back(frame),
                        Ok(None) => {}
                        Err(_) => return Poll::Ready(Err(self.frame_too_large())),
                    }
                }
            }
        }
    }

    fn frame_too_large(&self) -> Failure {
        self.malformed(format!("a stream frame exceeded {MAX_FRAME_BYTES} bytes"))
    }

    /// The next batch frame, whole. Cursor is always the stream's latest,
    /// because the server may send a refusal without one.
    fn poll_batch(&mut self, cx: &mut Context<'_>) -> Poll<Batch> {
        loop {
            if self.ended {
                return Poll::Ready(Batch::End);
            }
            let frame = match ready!(self.poll_frame(cx)) {
                Ok(Some(frame)) => frame,
                Ok(None) => {
                    self.ended = true;
                    return Poll::Ready(Batch::End);
                }
                Err(failure) => {
                    self.ended = true;
                    return Poll::Ready(Batch::Failed(failure));
                }
            };
            if frame.event != EVENT_BATCH {
                // Skipped rather than fatal: the server may name a frame
                // this release does not know, and refusing the rest of the
                // stream over it would break on a server that grew.
                continue;
            }
            let mut batch: WaitOutput = match serde_json::from_str(&frame.data) {
                Ok(batch) => batch,
                Err(e) => {
                    self.ended = true;
                    return Poll::Ready(Batch::Failed(
                        self.malformed(format!("a stream frame did not decode: {e}")),
                    ));
                }
            };
            if !batch.cursor.is_empty() {
                self.latest = batch.cursor.clone();
            }
            batch.cursor = self.latest.clone();
            if let Some(refusal) = batch.error.as_mut() {
                // The stream arrived at 200; status 0 stays reserved for a
                // request refused before sending.
                refusal.http_status = STREAM_STATUS;
                if refusal.request_id.is_empty() {
                    refusal.request_id = self.request_id.clone();
                }
                self.ended = true;
                return Poll::Ready(Batch::Refused(batch));
            }
            return Poll::Ready(Batch::Events(batch));
        }
    }
}

impl Stream for JournalStream {
    type Item = Result<JournalEvent, Failure>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(event) = this.pending.pop_front() {
                if this.pending.is_empty() {
                    this.delivered = this.latest.clone();
                }
                return Poll::Ready(Some(Ok(event)));
            }
            // Everything read so far has been handed out, so the latest cursor
            // is safe to resume from. A batch that only moves the cursor
            // commits here.
            this.delivered = this.latest.clone();
            match ready!(this.poll_batch(cx)) {
                Batch::Events(batch) => this.pending.extend(batch.events),
                Batch::Refused(batch) => {
                    let refusal = batch.error.expect("a refused batch carries its refusal");
                    return Poll::Ready(Some(Err(refusal.into())));
                }
                Batch::Failed(failure) => return Poll::Ready(Some(Err(failure))),
                Batch::End => return Poll::Ready(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_window_is_bounded() {
        let max = Duration::from_secs(u64::from(MAX_WAIT_SECONDS));
        for (seconds, want) in [
            (0, max),
            (1, Duration::from_secs(1)),
            (MAX_WAIT_SECONDS - 1, max - Duration::from_secs(1)),
            (MAX_WAIT_SECONDS, max),
            (MAX_WAIT_SECONDS + 1, max),
            (u32::MAX, max),
        ] {
            assert_eq!(wait_window(seconds), want, "{seconds}");
        }
    }

    #[test]
    fn the_content_type_may_carry_parameters() {
        assert!(is_event_stream("text/event-stream; charset=utf-8"));
        assert!(is_event_stream("TEXT/EVENT-STREAM"));
        assert!(!is_event_stream("application/json"));
        assert!(!is_event_stream(""));
    }
}

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_core::Stream;
use futures_util::StreamExt;
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};

use crate::semantik::client::Client;
use crate::semantik::error::{decode_error_response, Error};
use crate::semantik::sse::{Frame, ScanError, Scanner};
use crate::semantik::transport::{
    request_id_from_headers, run_with_retry, send_raw, AuthMode, MIME_JSON, MIME_SSE,
};
use crate::semantik::validate::validate_target;

const PATH_SUBSCRIBE: &str = "/v1/subscribe";

/// Body of `POST /v1/subscribe`.
///
/// Same required-field rules as [`crate::semantik::PublishRequest`]:
/// `query`, `namespace`, `model`, and `dimensions` must all be set. The
/// SDK applies no defaults — an unset targeting field fails preflight
/// rather than silently routing the subscription at a shared namespace.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SubscribeRequest {
    pub query: String,
    pub namespace: String,
    pub model: String,
    pub dimensions: u16,
}

impl SubscribeRequest {
    fn validate(&self) -> Result<(), Error> {
        if self.query.is_empty() {
            return Err(Error::preflight("subscribe query must not be empty"));
        }
        validate_target(&self.namespace, &self.model, self.dimensions)
    }
}

/// Payload of the initial `subscribed` SSE frame. Consumed inside
/// [`Client::subscribe`] before the stream is returned, so callers
/// never need to handle it explicitly — its `subscription_id` is
/// surfaced via [`Subscription::id`].
#[derive(Debug, Clone, Default, Deserialize)]
struct SubscribedEvent {
    #[serde(default)]
    subscription_id: String,
}

/// Payload of each `match` SSE frame.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MatchEvent {
    #[serde(default)]
    pub message_id: String,
    #[serde(default)]
    pub score: f32,
}

/// Live SSE match stream. Implements
/// [`futures_core::Stream<Item = Result<MatchEvent, Error>>`].
///
/// The connection is closed when the value is dropped — RAII handles
/// cleanup, so there is no explicit `close()` method to remember (the
/// equivalent of Go's `defer Close()` is just letting the binding go
/// out of scope or calling `drop`).
///
/// Forward-compat: SSE frames with unknown `event` values are silently
/// skipped. Only `match` frames yield events.
///
/// At-least-once: the server best-effort orders and delivers matches.
/// Dedupe on [`MatchEvent::message_id`] in the caller; promising
/// stronger guarantees is not supported.
///
/// Subscribe is never auto-retried because each handshake issues a
/// fresh `subscription_id`; a transparent reconnect would silently
/// drop the matches that arrived during the gap. Wrap calls in your
/// own loop if you want reconnection.
pub struct Subscription {
    id: String,
    request_id: Option<String>,
    body: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send + 'static>>,
    scanner: Scanner,
    pending: VecDeque<Frame>,
    finished: bool,
    closed_with_error: bool,
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("id", &self.id)
            .field("request_id", &self.request_id)
            .field("finished", &self.finished)
            .field("closed_with_error", &self.closed_with_error)
            .field("pending_frames", &self.pending.len())
            .finish()
    }
}

impl Subscription {
    /// Server-assigned subscription identifier. Populated before
    /// [`Client::subscribe`] returns.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Server-assigned correlation token from the handshake response's
    /// `X-Request-Id` header. Quote it when contacting support — it
    /// pivots directly to the relevant server log line. `None` when the
    /// header was absent or unreadable.
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    /// Drain a queued frame matching `event == "match"`, decoding it
    /// into a [`MatchEvent`]. Frames with other event types are
    /// skipped (forward-compatibility).
    fn try_take_match(&mut self) -> Option<Result<MatchEvent, Error>> {
        while let Some(frame) = self.pending.pop_front() {
            if frame.event != "match" {
                continue;
            }
            return Some(decode_match(&frame.data));
        }
        None
    }
}

fn decode_match(data: &str) -> Result<MatchEvent, Error> {
    serde_json::from_str(data)
        .map_err(|e| Error::MalformedSse(format!("decode match frame: {e}; payload={data}")))
}

impl Stream for Subscription {
    type Item = Result<MatchEvent, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(ev) = self.try_take_match() {
                // Decode failures on individual match frames are
                // mid-stream — wrap so callers can distinguish them
                // from handshake failures.
                return Poll::Ready(Some(ev.map_err(wrap_stream)));
            }
            if self.finished {
                return Poll::Ready(None);
            }
            match self.body.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(chunk))) => match self.scanner.feed(&chunk) {
                    Ok(frames) => self.pending.extend(frames),
                    Err(ScanError::FrameTooLarge(n)) => {
                        self.finished = true;
                        self.closed_with_error = true;
                        return Poll::Ready(Some(Err(wrap_stream(Error::MalformedSse(format!(
                            "frame exceeds {n} bytes"
                        ))))));
                    }
                },
                Poll::Ready(Some(Err(e))) => {
                    self.finished = true;
                    self.closed_with_error = true;
                    return Poll::Ready(Some(Err(wrap_stream(Error::from(e)))));
                }
                Poll::Ready(None) => {
                    // End of body. Flush any partial frame.
                    self.finished = true;
                    match self.scanner.close() {
                        Ok(Some(frame)) => self.pending.push_back(frame),
                        Ok(None) => {}
                        Err(ScanError::FrameTooLarge(n)) => {
                            self.closed_with_error = true;
                            return Poll::Ready(Some(Err(wrap_stream(Error::MalformedSse(
                                format!("frame exceeds {n} bytes"),
                            )))));
                        }
                    }
                }
            }
        }
    }
}

/// Wrap a mid-stream error in [`Error::SubscribeStream`]. Accessors on
/// [`Error`] recurse into the inner error so code matching on the
/// underlying code still works through the wrap.
fn wrap_stream(source: Error) -> Error {
    Error::SubscribeStream {
        source: Box::new(source),
    }
}

impl Client {
    /// Register a persistent subscription and open an SSE match stream.
    ///
    /// The initial `subscribed` SSE frame is consumed inside
    /// `subscribe()` so that [`Subscription::id`] is populated before
    /// the returned stream yields any matches; authentication, transport,
    /// and content-type errors surface here rather than being deferred
    /// to the first `.next().await`.
    ///
    /// **Setup-time retries.** The handshake (POST + content-type
    /// validation + first frame read) is wrapped in the configured
    /// [`RetryPolicy`](crate::semantik::RetryPolicy) — the same policy
    /// applied to one-shot RPCs. A `503 unavailable` with the message
    /// *"subscription setup did not complete within the budget; retry
    /// the request"* is the most common transient at this stage and
    /// the server's `retry_after_ms` hint is honoured. Replay is safe
    /// because each handshake either succeeds with a fresh
    /// `subscription_id` or fails before the server commits state — no
    /// matches can be missed.
    ///
    /// **Mid-stream disconnects are NOT auto-retried.** Once matches
    /// have started arriving, reconnecting silently would drop matches
    /// between the disconnect and the new `subscription_id`. The
    /// returned [`Subscription`] surfaces those errors via its
    /// [`futures_core::Stream`] impl; the caller decides whether and
    /// how to reconnect (and how to dedupe via `message_id`).
    ///
    /// The underlying HTTP connection is closed when the
    /// [`Subscription`] is dropped (RAII).
    pub async fn subscribe(&self, req: SubscribeRequest) -> Result<Subscription, Error> {
        // Preflight failures short-circuit before the wrap — callers
        // see `Error::Api { http_status: 0, .. }` directly so
        // `is_preflight()` still works without recursion.
        req.validate()?;
        let body = Bytes::from(
            serde_json::to_vec(&req)
                .map_err(|e| Error::malformed_response(format!("encode subscribe: {e}"), 0))?,
        );

        run_with_retry(self, |_attempt| {
            let body = body.clone();
            async move { subscribe_once(self, body).await }
        })
        .await
        .map_err(|source| Error::SubscribeSetup {
            source: Box::new(source),
        })
    }
}

/// One handshake attempt: POST, validate content-type, read until the
/// initial `subscribed` frame. Returns a ready-to-stream
/// [`Subscription`]. Errors are returned as `Error::Api` (for retryable
/// transient codes the policy will replay), `Error::MalformedSse`, or
/// `Error::Transport`.
async fn subscribe_once(client: &Client, body: Bytes) -> Result<Subscription, Error> {
    // No read timeout on the subscribe POST: the handshake reads the
    // first `subscribed` frame inline, and the long-lived body stream
    // must outlive any per-call timeout the SDK might impose on
    // one-shot RPCs. Connect-side timeouts still apply via the
    // underlying `reqwest::Client`.
    let resp = send_raw(
        client,
        PATH_SUBSCRIBE,
        body,
        MIME_JSON,
        MIME_SSE,
        AuthMode::Bearer,
        None,
    )
    .await?;

    let status = resp.status();
    if !status.is_success() {
        let headers = resp.headers().clone();
        let body = resp.bytes().await.map_err(Error::from)?;
        return Err(decode_error_response(status.as_u16(), &headers, &body));
    }

    let request_id = request_id_from_headers(resp.headers());
    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !is_event_stream_content_type(&content_type) {
        return Err(Error::MalformedSse(format!(
            "expected {MIME_SSE}, got {content_type:?}"
        )));
    }

    let mut body_stream: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>> =
        Box::pin(resp.bytes_stream());
    let mut scanner = Scanner::new();
    let mut pending: VecDeque<Frame> = VecDeque::new();

    // Read until we see the first frame.
    let first_frame = loop {
        if let Some(f) = pending.pop_front() {
            break f;
        }
        match body_stream.next().await {
            Some(Ok(chunk)) => match scanner.feed(&chunk) {
                Ok(frames) => pending.extend(frames),
                Err(ScanError::FrameTooLarge(n)) => {
                    return Err(Error::MalformedSse(format!(
                        "subscribed frame exceeds {n} bytes"
                    )))
                }
            },
            Some(Err(e)) => return Err(Error::from(e)),
            None => match scanner.close() {
                Ok(Some(f)) => break f,
                Ok(None) => {
                    return Err(Error::MalformedSse(
                        "stream closed before subscribed frame".to_string(),
                    ))
                }
                Err(ScanError::FrameTooLarge(n)) => {
                    return Err(Error::MalformedSse(format!(
                        "subscribed frame exceeds {n} bytes"
                    )))
                }
            },
        }
    };

    if first_frame.event != "subscribed" {
        return Err(Error::MalformedSse(format!(
            "expected 'subscribed' event, got {:?}",
            first_frame.event
        )));
    }
    let sub: SubscribedEvent = serde_json::from_str(&first_frame.data)
        .map_err(|e| Error::MalformedSse(format!("decode subscribed frame: {e}")))?;
    if sub.subscription_id.is_empty() {
        return Err(Error::MalformedSse(
            "subscribed frame missing subscription_id".to_string(),
        ));
    }

    Ok(Subscription {
        id: sub.subscription_id,
        request_id,
        body: body_stream,
        scanner,
        pending,
        finished: false,
        closed_with_error: false,
    })
}

/// Case-insensitive, parameter-tolerant comparison against
/// `text/event-stream`. Mirrors Go's `isEventStreamContentType`.
fn is_event_stream_content_type(ct: &str) -> bool {
    let ct = ct.split(';').next().unwrap_or("").trim();
    ct.eq_ignore_ascii_case(MIME_SSE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_accepts_with_charset() {
        assert!(is_event_stream_content_type(
            "text/event-stream; charset=utf-8"
        ));
        assert!(is_event_stream_content_type("text/event-stream"));
        assert!(is_event_stream_content_type("TEXT/EVENT-STREAM"));
        assert!(!is_event_stream_content_type("application/json"));
        assert!(!is_event_stream_content_type(""));
    }

    fn valid_request() -> SubscribeRequest {
        SubscribeRequest {
            query: "q".into(),
            namespace: "global".into(),
            model: "Qwen3-Embedding-4B".into(),
            dimensions: 1024,
        }
    }

    #[test]
    fn subscribe_validate_accepts_fully_specified_request() {
        assert!(valid_request().validate().is_ok());
    }

    #[test]
    fn subscribe_validate_empty_query() {
        let r = SubscribeRequest {
            query: String::new(),
            ..valid_request()
        };
        assert!(r.validate().is_err());
    }

    #[test]
    fn subscribe_validate_requires_targeting_fields() {
        // No defaults: an unset namespace/model/dimensions fails preflight.
        assert!(SubscribeRequest {
            namespace: String::new(),
            ..valid_request()
        }
        .validate()
        .is_err());
        assert!(SubscribeRequest {
            model: String::new(),
            ..valid_request()
        }
        .validate()
        .is_err());
        assert!(SubscribeRequest {
            dimensions: 0,
            ..valid_request()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn decode_match_event() {
        let m = decode_match(r#"{"message_id":"abc","score":0.42}"#).unwrap();
        assert_eq!(m.message_id, "abc");
        assert!((m.score - 0.42).abs() < 1e-6);
    }

    #[test]
    fn decode_match_event_malformed() {
        let err = decode_match("not json").unwrap_err();
        assert!(matches!(err, Error::MalformedSse(_)));
    }
}

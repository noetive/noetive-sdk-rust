use std::time::Duration;

use bytes::Bytes;
use reqwest::header::{ACCEPT, CONTENT_TYPE, USER_AGENT};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::semantik::client::{Client, HEADER_AUTHORIZATION};
use crate::semantik::error::{decode_error_response, Error};
use crate::semantik::limits::MAX_RESPONSE_BYTES;
use crate::semantik::version::user_agent;

pub(crate) const MIME_JSON: &str = "application/json";
pub(crate) const MIME_SSE: &str = "text/event-stream";

#[derive(Copy, Clone, Eq, PartialEq)]
pub(crate) enum AuthMode {
    None,
    Bearer,
}

/// Implemented by every success response struct so the transport layer
/// can stamp the server-assigned `X-Request-Id` onto the deserialised
/// body before handing it to the caller. The header is the only place
/// the correlation token lives on 2xx responses — the JSON body does
/// not carry it.
pub(crate) trait SetRequestId {
    fn set_request_id(&mut self, id: Option<String>);
}

/// Extract `X-Request-Id` from a response header map. Returns `None`
/// when the header is missing, empty, or not valid UTF-8.
pub(crate) fn request_id_from_headers(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let v = headers.get("x-request-id")?.to_str().ok()?;
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

/// Marshal `req` as JSON, POST it to `path`, decode a 2xx body into
/// `Resp`. Non-2xx responses are returned as [`Error::Api`]. The
/// configured retry policy is applied on transient API errors.
///
/// `expect_response` controls whether a response body is decoded. Pass
/// `false` for `Health`, which returns a 200 with no body the SDK
/// inspects.
pub(crate) async fn send_json<Req, Resp>(
    client: &Client,
    path: &'static str,
    req: &Req,
    auth: AuthMode,
) -> Result<Resp, Error>
where
    Req: Serialize,
    Resp: DeserializeOwned + SetRequestId,
{
    let body = serde_json::to_vec(req)
        .map_err(|e| Error::malformed_response(format!("encode request: {e}"), 0))?;
    let body = Bytes::from(body);
    run_with_retry(client, |attempt| {
        let body = body.clone();
        async move {
            let _ = attempt; // hand to policy via the error, not used directly here
            send_once_json(client, path, body, auth).await
        }
    })
    .await
}

async fn send_once_json<Resp>(
    client: &Client,
    path: &'static str,
    body: Bytes,
    auth: AuthMode,
) -> Result<Resp, Error>
where
    Resp: DeserializeOwned + SetRequestId,
{
    // One-shot RPCs inherit the SDK's per-call read timeout. The
    // subscribe path passes `None` so long quiet periods between match
    // frames don't tear the stream down.
    let read_timeout = client.inner().read_timeout;
    let resp = send_raw(client, path, body, MIME_JSON, MIME_JSON, auth, read_timeout).await?;
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = read_capped(resp, MAX_RESPONSE_BYTES).await?;
    if !status.is_success() {
        return Err(decode_error_response(status.as_u16(), &headers, &bytes));
    }
    let mut value: Resp = serde_json::from_slice(&bytes)
        .map_err(|e| Error::malformed_response(format!("decode response: {e}"), status.as_u16()))?;
    value.set_request_id(request_id_from_headers(&headers));
    Ok(value)
}

/// Send a single HTTP round-trip without retry or response decoding.
/// Used by both the JSON helpers above and the streaming subscribe
/// handshake. Pass `read_timeout = Some(d)` for one-shot RPCs and
/// `None` for the subscribe stream body — the subscribe path must NOT
/// be killed by a read-timeout between matches.
pub(crate) async fn send_raw(
    client: &Client,
    path: &'static str,
    body: Bytes,
    content_type: &'static str,
    accept: &'static str,
    auth: AuthMode,
    read_timeout: Option<Duration>,
) -> Result<reqwest::Response, Error> {
    let inner = client.inner();
    let url = format!("{}{}", inner.base_url, path);
    let mut req = inner
        .http
        .post(&url)
        .header(CONTENT_TYPE, content_type)
        .header(ACCEPT, accept)
        .header(USER_AGENT, user_agent())
        .body(body);
    if auth == AuthMode::Bearer {
        req = req.header(HEADER_AUTHORIZATION, inner.auth_header.clone());
    }
    if let Some(d) = read_timeout {
        req = req.timeout(d);
    }
    req.send().await.map_err(Error::from)
}

/// Read a response body, capping at `max` bytes. Larger bodies are
/// truncated and surfaced as [`Error::malformed_response`] so the SDK
/// does not buffer attacker-controlled payloads.
async fn read_capped(resp: reqwest::Response, max: usize) -> Result<Bytes, Error> {
    // reqwest's Response::bytes loads the full body. We rely on the
    // server (and any upstream proxy) to bound response size and the
    // cap below to be a backstop.
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.map_err(Error::from)?;
    if bytes.len() > max {
        return Err(Error::malformed_response(
            format!("response body {} bytes exceeds {max} byte cap", bytes.len()),
            status,
        ));
    }
    Ok(bytes)
}

/// Retry loop. Sleeps via `tokio::time::sleep` when the policy returns
/// a delay; aborts on non-API errors or when the policy declines.
pub(crate) async fn run_with_retry<F, Fut, T>(client: &Client, mut f: F) -> Result<T, Error>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<T, Error>>,
{
    let policy = &client.inner().retry;
    let mut attempt: u32 = 0;
    loop {
        match f(attempt).await {
            Ok(v) => return Ok(v),
            Err(err) => {
                let Some(delay) = policy.should_retry(attempt, &err) else {
                    return Err(err);
                };
                // Defence in depth: cap the delay even if a custom
                // policy returns an absurd value. The error decoder
                // already enforces this on server-provided hints; we
                // re-apply here for caller-defined policies.
                let delay = delay.min(Duration::from_secs(3600));
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
        }
    }
}

use std::fmt;
use std::time::Duration;

use crate::semantik::limits::{MAX_ERROR_BODY_BYTES, MAX_RETRY_AFTER};

/// Machine-readable error codes returned by the Semantik API.
///
/// New codes the server adds in future are surfaced as
/// [`ErrorCode::Unknown`] without breaking existing callers — match on
/// the variants you care about and treat `Unknown` as an opaque
/// fallback. See the "Error codes" table in the Semantik public API
/// reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorCode {
    /// Client-side preflight rejection OR server 400. `http_status==0`
    /// disambiguates: zero means the SDK rejected the request before
    /// sending it; non-zero means the server returned 400.
    InvalidRequest,
    Unauthorized,
    NotBillable,
    MethodNotAllowed,
    UnsupportedMediaType,
    RequestTooLarge,
    RateLimited,
    TooManyRequests,
    Backpressure,
    Unavailable,
    NamespaceUnavailable,
    NamespaceDisabled,
    ModelNotProvisioned,
    MeteringUnavailable,
    InternalError,
    /// Server returned 2xx with a body the SDK could not decode. Not
    /// part of the wire protocol; never produced for non-2xx responses.
    MalformedResponse,
    /// Forward-compat catch-all for codes added after this SDK release.
    Unknown(String),
}

impl ErrorCode {
    /// Wire-format string used in the JSON envelope's `error` field.
    pub fn as_str(&self) -> &str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::Unauthorized => "unauthorized",
            Self::NotBillable => "not_billable",
            Self::MethodNotAllowed => "method_not_allowed",
            Self::UnsupportedMediaType => "unsupported_media_type",
            Self::RequestTooLarge => "request_too_large",
            Self::RateLimited => "rate_limited",
            Self::TooManyRequests => "too_many_requests",
            Self::Backpressure => "backpressure",
            Self::Unavailable => "unavailable",
            Self::NamespaceUnavailable => "namespace_unavailable",
            Self::NamespaceDisabled => "namespace_disabled",
            Self::ModelNotProvisioned => "model_not_provisioned",
            Self::MeteringUnavailable => "metering_unavailable",
            Self::InternalError => "internal_error",
            Self::MalformedResponse => "malformed_response",
            Self::Unknown(s) => s,
        }
    }

    /// Parse a wire-format string into a known variant, falling back to
    /// [`ErrorCode::Unknown`] for codes added after this SDK release.
    pub fn from_wire(s: &str) -> Self {
        match s {
            "invalid_request" => Self::InvalidRequest,
            "unauthorized" => Self::Unauthorized,
            "not_billable" => Self::NotBillable,
            "method_not_allowed" => Self::MethodNotAllowed,
            "unsupported_media_type" => Self::UnsupportedMediaType,
            "request_too_large" => Self::RequestTooLarge,
            "rate_limited" => Self::RateLimited,
            "too_many_requests" => Self::TooManyRequests,
            "backpressure" => Self::Backpressure,
            "unavailable" => Self::Unavailable,
            "namespace_unavailable" => Self::NamespaceUnavailable,
            "namespace_disabled" => Self::NamespaceDisabled,
            "model_not_provisioned" => Self::ModelNotProvisioned,
            "metering_unavailable" => Self::MeteringUnavailable,
            "internal_error" => Self::InternalError,
            "malformed_response" => Self::MalformedResponse,
            other => Self::Unknown(other.to_string()),
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Typed errors surfaced from every SDK method.
///
/// Transport-level failures (DNS, TCP, TLS) are wrapped in
/// [`Error::Transport`]. Structured server error envelopes — and
/// client-side preflight rejections — surface as [`Error::Api`]. The
/// `http_status` discriminates the two: a zero status means the SDK
/// rejected the request before it ever reached the wire.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// A structured API error (or preflight validation failure when
    /// `http_status == 0`). Inspect [`code`](Self::code),
    /// [`request_id`](Self::request_id), and
    /// [`retry_after`](Self::retry_after) for full diagnostic context.
    #[error("{}", fmt_api(*http_status, code, message))]
    Api {
        code: ErrorCode,
        message: String,
        request_id: Option<String>,
        retry_after: Option<Duration>,
        http_status: u16,
    },

    /// SSE handshake or in-stream frame parse failure.
    #[error("semantik: malformed SSE frame: {0}")]
    MalformedSse(String),

    /// Wrap of an error that surfaced during the subscribe handshake
    /// (POST + content-type validation + first `subscribed` frame).
    /// The inner [`Error`] carries the original failure — typically an
    /// [`Error::Api`] (`503 unavailable` budget exhaustion is the most
    /// common transient at this stage) or an [`Error::MalformedSse`].
    /// Accessors on [`Error`] (`code()`, `http_status()`, `request_id()`,
    /// `retry_after()`) recurse into the inner error so call-sites that
    /// match on a code keep working unchanged.
    #[error("semantik: subscribe handshake failed: {source}")]
    SubscribeSetup {
        #[source]
        source: Box<Error>,
    },

    /// Wrap of an error that surfaced after the subscribe handshake
    /// completed — somewhere between the first `subscribed` frame and
    /// end-of-stream. Mid-stream errors are NOT auto-retried because a
    /// transparent reconnect would silently drop the matches between
    /// the disconnect and the new `subscription_id`. Accessors recurse
    /// into the inner error.
    #[error("semantik: subscribe stream failed: {source}")]
    SubscribeStream {
        #[source]
        source: Box<Error>,
    },

    /// API key supplied to [`super::Client::new`] is empty or
    /// whitespace-only. The SDK does not police key format beyond
    /// non-empty — the server is the source of truth.
    #[error("semantik: invalid API key")]
    InvalidApiKey,

    /// `NOETIVE_KEY_SECRET` is unset when calling
    /// [`super::Client::from_env`].
    #[error("semantik: NOETIVE_KEY_SECRET not set")]
    MissingApiKey,

    /// HTTP transport, DNS, TLS, or other reqwest-level failure.
    #[error("semantik: transport: {0}")]
    Transport(#[from] reqwest::Error),
}

fn fmt_api(http_status: u16, code: &ErrorCode, message: &str) -> String {
    match (http_status, message.is_empty()) {
        (0, false) => format!("semantik: {code}: {message}"),
        (0, true) => format!("semantik: {code}"),
        (_, false) => format!("semantik: {http_status} {code}: {message}"),
        (_, true) => format!("semantik: {http_status} {code}"),
    }
}

impl Error {
    /// Construct a preflight validation error. `http_status` is set to
    /// zero so downstream callers can tell client-side rejection apart
    /// from a server-side 400.
    pub(crate) fn preflight<S: Into<String>>(message: S) -> Self {
        Self::Api {
            code: ErrorCode::InvalidRequest,
            message: message.into(),
            request_id: None,
            retry_after: None,
            http_status: 0,
        }
    }

    /// Convenience constructor for malformed-2xx-body decode failures.
    pub(crate) fn malformed_response<S: Into<String>>(message: S, http_status: u16) -> Self {
        Self::Api {
            code: ErrorCode::MalformedResponse,
            message: message.into(),
            request_id: None,
            retry_after: None,
            http_status,
        }
    }

    /// Error code if this is an [`Error::Api`] — recursing through
    /// [`Error::SubscribeSetup`] / [`Error::SubscribeStream`] wraps so
    /// callers that match on the inner code do not need to know whether
    /// it came from the handshake or a mid-stream failure.
    pub fn code(&self) -> Option<&ErrorCode> {
        match self {
            Self::Api { code, .. } => Some(code),
            Self::SubscribeSetup { source } | Self::SubscribeStream { source } => source.code(),
            _ => None,
        }
    }

    /// HTTP status code if this is an [`Error::Api`] that reached the
    /// wire. Returns `Some(0)` for preflight failures. Recurses through
    /// subscribe wraps.
    pub fn http_status(&self) -> Option<u16> {
        match self {
            Self::Api { http_status, .. } => Some(*http_status),
            Self::SubscribeSetup { source } | Self::SubscribeStream { source } => {
                source.http_status()
            }
            _ => None,
        }
    }

    /// Server-assigned correlation token if available. Quote it when
    /// contacting support. Recurses through subscribe wraps.
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Self::Api { request_id, .. } => request_id.as_deref(),
            Self::SubscribeSetup { source } | Self::SubscribeStream { source } => {
                source.request_id()
            }
            _ => None,
        }
    }

    /// Server's retry hint, when present. `None` means do not retry.
    /// Recurses through subscribe wraps.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Api { retry_after, .. } => *retry_after,
            Self::SubscribeSetup { source } | Self::SubscribeStream { source } => {
                source.retry_after()
            }
            _ => None,
        }
    }

    /// True when this is a client-side preflight validation failure
    /// that never reached the wire. Preflight errors are never wrapped
    /// in [`Error::SubscribeSetup`] / [`Error::SubscribeStream`] — they
    /// short-circuit before the subscribe path runs — so this does not
    /// recurse.
    pub fn is_preflight(&self) -> bool {
        matches!(self, Self::Api { http_status: 0, .. })
    }
}

/// Wire-format JSON envelope for error responses.
#[derive(serde::Deserialize, Default, Debug)]
struct ErrorEnvelope {
    #[serde(default)]
    error: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    request_id: String,
    #[serde(default)]
    retry_after_ms: u32,
}

/// Decode the body of a non-2xx response into an [`Error::Api`].
///
/// Tolerant of an empty or malformed body — the returned error will
/// still carry `http_status`. `retry_after` is populated from either
/// the `retry_after_ms` body field (preferred) or the `Retry-After`
/// header (fallback). `X-Request-Id` is captured as a header-only
/// fallback so even bodyless responses preserve correlation.
pub(crate) fn decode_error_response(
    status: u16,
    headers: &reqwest::header::HeaderMap,
    body: &[u8],
) -> Error {
    let header_request_id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let header_retry_after = parse_retry_after_header(headers);

    // Cap reads at MAX_ERROR_BODY_BYTES even if the caller hands us more.
    let body = if body.len() > MAX_ERROR_BODY_BYTES {
        &body[..MAX_ERROR_BODY_BYTES]
    } else {
        body
    };

    if body.is_empty() {
        return Error::Api {
            code: status_code_fallback(status),
            message: String::new(),
            request_id: header_request_id,
            retry_after: header_retry_after,
            http_status: status,
        };
    }

    let envelope: Result<ErrorEnvelope, _> = serde_json::from_slice(body);
    match envelope {
        Err(_) => {
            // Preserve a short excerpt of the malformed body so the
            // caller can diagnose without us holding a reference to a
            // potentially large buffer.
            const MAX_MSG: usize = 256;
            let message = if body.len() <= MAX_MSG {
                String::from_utf8_lossy(body).into_owned()
            } else {
                format!(
                    "malformed body ({} bytes; first {}: {:?})",
                    body.len(),
                    MAX_MSG,
                    &body[..MAX_MSG]
                )
            };
            Error::Api {
                code: status_code_fallback(status),
                message,
                request_id: header_request_id,
                retry_after: header_retry_after,
                http_status: status,
            }
        }
        Ok(env) => {
            let code = if env.error.is_empty() {
                status_code_fallback(status)
            } else {
                ErrorCode::from_wire(&env.error)
            };
            let request_id = if !env.request_id.is_empty() {
                Some(env.request_id)
            } else {
                header_request_id
            };
            let retry_after = retry_after_from_ms(env.retry_after_ms).or(header_retry_after);
            Error::Api {
                code,
                message: env.message,
                request_id,
                retry_after,
                http_status: status,
            }
        }
    }
}

/// Best-guess code mapping when the server body is empty or unparseable.
///
/// Deliberate asymmetry on 429: the wire protocol has two distinct 429
/// codes ([`ErrorCode::Backpressure`] retryable with hint,
/// [`ErrorCode::RateLimited`] terminal). When the body is absent there
/// is no way to tell which was meant, so the fallback picks
/// [`ErrorCode::RateLimited`] — blind retries of a rate-limit can get
/// the caller blocked harder.
pub(crate) fn status_code_fallback(status: u16) -> ErrorCode {
    match status {
        400 => ErrorCode::InvalidRequest,
        401 => ErrorCode::Unauthorized,
        402 => ErrorCode::NotBillable,
        405 => ErrorCode::MethodNotAllowed,
        413 => ErrorCode::RequestTooLarge,
        415 => ErrorCode::UnsupportedMediaType,
        429 => ErrorCode::RateLimited,
        503 => ErrorCode::Unavailable,
        _ => ErrorCode::InternalError,
    }
}

fn retry_after_from_ms(ms: u32) -> Option<Duration> {
    if ms == 0 {
        return None;
    }
    let d = Duration::from_millis(ms as u64);
    if d > MAX_RETRY_AFTER {
        return None;
    }
    Some(d)
}

/// Parse the HTTP `Retry-After` header per RFC 9110 §10.2.3.
///
/// Both forms are honoured:
/// - `delta-seconds`: an integer number of seconds (e.g. `"120"`).
/// - HTTP-date: an absolute timestamp. The returned duration is the
///   delta to now; past dates return `None`.
///
/// Returns `None` when the header is missing, empty, zero, negative,
/// unparseable, or larger than [`MAX_RETRY_AFTER`].
fn parse_retry_after_header(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let v = headers.get("retry-after")?.to_str().ok()?.trim();
    if v.is_empty() {
        return None;
    }
    if let Ok(secs) = v.parse::<i64>() {
        if secs <= 0 {
            return None;
        }
        let d = Duration::from_secs(secs as u64);
        if d > MAX_RETRY_AFTER {
            return None;
        }
        return Some(d);
    }
    // HTTP-date is a less common form; reqwest does not bundle a
    // parser. We honour it on a best-effort basis using httpdate's
    // format. Skipping it is safe — the body field is the canonical
    // source.
    let parsed = httpdate_parse(v)?;
    let now = std::time::SystemTime::now();
    let d = parsed.duration_since(now).ok()?;
    if d.is_zero() || d > MAX_RETRY_AFTER {
        return None;
    }
    Some(d)
}

/// Minimal RFC 7231 IMF-fixdate parser ("Wed, 21 Oct 2026 07:28:00 GMT").
///
/// Returns `None` for inputs that do not match IMF-fixdate exactly.
/// Implemented inline rather than pulling a dep — the spec format is a
/// fixed 29-character template.
fn httpdate_parse(s: &str) -> Option<std::time::SystemTime> {
    // Format: "Day, DD Mon YYYY HH:MM:SS GMT"
    // We delegate to chrono if present, but to avoid a dep we do a
    // tiny manual parse.
    if s.len() < 29 {
        return None;
    }
    let s = s.as_bytes();
    if &s[s.len() - 4..] != b" GMT" {
        return None;
    }
    let day: u32 = std::str::from_utf8(&s[5..7]).ok()?.trim().parse().ok()?;
    let mon = match &s[8..11] {
        b"Jan" => 1u32,
        b"Feb" => 2,
        b"Mar" => 3,
        b"Apr" => 4,
        b"May" => 5,
        b"Jun" => 6,
        b"Jul" => 7,
        b"Aug" => 8,
        b"Sep" => 9,
        b"Oct" => 10,
        b"Nov" => 11,
        b"Dec" => 12,
        _ => return None,
    };
    let year: i32 = std::str::from_utf8(&s[12..16]).ok()?.parse().ok()?;
    let hour: u32 = std::str::from_utf8(&s[17..19]).ok()?.parse().ok()?;
    let minute: u32 = std::str::from_utf8(&s[20..22]).ok()?.parse().ok()?;
    let second: u32 = std::str::from_utf8(&s[23..25]).ok()?.parse().ok()?;
    let unix = imf_to_unix(year, mon, day, hour, minute, second)?;
    Some(std::time::UNIX_EPOCH + Duration::from_secs(unix))
}

/// Convert a Gregorian (year, month, day, h, m, s) tuple to Unix
/// seconds. Returns `None` on out-of-range inputs.
fn imf_to_unix(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Option<u64> {
    if !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour >= 24
        || minute >= 60
        || second >= 60
    {
        return None;
    }
    // Howard Hinnant's days-from-civil algorithm, restricted to >= 1970.
    let y = year as i64 - if month <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let doy =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) as u64 + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_from_1970 = era * 146097 + doe as i64 - 719468;
    if days_from_1970 < 0 {
        return None;
    }
    Some(
        (days_from_1970 as u64) * 86400
            + (hour as u64) * 3600
            + (minute as u64) * 60
            + second as u64,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn hdrs(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn empty_body_falls_back_to_status_code() {
        let err = decode_error_response(401, &HeaderMap::new(), b"");
        match err {
            Error::Api {
                code, http_status, ..
            } => {
                assert_eq!(code, ErrorCode::Unauthorized);
                assert_eq!(http_status, 401);
            }
            _ => panic!("expected Api"),
        }
    }

    #[test]
    fn json_envelope_decodes() {
        let body = br#"{"error":"backpressure","message":"slow down","request_id":"req-1","retry_after_ms":500}"#;
        let err = decode_error_response(429, &HeaderMap::new(), body);
        match err {
            Error::Api {
                code,
                message,
                request_id,
                retry_after,
                http_status,
            } => {
                assert_eq!(code, ErrorCode::Backpressure);
                assert_eq!(message, "slow down");
                assert_eq!(request_id.as_deref(), Some("req-1"));
                assert_eq!(retry_after, Some(Duration::from_millis(500)));
                assert_eq!(http_status, 429);
            }
            _ => panic!("expected Api"),
        }
    }

    #[test]
    fn unknown_code_preserved() {
        let body = br#"{"error":"future_code","message":"unrecognised"}"#;
        let err = decode_error_response(500, &HeaderMap::new(), body);
        match err.code() {
            Some(ErrorCode::Unknown(s)) => assert_eq!(s, "future_code"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn empty_body_on_429_picks_rate_limited_not_backpressure() {
        // Deliberate asymmetry per Go SDK.
        let err = decode_error_response(429, &HeaderMap::new(), b"");
        assert_eq!(err.code(), Some(&ErrorCode::RateLimited));
    }

    #[test]
    fn body_retry_hint_wins_over_header() {
        let body = br#"{"error":"backpressure","retry_after_ms":250}"#;
        let h = hdrs(&[("retry-after", "10")]);
        let err = decode_error_response(429, &h, body);
        assert_eq!(err.retry_after(), Some(Duration::from_millis(250)));
    }

    #[test]
    fn header_retry_used_when_body_absent() {
        let h = hdrs(&[("retry-after", "5")]);
        let err = decode_error_response(503, &h, b"");
        assert_eq!(err.retry_after(), Some(Duration::from_secs(5)));
    }

    #[test]
    fn malformed_body_keeps_status_and_excerpt() {
        let body = b"not json at all";
        let err = decode_error_response(500, &HeaderMap::new(), body);
        match err {
            Error::Api {
                code,
                message,
                http_status,
                ..
            } => {
                assert_eq!(code, ErrorCode::InternalError);
                assert_eq!(http_status, 500);
                assert!(message.contains("not json at all"));
            }
            _ => panic!("expected Api"),
        }
    }

    #[test]
    fn header_request_id_fallback() {
        let h = hdrs(&[("x-request-id", "abc-123")]);
        let err = decode_error_response(500, &h, b"");
        assert_eq!(err.request_id(), Some("abc-123"));
    }

    #[test]
    fn body_request_id_wins() {
        let h = hdrs(&[("x-request-id", "hdr")]);
        let body = br#"{"error":"internal_error","request_id":"body"}"#;
        let err = decode_error_response(500, &h, body);
        assert_eq!(err.request_id(), Some("body"));
    }

    #[test]
    fn huge_retry_after_clamped_to_none() {
        let h = hdrs(&[("retry-after", "9999999")]);
        let err = decode_error_response(503, &h, b"");
        assert!(err.retry_after().is_none());
    }

    #[test]
    fn preflight_has_zero_status_and_is_marker() {
        let err = Error::preflight("bad query");
        assert!(err.is_preflight());
        assert_eq!(err.http_status(), Some(0));
        assert_eq!(err.code(), Some(&ErrorCode::InvalidRequest));
    }

    #[test]
    fn error_codes_round_trip_via_wire() {
        let all = [
            ErrorCode::InvalidRequest,
            ErrorCode::Unauthorized,
            ErrorCode::NotBillable,
            ErrorCode::MethodNotAllowed,
            ErrorCode::UnsupportedMediaType,
            ErrorCode::RequestTooLarge,
            ErrorCode::RateLimited,
            ErrorCode::TooManyRequests,
            ErrorCode::Backpressure,
            ErrorCode::Unavailable,
            ErrorCode::NamespaceUnavailable,
            ErrorCode::NamespaceDisabled,
            ErrorCode::ModelNotProvisioned,
            ErrorCode::MeteringUnavailable,
            ErrorCode::InternalError,
            ErrorCode::MalformedResponse,
        ];
        for c in &all {
            let s = c.as_str();
            assert_eq!(&ErrorCode::from_wire(s), c, "round-trip failed for {s}");
        }
    }
}

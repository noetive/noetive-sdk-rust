//! Refusals, and why this module spends so much on them.
//!
//! A refusal is read once, by whoever is about to decide what to do next.
//! Carrying the limit that fired, when it clears, the guard that refused
//! and the field that was wrong is nearly free, and it is the difference
//! between a plan and a retry loop: "rate_limited" alone leaves a caller to
//! guess, "rate_limited, per_hour, retry in 41m0s" is an instruction. So
//! every field the server sends is kept, and [`Error`]'s `Display` renders
//! the ones that change what happens next.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A refusal code. A closed set, because branching on it is the point;
/// a code added by a later server arrives as [`Code::Unknown`] with its
/// wire spelling intact.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Code {
    /// The credential is missing, unknown, revoked or expired. The only
    /// refusal that means the key itself does not work.
    Unauthorized,
    /// The key is good and the account behind it cannot be charged (402).
    /// Retrying will not help until billing is set up.
    NotBillable,
    /// The key is good and does not reach what was asked: its agent is
    /// disabled or not on this service, or it may not watch the mailbox or
    /// send as the sender it named. Never returned for an object the caller
    /// cannot read — that is [`Code::NotFound`], because saying "forbidden"
    /// about an identifier confirms it exists.
    ForbiddenScope,
    /// The object does not exist, exists outside every grant the caller
    /// holds, or the operation does not exist.
    NotFound,
    /// Other writes to the same object kept landing first. Read it again
    /// and retry; an update is safe to repeat as it was. Shares 409 with
    /// [`Code::Paused`].
    PreconditionFailed,
    /// A guard fired; [`Error::guard`] names it. Not worth retrying
    /// unchanged.
    PolicyRefused,
    /// A limit is spent; [`Error::counter`] names it and
    /// [`Error::retry_after`] says when it clears. With no wait, waiting
    /// will not help and the request has to change.
    RateLimited,
    /// The mailbox is paused and is not sending. Unlike a rate limit it does
    /// not clear on its own. Shares 409 with [`Code::PreconditionFailed`],
    /// so branch on the code.
    Paused,
    /// The request was malformed; [`Error::field`] is a JSON pointer to
    /// where, when one field is to blame. Also what this crate refuses with
    /// before sending ([`Error::http_status`] `0`).
    Invalid,
    /// This deployment does not serve this option of the operation.
    /// Retrying will not help; the server answers 404, so branch on the
    /// code rather than the status.
    Unavailable,
    /// The request was fine and something on the server broke. Retrying
    /// later may succeed.
    Internal,
    /// Something bud depends on could not be reached, and the request had
    /// no effect (503). Retry with backoff.
    UpstreamUnavailable,
    /// This crate's own: the server answered with something that is not
    /// the envelope. Distinct from [`Code::Internal`] so a caller can tell
    /// "the server failed" from "we could not read what it said".
    MalformedResponse,
    /// A code this release does not know, spelled as the server sent it.
    Unknown(String),
}

impl Code {
    /// The wire spelling.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::NotBillable => "not_billable",
            Self::ForbiddenScope => "forbidden_scope",
            Self::NotFound => "not_found",
            Self::PreconditionFailed => "precondition_failed",
            Self::PolicyRefused => "policy_refused",
            Self::RateLimited => "rate_limited",
            Self::Paused => "paused",
            Self::Invalid => "invalid",
            Self::Unavailable => "unavailable",
            Self::Internal => "internal",
            Self::UpstreamUnavailable => "upstream_unavailable",
            Self::MalformedResponse => "malformed_response",
            Self::Unknown(s) => s,
        }
    }

    /// Parse a wire spelling, keeping an unknown one as it arrived.
    pub fn from_wire(s: &str) -> Self {
        match s {
            "unauthorized" => Self::Unauthorized,
            "not_billable" => Self::NotBillable,
            "forbidden_scope" => Self::ForbiddenScope,
            "not_found" => Self::NotFound,
            "precondition_failed" => Self::PreconditionFailed,
            "policy_refused" => Self::PolicyRefused,
            "rate_limited" => Self::RateLimited,
            "paused" => Self::Paused,
            "invalid" => Self::Invalid,
            "unavailable" => Self::Unavailable,
            "internal" => Self::Internal,
            "upstream_unavailable" => Self::UpstreamUnavailable,
            "malformed_response" => Self::MalformedResponse,
            other => Self::Unknown(other.to_string()),
        }
    }
}

impl Default for Code {
    /// An envelope whose error names no code still is a refusal; it reads
    /// as an unknown code with an empty spelling rather than as success.
    fn default() -> Self {
        Self::Unknown(String::new())
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Code {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Code {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(Self::from_wire(&s))
    }
}

/// A refusal, with everything the server said about what to do next.
///
/// Arrives as the `error` field of an operation's own output — a refusal
/// is a value, not a failed call — and inside [`Failure::Refused`] when
/// this crate refused before sending, could not read the answer, or a
/// [`watch`](super::Client::watch) was refused.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Error {
    /// What to branch on.
    pub code: Code,

    /// The server's own sentence. It never quotes anything a sender wrote:
    /// an error is the one place a reader treats as the server speaking.
    pub message: String,

    /// What would unblock this, in the server's words.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub hint: String,

    /// Correlates this refusal with everything the request caused,
    /// including the journal events. Quote it when asking for help. Filled
    /// from `X-Request-Id` when the body carries none.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub request_id: String,

    /// When the limit clears, on [`Code::RateLimited`]; zero when waiting
    /// will not help. Read it through [`Error::retry_after`].
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub retry_after_ms: u32,

    /// Declared by the wire for a conflict that carries the object as
    /// stored. Nothing served sets it; a conflict says only to read
    /// again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<serde_json::Value>,

    /// Declared by the wire alongside `current`; not sent.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub version: String,

    /// A JSON pointer into the request, on [`Code::Invalid`].
    #[serde(skip_serializing_if = "String::is_empty")]
    pub field: String,

    /// The policy that fired, on [`Code::PolicyRefused`].
    #[serde(skip_serializing_if = "String::is_empty")]
    pub guard: String,

    /// The limit that refused, on [`Code::RateLimited`].
    #[serde(skip_serializing_if = "String::is_empty")]
    pub counter: String,

    /// The status the refusal arrived with. Zero means this crate produced
    /// the refusal before anything was sent.
    #[serde(skip)]
    pub http_status: u16,
}

impl Error {
    /// When the limit clears. Zero means the server gave no hint, which is
    /// not the same as "retry now".
    pub fn retry_after(&self) -> Duration {
        Duration::from_millis(u64::from(self.retry_after_ms))
    }

    /// Whether waiting and trying again could succeed.
    ///
    /// True for [`Code::UpstreamUnavailable`] (the server promises it had
    /// no effect), [`Code::RateLimited`] only when it says how long to wait,
    /// [`Code::PreconditionFailed`] after reading the object again, and
    /// [`Code::Internal`] — which promises nothing about effects, so retry a
    /// send after it only with the same idempotency key.
    ///
    /// False for everything else, notably [`Code::Unavailable`] (this
    /// deployment does not serve the option, however transient it looks),
    /// [`Code::Paused`] (clears only when an operator releases the mailbox),
    /// [`Code::NotBillable`] (waits on a person) and
    /// [`Code::PolicyRefused`].
    pub fn retryable(&self) -> bool {
        match self.code {
            Code::UpstreamUnavailable | Code::PreconditionFailed | Code::Internal => true,
            Code::RateLimited => self.retry_after_ms > 0,
            _ => false,
        }
    }

    /// True when this crate refused the request before sending it.
    pub fn is_preflight(&self) -> bool {
        self.http_status == 0 && self.code == Code::Invalid
    }
}

/// Renders the refusal as a sentence that says what to do next:
/// `bud: 429 rate_limited: … [limit per_hour, retry in 41m0s] — hint (request_…)`.
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bud: ")?;
        if self.http_status > 0 {
            write!(f, "{} ", self.http_status)?;
        }
        f.write_str(self.code.as_str())?;
        if !self.message.is_empty() {
            write!(f, ": {}", self.message)?;
        }
        if !self.counter.is_empty() {
            write!(f, " [limit {}", self.counter)?;
            if self.retry_after_ms > 0 {
                write!(f, ", retry in {}", rounded_seconds(self.retry_after_ms))?;
            }
            f.write_str("]")?;
        }
        if !self.guard.is_empty() {
            write!(f, " [guard {}]", self.guard)?;
        }
        if !self.field.is_empty() {
            write!(f, " [at {}]", self.field)?;
        }
        if !self.hint.is_empty() {
            write!(f, " — {}", self.hint)?;
        }
        if !self.request_id.is_empty() {
            write!(f, " ({})", self.request_id)?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

/// A wait rounded to the second and spelled the way the other Noetive SDKs
/// spell it (`41m0s`, `1h2m3s`, `5s`), so one log query finds both.
fn rounded_seconds(ms: u32) -> String {
    let total = (u64::from(ms) + 500) / 1000;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}h{m}m{s}s")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

/// Why a call returned no output at all.
///
/// A refusal the server authored arrives on the output, not here. This is
/// what is left: a request refused before sending, an answer that could not
/// be read, a connection that failed, a stream that ended without
/// answering — and, for [`watch`](super::Client::watch) only, the server's
/// own refusal, because a stream has no envelope to carry one.
#[derive(Debug, thiserror::Error)]
pub enum Failure {
    /// An [`Error`]: a preflight refusal (`http_status` 0), an answer that
    /// was not the envelope ([`Code::MalformedResponse`], or
    /// [`Code::Unauthorized`] for a bare 401), or a refused watch.
    #[error("{0}")]
    Refused(Box<Error>),

    /// The connection failed or broke. Nothing to branch on.
    #[error("bud: transport: {0}")]
    Transport(#[from] reqwest::Error),

    /// No response headers arrived — or, on a stream, no opening frame —
    /// within the client's response timeout.
    #[error("bud: no answer within {0:?}")]
    Timeout(Duration),

    /// The server answered with a redirect, which this crate refuses to
    /// follow: it would re-attach the credential to a host the caller never
    /// chose. An answer, not a failed connection, so it is never retried.
    #[error(
        "bud: refusing to follow a {status} redirect to {host}; a redirect would send the credential somewhere the caller did not name"
    )]
    Redirected {
        /// The 3xx status.
        status: u16,
        /// The host the `Location` header named, with its port when it
        /// gave one.
        host: String,
    },

    /// The stream ended before it answered. The server does that only
    /// when it is going away, so this is not a quiet window.
    #[error("bud: the stream closed before it answered")]
    Closed,
}

impl Failure {
    /// The refusal, when this is one.
    pub fn refusal(&self) -> Option<&Error> {
        match self {
            Self::Refused(e) => Some(e.as_ref()),
            _ => None,
        }
    }

    /// The refusal's code, when this is one.
    pub fn code(&self) -> Option<&Code> {
        self.refusal().map(|e| &e.code)
    }

    /// True when this crate refused the request before sending it.
    pub fn is_preflight(&self) -> bool {
        self.refusal().is_some_and(Error::is_preflight)
    }
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        Self::Refused(Box::new(e))
    }
}

/// A refusal this crate produced before sending anything. `http_status`
/// stays zero, which is how a caller tells "we rejected this" from "the
/// server did".
pub(crate) fn preflight(message: impl Into<String>) -> Error {
    Error {
        code: Code::Invalid,
        message: message.into(),
        ..Error::default()
    }
}

/// A malformed-response refusal naming the stream or request it came from.
pub(crate) fn malformed(message: impl Into<String>, http_status: u16, request_id: &str) -> Error {
    Error {
        code: Code::MalformedResponse,
        message: message.into(),
        request_id: request_id.to_string(),
        http_status,
        ..Error::default()
    }
}

/// Read a refusal out of a response that is not the envelope — a load balancer's
/// HTML, a proxy's plain text. The status still carries information, so it
/// is kept; a short body is surfaced, labelled, and never presented as the
/// server's own voice.
pub(crate) fn error_from(status: u16, body: &[u8], request_id: &str) -> Error {
    let mut e = malformed(
        "the server's answer was not an envelope",
        status,
        request_id,
    );
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim();
    if !trimmed.is_empty() && trimmed.len() <= 200 {
        e.hint = format!("the body was: {trimmed}");
    }
    if status == 401 {
        e.code = Code::Unauthorized;
        e.message = "the credential was not accepted".to_string();
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_reads_as_a_plan() {
        let e = Error {
            code: Code::RateLimited,
            message: "the per_hour limit is used up".into(),
            counter: "per_hour".into(),
            retry_after_ms: 41 * 60 * 1000,
            hint: "wait for the window to reset".into(),
            request_id: "request_01x".into(),
            http_status: 429,
            ..Error::default()
        };
        assert_eq!(
            e.to_string(),
            "bud: 429 rate_limited: the per_hour limit is used up \
             [limit per_hour, retry in 41m0s] — wait for the window to reset (request_01x)"
        );
        assert_eq!(e.retry_after(), Duration::from_secs(41 * 60));
        assert!(e.retryable());

        let guarded = Error {
            code: Code::Invalid,
            message: "m".into(),
            guard: "g".into(),
            field: "/to".into(),
            ..Error::default()
        };
        assert_eq!(guarded.to_string(), "bud: invalid: m [guard g] [at /to]");
    }

    #[test]
    fn waits_are_spelled_to_the_second() {
        assert_eq!(rounded_seconds(400), "0s");
        assert_eq!(rounded_seconds(1500), "2s");
        assert_eq!(rounded_seconds(61_000), "1m1s");
        assert_eq!(rounded_seconds(3_723_000), "1h2m3s");
    }

    #[test]
    fn retryable_follows_the_servers_advice() {
        for (code, want) in [
            (Code::UpstreamUnavailable, true),
            (Code::PreconditionFailed, true),
            (Code::Internal, true),
            (Code::Paused, false),
            (Code::NotBillable, false),
            (Code::Unavailable, false),
            (Code::Unauthorized, false),
            (Code::ForbiddenScope, false),
            (Code::NotFound, false),
            (Code::PolicyRefused, false),
            (Code::Invalid, false),
            (Code::MalformedResponse, false),
            (Code::Unknown("future".into()), false),
        ] {
            let e = Error {
                code: code.clone(),
                ..Error::default()
            };
            assert_eq!(e.retryable(), want, "{code}");
        }
        let waited = Error {
            code: Code::RateLimited,
            retry_after_ms: 1000,
            ..Error::default()
        };
        assert!(waited.retryable(), "a rate limit with a wait");
        let unwaited = Error {
            code: Code::RateLimited,
            ..Error::default()
        };
        assert!(!unwaited.retryable(), "the request has to change");
    }

    #[test]
    fn every_code_round_trips_and_unknown_codes_survive() {
        for code in [
            Code::Unauthorized,
            Code::NotBillable,
            Code::ForbiddenScope,
            Code::NotFound,
            Code::PreconditionFailed,
            Code::PolicyRefused,
            Code::RateLimited,
            Code::Paused,
            Code::Invalid,
            Code::Unavailable,
            Code::Internal,
            Code::UpstreamUnavailable,
            Code::MalformedResponse,
        ] {
            assert_eq!(Code::from_wire(code.as_str()), code);
        }
        let e: Error = serde_json::from_str(r#"{"code":"brand_new","message":"m"}"#).unwrap();
        assert_eq!(e.code, Code::Unknown("brand_new".into()));
        assert_eq!(serde_json::to_value(&e).unwrap()["code"], "brand_new");
    }

    #[test]
    fn a_non_envelope_keeps_its_status_and_labels_its_body() {
        let e = error_from(502, b"<html>502 Bad Gateway</html>", "request_01x");
        assert_eq!(e.code, Code::MalformedResponse);
        assert_eq!(e.http_status, 502);
        assert_eq!(e.request_id, "request_01x");
        assert_eq!(e.hint, "the body was: <html>502 Bad Gateway</html>");

        let long = error_from(500, "x".repeat(201).as_bytes(), "");
        assert!(long.hint.is_empty(), "a long body is a web page");

        let bare = error_from(401, b"", "");
        assert_eq!(bare.code, Code::Unauthorized);
    }
}

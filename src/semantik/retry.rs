use std::time::Duration;

use crate::semantik::error::{Error, ErrorCode};

/// Decide whether a failed request should be retried and how long to
/// wait before the next attempt.
///
/// The trait exists so callers can plug in bespoke policies through
/// [`super::ClientBuilder::retry`] — e.g. a token-bucket shaper or a
/// domain-specific jitter schedule — without having to fork the SDK.
///
/// `attempt` is the zero-based attempt number (`0` for the first
/// retry decision after the initial failure). The returned `Some(d)`
/// instructs the SDK to wait `d` and retry; `None` aborts the call
/// with the error.
///
/// The original future passed to the SDK call bounds the whole retry
/// loop: cancelling it (e.g. via [`tokio::time::timeout`] or dropping)
/// also cancels any pending backoff sleep.
pub trait RetryPolicy: Send + Sync + 'static {
    fn should_retry(&self, attempt: u32, err: &Error) -> Option<Duration>;
}

/// Disables retry entirely. Pass to [`super::ClientBuilder::retry`] to
/// opt out of the SDK's built-in retry — useful when the caller wraps
/// requests in their own retry loop or needs strict one-shot
/// semantics.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoRetry;

impl RetryPolicy for NoRetry {
    fn should_retry(&self, _attempt: u32, _err: &Error) -> Option<Duration> {
        None
    }
}

/// Retries up to `max_attempts` times on transient server conditions
/// the API documents as safe to re-issue.
///
/// Retried codes:
/// - [`ErrorCode::Backpressure`]
/// - [`ErrorCode::Unavailable`]
/// - [`ErrorCode::NamespaceUnavailable`]
/// - [`ErrorCode::MeteringUnavailable`]
///
/// [`ErrorCode::ModelNotProvisioned`] is retried only when the server
/// sends a retry hint. The code is returned both for a namespace that
/// will never carry the requested `(model, dimensions)` pair and for
/// one that is provisioned but not yet ready to serve; the hint is what
/// separates the waitable case from the permanent one.
///
/// The server's retry hint ([`Error::retry_after`]) is honoured when
/// present; when the hint is missing the SDK falls back to a
/// `100ms, 2s, 5s, 10s` schedule, saturating at 10s for later attempts.
/// The first 100ms step keeps the common-case latency low when a hiccup
/// clears immediately; the longer tail gives upstream dependencies time
/// to recover when they actually need it.
///
/// Errors outside this set are never retried. In particular
/// [`ErrorCode::NotBillable`] is terminal (retrying will keep
/// failing) and [`ErrorCode::RateLimited`] /
/// [`ErrorCode::TooManyRequests`] signal a hard rate-limit where blind
/// retries are harmful.
///
/// `max_attempts == 0` disables retry. This policy is safe to pair
/// with publish requests that carry an `idempotency_key`; retrying a
/// publish without one risks duplicate delivery.
#[derive(Debug, Clone, Copy)]
pub struct TransientRetry {
    max_attempts: u32,
}

impl TransientRetry {
    pub fn new(max_attempts: u32) -> Self {
        Self { max_attempts }
    }
}

/// Fallback backoff sequence when the server omits a retry hint.
/// A 100ms first step keeps the common-case latency low when a
/// transient hiccup clears immediately, then the schedule widens to
/// give upstream layers time to recover. Indices beyond the last entry
/// saturate at the final value (10s).
const BACKOFF_SCHEDULE: [Duration; 4] = [
    Duration::from_millis(100),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
];

pub(crate) fn fallback_backoff(attempt: u32) -> Duration {
    let idx = (attempt as usize).min(BACKOFF_SCHEDULE.len() - 1);
    BACKOFF_SCHEDULE[idx]
}

impl RetryPolicy for TransientRetry {
    fn should_retry(&self, attempt: u32, err: &Error) -> Option<Duration> {
        if attempt >= self.max_attempts {
            return None;
        }
        let Error::Api {
            code, retry_after, ..
        } = err
        else {
            return None;
        };
        match code {
            ErrorCode::Backpressure
            | ErrorCode::Unavailable
            | ErrorCode::NamespaceUnavailable
            | ErrorCode::MeteringUnavailable => {
                // Prefer the server's hint; fall back to the bounded
                // linear schedule when the server, a proxy, or a
                // truncated body dropped it.
                Some(retry_after.unwrap_or_else(|| fallback_backoff(attempt)))
            }
            ErrorCode::ModelNotProvisioned => {
                // One code covers two conditions the client cannot tell
                // apart: a namespace that will never carry this
                // (model, dimensions) pair, and one that is provisioned
                // but not yet ready to serve. The server distinguishes
                // them by sending a retry hint on the second, so the
                // hint — not the code — decides. Absent a hint the pair
                // is treated as permanently unsatisfiable and fails
                // fast, because waiting cannot conjure a pairing that
                // was never provisioned.
                //
                // No fallback_backoff here: unlike the transient codes
                // above, a missing hint is meaningful rather than lost.
                *retry_after
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_err(code: ErrorCode, retry_after: Option<Duration>) -> Error {
        Error::Api {
            code,
            message: String::new(),
            request_id: None,
            retry_after,
            http_status: 503,
        }
    }

    #[test]
    fn no_retry_returns_none() {
        let policy = NoRetry;
        let err = api_err(ErrorCode::Backpressure, Some(Duration::from_secs(1)));
        assert!(policy.should_retry(0, &err).is_none());
    }

    #[test]
    fn transient_retry_retries_documented_codes() {
        let policy = TransientRetry::new(5);
        for code in [
            ErrorCode::Backpressure,
            ErrorCode::Unavailable,
            ErrorCode::NamespaceUnavailable,
            ErrorCode::MeteringUnavailable,
        ] {
            let err = api_err(code.clone(), None);
            assert!(
                policy.should_retry(0, &err).is_some(),
                "code {code:?} should retry"
            );
        }
    }

    #[test]
    fn transient_retry_skips_terminal_codes() {
        let policy = TransientRetry::new(5);
        for code in [
            ErrorCode::Unauthorized,
            ErrorCode::NotBillable,
            ErrorCode::InvalidRequest,
            ErrorCode::RateLimited,
            ErrorCode::TooManyRequests,
            ErrorCode::RequestTooLarge,
        ] {
            let err = api_err(code.clone(), None);
            assert!(
                policy.should_retry(0, &err).is_none(),
                "code {code:?} should NOT retry"
            );
        }
    }

    // model_not_provisioned is returned both while a pairing is still
    // becoming ready and for one that will never exist. The retry hint
    // is the only thing separating them, so pin both sides of the gate.

    #[test]
    fn model_not_provisioned_without_hint_is_terminal() {
        let policy = TransientRetry::new(5);
        let err = api_err(ErrorCode::ModelNotProvisioned, None);
        assert!(
            policy.should_retry(0, &err).is_none(),
            "no hint means the pairing will not appear on its own"
        );
    }

    #[test]
    fn model_not_provisioned_with_hint_retries_at_hint() {
        let policy = TransientRetry::new(5);
        let hint = Duration::from_millis(2000);
        let err = api_err(ErrorCode::ModelNotProvisioned, Some(hint));
        assert_eq!(
            policy.should_retry(0, &err),
            Some(hint),
            "a hinted pairing is still becoming ready; honour the server's number"
        );
    }

    #[test]
    fn model_not_provisioned_with_hint_never_falls_back_to_schedule() {
        // The fallback schedule would mask a missing hint as a retry.
        // Later attempts must keep using the hint, not drift onto the
        // backoff table.
        let policy = TransientRetry::new(5);
        let hint = Duration::from_millis(2000);
        let err = api_err(ErrorCode::ModelNotProvisioned, Some(hint));
        assert_eq!(policy.should_retry(3, &err), Some(hint));
    }

    #[test]
    fn model_not_provisioned_with_hint_still_respects_max_attempts() {
        let policy = TransientRetry::new(2);
        let err = api_err(
            ErrorCode::ModelNotProvisioned,
            Some(Duration::from_millis(500)),
        );
        assert!(policy.should_retry(1, &err).is_some());
        assert!(
            policy.should_retry(2, &err).is_none(),
            "hint does not mean unbounded"
        );
    }

    #[test]
    fn terminal_code_with_stray_hint_stays_terminal() {
        // Only model_not_provisioned opts into hint-gated retry; a hint
        // on a genuinely terminal code must not make it retriable.
        let policy = TransientRetry::new(5);
        let err = api_err(ErrorCode::NotBillable, Some(Duration::from_millis(500)));
        assert!(policy.should_retry(0, &err).is_none());
    }

    #[test]
    fn transient_retry_caps_attempts() {
        let policy = TransientRetry::new(3);
        let err = api_err(ErrorCode::Unavailable, None);
        assert!(policy.should_retry(0, &err).is_some());
        assert!(policy.should_retry(2, &err).is_some());
        assert!(policy.should_retry(3, &err).is_none()); // hit max
        assert!(policy.should_retry(10, &err).is_none());
    }

    #[test]
    fn transient_retry_zero_attempts_disables() {
        let policy = TransientRetry::new(0);
        let err = api_err(ErrorCode::Backpressure, None);
        assert!(policy.should_retry(0, &err).is_none());
    }

    #[test]
    fn server_hint_preferred_over_fallback() {
        let policy = TransientRetry::new(5);
        let err = api_err(ErrorCode::Backpressure, Some(Duration::from_millis(250)));
        assert_eq!(
            policy.should_retry(0, &err),
            Some(Duration::from_millis(250))
        );
    }

    #[test]
    fn fallback_schedule_matches_100ms_2s_5s_10s_and_saturates() {
        assert_eq!(fallback_backoff(0), Duration::from_millis(100));
        assert_eq!(fallback_backoff(1), Duration::from_secs(2));
        assert_eq!(fallback_backoff(2), Duration::from_secs(5));
        assert_eq!(fallback_backoff(3), Duration::from_secs(10));
        assert_eq!(fallback_backoff(4), Duration::from_secs(10));
        assert_eq!(fallback_backoff(100), Duration::from_secs(10));
    }

    #[test]
    fn non_api_errors_are_not_retried() {
        let policy = TransientRetry::new(5);
        let err = Error::InvalidApiKey;
        assert!(policy.should_retry(0, &err).is_none());
    }
}

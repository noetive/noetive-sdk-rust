//! Retries, and why there are almost none.
//!
//! A write changes the world. `send` is safe to re-issue only with an
//! idempotency key the caller chose; `message.update` is safe to repeat as
//! it was, but a repeat can overwrite labels another agent wrote in between,
//! so it goes through the same gate. This crate cannot invent a key: one it
//! generated would differ across a process restart, so a genuine retry would
//! send a second copy while two distinct calls would collapse into one.
//!
//! So the rule is narrow and mechanical:
//!
//! - A refusal is never retried. It is an answer, and the envelope already
//!   says what to do instead.
//! - A wait whose window closed empty is never retried. It succeeded.
//! - Only a connection that failed before any response arrived is retried.
//! - A write is retried only when it carried an idempotency key. The client
//!   applies this gate before consulting the policy, so no policy — the
//!   built-in ones or a caller's own — can widen it.

use std::time::Duration;

/// Decides whether to re-issue a request whose connection failed before
/// any response arrived.
///
/// Consulted only for a request the idempotency gate already allows: a
/// read, or a write carrying a key. A refusal never reaches a policy.
pub trait RetryPolicy: Send + Sync + 'static {
    /// Whether retry number `attempt` (zero-based) of operation `op` may go
    /// ahead. `op` is the operation's path, such as `"send"` or `"watch"`.
    fn should_retry(&self, attempt: u32, op: &str) -> bool;

    /// The pause before retry number `attempt`. Dropping the call's future
    /// cancels it.
    fn backoff(&self, attempt: u32) -> Duration;
}

/// Issues every request exactly once.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoRetry;

impl RetryPolicy for NoRetry {
    fn should_retry(&self, _attempt: u32, _op: &str) -> bool {
        false
    }

    fn backoff(&self, _attempt: u32) -> Duration {
        Duration::ZERO
    }
}

/// The pause schedule [`TransientRetry`] uses when given none: short,
/// because the only thing retried is a connection that did not open.
pub const DEFAULT_BACKOFF: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_secs(1),
    Duration::from_secs(3),
];

/// Re-issues a request at most `attempts` times. The default policy is one
/// retry on [`DEFAULT_BACKOFF`].
#[derive(Debug, Clone)]
pub struct TransientRetry {
    /// How many retries, not how many tries. Zero behaves as [`NoRetry`].
    pub attempts: u32,
    /// The pause before each retry; the last entry repeats. Empty means
    /// [`DEFAULT_BACKOFF`].
    pub backoff: Vec<Duration>,
}

impl TransientRetry {
    /// Up to `attempts` retries on [`DEFAULT_BACKOFF`].
    pub fn new(attempts: u32) -> Self {
        Self {
            attempts,
            backoff: Vec::new(),
        }
    }

    /// Replace the pause schedule.
    pub fn with_backoff(mut self, backoff: Vec<Duration>) -> Self {
        self.backoff = backoff;
        self
    }
}

impl Default for TransientRetry {
    fn default() -> Self {
        Self::new(1)
    }
}

impl RetryPolicy for TransientRetry {
    fn should_retry(&self, attempt: u32, _op: &str) -> bool {
        attempt < self.attempts
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let schedule: &[Duration] = if self.backoff.is_empty() {
            &DEFAULT_BACKOFF
        } else {
            &self.backoff
        };
        let i = (attempt as usize).min(schedule.len() - 1);
        schedule[i]
    }
}

/// Whether an operation changes anything.
///
/// By name, from the closed set this crate calls. Unclassified means
/// "writes", the safe direction: adding an operation cannot quietly make it
/// retryable.
pub(crate) fn writes(op: &str) -> bool {
    !matches!(
        op,
        "me.describe"
            | "health"
            | "catalog.describe"
            | "folder.list"
            | "message.describe"
            | "thread.describe"
            | "part.describe"
            | "mailbox.describe"
            | "correspondent.list"
            | "addressbook.describe"
            | "help.describe"
            | "watch"
    )
}

/// The idempotency gate: a read is repeatable, a write only with a key the
/// caller chose.
pub(crate) fn repeatable(op: &str, idempotency_key: &str) -> bool {
    !writes(op) || !idempotency_key.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_need_a_key_reads_do_not() {
        assert!(repeatable("me.describe", ""));
        assert!(repeatable("addressbook.describe", ""));
        assert!(repeatable("watch", ""));
        assert!(!repeatable("send", ""));
        assert!(!repeatable("message.update", ""));
        assert!(!repeatable("something.new", ""));
        assert!(repeatable("send", "k1"));
    }

    #[test]
    fn the_schedule_saturates() {
        let p = TransientRetry::default();
        assert_eq!(p.backoff(0), Duration::from_millis(100));
        assert_eq!(p.backoff(1), Duration::from_secs(1));
        assert_eq!(p.backoff(9), Duration::from_secs(3));
        assert!(p.should_retry(0, "health"));
        assert!(!p.should_retry(1, "health"));
        assert!(!TransientRetry::new(0).should_retry(0, "health"));
    }
}

//! Configure a custom retry policy. The default
//! `TransientRetry::new(5)` is a reasonable starting point; raise it
//! for batch workloads, lower it for latency-sensitive calls, or
//! install a fully custom [`RetryPolicy`].
//!
//! ```text
//! NOETIVE_KEY_SECRET=keyu_... cargo run --example custom_retry
//! ```

use std::time::Duration;

use noetive::semantik::{Client, Error, RetryPolicy, SearchRequest, TransientRetry};

/// A toy custom policy: retry transient errors with a fixed 250 ms
/// delay, up to two attempts. Real-world policies might add jitter,
/// circuit-breaking, or per-code shaping.
struct FixedDelayRetry {
    max_attempts: u32,
    delay: Duration,
}

impl RetryPolicy for FixedDelayRetry {
    fn should_retry(&self, attempt: u32, err: &Error) -> Option<Duration> {
        if attempt >= self.max_attempts {
            return None;
        }
        // Only retry the documented transient codes; fall back to
        // TransientRetry to centralise that decision.
        TransientRetry::new(u32::MAX)
            .should_retry(attempt, err)
            .map(|_| self.delay)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::builder()
        .api_key(std::env::var("NOETIVE_KEY_SECRET")?)
        .read_timeout(Duration::from_secs(15))
        .retry(FixedDelayRetry {
            max_attempts: 2,
            delay: Duration::from_millis(250),
        })
        .build()?;

    let res = client
        .search(SearchRequest {
            query: r#"MATCH DISTANCE("rust") WITHIN 0.4 LIMIT 3"#.into(),
            namespace: "global".into(),
            model: "Qwen3-Embedding-4B".into(),
            dimensions: 1024,
            ..Default::default()
        })
        .await?;

    for (i, r) in res.results.iter().enumerate() {
        println!("{i:>2}  score={:.3}  id={}", r.score, r.message_id);
    }
    Ok(())
}

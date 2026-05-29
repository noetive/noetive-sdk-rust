//! Unauthenticated liveness probe.
//!
//! ```text
//! cargo run --example health
//! NOETIVE_BASE_URL=https://staging.semantik.noetive.io cargo run --example health
//! ```
//!
//! Exits with code 0 on 200, 1 on any other response or transport
//! failure.

use std::process::ExitCode;
use std::time::Duration;

use noetive::semantik::Client;

#[tokio::main]
async fn main() -> ExitCode {
    // Health is unauthenticated, so any syntactically valid key works.
    // `from_env` is not used because it requires NOETIVE_KEY_SECRET to
    // be set — which would be noise for this particular probe.
    let key = std::env::var("NOETIVE_KEY_SECRET")
        .unwrap_or_else(|_| "keyu_placeholder_for_health_only".to_string());
    let mut builder = Client::builder()
        .api_key(key)
        .read_timeout(Duration::from_secs(5));
    if let Ok(base) = std::env::var("NOETIVE_BASE_URL") {
        builder = builder.base_url(base);
    }
    let client = match builder.build() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("init: {e}");
            return ExitCode::from(1);
        }
    };

    match client.health().await {
        Ok(()) => {
            println!("UP");
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!("DOWN: {e}");
            ExitCode::from(1)
        }
    }
}

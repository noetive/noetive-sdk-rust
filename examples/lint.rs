//! Validate a SemQL query and surface diagnostics + completions.
//!
//! Lint is unauthenticated. The SDK still needs any syntactically
//! valid key prefix to satisfy [`Client::new`]; a placeholder works.
//!
//! ```text
//! cargo run --example lint
//! ```

use std::time::Duration;

use noetive::semantik::{Client, LintRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::builder()
        .api_key("keyu_placeholder_for_lint_only")
        .read_timeout(Duration::from_secs(5))
        .build()?;

    let query = r#"MATCH DISTANCE("climate change") WITHIN "#;
    let res = client
        .lint(LintRequest {
            query: query.to_string(),
            cursor: query.len() as u32,
        })
        .await?;

    println!("valid={} normalized={:?}", res.valid, res.normalized);
    for d in &res.diagnostics {
        println!("  [{}] {}:{} {}", d.severity, d.line, d.col, d.message);
    }
    for c in &res.completions {
        println!("  -> {:<12} {} — {}", c.kind, c.label, c.detail);
    }
    Ok(())
}

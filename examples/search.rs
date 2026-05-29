//! SemQL text-anchor search against the shared `"global"` namespace.
//!
//! ```text
//! NOETIVE_KEY_SECRET=keyu_... cargo run --example search
//! ```

use noetive::semantik::{Client, SearchRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::from_env()?;

    // namespace, model and dimensions are required on every search —
    // the SDK applies no defaults. These are the global-namespace values.
    let res = client
        .search(SearchRequest {
            query: r#"MATCH DISTANCE("machine learning research") WITHIN 0.4 LIMIT 10"#.into(),
            namespace: "global".into(),
            model: "Qwen3-Embedding-4B".into(),
            dimensions: 1024,
            ..Default::default()
        })
        .await?;

    for (i, r) in res.results.iter().enumerate() {
        println!(
            "{:>2}  score={:.3}  id={}  ns={}",
            i, r.score, r.message_id, r.namespace
        );
    }
    Ok(())
}

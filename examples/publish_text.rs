//! Publish a text message to the shared `"global"` namespace; the
//! server computes the embedding. `namespace`, `model`, and
//! `dimensions` are required on every publish — the SDK applies no
//! defaults. `AckMode::Stored` (the default) returns once the write
//! survives a single server restart.
//!
//! ```text
//! NOETIVE_KEY_SECRET=keyu_... cargo run --example publish_text
//! ```

use std::collections::HashMap;

use noetive::semantik::{AckMode, Client, PublishItem, PublishRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::from_env()?;

    let mut metadata = HashMap::new();
    metadata.insert("source".to_string(), "arxiv".to_string());
    metadata.insert("author".to_string(), "jdoe".to_string());

    let res = client
        .publish(PublishRequest {
            items: vec![PublishItem::text(
                "Transformer models have reshaped NLP benchmarks.",
            )],
            namespace: "global".into(),
            model: "Qwen3-Embedding-4B".into(),
            dimensions: 1024,
            metadata,
            ack: AckMode::Stored,
            ..Default::default()
        })
        .await?;

    println!(
        "message_id={} epoch={} seq={}",
        res.message_id, res.epoch, res.seq
    );
    Ok(())
}

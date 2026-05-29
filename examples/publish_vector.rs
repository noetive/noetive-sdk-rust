//! Publish a pre-computed embedding vector to the shared `"global"`
//! namespace with `ack=durable`. Pairing `idempotency_key` with durable
//! ack means retries collapse to a single stored message that has
//! survived power loss — making the write safe to retry. The SDK's
//! default retry policy rides out transient hiccups automatically.
//!
//! `namespace`, `model`, and `dimensions` are required on every publish;
//! the SDK applies no defaults. The vector length must match
//! `dimensions`, which in turn must match the namespace's model.
//!
//! ```text
//! NOETIVE_KEY_SECRET=keyu_... cargo run --example publish_vector
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use noetive::semantik::{AckMode, Client, PublishItem, PublishRequest};

/// The `"global"` namespace is provisioned with this model at this
/// dimensionality. Other namespaces use their own (model, dimensions).
const GLOBAL_MODEL: &str = "Qwen3-Embedding-4B";
const GLOBAL_DIMENSIONS: u16 = 1024;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::from_env()?;

    // Caller-side vector must match `dimensions`.
    let mut vec: Vec<f32> = Vec::with_capacity(GLOBAL_DIMENSIONS as usize);
    for i in 0..GLOBAL_DIMENSIONS as usize {
        vec.push((i % 100) as f32 * 0.01);
    }

    let res = client
        .publish(PublishRequest {
            items: vec![PublishItem::vector(vec)],
            namespace: "global".into(),
            model: GLOBAL_MODEL.into(),
            dimensions: GLOBAL_DIMENSIONS,
            ack: AckMode::Durable,
            idempotency_key: Some(new_key()),
            ..Default::default()
        })
        .await?;

    println!(
        "message_id={} epoch={} seq={}",
        res.message_id, res.epoch, res.seq
    );
    Ok(())
}

/// Generate a unique idempotency key. Uses process pid + wall-clock
/// nanos to avoid pulling in a uuid dependency.
fn new_key() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("pub-{}-{}", std::process::id(), nanos)
}

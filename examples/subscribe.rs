//! Open an SSE match stream for a SemQL query and print events as
//! they arrive. Exits cleanly on Ctrl-C.
//!
//! ```text
//! NOETIVE_KEY_SECRET=keyu_... cargo run --example subscribe
//! ```

use std::time::Duration;

use futures_util::StreamExt;
use noetive::semantik::{Client, SubscribeRequest};
use tokio::signal;
use tokio::time::timeout;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::from_env()?;

    // Bound the initial subscribe handshake; the stream itself is
    // long-lived.
    let mut sub = timeout(
        Duration::from_secs(10),
        // namespace, model, and dimensions are required — no defaults.
        client.subscribe(SubscribeRequest {
            query: r#"MATCH DISTANCE("gpu shortage") WITHIN 0.5"#.into(),
            namespace: "global".into(),
            model: "Qwen3-Embedding-4B".into(),
            dimensions: 1024,
        }),
    )
    .await??;

    eprintln!("subscribed: {}", sub.id());

    loop {
        tokio::select! {
            biased;
            _ = signal::ctrl_c() => {
                eprintln!("\ninterrupt; closing stream");
                break;
            }
            ev = sub.next() => match ev {
                Some(Ok(m)) => println!("match: {} score={:.3}", m.message_id, m.score),
                Some(Err(e)) => {
                    eprintln!("stream ended: {e}");
                    break;
                }
                None => {
                    eprintln!("stream closed by server");
                    break;
                }
            }
        }
    }
    Ok(())
}

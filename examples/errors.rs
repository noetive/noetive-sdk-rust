//! Demonstrate the SDK's error classes and idiomatic inspection.
//!
//! The program intentionally triggers:
//!
//! 1. A client-side preflight rejection (over-sized vector — never
//!    reaches the wire).
//! 2. An authenticated server response (either success or an API
//!    error, depending on the key's state).
//!
//! Both are then classified.
//!
//! ```text
//! NOETIVE_KEY_SECRET=keyu_... cargo run --example errors
//! ```

use noetive::semantik::{Client, Error, ErrorCode, PublishItem, PublishRequest, SearchRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::from_env()?;

    // --- 1. Preflight rejection -------------------------------------
    //
    // 5000-dim vector exceeds MAX_VECTOR_DIM. namespace/model/dimensions
    // are required on every publish (no defaults); the over-sized vector
    // is what trips preflight here.
    let res = client
        .publish(PublishRequest {
            namespace: "private-x".into(),
            model: "text-embedding-3-small".into(),
            dimensions: 5000,
            items: vec![PublishItem::vector(vec![0.0; 5000])],
            ..Default::default()
        })
        .await;
    classify("preflight oversized vector", res.err());

    // --- 2. Real round trip ----------------------------------------
    let res = client
        .search(SearchRequest {
            query: r#"MATCH DISTANCE("machine learning") WITHIN 0.4 LIMIT 5"#.into(),
            namespace: "global".into(),
            model: "Qwen3-Embedding-4B".into(),
            dimensions: 1024,
            ..Default::default()
        })
        .await;
    classify("search round trip", res.err());

    Ok(())
}

fn classify(label: &str, err: Option<Error>) {
    println!("\n--- {label} ---");
    let Some(err) = err else {
        println!("ok");
        return;
    };

    // Match on the structured code.
    if let Some(code) = err.code() {
        match code {
            ErrorCode::InvalidRequest => println!("matched: InvalidRequest"),
            ErrorCode::Unauthorized => println!("matched: Unauthorized"),
            ErrorCode::NotBillable => {
                println!("matched: NotBillable (terminal until billing resolved)")
            }
            ErrorCode::Backpressure => println!("matched: Backpressure"),
            ErrorCode::Unavailable => println!("matched: Unavailable"),
            ErrorCode::MeteringUnavailable => {
                println!("matched: MeteringUnavailable (retry with backoff)")
            }
            ErrorCode::Unknown(s) => println!("matched: Unknown({s})"),
            other => println!("matched: {other:?}"),
        }
    }

    // Full detail extraction.
    if let Error::Api {
        code,
        message,
        request_id,
        retry_after,
        http_status,
    } = &err
    {
        let source = if *http_status == 0 {
            "preflight"
        } else {
            "server"
        };
        println!(
            "source={source} http={http_status} code={code} retry_after={retry_after:?} request_id={request_id:?} message={message:?}"
        );
        return;
    }

    // Anything else is a transport-level failure.
    println!("raw: {err}");
}

//! Official Rust SDK for the [Noetive](https://noetive.io) platform.
//!
//! Today this crate exposes [`semantik`] — a managed semantic broker
//! where you publish messages tagged with embedding vectors, query them
//! with SemQL, and subscribe to live match streams over Server-Sent
//! Events. Additional services will land alongside `semantik` as the
//! platform grows.
//!
//! # Quick start
//!
//! ```no_run
//! use noetive::semantik::{Client, PublishRequest, PublishItem, SearchRequest};
//!
//! # async fn run() -> Result<(), noetive::semantik::Error> {
//! let client = Client::from_env()?; // reads NOETIVE_KEY_SECRET
//!
//! // namespace, model, and dimensions are required — no defaults. The
//! // shared "global" namespace is backed by Qwen3-Embedding-4B / 1024.
//! client.publish(PublishRequest {
//!     items: vec![PublishItem::text("Transformer models reshaped NLP.")],
//!     namespace: "global".into(),
//!     model: "Qwen3-Embedding-4B".into(),
//!     dimensions: 1024,
//!     ..Default::default()
//! }).await?;
//!
//! let results = client.search(SearchRequest {
//!     query: "MATCH DISTANCE(\"machine learning\") WITHIN 0.4 LIMIT 10".into(),
//!     namespace: "global".into(),
//!     model: "Qwen3-Embedding-4B".into(),
//!     dimensions: 1024,
//!     ..Default::default()
//! }).await?;
//!
//! for hit in results.results {
//!     println!("{}: {}", hit.score, hit.content);
//! }
//! # Ok(())
//! # }
//! ```

pub mod semantik;

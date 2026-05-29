use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::semantik::client::Client;
use crate::semantik::error::Error;
use crate::semantik::limits::MAX_IDEMPOTENCY_KEY_LEN;
use crate::semantik::transport::{send_json, AuthMode, SetRequestId};
use crate::semantik::validate::{
    validate_metadata, validate_publish_item, validate_target, validate_vector_matches_dimensions,
};

const PATH_PUBLISH: &str = "/v1/publish";

/// Acknowledgement durability requested on a publish.
///
/// The server treats a missing `ack` field as [`AckMode::Stored`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AckMode {
    /// Return once the write survives a single server restart. Default.
    #[default]
    Stored,
    /// Return once the write survives power loss.
    Durable,
}

/// Single message to publish. Exactly one of `text` or `vector` must
/// be non-empty.
///
/// Use the [`PublishItem::text`] and [`PublishItem::vector`]
/// constructors to avoid accidentally setting both.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PublishItem {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub vector: Vec<f32>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub text: String,
}

impl PublishItem {
    /// Item with embedded text. The server computes the embedding.
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            text: s.into(),
            vector: Vec::new(),
        }
    }

    /// Item with a pre-computed embedding vector.
    pub fn vector(v: Vec<f32>) -> Self {
        Self {
            text: String::new(),
            vector: v,
        }
    }
}

/// Body of `POST /v1/publish`.
///
/// `namespace`, `model`, and `dimensions` are required — the SDK applies
/// no defaults. A request that leaves any of them unset is rejected at
/// preflight rather than silently routed: defaulting `namespace` to a
/// shared value would let a forgotten field publish sensitive data into
/// a namespace the caller never intended. `model` and `dimensions` are
/// model-coupled properties with no server default, and `dimensions`
/// must match both the embedding `model` and, when publishing a vector,
/// the length of the vector itself.
///
/// A minimal text publish to the shared `global` namespace is therefore:
///
/// ```
/// # use noetive::semantik::{PublishRequest, PublishItem};
/// let req = PublishRequest {
///     items: vec![PublishItem::text("Transformer models reshaped NLP.")],
///     namespace: "global".into(),
///     model: "Qwen3-Embedding-4B".into(),
///     dimensions: 1024,
///     ..Default::default()
/// };
/// ```
///
/// `items` must currently contain exactly one element; the `Vec` shape
/// is forward-compatible with future batched publishes.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PublishRequest {
    pub items: Vec<PublishItem>,
    pub namespace: String,
    pub model: String,
    pub dimensions: u16,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
    /// Set this to make retries safe: within the server's retention
    /// window, the same key yields the same `message_id` and `seq`,
    /// collapsing duplicates that would otherwise occur when a retry
    /// hits the server twice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Durability requested. [`AckMode::Stored`] is the default and is
    /// equivalent to omitting the field on the wire.
    #[serde(skip_serializing_if = "is_default_ack")]
    pub ack: AckMode,
}

fn is_default_ack(a: &AckMode) -> bool {
    matches!(a, AckMode::Stored)
}

/// Server-assigned identifiers for a newly-published message.
///
/// `request_id` is populated from the response's `X-Request-Id` header
/// — the server-assigned correlation token. Quote it when contacting
/// support; it pivots directly to the relevant server log line.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PublishResponse {
    #[serde(default)]
    pub message_id: String,
    #[serde(default)]
    pub epoch: u64,
    #[serde(default)]
    pub seq: u64,
    #[serde(default, skip)]
    pub request_id: Option<String>,
}

impl SetRequestId for PublishResponse {
    fn set_request_id(&mut self, id: Option<String>) {
        self.request_id = id;
    }
}

impl PublishRequest {
    fn validate(&self) -> Result<(), Error> {
        validate_target(&self.namespace, &self.model, self.dimensions)?;
        if self.items.len() != 1 {
            return Err(Error::preflight(format!(
                "publish requires exactly 1 item, got {}",
                self.items.len()
            )));
        }
        let item = &self.items[0];
        validate_publish_item(&item.text, &item.vector)?;
        validate_vector_matches_dimensions(&item.vector, self.dimensions)?;
        validate_metadata(&self.metadata)?;
        if let Some(k) = &self.idempotency_key {
            if k.len() > MAX_IDEMPOTENCY_KEY_LEN {
                return Err(Error::preflight(format!(
                    "idempotency_key exceeds {MAX_IDEMPOTENCY_KEY_LEN} bytes"
                )));
            }
        }
        Ok(())
    }
}

impl Client {
    /// Publish a single message to a namespace. On success the response
    /// carries a stable `message_id` and ordering tokens (`epoch`,
    /// `seq`).
    ///
    /// Publish without [`PublishRequest::idempotency_key`] is NOT safe
    /// to retry — a retry that the first attempt already completed
    /// will create a duplicate stored message. Pass an idempotency key
    /// to make publishes retry-safe.
    pub async fn publish(&self, req: PublishRequest) -> Result<PublishResponse, Error> {
        req.validate()?;
        send_json(self, PATH_PUBLISH, &req, AuthMode::Bearer).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fully-specified request the validator should accept, so each
    /// negative test can flip exactly one field.
    fn valid_request() -> PublishRequest {
        PublishRequest {
            items: vec![PublishItem::text("hi")],
            namespace: "global".into(),
            model: "Qwen3-Embedding-4B".into(),
            dimensions: 1024,
            ..Default::default()
        }
    }

    #[test]
    fn validate_accepts_fully_specified_request() {
        assert!(valid_request().validate().is_ok());
    }

    #[test]
    fn validate_requires_namespace() {
        // No default is applied: an unset namespace is a hard error, not
        // a silent fall-back to a shared namespace.
        let r = PublishRequest {
            namespace: String::new(),
            ..valid_request()
        };
        assert!(r.validate().is_err());
    }

    #[test]
    fn validate_requires_model() {
        let r = PublishRequest {
            model: String::new(),
            ..valid_request()
        };
        assert!(r.validate().is_err());
    }

    #[test]
    fn validate_requires_dimensions() {
        let r = PublishRequest {
            dimensions: 0,
            ..valid_request()
        };
        assert!(r.validate().is_err());
    }

    #[test]
    fn validate_rejects_long_idempotency_key() {
        let r = PublishRequest {
            idempotency_key: Some("a".repeat(MAX_IDEMPOTENCY_KEY_LEN + 1)),
            ..valid_request()
        };
        assert!(r.validate().is_err());
    }

    #[test]
    fn ack_serialization_omits_default() {
        let r = PublishRequest {
            items: vec![PublishItem::text("hi")],
            namespace: "global".into(),
            model: "m".into(),
            dimensions: 1,
            ack: AckMode::Stored,
            ..Default::default()
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("\"ack\""));
    }

    #[test]
    fn ack_serialization_includes_durable() {
        let r = PublishRequest {
            items: vec![PublishItem::text("hi")],
            namespace: "global".into(),
            model: "m".into(),
            dimensions: 1,
            ack: AckMode::Durable,
            ..Default::default()
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"ack\":\"durable\""));
    }

    #[test]
    fn publish_item_constructors_are_xor() {
        let t = PublishItem::text("hello");
        assert!(!t.text.is_empty());
        assert!(t.vector.is_empty());
        let v = PublishItem::vector(vec![1.0, 2.0]);
        assert!(v.text.is_empty());
        assert!(!v.vector.is_empty());
    }
}

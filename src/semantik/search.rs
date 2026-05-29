use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::semantik::client::Client;
use crate::semantik::error::Error;
use crate::semantik::transport::{send_json, AuthMode, SetRequestId};
use crate::semantik::validate::validate_target;

const PATH_SEARCH: &str = "/v1/search";

/// Body of `POST /v1/search`.
///
/// `query`, `namespace`, `model`, and `dimensions` are required — the
/// SDK applies no defaults, so an unset targeting field is a preflight
/// error rather than a silent fall-back to a shared namespace. The
/// `(model, dimensions)` pair must match the namespace's provisioned
/// configuration.
///
/// `limit` zero means "use the SemQL `LIMIT` clause or the server
/// default" and is the one genuinely optional field.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SearchRequest {
    pub query: String,
    pub namespace: String,
    pub model: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub limit: u32,
    pub dimensions: u16,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Ranked list of matches.
///
/// `request_id` is populated from the response's `X-Request-Id` header
/// — the server-assigned correlation token. Quote it when contacting
/// support.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchResponse {
    #[serde(default)]
    pub results: Vec<ResultItem>,
    #[serde(default, skip)]
    pub request_id: Option<String>,
}

impl SetRequestId for SearchResponse {
    fn set_request_id(&mut self, id: Option<String>) {
        self.request_id = id;
    }
}

/// Single ranked match.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ResultItem {
    #[serde(default)]
    pub metadata: HashMap<String, String>,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub message_id: String,
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub score: f32,
}

impl SearchRequest {
    fn validate(&self) -> Result<(), Error> {
        if self.query.is_empty() {
            return Err(Error::preflight("search query must not be empty"));
        }
        validate_target(&self.namespace, &self.model, self.dimensions)
    }
}

impl Client {
    /// Run a SemQL query against the namespace.
    pub async fn search(&self, req: SearchRequest) -> Result<SearchResponse, Error> {
        req.validate()?;
        send_json(self, PATH_SEARCH, &req, AuthMode::Bearer).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_request() -> SearchRequest {
        SearchRequest {
            query: "q".into(),
            namespace: "global".into(),
            model: "Qwen3-Embedding-4B".into(),
            dimensions: 1024,
            limit: 0,
        }
    }

    #[test]
    fn validate_accepts_fully_specified_request() {
        assert!(valid_request().validate().is_ok());
    }

    #[test]
    fn empty_query_rejected() {
        let r = SearchRequest {
            query: String::new(),
            ..valid_request()
        };
        assert!(r.validate().is_err());
    }

    #[test]
    fn missing_targeting_fields_rejected() {
        // No defaults: each unset targeting field fails preflight.
        assert!(SearchRequest {
            namespace: String::new(),
            ..valid_request()
        }
        .validate()
        .is_err());
        assert!(SearchRequest {
            model: String::new(),
            ..valid_request()
        }
        .validate()
        .is_err());
        assert!(SearchRequest {
            dimensions: 0,
            ..valid_request()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn limit_omitted_when_zero() {
        let r = SearchRequest {
            query: "q".into(),
            namespace: "global".into(),
            model: "m".into(),
            dimensions: 1,
            limit: 0,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("\"limit\""));
    }

    #[test]
    fn limit_included_when_positive() {
        let r = SearchRequest {
            query: "q".into(),
            namespace: "global".into(),
            model: "m".into(),
            dimensions: 1,
            limit: 10,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"limit\":10"));
    }
}

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
/// default" and is optional.
///
/// `include_vector` asks for each result's stored embedding in
/// [`ResultItem::vector`]. It defaults to `false`, and the field is
/// left out of the request body unless set. With it on, a page holds at
/// most 50 results: a larger `limit` is clamped, not rejected. Each
/// vector adds roughly `dimensions` x 13 bytes of JSON to the response.
/// A stored vector is readable by anyone who can reach the namespace,
/// exactly like `content` and `metadata`.
///
/// ```
/// use noetive::semantik::SearchRequest;
///
/// let req = SearchRequest {
///     query: r#"MATCH DISTANCE("machine learning") LIMIT 10"#.into(),
///     namespace: "global".into(),
///     model: "Qwen3-Embedding-4B".into(),
///     dimensions: 1024,
///     include_vector: true,
///     ..Default::default()
/// };
/// # let _ = req;
/// ```
#[derive(Debug, Clone, Default, Serialize)]
pub struct SearchRequest {
    pub query: String,
    pub namespace: String,
    pub model: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub limit: u32,
    pub dimensions: u16,
    /// Return each result's stored embedding in [`ResultItem::vector`].
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub include_vector: bool,
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
    /// `true` only when the page was cut short to stay under the
    /// response size cap. The results returned are the best-ranked ones;
    /// the dropped ones cannot be paged to, so lower `limit` or narrow
    /// the query. Stays `false` when a page is merely clamped to the
    /// result ceiling (1000, or 50 with `include_vector`).
    #[serde(default)]
    pub truncated: bool,
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
    /// The stored embedding (`dimensions` values). Present only when
    /// the request set `include_vector`; `None` otherwise.
    #[serde(default)]
    pub vector: Option<Vec<f32>>,
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
            include_vector: false,
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
            include_vector: false,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("\"limit\""));
    }

    // The vector flag changes what the server returns and what a result
    // costs on the wire, so it must be absent unless the caller opts in:
    // default bodies stay unchanged, and only `true` is ever sent.
    #[test]
    fn include_vector_sent_only_when_set() {
        let off = serde_json::to_value(valid_request()).unwrap();
        assert!(off.get("include_vector").is_none());
        let on = SearchRequest {
            include_vector: true,
            ..valid_request()
        };
        assert_eq!(serde_json::to_value(on).unwrap()["include_vector"], true);
    }

    // A result without a requested vector must decode with `vector` as
    // `None` (not an empty vec), and a present vector must round-trip
    // all of its values; `truncated` defaults to false when omitted.
    #[test]
    fn result_vector_and_truncated_decode() {
        let plain: SearchResponse =
            serde_json::from_str(r#"{"results":[{"message_id":"m","score":0.5}]}"#).unwrap();
        assert_eq!(plain.results[0].vector, None);
        assert!(!plain.truncated);
        let full: SearchResponse = serde_json::from_str(
            r#"{"results":[{"message_id":"m","score":0.5,"vector":[0.25,-1.5,3]}],"truncated":true}"#,
        )
        .unwrap();
        assert_eq!(full.results[0].vector, Some(vec![0.25, -1.5, 3.0]));
        assert!(full.truncated);
    }

    #[test]
    fn limit_included_when_positive() {
        let r = SearchRequest {
            query: "q".into(),
            namespace: "global".into(),
            model: "m".into(),
            dimensions: 1,
            limit: 10,
            include_vector: false,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"limit\":10"));
    }
}

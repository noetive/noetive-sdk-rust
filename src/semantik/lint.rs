use serde::{Deserialize, Serialize};

use crate::semantik::client::Client;
use crate::semantik::error::Error;
use crate::semantik::transport::{send_json, AuthMode, SetRequestId};

const PATH_LINT: &str = "/v1/lint";

/// Body of `POST /v1/lint`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LintRequest {
    /// SemQL source to validate. Required.
    pub query: String,
    /// Byte offset within `query` to compute completions for. Zero
    /// means "end of query".
    #[serde(default)]
    pub cursor: u32,
}

/// Response from `POST /v1/lint`.
///
/// `request_id` is populated from the response's `X-Request-Id` header
/// — the server-assigned correlation token. Quote it when contacting
/// support.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LintResponse {
    #[serde(default)]
    pub normalized: String,
    #[serde(default)]
    pub diagnostics: Vec<LintDiagnostic>,
    #[serde(default)]
    pub completions: Vec<LintCompletion>,
    #[serde(default)]
    pub valid: bool,
    #[serde(default, skip)]
    pub request_id: Option<String>,
}

impl SetRequestId for LintResponse {
    fn set_request_id(&mut self, id: Option<String>) {
        self.request_id = id;
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LintDiagnostic {
    #[serde(default)]
    pub severity: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub line: u32,
    #[serde(default)]
    pub col: u32,
    #[serde(default)]
    pub end_line: u32,
    #[serde(default)]
    pub end_col: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LintCompletion {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub detail: String,
}

impl LintRequest {
    fn validate(&self) -> Result<(), Error> {
        if self.query.is_empty() {
            return Err(Error::preflight("lint query must not be empty"));
        }
        if (self.cursor as usize) > self.query.len() {
            return Err(Error::preflight(format!(
                "lint cursor {} exceeds query length {}",
                self.cursor,
                self.query.len()
            )));
        }
        Ok(())
    }
}

impl Client {
    /// Validate a SemQL query and surface diagnostics and completions.
    /// Does not require authentication.
    pub async fn lint(&self, req: LintRequest) -> Result<LintResponse, Error> {
        req.validate()?;
        send_json(self, PATH_LINT, &req, AuthMode::None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_rejected() {
        let r = LintRequest::default();
        assert!(r.validate().is_err());
    }

    #[test]
    fn cursor_past_end_rejected() {
        let r = LintRequest {
            query: "abc".into(),
            cursor: 10,
        };
        assert!(r.validate().is_err());
    }

    #[test]
    fn cursor_at_end_is_ok() {
        let r = LintRequest {
            query: "abc".into(),
            cursor: 3,
        };
        assert!(r.validate().is_ok());
    }
}

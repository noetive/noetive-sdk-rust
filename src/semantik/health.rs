use bytes::Bytes;

use crate::semantik::client::Client;
use crate::semantik::error::{decode_error_response, Error};
use crate::semantik::transport::{send_raw, AuthMode, MIME_JSON};

const PATH_HEALTH: &str = "/v1/health";

impl Client {
    /// Unauthenticated liveness probe. Returns `Ok(())` when the server
    /// responds with 2xx, or an [`Error::Api`] otherwise.
    ///
    /// Health is not retried: a sustained outage should surface
    /// quickly so that callers can fall back to other strategies. To
    /// retry, wrap the call yourself.
    pub async fn health(&self) -> Result<(), Error> {
        // No body; no auth. Goes through send_raw so the configured
        // per-call read timeout still applies.
        let read_timeout = self.inner().read_timeout;
        let resp = send_raw(
            self,
            PATH_HEALTH,
            Bytes::new(),
            MIME_JSON,
            MIME_JSON,
            AuthMode::None,
            read_timeout,
        )
        .await?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.bytes().await.map_err(Error::from)?;
        if status.is_success() {
            return Ok(());
        }
        Err(decode_error_response(status.as_u16(), &headers, &body))
    }
}

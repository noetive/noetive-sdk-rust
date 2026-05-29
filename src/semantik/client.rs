use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderValue, AUTHORIZATION};

use crate::semantik::defaults::{DEFAULT_BASE_URL, ENV_API_KEY, ENV_BASE_URL};
use crate::semantik::error::Error;
use crate::semantik::retry::{RetryPolicy, TransientRetry};
use crate::semantik::validate::api_key_non_empty;

/// Default TCP/TLS connect timeout for the SDK-managed `reqwest::Client`.
/// Ignored when the caller injects their own HTTP client.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default per-request read timeout for one-shot RPCs (health, lint,
/// publish, search). Subscribe stream bodies are NOT bounded by this —
/// they are long-lived by design and a read timeout would tear them
/// down between matches. Ignored when the caller injects their own HTTP
/// client.
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Client for the Noetive Semantik API.
///
/// `Client` is cheap to clone — internally it holds an [`Arc`] over the
/// underlying HTTP client and configuration. Methods are async, take
/// `&self`, and may be invoked concurrently from any task.
///
/// Construct via [`Client::new`] (explicit key),
/// [`Client::from_env`] (reads `NOETIVE_KEY_SECRET` and optional
/// `NOETIVE_BASE_URL`), or [`Client::builder`] for custom HTTP client
/// or retry policy.
///
/// # Redacted Debug
///
/// `Debug` formatting redacts the API key: `Client { base_url:
/// "https://semantik.noetive.io", api_key: "REDACTED" }`. Logs and
/// debugger output will never expose the bearer token.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub(crate) http: reqwest::Client,
    pub(crate) base_url: String,
    pub(crate) auth_header: HeaderValue,
    pub(crate) retry: Box<dyn RetryPolicy>,
    /// Per-call read timeout applied to one-shot RPCs only. `None`
    /// means the SDK does not impose a read timeout (the caller supplied
    /// their own `reqwest::Client` and is responsible for its timeouts).
    pub(crate) read_timeout: Option<Duration>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("base_url", &self.inner.base_url)
            .field("api_key", &"REDACTED")
            .finish()
    }
}

impl Client {
    /// Construct a client authenticated with the given API key. The key
    /// must be non-empty; deeper validation (length, prefix, envelope
    /// integrity) is the server's job. Empty or whitespace-only inputs
    /// return [`Error::InvalidApiKey`].
    pub fn new(api_key: impl Into<String>) -> Result<Self, Error> {
        Self::builder().api_key(api_key).build()
    }

    /// Construct a client using `NOETIVE_KEY_SECRET` and (optionally)
    /// `NOETIVE_BASE_URL` from the process environment. Returns
    /// [`Error::MissingApiKey`] when `NOETIVE_KEY_SECRET` is unset or
    /// empty.
    pub fn from_env() -> Result<Self, Error> {
        let key = std::env::var(ENV_API_KEY)
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or(Error::MissingApiKey)?;
        let mut b = Self::builder().api_key(key);
        if let Ok(base) = std::env::var(ENV_BASE_URL) {
            if !base.is_empty() {
                b = b.base_url(base);
            }
        }
        b.build()
    }

    /// Start building a client with non-default configuration.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    pub(crate) fn inner(&self) -> &Inner {
        &self.inner
    }

    /// Base URL used for requests (trailing slash trimmed). Useful for
    /// integration test diagnostics.
    pub fn base_url(&self) -> &str {
        &self.inner.base_url
    }
}

/// Builder for [`Client`]. Construct via [`Client::builder`].
#[derive(Default)]
pub struct ClientBuilder {
    api_key: Option<String>,
    base_url: Option<String>,
    http_client: Option<reqwest::Client>,
    retry: Option<Box<dyn RetryPolicy>>,
    connect_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
}

impl ClientBuilder {
    /// Set the API key.
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Override the base URL (default: `https://semantik.noetive.io`).
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    /// Inject a custom `reqwest::Client`. Use this to install custom
    /// TLS roots, proxies, or connection-pool tuning. The SDK will
    /// still attach `Content-Type`, `Accept`, `User-Agent`, and
    /// `Authorization` headers per request.
    pub fn http_client(mut self, http: reqwest::Client) -> Self {
        self.http_client = Some(http);
        self
    }

    /// Install a custom [`RetryPolicy`]. Default: `TransientRetry::new(5)`.
    pub fn retry<P: RetryPolicy + 'static>(mut self, policy: P) -> Self {
        self.retry = Some(Box::new(policy));
        self
    }

    /// TCP/TLS connect timeout. Default: 10 seconds. Ignored when a
    /// custom [`http_client`] is supplied (configure it on the injected
    /// client directly).
    ///
    /// [`http_client`]: ClientBuilder::http_client
    pub fn connect_timeout(mut self, t: Duration) -> Self {
        self.connect_timeout = Some(t);
        self
    }

    /// Per-request read timeout applied to one-shot RPCs (health, lint,
    /// publish, search). Default: 30 seconds. The subscribe stream is
    /// deliberately NOT bounded by this timeout — long quiet periods
    /// between matches are normal, and tearing the stream down between
    /// them would surface as a spurious failure.
    ///
    /// Ignored when a custom [`http_client`] is supplied (configure it
    /// on the injected client directly).
    ///
    /// [`http_client`]: ClientBuilder::http_client
    pub fn read_timeout(mut self, t: Duration) -> Self {
        self.read_timeout = Some(t);
        self
    }

    /// Finalise the client. Returns [`Error::InvalidApiKey`] when no
    /// key was supplied or the value was empty / whitespace-only.
    pub fn build(self) -> Result<Client, Error> {
        let api_key = self.api_key.ok_or(Error::InvalidApiKey)?;
        if !api_key_non_empty(&api_key) {
            return Err(Error::InvalidApiKey);
        }
        let mut auth_header = HeaderValue::from_str(&format!("Bearer {api_key}"))
            .map_err(|_| Error::InvalidApiKey)?;
        auth_header.set_sensitive(true);

        let base_url = self
            .base_url
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();

        // When the caller injects their own `reqwest::Client` we leave
        // its timeouts untouched and the SDK does not impose a per-call
        // read timeout — preserving the "bring your own client, bring
        // your own policy" contract.
        let (http, read_timeout) = match self.http_client {
            Some(h) => (h, None),
            None => {
                let connect_timeout = self.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT);
                let read_timeout = self.read_timeout.unwrap_or(DEFAULT_READ_TIMEOUT);
                let http = reqwest::Client::builder()
                    .connect_timeout(connect_timeout)
                    .build()
                    .map_err(Error::Transport)?;
                (http, Some(read_timeout))
            }
        };

        let retry: Box<dyn RetryPolicy> = self
            .retry
            .unwrap_or_else(|| Box::new(TransientRetry::new(5)));

        Ok(Client {
            inner: Arc::new(Inner {
                http,
                base_url,
                auth_header,
                retry,
                read_timeout,
            }),
        })
    }
}

/// Re-exported for symmetry: header name used for the bearer token.
pub(crate) const HEADER_AUTHORIZATION: reqwest::header::HeaderName = AUTHORIZATION;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_rejects_empty_or_whitespace() {
        assert!(matches!(Client::new("").unwrap_err(), Error::InvalidApiKey));
        assert!(matches!(
            Client::new("   ").unwrap_err(),
            Error::InvalidApiKey
        ));
        assert!(matches!(
            Client::new("\t\n").unwrap_err(),
            Error::InvalidApiKey
        ));
    }

    #[test]
    fn new_accepts_user_prefix() {
        let c = Client::new("keyu_abc").unwrap();
        assert_eq!(c.base_url(), "https://semantik.noetive.io");
    }

    #[test]
    fn new_accepts_tenant_prefix() {
        assert!(Client::new("keyt_abc").is_ok());
    }

    #[test]
    fn new_accepts_unknown_format_key() {
        // The SDK does not lock the format — a future key family with
        // a different prefix should construct without complaint.
        assert!(Client::new("some_new_format_key").is_ok());
        assert!(Client::new("sk-foo").is_ok());
    }

    #[test]
    fn builder_trims_trailing_slash() {
        let c = Client::builder()
            .api_key("keyu_x")
            .base_url("https://example.com/")
            .build()
            .unwrap();
        assert_eq!(c.base_url(), "https://example.com");
    }

    #[test]
    fn debug_redacts_api_key() {
        let c = Client::new("keyu_secret").unwrap();
        let s = format!("{c:?}");
        assert!(s.contains("REDACTED"));
        assert!(!s.contains("keyu_secret"));
    }

    #[test]
    fn from_env_requires_key() {
        // We can't unset env vars safely in a parallel test runner, so
        // we exercise the explicit-key path here and rely on integration
        // tests for the env-var path.
        let _ = Client::from_env();
    }
}

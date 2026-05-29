//! Client for the Noetive Semantik API.
//!
//! Semantik is a managed semantic broker. Publish messages (text or
//! pre-computed embedding vectors), search them with SemQL, and
//! subscribe to live match streams.
//!
//! # Authentication
//!
//! Every authenticated call carries a bearer token in the
//! `Authorization` header. [`Client::from_env`] reads the key from the
//! `NOETIVE_KEY_SECRET` environment variable; [`Client::new`] accepts
//! the key explicitly. Keys are issued on the
//! [dashboard](https://www.noetive.io/settings/developer-keys). The SDK
//! accepts any non-empty key — the server is the source of truth for
//! key validity.
//!
//! [`health`] and [`lint`] do not require an API key; [`publish`],
//! [`search`] and [`subscribe`] do.
//!
//! [`health`]: Client::health
//! [`lint`]: Client::lint
//! [`publish`]: Client::publish
//! [`search`]: Client::search
//! [`subscribe`]: Client::subscribe
//!
//! # Namespaces
//!
//! Every [`publish`], [`search`], and [`subscribe`] call MUST set
//! `namespace`, `model`, and `dimensions` explicitly — the SDK applies
//! no defaults. An unset targeting field is a preflight error, not a
//! silent fall-back: defaulting `namespace` to a shared value would let
//! a forgotten field route sensitive data into a namespace the caller
//! never intended. `model` and `dimensions` are model-coupled
//! properties with no server default and must match the namespace's
//! provisioned configuration.
//!
//! The shared `"global"` namespace is pre-provisioned for every account
//! (backed by `Qwen3-Embedding-4B` at 1024 dimensions); set
//! [`PublishRequest::namespace`] to `"global"` with the matching `model`
//! and `dimensions` to use it, or to a private namespace you provisioned
//! on the dashboard.
//!
//! # Errors
//!
//! All operations return [`Result`]`<T, `[`Error`]`>`. The [`Error`]
//! enum surfaces structured server error envelopes ([`Error::Api`]),
//! preflight validation failures (also [`Error::Api`] with
//! `http_status = 0`), transport failures ([`Error::Transport`]), and
//! malformed-response conditions. Inspect the
//! [`code`](Error::code), [`http_status`](Error::http_status),
//! [`request_id`](Error::request_id), and
//! [`retry_after`](Error::retry_after) accessors.
//!
//! # Retries
//!
//! The default [`RetryPolicy`] is `TransientRetry::new(5)` —
//! up to 5 retries on the documented transient codes
//! (`backpressure`, `unavailable`, `namespace_unavailable`,
//! `metering_unavailable`). Server `retry_after_ms` hints
//! (body or RFC 9110 `Retry-After` header) are honoured in preference
//! to the `100ms, 2s, 5s, 10s` fallback schedule (saturating at 10s).
//! The same policy is applied to the `subscribe` handshake — replay is
//! safe at setup time because no matches can be missed before the
//! `subscription_id` is issued. Mid-stream disconnects are NOT
//! auto-retried; surface them and let the caller decide how to
//! reconnect and dedupe.
//!
//! Publish without an [`PublishRequest::idempotency_key`] is NOT safe
//! to retry — pass an idempotency key to make publishes retry-safe.

mod client;
mod defaults;
mod error;
mod health;
mod limits;
mod lint;
mod publish;
mod retry;
mod search;
mod sse;
mod subscribe;
mod transport;
mod validate;
mod version;

pub use client::{Client, ClientBuilder};
pub use error::{Error, ErrorCode};
pub use limits::{
    MAX_IDEMPOTENCY_KEY_LEN, MAX_LINT_BODY_BYTES, MAX_METADATA_KEYS, MAX_METADATA_KEY_LEN,
    MAX_METADATA_TOTAL_BYTES, MAX_METADATA_VALUE_LEN, MAX_PUBLISH_BODY_BYTES,
    MAX_SEARCH_BODY_BYTES, MAX_SUBSCRIBE_BODY_BYTES, MAX_TEXT_BYTES, MAX_VECTOR_DIM,
};
pub use lint::{LintCompletion, LintDiagnostic, LintRequest, LintResponse};
pub use publish::{AckMode, PublishItem, PublishRequest, PublishResponse};
pub use retry::{NoRetry, RetryPolicy, TransientRetry};
pub use search::{ResultItem, SearchRequest, SearchResponse};
pub use subscribe::{MatchEvent, SubscribeRequest, Subscription};
pub use version::{user_agent, VERSION};

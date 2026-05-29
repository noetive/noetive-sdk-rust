//! Server-side limits mirrored from the Noetive Semantik public API.
//!
//! The SDK's pre-flight validation uses these to fail fast before
//! sending a request the server would reject.

/// Largest embedding vector dimensionality the server accepts in a
/// single publish item.
pub const MAX_VECTOR_DIM: u16 = 4096;

/// Largest text payload (UTF-8 bytes) the server accepts in a single
/// publish item.
pub const MAX_TEXT_BYTES: usize = 32 * 1024;

/// Largest number of metadata keys accepted.
pub const MAX_METADATA_KEYS: usize = 16;

/// Largest single metadata key length in bytes.
pub const MAX_METADATA_KEY_LEN: usize = 64;

/// Largest single metadata value length in bytes.
pub const MAX_METADATA_VALUE_LEN: usize = 256;

/// Largest sum of all metadata key and value UTF-8 bytes.
pub const MAX_METADATA_TOTAL_BYTES: usize = 4 * 1024;

/// `/v1/search` body size limit.
pub const MAX_SEARCH_BODY_BYTES: usize = 1024 * 1024;

/// `/v1/publish` body size limit.
pub const MAX_PUBLISH_BODY_BYTES: usize = 2 * 1024 * 1024;

/// `/v1/subscribe` body size limit.
pub const MAX_SUBSCRIBE_BODY_BYTES: usize = 1024 * 1024;

/// `/v1/lint` body size limit.
pub const MAX_LINT_BODY_BYTES: usize = 64 * 1024;

/// Conservative cap on idempotency keys published by clients.
pub const MAX_IDEMPOTENCY_KEY_LEN: usize = 256;

/// Cap on response bodies read for decoding (errors and structured
/// responses). Larger payloads are rejected as malformed without ever
/// being buffered in full. 1 MiB comfortably covers every documented
/// Semantik response.
pub(crate) const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Cap on a single SSE frame's accumulated `data` payload. Mirrors the
/// Go SDK's `internal/sse.MaxFrameBytes`.
pub(crate) const MAX_SSE_FRAME_BYTES: usize = 64 * 1024;

/// Cap on error response bodies read for decoding. Larger error bodies
/// are unusual; this prevents an unbounded read on a misbehaving proxy.
pub(crate) const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Largest retry-after hint the SDK accepts. A misconfigured or
/// malicious server emitting a huge value would otherwise park a
/// retrying caller for weeks; one hour is far longer than any
/// legitimate transient-retry hint.
pub(crate) const MAX_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(3600);

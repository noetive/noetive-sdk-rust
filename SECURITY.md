# Security Policy

## Scope

This document covers vulnerabilities in the `noetive` crate source
tree: the Rust modules under `src/`, their direct dependencies as
pinned in `Cargo.toml`, and the example programs under `examples/`.

Out of scope:

- The Semantik service itself (different repository, different
  operational boundary). Report service vulnerabilities through the
  channels on <https://noetive.io>.
- Issues in transitive dependencies that are not reachable from SDK
  code.
- API keys or other credentials leaked outside the SDK (e.g. in your
  application logs, environment, or shell history).

## Supported versions

Only the latest released minor line receives security fixes. The SDK
follows semantic versioning; once 1.0 is cut, the previous stable
minor line will be maintained until the next release and a
deprecation note will land here.

| Version | Status |
|---|---|
| 0.1.x | Supported |
| < 0.1 | Unsupported |

## Reporting a vulnerability

Please report vulnerabilities privately. Do **not** open a public
GitHub issue or pull request for a suspected security bug.

Report by **email** to <security@noetive.eu>, using subject line
`[noetive-sdk-rust security] <short description>`.

Include, where possible:

- SDK version (`noetive::semantik::VERSION` or the `Cargo.toml` pin).
- Rust toolchain (`rustc --version`) and OS / architecture.
- A minimal reproduction (a `wiremock` server is ideal for decoder
  or scanner bugs).
- Impact assessment (crash, hang, memory disclosure, credential leak,
  request forgery, etc.).
- Any known workaround.

## What happens next

- **Acknowledgement** within 3 business days.
- **Triage and severity assessment** using CVSS v3.1.
- **Fix window** proportional to severity:
  - Critical/High: aim for a patched release within 30 days.
  - Medium/Low: bundled into the next scheduled release.
- **Coordinated disclosure**: we publish the advisory and credit the
  reporter (if they consent) after a fix is released. Embargo length
  is negotiated with the reporter.
- **CVE assignment**: we request a CVE for anything Medium or above.

## Known security-relevant behaviour

This section documents load-bearing safety decisions so that
third-party auditors can confirm them rather than having to infer from
code.

- **Credential handling.** API keys are held in the `Client` struct
  inside a `HeaderValue` marked `sensitive`, then sent as
  `Authorization: Bearer <key>`. The SDK never logs the key and
  never writes it to disk. The `Debug` impl on `Client` redacts the
  key (`api_key: "REDACTED"`). The User-Agent does not include any
  secret material. `Client::new` rejects only empty / whitespace-only
  keys; it does not inspect the key's prefix or contents. Deeper
  validation is the server's job, and prefix-locking the client would
  break the moment Noetive introduces a new key family.
- **TLS.** The default `reqwest::Client` is built with
  `rustls-tls` (not OpenSSL). TLS verification is enabled by
  default; callers who need custom roots or a corporate proxy must
  supply their own `reqwest::Client` via
  `ClientBuilder::http_client`.
- **JSON decoder.** The SDK uses `serde_json` for encode and decode.
  Response bodies (both 2xx and error envelopes) are size-capped
  before decoding (`MAX_RESPONSE_BYTES` = 1 MiB,
  `MAX_ERROR_BODY_BYTES` = 64 KiB) so that a misbehaving server
  cannot exhaust client memory.
- **SSE stream.** Per-frame `data` is capped at
  `MAX_SSE_FRAME_BYTES` = 64 KiB. The `Content-Type` of
  `/v1/subscribe` responses is validated to be `text/event-stream`
  before any body bytes are consumed. Bare `\r`, `\r\n` and `\n`
  line endings are all tolerated; unknown SSE directives (`id`,
  `retry`, ...) are silently ignored per the spec.
- **Retry-after caps.** Server-provided `retry_after_ms` (body) and
  `Retry-After` (header) hints are clamped to one hour. A
  misconfigured or malicious server emitting `u32::MAX` ms (~49
  days) cannot park a retrying caller for weeks.
- **Retry safety.** By default the SDK absorbs up to five transient
  server hiccups per call. It honours the server's wait hint when
  one is offered and otherwise falls back to a bounded linear delay
  so a missing hint cannot turn a transient failure into a terminal
  one. Errors that signal a caller-side problem — bad input,
  missing or invalid auth, billing not in good standing, hard rate
  limits — are never retried automatically; they fail fast.
  Callers needing strict one-shot semantics can pass
  `ClientBuilder::retry(NoRetry)`. Publish without an
  `idempotency_key` is the caller's responsibility: pair every
  retry-eligible publish with a key so a retry cannot duplicate.
- **Test coverage.** Wire tests in `tests/wire.rs` cover the
  HTTP-level contract using `wiremock`. The SSE scanner has
  property-style smoke tests for arbitrary byte input. Integration
  tests in `tests/integration.rs` exercise the live endpoint when
  `NOETIVE_KEY_SECRET` is set.

## Hall of fame

Reporters who submit valid vulnerabilities will be credited here
(with their consent) once a fix is released.

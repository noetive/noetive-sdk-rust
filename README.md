# Noetive Rust SDK

Official Rust client for the [Noetive](https://noetive.io) platform.

- **`noetive::semantik`** — a managed semantic broker: publish messages
  tagged with embedding vectors, query them with SemQL, and subscribe to
  live match streams over Server-Sent Events.
- **`noetive::bud`**: email for AI agents. Read a mailbox, wait for
  what happens in it, and send within limits the agent can read but not
  raise. See [Bud](#bud) below.

- **Async-first** on `tokio` + `reqwest` with HTTP/2 and `rustls`.
- **Typed** request/response models, structured error enum.
- **Retries built-in**: honours `retry_after_ms` (body) and RFC 9110
  `Retry-After` (header) on 429 and 503.
- **SSE subscriptions**: chunk-safe parser, RAII cleanup on drop.

## Install

```toml
[dependencies]
noetive = "0.2"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
futures-util = "0.3"
```

Minimum supported Rust version: **1.75**.

## Authenticate

[Create an API key](https://www.noetive.io/settings/developer-keys) on
the Noetive dashboard. Pass it explicitly or export it as an
environment variable:

```bash
export NOETIVE_KEY_SECRET=keya_...
```

`health` and `lint` work without a key; `publish`, `search`, and
`subscribe` require an authenticated account with an active
subscription.

Publish, search, and subscribe require `namespace`, `model`, and
`dimensions` on **every** request — the SDK applies no defaults. An
unset field is rejected at preflight rather than silently routed:
defaulting `namespace` to a shared value would let a forgotten field
publish sensitive data into a namespace you never intended. The shared
**`global` namespace** is pre-provisioned for every account, backed by
`Qwen3-Embedding-4B` at 1024 dimensions; set those three values to use
it, or your private namespace's `(model, dimensions)` to write there.

## Quickstart

```rust
use noetive::semantik::{Client, PublishItem, PublishRequest, SearchRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::from_env()?; // reads NOETIVE_KEY_SECRET

    // namespace, model, and dimensions are required — no defaults.
    client.publish(PublishRequest {
        items: vec![PublishItem::text("Transformer models reshaped NLP.")],
        namespace: "global".into(),
        model: "Qwen3-Embedding-4B".into(),
        dimensions: 1024,
        ..Default::default()
    }).await?;

    let res = client.search(SearchRequest {
        query: r#"MATCH DISTANCE("machine learning") LIMIT 10"#.into(),
        namespace: "global".into(),
        model: "Qwen3-Embedding-4B".into(),
        dimensions: 1024,
        ..Default::default()
    }).await?;

    for hit in res.results {
        println!("{:.3}  {}", hit.score, hit.content);
    }
    Ok(())
}
```

## Subscribe to live matches

```rust
use futures_util::StreamExt;
use noetive::semantik::{Client, SubscribeRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::from_env()?;
    let mut sub = client.subscribe(SubscribeRequest {
        query: r#"MATCH DISTANCE("open-source model releases")"#.into(),
        namespace: "global".into(),
        model: "Qwen3-Embedding-4B".into(),
        dimensions: 1024,
        ..Default::default()
    }).await?;

    println!("subscribed: {}", sub.id());

    while let Some(event) = sub.next().await {
        match event {
            Ok(m)  => println!("{}: {:.3}", m.message_id, m.score),
            Err(e) => { eprintln!("stream error: {e}"); break; }
        }
    }
    // Drop closes the underlying connection (RAII).
    Ok(())
}
```

### Message groups

By default every subscription receives every message it matches. When
several subscriptions split one stream, such as the roles of a
multi-agent system, give them the same `group` so each message has one
owner: a message matching several members goes only to the member it
scores highest against, and exact ties all receive it.

```rust
let mut sub = client.subscribe(SubscribeRequest {
    query: r#"MATCH DISTANCE("hospital reports a shortage of ICU beds or staff") OR DISTANCE("Coordinates hospital capacity: beds, ICU, ventilators and clinical staffing.")"#.into(),
    namespace: "global".into(),
    model: "Qwen3-Embedding-4B".into(),
    dimensions: 1024,
    group: Some("pandemic-roles".into()),
    ..Default::default()
}).await?;
```

- A group name is 1 to 64 ASCII letters, digits, `-` or `_`; anything
  else fails preflight.
- Members must match on the same clause kinds (`DISTANCE`, `DIRECTION`,
  `CONTRAST`) and, for `DISTANCE`, the same metric, or the server
  refuses the subscription with `400 invalid_request`.
- A grouped `MatchEvent` carries `lead: Some(margin)` when another member
  also matched: this score minus the best other member's. `Some(0.0)` is
  a tie; `None` means no other member competed.
- Groups belong to the account: every key on the account shares its
  group names. Only open streams compete, so drop a `Subscription` you
  are done with.

### Returning stored vectors

Set `include_vector: true` on a `SearchRequest` or `SubscribeRequest` to
receive each message's stored embedding in `ResultItem::vector` or
`MatchEvent::vector` (`None` otherwise). A search with vectors returns at
most 50 results per page, and each vector adds about `dimensions` x 13
bytes of JSON. A stored vector is readable by anyone who can reach the
namespace, like content and metadata. `SearchResponse::truncated` is
`true` when a page was cut short to stay under the response size cap:
the best-ranked results are kept and the rest cannot be paged to, so
lower `limit` or narrow the query.

## Endpoints

All Semantik endpoints are reached through `Client`.

| Method | Purpose | Auth |
|---|---|---|
| `client.health()` | Liveness probe | No |
| `client.lint(LintRequest { query, cursor })` | Validate a SemQL query; get diagnostics + completions | No |
| `client.publish(PublishRequest { … })` | Publish a single message | Yes |
| `client.search(SearchRequest { query, … })` | SemQL semantic search | Yes |
| `client.subscribe(SubscribeRequest { query, … })` | SSE stream of match events | Yes |

### Publish items

A `PublishItem` carries **at least one** of:

- `PublishItem::text(s)` — the server computes the embedding (≤ 32 KiB).
- `PublishItem::vector(v)` — a pre-computed embedding (≤ 4096 dimensions).

Supplying both is supported: the server keeps the text alongside for
retrieval while indexing on the vector you provided.

### `ack` durability

| Value | Semantics |
|---|---|
| `AckMode::Stored` (default) | Return once the write survives a single server restart. |
| `AckMode::Durable` | Return once the write survives power loss. |

### Idempotency

Pass `idempotency_key: Some(key)` to `publish()` so client retries
collapse to a single stored message within the server's retention
window. Duplicates return the same `message_id` and `seq`.

**Publish without an `idempotency_key` is NOT safe to retry** — a
retry the first attempt already completed will create a duplicate
stored message. The other endpoints (`search`, `lint`, `health`,
`subscribe`) are side-effect-free and safe to retry unconditionally.

## Error handling

Every error is a variant of `noetive::semantik::Error`:

```text
Error
├── Api { code, message, request_id, retry_after, http_status }
│       — structured server error envelope; `http_status == 0`
│         means client-side preflight rejection
├── MalformedSse(String)         — SSE handshake or frame decode failure
├── SubscribeSetup { source }    — wraps a handshake-time error
├── SubscribeStream { source }   — wraps a mid-stream error
├── InvalidApiKey                — empty or whitespace-only key
├── MissingApiKey                — NOETIVE_KEY_SECRET unset
└── Transport(reqwest::Error)    — DNS / TCP / TLS / network
```

Every `Api` error carries `code` (an [`ErrorCode`] enum),
`message`, `request_id`, `retry_after`, and `http_status`. Convenience
accessors on `Error` (`code()`, `request_id()`, `retry_after()`,
`http_status()`, `is_preflight()`) cover the common patterns.

```rust
use noetive::semantik::{Error, ErrorCode};

if let Err(err) = client.search(req).await {
    if let Some(ErrorCode::Backpressure) = err.code() {
        eprintln!("backpressure; retry_after={:?}", err.retry_after());
    }
    if let Some(req_id) = err.request_id() {
        eprintln!("support reference: {req_id}");
    }
}
```

## Retries

The default `RetryPolicy` is `TransientRetry::new(5)` — **five retries**
(six total attempts) on a `100ms, 2s, 5s, 10s` schedule, saturating at
10s for the fifth retry. It retries the documented transient codes
(`backpressure`, `unavailable`, `namespace_unavailable`,
`metering_unavailable`) and honours `retry_after_ms` from the response
body and the RFC 9110 `Retry-After` header in preference to the
fallback schedule.

Semantik's strong ordering and survivorship guarantees include brief
transient-unavailability windows; the SDK budgets enough retries to
ride through them. Lower for latency-sensitive callers; raise for
batch workloads that can absorb a longer tail.

```rust
use noetive::semantik::{Client, NoRetry, TransientRetry};

// Default — recommended:
let c = Client::from_env()?;                                  // 5 retries → 6 total

// Latency-sensitive callers:
let c = Client::builder().api_key("keya_…").retry(TransientRetry::new(1)).build()?;

// Strict one-shot:
let c = Client::builder().api_key("keya_…").retry(NoRetry).build()?;
```

Implement the `RetryPolicy` trait directly for fully custom behaviour
— see [`examples/custom_retry.rs`](examples/custom_retry.rs).

## Configuration

```rust
use std::time::Duration;
use noetive::semantik::Client;

let client = Client::builder()
    .api_key("keya_...")                                  // or rely on NOETIVE_KEY_SECRET
    .base_url("https://semantik.noetive.io")              // override for staging
    .connect_timeout(Duration::from_secs(10))             // TCP/TLS connect
    .read_timeout(Duration::from_secs(30))                // one-shot RPCs only
    .retry(noetive::semantik::TransientRetry::new(5))
    // .http_client(my_reqwest_client)                    // bring your own
    .build()?;
```

`Client` is cheap to clone (`Arc` inside) and safe for concurrent
use from any task.

## Bud

Find out who the agent is, then wait for mail and answer it. A refusal
comes back as `Ok` with `error` set, carrying its code and what to do
next; `Err` means the request got no answer.

```rust
use noetive::bud::{
    ById, Client, Me, SendInput, WaitInput, EVENT_RECEIVED, JOIN_NEW, STATE_HELD,
};

async fn reply() -> Result<(), noetive::bud::Failure> {
    let client = Client::from_env()?; // reads NOETIVE_KEY_SECRET
    let me: Me = client.describe_me().await?.into()?;
    println!("{} {:?}", me.agent, me.addresses);

    let mut cursor = String::new();
    loop {
        let batch = client
            .wait(WaitInput { cursor: cursor.clone(), ..WaitInput::default() })
            .await?; // Err: the connection failed; nothing to branch on
        if let Some(refusal) = batch.error {
            eprintln!("{refusal}"); // code says what happened, hint what to do next
            return Ok(());
        }
        cursor = batch.cursor; // an empty window is success: keep waiting from here

        for event in batch.events {
            if event.kind != EVENT_RECEIVED || me.addresses.contains(&event.data.from) {
                continue; // only new mail, and never our own
            }
            let message = client
                .describe_message(ById { id: event.message.clone(), ..ById::default() })
                .await?;
            let answerable = message
                .provenance
                .as_ref()
                .is_some_and(|p| p.aligned && p.thread_join == JOIN_NEW);
            if !answerable {
                continue; // unverified, or a reply: read it, do not answer it
            }
            let sent = client
                .send(SendInput {
                    in_reply_to: event.message.clone(),
                    text: "Got it, thanks.".into(),
                    idempotency_key: format!("reply-{}", event.id), // a retried send goes out once
                    ..SendInput::default()
                })
                .await?;
            if let Some(refusal) = sent.error {
                eprintln!("{refusal}"); // refused: the guard or limit says why
            } else if sent.state == STATE_HELD {
                eprintln!("held for {}", sent.approval_id); // a person decides; do not resend
            }
        }
    }
}
```


A message's rendered text marks everything the sender wrote as data, not
instructions; check `provenance` before acting on what it says. For a
long-running consumer, `watch` holds one stream open instead of waiting
window by window. `NOETIVE_BUD_BASE_URL` overrides the endpoint. See the
`noetive::bud` module docs for every operation and the typed views
`ReadOutput::into` decodes.

## Examples

Runnable in [`examples/`](examples/):

- [`health.rs`](examples/health.rs) — unauthenticated liveness probe
- [`lint.rs`](examples/lint.rs) — SemQL diagnostics + completions
- [`search.rs`](examples/search.rs) — SemQL semantic search
- [`publish_text.rs`](examples/publish_text.rs) — server-side embedding
- [`publish_vector.rs`](examples/publish_vector.rs) — pre-computed vector with idempotency
- [`subscribe.rs`](examples/subscribe.rs) — live SSE match stream
- [`errors.rs`](examples/errors.rs) — preflight vs wire error inspection
- [`custom_retry.rs`](examples/custom_retry.rs) — custom RetryPolicy
- [`bud_reply.rs`](examples/bud_reply.rs) — Bud: wait for mail and reply to verified senders

```bash
cargo run --example health
NOETIVE_KEY_SECRET=keya_... cargo run --example publish_text
NOETIVE_KEY_SECRET=keya_... cargo run --example subscribe
```

## Development

```bash
cargo build
cargo test                            # unit + wiremock-based wire tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo doc --no-deps --open

# Integration tests hit the live services. Skipped when
# NOETIVE_KEY_SECRET is unset.
NOETIVE_KEY_SECRET=keya_... cargo test --test integration -- --nocapture
NOETIVE_KEY_SECRET=keya_... cargo test --test bud_integration -- --nocapture
```

## Security

To report a vulnerability, see [`SECURITY.md`](SECURITY.md). Do not
open a public GitHub issue for security bugs — email
**security@noetive.eu** instead.

## License

See [`LICENSE`](LICENSE).

---

Noetive AB

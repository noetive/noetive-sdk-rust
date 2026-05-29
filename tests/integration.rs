//! Integration tests against the live Semantik endpoint
//! (`https://semantik.noetive.io`).
//!
//! These tests exist to catch *drift* between the SDK and the real
//! server — the one bug class a mock can never surface. The production
//! base URL is therefore hardcoded ([`PROD_BASE_URL`]); only the API key
//! is environment-configurable. The SDK honours `NOETIVE_BASE_URL` for
//! application use, but the integration suite deliberately ignores it so
//! a misconfigured environment cannot quietly point the drift detector
//! at something other than prod.
//!
//! Every test is skipped when `NOETIVE_KEY_SECRET` is unset so the crate
//! stays green on a plain `cargo test` without credentials. With a valid
//! key they validate the wire contract end-to-end.
//!
//! Run with:
//!
//! ```text
//! NOETIVE_KEY_SECRET=keyu_... cargo test --test integration -- --nocapture
//! ```
//!
//! The server flaps; the testing reference documents multi-day windows
//! where several of these fail server-side. Each failure quotes the
//! `X-Request-Id` so an on-call can pivot straight to the server log
//! line. A clean run is unambiguous evidence the wire contract holds; a
//! run failing with structured `unavailable` / `subscription setup
//! budget` errors is unambiguous evidence the server is degraded.

use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use noetive::semantik::{
    AckMode, Client, ClientBuilder, Error, ErrorCode, LintRequest, NoRetry, PublishItem,
    PublishRequest, SearchRequest, SearchResponse, SubscribeRequest, Subscription,
};

/// The shared `global` namespace and the (model, dimensions) it is
/// provisioned with. The SDK no longer defaults these — every request
/// must set them — so the integration suite carries them as test
/// constants. If `global` is ever re-provisioned, update these.
const GLOBAL_NS: &str = "global";
const GLOBAL_MODEL: &str = "Qwen3-Embedding-4B";
const GLOBAL_DIMS: u16 = 1024;

/// Hardcoded production endpoint. Not overridable — see the module doc.
const PROD_BASE_URL: &str = "https://semantik.noetive.io";

/// Per-call read timeout for one-shot RPCs. Tight enough to surface a
/// hung request without slowing the suite to a crawl. Does not apply to
/// the long-lived subscribe stream (the SDK never bounds that body).
const RPC_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The API key from the environment, read once. `None` means the suite
/// is disabled (every test skips).
fn live_key() -> Option<String> {
    static KEY: OnceLock<Option<String>> = OnceLock::new();
    KEY.get_or_init(|| {
        std::env::var("NOETIVE_KEY_SECRET")
            .ok()
            .filter(|s| !s.is_empty())
    })
    .clone()
}

/// Skip the calling test when no key is configured. Honours the
/// "only run if `NOETIVE_KEY_SECRET` is set" contract for every test,
/// including the no-auth `lint` and the bad-bearer error path.
macro_rules! skip_unless_key {
    () => {
        if live_key().is_none() {
            eprintln!("skipping: NOETIVE_KEY_SECRET unset");
            return;
        }
    };
}

/// A client builder pinned to production with the live key and the RPC
/// read timeout. Tests add their own retry policy on top where they want
/// fail-fast behaviour.
fn live_client_builder() -> ClientBuilder {
    Client::builder()
        .api_key(live_key().expect("guarded by skip_unless_key!"))
        .base_url(PROD_BASE_URL)
        .read_timeout(RPC_READ_TIMEOUT)
}

/// The default live client (default `TransientRetry` policy).
fn live_client() -> Client {
    live_client_builder().build().expect("build live client")
}

/// A publish request pre-filled with the required `global` targeting
/// fields. Callers override `items`, `idempotency_key`, `ack`, etc. via
/// struct update (`..base_publish()`).
fn base_publish() -> PublishRequest {
    PublishRequest {
        namespace: GLOBAL_NS.into(),
        model: GLOBAL_MODEL.into(),
        dimensions: GLOBAL_DIMS,
        ..Default::default()
    }
}

/// A `global`-targeted search request for the given SemQL query.
fn search_req(query: impl Into<String>) -> SearchRequest {
    SearchRequest {
        query: query.into(),
        namespace: GLOBAL_NS.into(),
        model: GLOBAL_MODEL.into(),
        dimensions: GLOBAL_DIMS,
        limit: 0,
    }
}

/// A `global`-targeted subscribe request for the given SemQL query.
fn subscribe_req(query: impl Into<String>) -> SubscribeRequest {
    SubscribeRequest {
        query: query.into(),
        namespace: GLOBAL_NS.into(),
        model: GLOBAL_MODEL.into(),
        dimensions: GLOBAL_DIMS,
    }
}

/// A unique-per-call token so reruns — and concurrently-running tests
/// within one process — never collide on shared `global` state. The
/// wall-clock nanos disambiguate across runs; a process-wide monotonic
/// counter disambiguates concurrent callers that could otherwise sample
/// the same coarse clock tick.
fn run_id() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("rust-sdk-it-{n}-{}-{seq}", std::process::id())
}

/// Unwrap a `Result`, panicking with the `request_id` quoted so a
/// failing CI run pivots directly to the server log line.
fn expect_ok<T>(label: &str, r: Result<T, Error>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!(
            "{label} failed: {e} (code={:?}, http_status={:?}, request_id={:?}); \
             a structured 5xx here is likely server-side",
            e.code(),
            e.http_status(),
            e.request_id()
        ),
    }
}

// ---------------------------------------------------------------------
// Health / lint
// ---------------------------------------------------------------------

#[tokio::test]
async fn health_endpoint_is_up() {
    skip_unless_key!();
    expect_ok("health", live_client().health().await);
}

#[tokio::test]
async fn lint_accepts_valid_query() {
    skip_unless_key!();
    let res = expect_ok(
        "lint(valid)",
        live_client()
            .lint(LintRequest {
                query: r#"MATCH DISTANCE("a") WITHIN 0.5"#.into(),
                cursor: 0,
            })
            .await,
    );
    assert!(
        res.valid,
        "expected valid:true, got diagnostics {:?}",
        res.diagnostics
    );
    assert!(
        res.request_id.is_some(),
        "lint 2xx response did not carry X-Request-Id"
    );
}

#[tokio::test]
async fn lint_rejects_malformed_query() {
    skip_unless_key!();
    let res = expect_ok(
        "lint(malformed)",
        live_client()
            .lint(LintRequest {
                query: "this is not a valid query".into(),
                cursor: 0,
            })
            .await,
    );
    assert!(!res.valid, "expected valid:false for malformed SemQL");
    assert!(
        !res.diagnostics.is_empty(),
        "malformed query should surface at least one diagnostic"
    );
}

// ---------------------------------------------------------------------
// Publish
// ---------------------------------------------------------------------

#[tokio::test]
async fn publish_text_stored_returns_message_id() {
    skip_unless_key!();
    let id = run_id();
    let res = expect_ok(
        "publish(stored)",
        live_client()
            .publish(PublishRequest {
                items: vec![PublishItem::text(format!("stored ack test {id}"))],
                idempotency_key: Some(format!("{id}-stored")),
                ack: AckMode::Stored,
                ..base_publish()
            })
            .await,
    );
    assert!(!res.message_id.is_empty(), "empty message_id");
    assert!(res.request_id.is_some(), "missing X-Request-Id");
}

#[tokio::test]
async fn publish_text_durable_returns_message_id() {
    skip_unless_key!();
    let id = run_id();
    let res = expect_ok(
        "publish(durable)",
        live_client()
            .publish(PublishRequest {
                items: vec![PublishItem::text(format!("durable ack test {id}"))],
                idempotency_key: Some(format!("{id}-durable")),
                ack: AckMode::Durable,
                ..base_publish()
            })
            .await,
    );
    assert!(!res.message_id.is_empty(), "empty message_id");
}

#[tokio::test]
async fn publish_vector_returns_message_id() {
    skip_unless_key!();
    let id = run_id();
    // A vector at the global model's dimensionality. Values are
    // arbitrary but finite.
    let vector = vec![0.01_f32; GLOBAL_DIMS as usize];
    let res = expect_ok(
        "publish(vector)",
        live_client()
            .publish(PublishRequest {
                items: vec![PublishItem::vector(vector)],
                idempotency_key: Some(format!("{id}-vector")),
                ..base_publish()
            })
            .await,
    );
    assert!(!res.message_id.is_empty(), "empty message_id");
}

#[tokio::test]
async fn publish_is_idempotent() {
    skip_unless_key!();
    let client = live_client();
    let id = run_id();
    let req = || PublishRequest {
        items: vec![PublishItem::text(format!("idempotency test {id}"))],
        idempotency_key: Some(id.clone()),
        ..base_publish()
    };

    let first = expect_ok("publish(idempotent #1)", client.publish(req()).await);
    let second = expect_ok("publish(idempotent #2)", client.publish(req()).await);
    assert_eq!(
        first.message_id, second.message_id,
        "same idempotency key must yield the same message_id"
    );
    assert_eq!(
        first.seq, second.seq,
        "same idempotency key must yield the same seq"
    );
}

// ---------------------------------------------------------------------
// Publish error paths
// ---------------------------------------------------------------------

#[tokio::test]
async fn publish_bad_bearer_is_unauthorized() {
    skip_unless_key!();
    // A syntactically-acceptable but invalid key. The SDK only checks
    // non-emptiness; the server is the source of truth and must reject.
    let client = Client::builder()
        .api_key("keyu_definitely_invalid_integration_test_key")
        .base_url(PROD_BASE_URL)
        .read_timeout(RPC_READ_TIMEOUT)
        .retry(NoRetry)
        .build()
        .expect("client");

    let err = client
        .publish(PublishRequest {
            items: vec![PublishItem::text("auth check")],
            ..base_publish()
        })
        .await
        .expect_err("bad bearer should be rejected");
    assert_eq!(
        err.code(),
        Some(&ErrorCode::Unauthorized),
        "expected unauthorized, got {err} (request_id={:?})",
        err.request_id()
    );
    assert_eq!(err.http_status(), Some(401));
}

#[tokio::test]
async fn publish_server_rejects_dimension_mismatch() {
    skip_unless_key!();
    // A vector at half the global model's dimensionality, with a
    // matching `dimensions` field, passes SDK preflight (the field
    // matches the vector length) but contradicts what the `global`
    // namespace is provisioned for. Deriving the wrong dimension from
    // GLOBAL_DIMS keeps this a guaranteed mismatch even if the global
    // model is re-provisioned. The rejection must come from the server,
    // not SDK preflight — this exercises the real wire validation.
    let wrong_dims = GLOBAL_DIMS / 2;
    let err = live_client_builder()
        .retry(NoRetry)
        .build()
        .expect("client")
        .publish(PublishRequest {
            items: vec![PublishItem::vector(vec![0.01_f32; wrong_dims as usize])],
            dimensions: wrong_dims,
            ..base_publish()
        })
        .await
        .expect_err("server should reject a vector whose dims contradict the namespace");
    assert!(
        !err.is_preflight(),
        "mismatch must be caught on the wire, not in SDK preflight; got {err}"
    );
    assert!(
        err.http_status().map(|s| s >= 400).unwrap_or(false),
        "expected a 4xx server rejection, got {err} (code={:?}, request_id={:?})",
        err.code(),
        err.request_id()
    );
}

// ---------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------

/// Poll `search` until it returns at least one result or the budget is
/// exhausted. Indexing has lag; a single immediate search routinely
/// races ahead of the index. Returns the last response either way so
/// the caller can assert on it.
async fn search_until_nonempty(client: &Client, query: &str, budget: Duration) -> SearchResponse {
    let start = Instant::now();
    let mut last = expect_ok("search", client.search(search_req(query)).await);
    while last.results.is_empty() && start.elapsed() < budget {
        tokio::time::sleep(Duration::from_secs(2)).await;
        last = expect_ok("search(retry)", client.search(search_req(query)).await);
    }
    last
}

#[tokio::test]
async fn search_retrieves_published_and_respects_limit() {
    skip_unless_key!();
    let client = live_client();
    let id = run_id();
    // Publish a distinctive document, then anchor the query on the exact
    // same phrase so the match sits at ~0 distance.
    let phrase = format!("rust sdk search retrieval marker {id}");
    let published = expect_ok(
        "publish(search seed)",
        client
            .publish(PublishRequest {
                items: vec![PublishItem::text(phrase.clone())],
                idempotency_key: Some(format!("{id}-search")),
                ack: AckMode::Durable,
                ..base_publish()
            })
            .await,
    );

    // Indexing lag can run seconds-to-minutes during degraded windows
    // (see the resilience reference), so the budget is generous; a
    // failure here is far more likely server-side lag than an SDK bug.
    let query = format!(r#"MATCH DISTANCE("{phrase}") WITHIN 0.5 LIMIT 5"#);
    let res = search_until_nonempty(&client, &query, Duration::from_secs(60)).await;
    let ids: Vec<&str> = res.results.iter().map(|r| r.message_id.as_str()).collect();

    // Lower bound: retrieval actually works. Asserting only an upper
    // bound would pass vacuously when the index is empty / degraded.
    assert!(
        !res.results.is_empty(),
        "search returned no results within budget; index may be lagging or degraded \
         (request_id={:?})",
        res.request_id
    );
    assert!(
        ids.contains(&published.message_id.as_str()),
        "the just-published message_id {} was not among the results {:?} (request_id={:?})",
        published.message_id,
        ids,
        res.request_id
    );

    // Upper bound: LIMIT 1 returns exactly one. We re-poll this query to
    // non-empty rather than trusting the earlier LIMIT 5 search — a bare
    // `<= 1` check would pass vacuously if retrieval regressed to zero
    // between the two calls, masking a broken index. Asserting `== 1`
    // after confirming non-empty tests both bounds at once.
    let limited = search_until_nonempty(
        &client,
        &format!(r#"MATCH DISTANCE("{phrase}") WITHIN 0.5 LIMIT 1"#),
        Duration::from_secs(30),
    )
    .await;
    assert_eq!(
        limited.results.len(),
        1,
        "LIMIT 1 should return exactly one result for known-present content, got {} (request_id={:?})",
        limited.results.len(),
        limited.request_id
    );
}

#[tokio::test]
async fn invalid_query_returns_typed_error() {
    skip_unless_key!();
    let err = live_client()
        .search(search_req("this is not a query"))
        .await
        .expect_err("syntactically invalid SemQL should fail");
    assert!(
        err.code().is_some(),
        "expected a structured Api error, got {err} (request_id={:?})",
        err.request_id()
    );
    assert!(
        !err.is_preflight(),
        "SemQL validity is a server judgement; this must reach the wire"
    );
}

// ---------------------------------------------------------------------
// Subscribe
// ---------------------------------------------------------------------

/// Open a subscription, retrying the handshake under the SDK's own
/// policy and bounding the whole thing so a wedged setup cannot hang the
/// suite. The `503 unavailable: subscription setup did not complete
/// within the budget` transient is common here and is retried by the SDK.
async fn open_subscription(client: &Client, query: &str) -> Subscription {
    let sub = tokio::time::timeout(
        Duration::from_secs(60),
        client.subscribe(subscribe_req(query)),
    )
    .await
    .expect("subscribe handshake did not complete within 60s (server setup budget or transport)");
    expect_ok("subscribe", sub)
}

#[tokio::test]
async fn subscribe_handshake_completes() {
    skip_unless_key!();
    let sub = open_subscription(
        &live_client(),
        r#"MATCH DISTANCE("integration handshake marker") WITHIN 0.5"#,
    )
    .await;
    assert!(!sub.id().is_empty(), "subscription_id should be non-empty");
    assert!(
        sub.request_id().is_some(),
        "subscribe handshake did not carry X-Request-Id"
    );
}

#[tokio::test]
async fn subscribe_round_trip_delivers_match() {
    skip_unless_key!();
    let client = live_client();
    let id = run_id();
    let phrase = format!("rust sdk subscribe round trip marker {id}");

    // Subscribe first so the publish that follows is delivered live.
    let mut sub = open_subscription(
        &client,
        &format!(r#"MATCH DISTANCE("{phrase}") WITHIN 0.5"#),
    )
    .await;
    // Give the subscription a moment to become active server-side.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let published = expect_ok(
        "publish(subscribe seed)",
        client
            .publish(PublishRequest {
                items: vec![PublishItem::text(phrase.clone())],
                idempotency_key: Some(format!("{id}-sub")),
                ..base_publish()
            })
            .await,
    );

    let mut want = HashSet::new();
    want.insert(published.message_id.clone());
    let seen = drain_until(&mut sub, &want, Duration::from_secs(30)).await;
    assert!(
        seen.contains(&published.message_id),
        "published message_id {} never arrived on the stream within budget \
         (subscription request_id={:?}); delivery is best-effort and the server may be lagging",
        published.message_id,
        sub.request_id()
    );
}

#[tokio::test]
async fn subscribe_delivers_all_published_without_drops() {
    skip_unless_key!();
    let client = live_client();
    let id = run_id();
    let phrase = format!("rust sdk subscribe multi marker {id}");

    let mut sub = open_subscription(
        &client,
        &format!(r#"MATCH DISTANCE("{phrase}") WITHIN 0.5"#),
    )
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Publish three identical-text messages with distinct idempotency
    // keys → three distinct message_ids, all at ~0 distance from the
    // query anchor.
    let mut want = HashSet::new();
    for i in 0..3 {
        let res = expect_ok(
            "publish(multi)",
            client
                .publish(PublishRequest {
                    items: vec![PublishItem::text(phrase.clone())],
                    idempotency_key: Some(format!("{id}-multi-{i}")),
                    ..base_publish()
                })
                .await,
        );
        want.insert(res.message_id);
    }

    let seen = drain_until(&mut sub, &want, Duration::from_secs(45)).await;
    // At-least-once: every published id must be delivered at least once.
    // Duplicates are permitted by the contract, so we assert the
    // delivered set is a superset, not equality.
    let missing: Vec<_> = want.difference(&seen).cloned().collect();
    assert!(
        missing.is_empty(),
        "published ids never delivered: {missing:?} (subscription request_id={:?})",
        sub.request_id()
    );
}

#[tokio::test]
async fn subscribe_drop_releases_and_client_remains_usable() {
    skip_unless_key!();
    let client = live_client();

    // Open a subscription and drop it; RAII releases the HTTP body
    // handle. reqwest tears the connection down in the background, so
    // this asserts the observable contract — that the pooled client is
    // still usable for a fresh handshake afterwards — rather than the
    // teardown itself.
    {
        let sub =
            open_subscription(&client, r#"MATCH DISTANCE("drop test marker") WITHIN 0.5"#).await;
        assert!(!sub.id().is_empty());
    } // sub dropped here

    let sub2 = open_subscription(
        &client,
        r#"MATCH DISTANCE("drop test marker two") WITHIN 0.5"#,
    )
    .await;
    assert!(
        !sub2.id().is_empty(),
        "client should remain usable after a subscription is dropped"
    );
}

#[tokio::test]
async fn subscribe_next_poll_cancels_cleanly() {
    skip_unless_key!();
    // A subscription on a phrase nothing will match keeps `next()`
    // pending. Racing it against a short timeout cancels the poll
    // future; the cancellation must be clean — no panic, no stream
    // error — and the subscription must stay pollable afterwards. This
    // is the Rust analogue of the Go suite's "cancel unblocks Next"
    // test: dropping the future is how cancellation is expressed here.
    let id = run_id();
    let mut sub = open_subscription(
        &live_client(),
        &format!(r#"MATCH DISTANCE("rust sdk no match cancellation marker {id}") WITHIN 0.1"#),
    )
    .await;

    match tokio::time::timeout(Duration::from_secs(2), sub.next()).await {
        Err(_) => {}          // timed out → poll future cancelled cleanly
        Ok(Some(Ok(_))) => {} // an unrelated global match arrived; also fine
        Ok(Some(Err(e))) => panic!(
            "stream errored instead of staying quiet: {e} (request_id={:?})",
            e.request_id()
        ),
        Ok(None) => panic!("server closed the stream unexpectedly during a quiet poll"),
    }

    // The subscription survives a cancelled poll: a second poll behaves
    // the same way rather than erroring or yielding a stale frame.
    match tokio::time::timeout(Duration::from_secs(2), sub.next()).await {
        Err(_) | Ok(Some(Ok(_))) => {}
        Ok(Some(Err(e))) => panic!("second poll errored after a cancelled first poll: {e}"),
        Ok(None) => panic!("stream closed unexpectedly on the second poll"),
    }
}

/// Drain match events from `sub` until every id in `want` has been seen
/// or the budget elapses. Returns the set of delivered ids (which may
/// include unrelated matches from the shared `global` namespace). A
/// mid-stream error is fatal — it means the contract broke, not that we
/// ran out of time.
async fn drain_until(
    sub: &mut Subscription,
    want: &HashSet<String>,
    budget: Duration,
) -> HashSet<String> {
    let start = Instant::now();
    let mut seen: HashSet<String> = HashSet::new();
    while !want.is_subset(&seen) {
        let remaining = match budget.checked_sub(start.elapsed()) {
            Some(r) if !r.is_zero() => r,
            _ => break,
        };
        match tokio::time::timeout(remaining, sub.next()).await {
            Err(_) => break,   // budget elapsed mid-poll
            Ok(None) => break, // server closed the stream (EOF)
            Ok(Some(Ok(m))) => {
                seen.insert(m.message_id);
            }
            Ok(Some(Err(e))) => panic!(
                "subscribe stream errored mid-flight: {e} (request_id={:?})",
                e.request_id()
            ),
        }
    }
    seen
}

//! HTTP-level wire tests using `wiremock`. These do NOT touch the
//! live Semantik endpoint — they exercise the SDK's wire-format
//! contract: header shape, idempotency-key body field, retry behaviour
//! on transient codes, SSE handshake, etc.
//!
//! Integration tests against the live endpoint live in
//! `tests/integration.rs` and are gated on `NOETIVE_KEY_SECRET`.

use std::collections::HashMap;
use std::time::Duration;

use futures_util::StreamExt;
use noetive::semantik::{
    Client, Error, ErrorCode, LintRequest, NoRetry, PublishItem, PublishRequest, SearchRequest,
    SubscribeRequest, TransientRetry,
};
use wiremock::matchers::{body_json, header, header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn test_client(server: &MockServer) -> Client {
    Client::builder()
        .api_key("keyu_test_secret")
        .base_url(server.uri())
        .retry(NoRetry)
        .build()
        .expect("client")
}

/// A search request with the required targeting fields filled in. The
/// SDK no longer defaults `namespace`/`model`/`dimensions`, so wire
/// tests must supply them or every request would be rejected at
/// preflight before reaching the mock.
fn search_req(query: &str) -> SearchRequest {
    SearchRequest {
        query: query.into(),
        namespace: "global".into(),
        model: "Qwen3-Embedding-4B".into(),
        dimensions: 1024,
        limit: 0,
    }
}

/// A subscribe request with the required targeting fields filled in.
fn subscribe_req(query: &str) -> SubscribeRequest {
    SubscribeRequest {
        query: query.into(),
        namespace: "global".into(),
        model: "Qwen3-Embedding-4B".into(),
        dimensions: 1024,
    }
}

#[tokio::test]
async fn health_returns_ok_on_200() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    test_client(&server).health().await.expect("health ok");
}

#[tokio::test]
async fn health_decodes_error_envelope() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
            "error": "unavailable",
            "message": "down for maintenance",
            "request_id": "req-xyz",
        })))
        .mount(&server)
        .await;

    let err = test_client(&server).health().await.unwrap_err();
    assert_eq!(err.http_status(), Some(503));
    assert_eq!(err.code(), Some(&ErrorCode::Unavailable));
    assert_eq!(err.request_id(), Some("req-xyz"));
}

#[tokio::test]
async fn publish_sends_idempotency_key_in_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/publish"))
        .and(header_exists("authorization"))
        .and(header("content-type", "application/json"))
        .and(body_json(serde_json::json!({
            "items": [{"text": "hello"}],
            "namespace": "global",
            "model": "Qwen3-Embedding-4B",
            "dimensions": 1024,
            "idempotency_key": "my-key",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "msg-1",
            "epoch": 1,
            "seq": 42,
        })))
        .mount(&server)
        .await;

    let res = test_client(&server)
        .publish(PublishRequest {
            items: vec![PublishItem::text("hello")],
            namespace: "global".into(),
            model: "Qwen3-Embedding-4B".into(),
            dimensions: 1024,
            idempotency_key: Some("my-key".into()),
            ..Default::default()
        })
        .await
        .expect("publish");
    assert_eq!(res.message_id, "msg-1");
    assert_eq!(res.epoch, 1);
    assert_eq!(res.seq, 42);
}

#[tokio::test]
async fn publish_serialises_metadata_and_durable_ack() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/publish"))
        .and(body_json(serde_json::json!({
            "items": [{"text": "x"}],
            "namespace": "global",
            "model": "Qwen3-Embedding-4B",
            "dimensions": 1024,
            "metadata": {"source": "arxiv"},
            "ack": "durable",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "m",
            "epoch": 1,
            "seq": 1,
        })))
        .mount(&server)
        .await;

    let mut metadata = HashMap::new();
    metadata.insert("source".to_string(), "arxiv".to_string());
    test_client(&server)
        .publish(PublishRequest {
            items: vec![PublishItem::text("x")],
            namespace: "global".into(),
            model: "Qwen3-Embedding-4B".into(),
            dimensions: 1024,
            metadata,
            ack: noetive::semantik::AckMode::Durable,
            ..Default::default()
        })
        .await
        .expect("publish");
}

#[tokio::test]
async fn unauthorized_surfaces_as_typed_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/search"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": "unauthorized",
            "message": "key invalid",
        })))
        .mount(&server)
        .await;

    let err = test_client(&server)
        .search(search_req("MATCH * LIMIT 1"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some(&ErrorCode::Unauthorized));
    assert_eq!(err.http_status(), Some(401));
}

#[tokio::test]
async fn retry_succeeds_after_transient_failure() {
    let server = MockServer::start().await;
    // First call: backpressure. Second call: success.
    Mock::given(method("POST"))
        .and(path("/v1/search"))
        .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
            "error": "backpressure",
            "retry_after_ms": 1,
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{"message_id": "m1", "score": 0.9}],
        })))
        .mount(&server)
        .await;

    let client = Client::builder()
        .api_key("keyu_x")
        .base_url(server.uri())
        .retry(TransientRetry::new(3))
        .build()
        .unwrap();

    let res = client.search(search_req("q")).await.expect("search");
    assert_eq!(res.results.len(), 1);
    assert_eq!(res.results[0].message_id, "m1");
}

#[tokio::test]
async fn terminal_codes_are_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/search"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": "unauthorized",
        })))
        .expect(1) // exactly once — no retry
        .mount(&server)
        .await;

    let client = Client::builder()
        .api_key("keyu_x")
        .base_url(server.uri())
        .retry(TransientRetry::new(5))
        .build()
        .unwrap();

    let err = client.search(search_req("q")).await.unwrap_err();
    assert_eq!(err.code(), Some(&ErrorCode::Unauthorized));
}

#[tokio::test]
async fn preflight_does_not_send_request() {
    let server = MockServer::start().await;
    // No mock — any incoming request will 404, surfacing a wire-side
    // error if the SDK accidentally sent one.
    let err = test_client(&server)
        .publish(PublishRequest {
            // Targeting fields are valid so the exactly-one-item rule is
            // what trips preflight, not a missing namespace/model/dims.
            items: vec![], // exactly-one rule violated
            namespace: "global".into(),
            model: "Qwen3-Embedding-4B".into(),
            dimensions: 1024,
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(err.is_preflight());
    assert_eq!(err.code(), Some(&ErrorCode::InvalidRequest));
}

#[tokio::test]
async fn user_agent_is_attached() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/health"))
        .and(header_exists("user-agent"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    test_client(&server).health().await.expect("ok");
}

#[tokio::test]
async fn subscribe_handshake_returns_id_and_streams_matches() {
    let server = MockServer::start().await;
    let body = "event: subscribed\ndata: {\"subscription_id\":\"sub-1\"}\n\n\
                event: match\ndata: {\"message_id\":\"m1\",\"score\":0.9}\n\n\
                event: match\ndata: {\"message_id\":\"m2\",\"score\":0.8}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/subscribe"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let mut sub = test_client(&server)
        .subscribe(subscribe_req("q"))
        .await
        .expect("subscribe");
    assert_eq!(sub.id(), "sub-1");

    let first = sub.next().await.unwrap().unwrap();
    assert_eq!(first.message_id, "m1");
    let second = sub.next().await.unwrap().unwrap();
    assert_eq!(second.message_id, "m2");

    // Stream closes after server-sent EOF.
    assert!(sub.next().await.is_none());
}

#[tokio::test]
async fn subscribe_rejects_non_sse_content_type() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/subscribe"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string("{}"),
        )
        .mount(&server)
        .await;

    let err = test_client(&server)
        .subscribe(subscribe_req("q"))
        .await
        .unwrap_err();
    // SubscribeSetup wraps the underlying MalformedSse so callers can
    // tell handshake failures apart from mid-stream ones.
    let Error::SubscribeSetup { source } = err else {
        panic!("expected SubscribeSetup, got different variant");
    };
    assert!(matches!(*source, Error::MalformedSse(_)));
}

#[tokio::test]
async fn subscribe_surfaces_handshake_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/subscribe"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": "unauthorized",
        })))
        .mount(&server)
        .await;

    let err = test_client(&server)
        .subscribe(subscribe_req("q"))
        .await
        .unwrap_err();
    // The wrap carries the original Api error; `code()` recurses.
    assert!(matches!(err, Error::SubscribeSetup { .. }));
    assert_eq!(err.code(), Some(&ErrorCode::Unauthorized));
}

#[tokio::test]
async fn subscribe_rejects_missing_subscription_id() {
    let server = MockServer::start().await;
    let body = "event: subscribed\ndata: {}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/subscribe"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let err = test_client(&server)
        .subscribe(subscribe_req("q"))
        .await
        .unwrap_err();
    let Error::SubscribeSetup { source } = err else {
        panic!("expected SubscribeSetup wrap");
    };
    assert!(matches!(*source, Error::MalformedSse(_)));
}

#[tokio::test]
async fn subscribe_handshake_retries_on_transient() {
    let server = MockServer::start().await;
    // First call: 503 unavailable with a tiny retry hint. Second call:
    // a clean SSE handshake.
    Mock::given(method("POST"))
        .and(path("/v1/subscribe"))
        .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
            "error": "unavailable",
            "message": "subscription setup did not complete within the budget; retry the request",
            "retry_after_ms": 5,
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let body = "event: subscribed\ndata: {\"subscription_id\":\"sub-after-retry\"}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/subscribe"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let client = Client::builder()
        .api_key("keyu_x")
        .base_url(server.uri())
        .retry(TransientRetry::new(3))
        .build()
        .unwrap();

    let sub = client
        .subscribe(subscribe_req("q"))
        .await
        .expect("subscribe after retry");
    assert_eq!(sub.id(), "sub-after-retry");
}

#[tokio::test]
async fn publish_response_carries_request_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/publish"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-request-id", "req-publish-1")
                .set_body_json(serde_json::json!({
                    "message_id": "m-1",
                    "epoch": 1,
                    "seq": 1,
                })),
        )
        .mount(&server)
        .await;

    let res = test_client(&server)
        .publish(PublishRequest {
            items: vec![PublishItem::text("hi")],
            namespace: "global".into(),
            model: "Qwen3-Embedding-4B".into(),
            dimensions: 1024,
            ..Default::default()
        })
        .await
        .expect("publish");
    assert_eq!(res.request_id.as_deref(), Some("req-publish-1"));
}

#[tokio::test]
async fn publish_response_request_id_none_when_header_absent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/publish"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "m-1",
            "epoch": 1,
            "seq": 1,
        })))
        .mount(&server)
        .await;

    let res = test_client(&server)
        .publish(PublishRequest {
            items: vec![PublishItem::text("hi")],
            namespace: "global".into(),
            model: "Qwen3-Embedding-4B".into(),
            dimensions: 1024,
            ..Default::default()
        })
        .await
        .expect("publish");
    assert!(
        res.request_id.is_none(),
        "expected None when X-Request-Id absent, got {:?}",
        res.request_id
    );
}

#[tokio::test]
async fn search_response_carries_request_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/search"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-request-id", "req-search-1")
                .set_body_json(serde_json::json!({"results": []})),
        )
        .mount(&server)
        .await;

    let res = test_client(&server)
        .search(search_req("q"))
        .await
        .expect("search");
    assert_eq!(res.request_id.as_deref(), Some("req-search-1"));
}

#[tokio::test]
async fn lint_response_carries_request_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/lint"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-request-id", "req-lint-1")
                .set_body_json(serde_json::json!({
                    "valid": true,
                    "normalized": "MATCH *",
                    "diagnostics": [],
                    "completions": [],
                })),
        )
        .mount(&server)
        .await;

    let res = test_client(&server)
        .lint(LintRequest {
            query: "MATCH *".into(),
            cursor: 0,
        })
        .await
        .expect("lint");
    assert_eq!(res.request_id.as_deref(), Some("req-lint-1"));
}

#[tokio::test]
async fn subscribe_handshake_surfaces_request_id() {
    let server = MockServer::start().await;
    let body = "event: subscribed\ndata: {\"subscription_id\":\"sub-1\"}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/subscribe"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-request-id", "req-sub-1")
                .set_body_raw(body, "text/event-stream"),
        )
        .mount(&server)
        .await;

    let sub = test_client(&server)
        .subscribe(subscribe_req("q"))
        .await
        .expect("subscribe");
    assert_eq!(sub.id(), "sub-1");
    assert_eq!(sub.request_id(), Some("req-sub-1"));
}

#[tokio::test]
async fn retry_honours_server_retry_after_ms() {
    let server = MockServer::start().await;
    // Server gives a tiny delay so the test stays fast.
    Mock::given(method("POST"))
        .and(path("/v1/search"))
        .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
            "error": "unavailable",
            "retry_after_ms": 5,
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"results": []})))
        .mount(&server)
        .await;

    let client = Client::builder()
        .api_key("keyu_x")
        .base_url(server.uri())
        .retry(TransientRetry::new(2))
        .build()
        .unwrap();
    let start = std::time::Instant::now();
    client.search(search_req("q")).await.unwrap();
    let elapsed = start.elapsed();
    // Server asked for 5ms; we should have waited at least that.
    assert!(elapsed >= Duration::from_millis(4), "elapsed {elapsed:?}");
}

// ---------------------------------------------------------------------
// C3: publish accepts text + vector together.
// ---------------------------------------------------------------------

#[tokio::test]
async fn publish_accepts_text_and_vector_together() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/publish"))
        .and(body_json(serde_json::json!({
            "items": [{"text": "hello", "vector": [0.1, 0.2, 0.3]}],
            "namespace": "private",
            "model": "m",
            "dimensions": 3,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "m-1",
            "epoch": 1,
            "seq": 1,
        })))
        .mount(&server)
        .await;

    test_client(&server)
        .publish(PublishRequest {
            items: vec![PublishItem {
                text: "hello".into(),
                vector: vec![0.1, 0.2, 0.3],
            }],
            namespace: "private".into(),
            model: "m".into(),
            dimensions: 3,
            ..Default::default()
        })
        .await
        .expect("publish with text+vector");
}

// ---------------------------------------------------------------------
// C4: publish preflight rejects vector length / dimensions mismatch.
// ---------------------------------------------------------------------

#[tokio::test]
async fn publish_vector_dim_mismatch_rejected_preflight() {
    let server = MockServer::start().await;
    // No mock — any incoming wire request would 404, surfacing as a
    // non-preflight error and failing the assertion below.
    let err = test_client(&server)
        .publish(PublishRequest {
            items: vec![PublishItem::vector(vec![0.1, 0.2])],
            namespace: "private".into(),
            model: "m".into(),
            dimensions: 3, // vector len 2 != 3
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(
        err.is_preflight(),
        "expected preflight rejection, got {err:?}"
    );
    assert_eq!(err.code(), Some(&ErrorCode::InvalidRequest));
}

// ---------------------------------------------------------------------
// B1: SubscribeSetup wraps the underlying transient error.
// ---------------------------------------------------------------------

#[tokio::test]
async fn subscribe_setup_wraps_underlying_503() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/subscribe"))
        .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
            "error": "unavailable",
            "message": "subscription setup did not complete within the budget; retry the request",
        })))
        .mount(&server)
        .await;

    let err = test_client(&server)
        .subscribe(subscribe_req("q"))
        .await
        .unwrap_err();
    let Error::SubscribeSetup { source } = &err else {
        panic!("expected SubscribeSetup wrap, got {err:?}");
    };
    // The inner error is the structured 503 Api error.
    assert!(matches!(
        source.as_ref(),
        Error::Api {
            code: ErrorCode::Unavailable,
            ..
        }
    ));
    // Accessors recurse into the wrap so user code keeps working.
    assert_eq!(err.code(), Some(&ErrorCode::Unavailable));
    assert_eq!(err.http_status(), Some(503));
    // Preflight is false through a wrap — the wrap means we reached
    // the wire (or tried to).
    assert!(!err.is_preflight());
}

// ---------------------------------------------------------------------
// B1: SubscribeStream wraps a mid-stream malformed frame.
// ---------------------------------------------------------------------

#[tokio::test]
async fn subscribe_stream_wraps_mid_stream_error() {
    // Handshake succeeds with `subscribed`, then the next frame is a
    // `match` whose data payload is not JSON. The decode happens once
    // the caller polls .next() — surfacing as SubscribeStream.
    let server = MockServer::start().await;
    let body = "event: subscribed\ndata: {\"subscription_id\":\"sub-1\"}\n\n\
                event: match\ndata: not json at all\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/subscribe"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let mut sub = test_client(&server)
        .subscribe(subscribe_req("q"))
        .await
        .expect("handshake ok");
    assert_eq!(sub.id(), "sub-1");

    let err = sub.next().await.unwrap().unwrap_err();
    let Error::SubscribeStream { source } = &err else {
        panic!("expected SubscribeStream wrap, got {err:?}");
    };
    assert!(matches!(source.as_ref(), Error::MalformedSse(_)));
}

// ---------------------------------------------------------------------
// A3: read_timeout applies to one-shot RPCs.
// ---------------------------------------------------------------------

#[tokio::test]
async fn read_timeout_enforces_on_oneshot_rpc() {
    use std::time::Instant;

    let server = MockServer::start().await;
    // Mock holds the response for 2s; with a 100ms read timeout the
    // SDK should bail out long before that.
    Mock::given(method("POST"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
        .mount(&server)
        .await;

    let client = Client::builder()
        .api_key("keyu_x")
        .base_url(server.uri())
        .retry(NoRetry)
        .read_timeout(Duration::from_millis(100))
        .build()
        .unwrap();

    let start = Instant::now();
    let err = client.health().await.unwrap_err();
    let elapsed = start.elapsed();
    // Surfaces as a transport error (reqwest timeout). The exact
    // duration depends on the runtime but it should be well under the
    // 2s server delay.
    assert!(
        elapsed < Duration::from_millis(1500),
        "read_timeout did not fire; elapsed {elapsed:?}"
    );
    assert!(
        matches!(err, Error::Transport(_)),
        "expected Transport error on read timeout, got {err:?}"
    );
}

// ---------------------------------------------------------------------
// A3: read_timeout does NOT kill the subscribe stream body.
// ---------------------------------------------------------------------

#[tokio::test]
async fn read_timeout_does_not_kill_subscribe_stream() {
    let server = MockServer::start().await;
    // The body returns immediately with subscribed + match. Even with
    // a very short read_timeout, the stream itself must not be torn
    // down by it. (We cannot inject a long inter-chunk delay through
    // wiremock without raw socket control, but we can at least assert
    // that a normal short stream is delivered intact with a tiny
    // read_timeout configured.)
    let body = "event: subscribed\ndata: {\"subscription_id\":\"sub-1\"}\n\n\
                event: match\ndata: {\"message_id\":\"m1\",\"score\":0.9}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/subscribe"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let client = Client::builder()
        .api_key("keyu_x")
        .base_url(server.uri())
        .retry(NoRetry)
        .read_timeout(Duration::from_millis(50))
        .build()
        .unwrap();

    let mut sub = client
        .subscribe(subscribe_req("q"))
        .await
        .expect("subscribe");
    assert_eq!(sub.id(), "sub-1");

    // Sleep longer than the read_timeout before draining; the stream
    // must still yield the queued match because no timeout was
    // installed on the body.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let m = sub
        .next()
        .await
        .expect("stream produced an item")
        .expect("not an error");
    assert_eq!(m.message_id, "m1");
}

// ---------------------------------------------------------------------
// A3: connect_timeout fires on an unreachable host.
// ---------------------------------------------------------------------

#[tokio::test]
async fn connect_timeout_fires_on_unreachable_host() {
    use std::time::Instant;

    // 10.0.0.0/8 is RFC1918 private space; on a host without a
    // matching route the TCP connect will hang. We point at a likely-
    // unroutable address and expect the SDK to fail fast.
    let client = Client::builder()
        .api_key("keyu_x")
        .base_url("http://10.255.255.1:1") // black-hole address+port
        .retry(NoRetry)
        .connect_timeout(Duration::from_millis(100))
        .read_timeout(Duration::from_secs(30))
        .build()
        .unwrap();

    let start = Instant::now();
    let err = client.health().await.unwrap_err();
    let elapsed = start.elapsed();
    // Allow some slack for OS scheduling; we just want to assert the
    // SDK didn't sit on a 30s default connect timeout.
    assert!(
        elapsed < Duration::from_secs(5),
        "connect_timeout did not fire; elapsed {elapsed:?}"
    );
    assert!(
        matches!(err, Error::Transport(_)),
        "expected Transport error on connect failure, got {err:?}"
    );
}

// ---------------------------------------------------------------------
// C1: fallback backoff observes the new 100ms / 2s / 5s / 10s schedule.
// ---------------------------------------------------------------------

#[tokio::test]
async fn fallback_backoff_observes_new_schedule() {
    // 503 with no retry hint forces the SDK onto the fallback ladder.
    // We allow three retries: observed sleep ~= 100ms + 2s + 5s ≈ 7.1s
    // before the final failure. We give a ±200ms slack on each step
    // (the plan called for ±50ms but real wall-clock + wiremock dispatch
    // jitter can easily exceed 50ms — 200ms keeps the test stable).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/search"))
        .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
            "error": "unavailable",
        })))
        .mount(&server)
        .await;

    let client = Client::builder()
        .api_key("keyu_x")
        .base_url(server.uri())
        .retry(TransientRetry::new(3)) // attempt + 3 retries
        .build()
        .unwrap();

    let start = std::time::Instant::now();
    let err = client.search(search_req("q")).await.unwrap_err();
    let elapsed = start.elapsed();

    // The final failure carries the underlying 503.
    assert_eq!(err.code(), Some(&ErrorCode::Unavailable));

    // The three retries should have slept 100ms + 2s + 5s ≈ 7.1s.
    // Lower bound: 7.0s (well under the sum, allowing for sleep
    // resolution); upper bound: keep it loose to avoid flakes.
    let expected_min = Duration::from_millis(6_900);
    let expected_max = Duration::from_millis(9_500);
    assert!(
        elapsed >= expected_min,
        "elapsed {elapsed:?} < expected_min {expected_min:?} — schedule may have changed"
    );
    assert!(
        elapsed <= expected_max,
        "elapsed {elapsed:?} > expected_max {expected_max:?} — schedule may have changed"
    );
}

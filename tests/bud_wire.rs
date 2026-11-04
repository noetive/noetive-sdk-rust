//! What a caller of the unary operations sees, and the one distinction the
//! module turns on: a refusal is a value, a transport failure is an error.
//! These use wiremock and never touch the live service; connection drops,
//! which wiremock cannot express, use `scripted_server`.

mod scripted_server;

use std::time::{Duration, Instant};

use noetive::bud::{
    ById, Change, Client, Code, Correspondents, Failure, In, Listing, MailboxView, Me, NoRetry,
    PartView, SendInput, TransientRetry, PART_MODE_BYTES, PART_MODE_TEXT, PREFIX_WORKSPACE,
    SENDING_PROVISIONING, STATE_HELD, STATE_QUEUED,
};
use scripted_server::ScriptedServer;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> Client {
    client_at(&server.uri())
}

fn client_at(url: &str) -> Client {
    Client::builder()
        .key("test-token")
        .base_url(url)
        .retry(NoRetry)
        .build()
        .expect("client")
}

/// Answers every request with `body` at `status`, as the server sends a
/// refusal: JSON with a request id in the header.
async fn refusing(status: u16, body: Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(status)
                .insert_header("X-Request-Id", "request_01refused")
                .set_body_json(body),
        )
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn a_refusal_is_a_value_not_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/message.update"))
        .respond_with(
            ResponseTemplate::new(409)
                .insert_header("X-Request-Id", "request_01abc")
                .set_body_json(json!({
                    "ref": "message_01x",
                    "error": {
                        "code": "precondition_failed",
                        "message": "other writes to this object kept landing first",
                        "hint": "read it and try again",
                    }
                })),
        )
        .mount(&server)
        .await;

    let out = client(&server)
        .update_message(Change {
            id: "message_01x".into(),
            changes: json!({"labels": ["urgent"]}),
            ..Change::default()
        })
        .await
        .expect("a refusal arrived as an error, so the caller gets no envelope");
    let refusal = out.error.expect("the refusal was dropped");
    assert_eq!(refusal.code, Code::PreconditionFailed);
    assert_eq!(refusal.http_status, 409);
    assert_eq!(
        refusal.request_id, "request_01abc",
        "filled from the header when the body omits it"
    );
    assert_eq!(refusal.hint, "read it and try again");
    assert!(refusal.retryable());
}

#[tokio::test]
async fn a_transport_failure_is_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(502).set_body_string("<html><body>502 Bad Gateway</body></html>"),
        )
        .mount(&server)
        .await;

    let failure = client(&server).describe_me().await.unwrap_err();
    let e = failure.refusal().expect("a malformed_response");
    assert_eq!(e.code, Code::MalformedResponse);
    assert_eq!(e.http_status, 502, "the status still carries information");
}

#[tokio::test]
async fn a_bare_401_names_the_credential() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let failure = client(&server).health().await.unwrap_err();
    assert_eq!(failure.code(), Some(&Code::Unauthorized));
}

#[tokio::test]
async fn preflight_refuses_before_sending() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .expect(0)
        .mount(&server)
        .await;
    let c = client(&server);

    let refusals: Vec<(&str, Failure)> = vec![
        (
            "a thread id for describe_message",
            c.describe_message(ById {
                id: "thread_01x".into(),
                ..ById::default()
            })
            .await
            .unwrap_err(),
        ),
        (
            "a send with no recipients and nothing to reply to",
            c.send(SendInput::default()).await.unwrap_err(),
        ),
        (
            "an update with no changes",
            c.update_message(Change {
                id: "message_01x".into(),
                ..Change::default()
            })
            .await
            .unwrap_err(),
        ),
        (
            "a part read naming no part",
            c.describe_part(ById {
                id: "message_01x".into(),
                ..ById::default()
            })
            .await
            .unwrap_err(),
        ),
        (
            "a part read aimed at a thread",
            c.describe_part(ById {
                id: "thread_01x".into(),
                part: "2".into(),
                ..ById::default()
            })
            .await
            .unwrap_err(),
        ),
        (
            "a mailbox named by a message",
            c.describe_mailbox(ById {
                id: "message_01x".into(),
                ..ById::default()
            })
            .await
            .unwrap_err(),
        ),
        (
            "correspondents of no mailbox",
            c.list_correspondents(In::default()).await.unwrap_err(),
        ),
        (
            "a listing of a thread",
            c.list_folder(In {
                mailbox: "thread_01x".into(),
                ..In::default()
            })
            .await
            .unwrap_err(),
        ),
        (
            "a thread named by a message",
            c.describe_thread(ById {
                id: "message_01x".into(),
                ..ById::default()
            })
            .await
            .unwrap_err(),
        ),
    ];
    for (name, failure) in refusals {
        let e = failure.refusal().expect(name);
        assert_eq!(
            (e.code.clone(), e.http_status),
            (Code::Invalid, 0),
            "{name}"
        );
        assert!(failure.is_preflight(), "{name}");
    }
    // `expect(0)` is verified when the server drops.
}

#[tokio::test]
async fn an_unkeyed_write_is_never_retried() {
    // Every connection closes before a response, as a dropped connection
    // does. A generous policy, to prove the gate and not the default.
    let srv = ScriptedServer::start(|_ex| {});
    let c = Client::builder()
        .key("t")
        .base_url(&srv.url)
        .retry(TransientRetry::new(5).with_backoff(vec![Duration::from_millis(1)]))
        .build()
        .unwrap();
    let to = vec!["a@b.example".to_string()];

    let failure = c
        .send(SendInput {
            to: to.clone(),
            ..SendInput::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(failure, Failure::Transport(_)), "{failure}");
    assert_eq!(srv.requests(), 1, "an unkeyed send is attempted once");

    let before = srv.requests();
    let _ = c
        .send(SendInput {
            to,
            idempotency_key: "k1".into(),
            ..SendInput::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        srv.requests() - before,
        6,
        "a keyed send is retried as the policy allows"
    );

    let before = srv.requests();
    let _ = c
        .update_message(Change {
            id: "message_01x".into(),
            changes: json!({"read": true}),
            ..Change::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        srv.requests() - before,
        1,
        "an unkeyed update is attempted once"
    );

    let before = srv.requests();
    let _ = c.describe_me().await.unwrap_err();
    assert_eq!(srv.requests() - before, 6, "a read is safe to repeat");
}

#[tokio::test]
async fn a_refusal_is_never_retried() {
    let srv = ScriptedServer::start(|mut ex| {
        ex.respond(
            503,
            &[],
            br#"{"error":{"code":"upstream_unavailable","message":"a dependency did not answer"}}"#,
        );
    });
    let c = Client::builder()
        .key("t")
        .base_url(&srv.url)
        .retry(TransientRetry::new(5).with_backoff(vec![Duration::from_millis(1)]))
        .build()
        .unwrap();
    let out = c.describe_me().await.expect("a refusal is a value");
    assert_eq!(out.error.unwrap().code, Code::UpstreamUnavailable);
    assert_eq!(srv.requests(), 1, "a refusal is an answer, not a failure");
}

#[tokio::test]
async fn a_forwarding_client_holds_no_credential() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/me.describe"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ref": "me", "kind": "me"})))
        .mount(&server)
        .await;

    let relay = Client::builder()
        .forwarding()
        .base_url(server.uri())
        .retry(NoRetry)
        .build()
        .unwrap();
    for header in ["Bearer one", "Bearer two", ""] {
        relay
            .forward(header)
            .unwrap()
            .describe_me()
            .await
            .expect("describe_me");
    }
    relay
        .describe_me()
        .await
        .expect("a bare relay call goes out");

    let seen: Vec<Option<String>> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| {
            r.headers
                .get("authorization")
                .map(|v| v.to_str().unwrap().to_string())
        })
        .collect();
    assert_eq!(
        seen,
        [
            Some("Bearer one".into()),
            Some("Bearer two".into()),
            None,
            None
        ],
        "each call carries only its own header, and none when given none"
    );
    assert!(!format!("{relay:?}").contains("Bearer"));
}

#[tokio::test]
async fn a_redirect_is_refused() {
    let elsewhere = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .expect(0)
        .mount(&elsewhere)
        .await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("Location", format!("{}/v1/me.describe", elsewhere.uri())),
        )
        .expect(1)
        .mount(&server)
        .await;

    // Following it would re-attach the credential to a host the caller
    // never chose. A retrying policy, to prove a redirect is an answer and
    // not a failed connection: it is asked for once.
    let c = Client::builder()
        .key("test-token")
        .base_url(server.uri())
        .retry(TransientRetry::new(3).with_backoff(vec![Duration::from_millis(1)]))
        .build()
        .unwrap();
    let failure = c.describe_me().await.expect_err("a redirect was followed");
    let want_host = elsewhere.uri().trim_start_matches("http://").to_string();
    match &failure {
        Failure::Redirected { status, host } => assert_eq!((*status, host), (307, &want_host)),
        other => panic!("want a refused redirect naming the host, got {other}"),
    }
    assert!(failure.to_string().contains(&want_host), "{failure}");
    // `expect` on both servers is verified when they drop.
}

#[tokio::test]
async fn a_response_that_broke_off_is_not_retried() {
    // The server answered — a status and part of a body — and then the
    // connection closed. That is not a connection that failed before any
    // response, so it is reported, not repeated.
    let srv = ScriptedServer::start(|mut ex| {
        ex.respond_partial(200, br#"{"ref":"me","kind":"#, 4096);
    });
    let c = Client::builder()
        .key("t")
        .base_url(&srv.url)
        .retry(TransientRetry::new(5).with_backoff(vec![Duration::from_millis(1)]))
        .build()
        .unwrap();
    let failure = c.describe_me().await.unwrap_err();
    assert!(matches!(failure, Failure::Transport(_)), "{failure}");
    assert_eq!(srv.requests(), 1, "an answered request is not retried");
}

#[tokio::test]
async fn the_key_never_appears_in_what_a_client_prints() {
    let secret = "keya_secret_never_printed";
    let keyed = Client::builder().key(secret).build().unwrap();
    let relay = Client::forwarding().unwrap();
    let forwarded = relay.forward(&format!("Bearer {secret}")).unwrap();
    for (name, printed) in [
        ("keyed Debug", format!("{keyed:?}")),
        ("keyed pretty Debug", format!("{keyed:#?}")),
        ("forwarded Debug", format!("{forwarded:?}")),
        ("forwarded pretty Debug", format!("{forwarded:#?}")),
    ] {
        assert!(!printed.contains(secret), "{name} leaks the key: {printed}");
        assert!(
            !printed.contains("Bearer"),
            "{name} leaks the scheme: {printed}"
        );
    }
    // A key that cannot be a header value is refused without quoting it.
    let bad = format!("{secret}\n");
    let refusal = Client::new(bad).unwrap_err().to_string();
    assert!(
        !refusal.contains(secret),
        "the refusal quotes the key: {refusal}"
    );
}

#[tokio::test]
async fn a_server_that_never_answers_is_bounded_by_the_response_timeout() {
    let srv = ScriptedServer::start(|mut ex| {
        std::thread::sleep(Duration::from_millis(800));
        ex.respond(200, &[], b"{}");
    });
    let c = Client::builder()
        .key("t")
        .base_url(&srv.url)
        .retry(NoRetry)
        .response_timeout(Duration::from_millis(100))
        .build()
        .unwrap();
    let start = Instant::now();
    let failure = c.describe_me().await.unwrap_err();
    assert!(matches!(failure, Failure::Timeout(_)), "{failure}");
    assert!(start.elapsed() < Duration::from_millis(600));
}

#[tokio::test]
async fn health_and_catalog_carry_refusals() {
    let server = refusing(
        403,
        json!({"error": {"code": "forbidden_scope", "message": "this key does not reach an agent on this service"}}),
    )
    .await;
    let c = client(&server);
    let health = c.health().await.expect("a refusal is a value");
    let catalog = c.describe_catalog().await.expect("a refusal is a value");
    for (name, refusal) in [("health", health.error), ("catalog", catalog.error)] {
        let e = refusal.expect(name);
        assert_eq!(e.code, Code::ForbiddenScope, "{name}");
        assert_eq!(
            (e.http_status, e.request_id.as_str()),
            (403, "request_01refused"),
            "{name}"
        );
    }
}

#[tokio::test]
async fn not_billable_is_its_own_code() {
    let server = refusing(
        402,
        json!({"error": {"code": "not_billable", "message": "this account cannot incur usage"}}),
    )
    .await;
    let out = client(&server).describe_me().await.expect("describe_me");
    let e = out.error.unwrap();
    assert_eq!(e.code, Code::NotBillable);
    assert!(!e.retryable());
}

/// Covers the outcome that most invites a mistake: a send a person has to
/// approve comes back held, not refused, and carries the approval it waits
/// on. Reading it as a failure and sending again would make a second message.
#[tokio::test]
async fn a_held_send_names_its_approval() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/send"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "message": "message_01x", "thread": "thread_01x",
            "state": "held", "approval_id": "appr_01x"})))
        .mount(&server)
        .await;
    let out = client(&server)
        .send(SendInput {
            to: vec!["ada@example.com".into()],
            text: "hi".into(),
            idempotency_key: "k1".into(),
            ..SendInput::default()
        })
        .await
        .expect("send");
    assert!(out.error.is_none(), "held is not a refusal");
    assert_eq!(
        (out.state.as_str(), out.approval_id.as_str()),
        (STATE_HELD, "appr_01x")
    );
}

/// Covers the fields renamed from tenant: a client still reading the old
/// names would see an empty workspace and every sender as outside it, with
/// no error.
#[tokio::test]
async fn the_workspace_is_where_the_server_says_it_is() {
    let server = MockServer::start().await;
    for (op, body) in [
        (
            "/v1/health",
            json!({"contract": "bud/v1", "agent": "ag_01x", "workspace": "workspace_01x"}),
        ),
        (
            "/v1/me.describe",
            json!({"ref": "me", "kind": "me", "content": {"agent": "ag_01x", "workspace": "workspace_01x"}}),
        ),
        (
            "/v1/folder.list",
            json!({"ref": "ag_01x/inbox", "kind": "folder", "content": {"mailbox": "ag_01x",
                "folder": "inbox", "messages": [{"message": "message_01x",
                "date": "2026-01-02T03:04:05Z", "aligned": true, "in_workspace": true, "known": true}]}}),
        ),
    ] {
        Mock::given(method("POST"))
            .and(path(op))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
    }
    let c = client(&server);

    let h = c.health().await.expect("health");
    assert_eq!(h.workspace, "workspace_01x");
    assert!(h.workspace.starts_with(PREFIX_WORKSPACE));
    let me: Me = c.describe_me().await.unwrap().into().unwrap();
    assert_eq!(me.workspace, "workspace_01x");
    let list: Listing = c
        .list_folder(In {
            mailbox: "ag_01x".into(),
            ..In::default()
        })
        .await
        .unwrap()
        .into()
        .unwrap();
    assert!(list.messages[0].in_workspace);
}

#[tokio::test]
async fn the_typed_views_read_what_the_server_sends() {
    let server = MockServer::start().await;
    for (op, body) in [
        (
            "/v1/mailbox.describe",
            json!({"ref": "ag_01x", "kind": "mailbox", "content": {"mailbox": "ag_01x",
                "addresses": ["ada@agents.example.com"], "folders": {"inbox": 3}, "sending": {"status": "provisioning", "reason": "domain verifying"}}}),
        ),
        (
            "/v1/correspondent.list",
            json!({"ref": "ag_01x/corr", "kind": "corr", "content": {"mailbox": "ag_01x",
                "correspondents": [{"addr": "ada@example.com", "first_seen": "2026-01-02T03:04:05Z",
                "last_seen": "2026-01-03T03:04:05Z", "inbound": 2, "last_aligned": true}]}}),
        ),
        (
            "/v1/part.describe",
            json!({"ref": "message_01x/part/2", "kind": "part", "content": {"message": "message_01x",
                "part": "2", "mode": "bytes", "type": "text/plain", "bytes_base64": "aGVsbG8="}}),
        ),
    ] {
        Mock::given(method("POST"))
            .and(path(op))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
    }
    let c = client(&server);

    let mv: MailboxView = c
        .describe_mailbox(ById {
            id: "ag_01x".into(),
            ..ById::default()
        })
        .await
        .unwrap()
        .into()
        .unwrap();
    assert_eq!(mv.folders.get("inbox"), Some(&3));
    // The mailbox names what it sends as; a client not reading the field
    // would show a mailbox that cannot send, with no error.
    assert_eq!(mv.addresses, ["ada@agents.example.com"]);
    assert_eq!(mv.sending.unwrap().status, SENDING_PROVISIONING);

    // A server that sends no addresses reads as an empty list, not a failure.
    let bare: MailboxView = serde_json::from_value(json!({"mailbox": "ag_01x"})).unwrap();
    assert!(bare.addresses.is_empty());

    let cv: Correspondents = c
        .list_correspondents(In {
            mailbox: "ag_01x".into(),
            q: "ada".into(),
            ..In::default()
        })
        .await
        .unwrap()
        .into()
        .unwrap();
    let first = &cv.correspondents[0];
    assert_eq!(first.addr, "ada@example.com");
    assert_eq!(first.first_seen, "2026-01-02T03:04:05Z");
    assert!(first.last_aligned);

    let part = c
        .describe_part(ById {
            id: "message_01x".into(),
            part: "2".into(),
            mode: PART_MODE_BYTES.into(),
            ..ById::default()
        })
        .await
        .unwrap();
    let pv: PartView = part.clone().into().unwrap();
    assert_eq!(
        pv.bytes, b"hello",
        "the base64 on the wire is decoded for the caller"
    );

    // A view of the wrong kind is refused rather than decoded into zero
    // values that would read as an empty mailbox.
    let wrong = part.into::<MailboxView>().unwrap_err();
    assert!(wrong.is_preflight(), "{wrong}");
}

#[tokio::test]
async fn the_requests_carry_the_fields_that_select_them() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;
    let c = client(&server);

    c.describe_part(ById {
        id: "message_01x".into(),
        part: "2".into(),
        mode: PART_MODE_TEXT.into(),
        ..ById::default()
    })
    .await
    .unwrap();
    c.describe_thread(ById {
        id: "thread_01x".into(),
        max_messages: 5,
        ..ById::default()
    })
    .await
    .unwrap();
    c.list_correspondents(In {
        mailbox: "ag_01x".into(),
        limit: 10,
        ..In::default()
    })
    .await
    .unwrap();

    let got: Vec<(String, Value)> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| {
            (
                r.url.path().to_string(),
                serde_json::from_slice(&r.body).unwrap(),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            (
                "/v1/part.describe".to_string(),
                json!({"id": "message_01x", "part": "2", "mode": "text"})
            ),
            (
                "/v1/thread.describe".to_string(),
                json!({"id": "thread_01x", "max_messages": 5})
            ),
            (
                "/v1/correspondent.list".to_string(),
                json!({"in": "ag_01x", "limit": 10})
            ),
        ]
    );
    let r = &server.received_requests().await.unwrap()[0];
    assert_eq!(r.headers.get("content-type").unwrap(), "application/json");
    assert_eq!(r.headers.get("authorization").unwrap(), "Bearer test-token");
    assert!(r
        .headers
        .get("user-agent")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("noetive-sdk-rust/"));
}

#[tokio::test]
async fn a_send_needs_a_recipient_anywhere() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/send"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"message": "message_01x", "state": "queued"})),
        )
        .mount(&server)
        .await;
    let c = client(&server);
    let ada = vec!["ada@example.com".to_string()];
    for (name, input) in [
        (
            "cc only",
            SendInput {
                cc: ada.clone(),
                text: "hi".into(),
                ..SendInput::default()
            },
        ),
        (
            "bcc only",
            SendInput {
                bcc: ada.clone(),
                text: "hi".into(),
                ..SendInput::default()
            },
        ),
        (
            "a reply",
            SendInput {
                in_reply_to: "message_01p".into(),
                text: "hi".into(),
                ..SendInput::default()
            },
        ),
    ] {
        let out = c
            .send(input)
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            out.state, STATE_QUEUED,
            "{name}: the server accepts this send"
        );
    }
}

/// The environment is process-wide, so every case that touches it runs in
/// this one test, and the variables are restored afterwards. Nothing else in
/// this binary reads them.
#[tokio::test]
async fn from_env_reads_the_one_noetive_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"contract": "bud/v1"})))
        .mount(&server)
        .await;

    let saved: Vec<(&str, Option<String>)> = [
        "NOETIVE_KEY_SECRET",
        "NOETIVE_BUD_BASE_URL",
        "NOETIVE_BUD_KEY_SECRET",
    ]
    .into_iter()
    .map(|k| (k, std::env::var(k).ok()))
    .collect();

    std::env::set_var("NOETIVE_KEY_SECRET", "keya_test");
    std::env::set_var("NOETIVE_BUD_BASE_URL", server.uri());
    let c = Client::builder()
        .environment()
        .unwrap()
        .retry(NoRetry)
        .build()
        .unwrap();
    let health = c.health().await;

    // An explicit base URL wins over the environment, whichever comes first.
    let explicit = Client::builder()
        .base_url("https://explicit.example")
        .environment()
        .unwrap()
        .build()
        .unwrap()
        .base_url()
        .to_string();

    std::env::set_var("NOETIVE_KEY_SECRET", "${NOETIVE_KEY_SECRET}");
    let placeholder = Client::from_env().unwrap_err();

    std::env::set_var("NOETIVE_KEY_SECRET", "");
    std::env::set_var("NOETIVE_BUD_KEY_SECRET", "keya_old");
    let unset = Client::from_env().unwrap_err();

    for (k, v) in saved {
        match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
    }

    assert_eq!(health.expect("health").contract, "bud/v1");
    let seen = &server.received_requests().await.unwrap()[0];
    assert_eq!(
        seen.headers.get("authorization").unwrap(),
        "Bearer keya_test"
    );
    assert_eq!(explicit, "https://explicit.example");
    assert!(placeholder.is_preflight(), "{placeholder}");
    assert!(placeholder.to_string().contains("placeholder"));
    assert!(
        unset.is_preflight() && unset.to_string().contains("NOETIVE_KEY_SECRET"),
        "with the shared key unset, the refusal names it and nothing else is read: {unset}"
    );
}

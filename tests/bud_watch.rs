//! What `watch` and `wait` make of each part of the journal stream: an
//! `open` frame, then `batch` frames and `: keepalive` comments. The server
//! side is scripted frame by frame (see `scripted_server`), because these
//! cases hold streams open, pace frames and drop connections.

mod scripted_server;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use noetive::bud::{Client, Code, Failure, NoRetry, TransientRetry, WaitInput};
use scripted_server::{Exchange, ScriptedServer};

/// Long enough that only a broken test reaches it.
const HOLD: Duration = Duration::from_secs(10);

const TWO_EVENTS: &str = r#"{"cursor":"43","events":[{"id":"journal_01a","type":"mail.received","mailbox":"ag_01x","message":"message_01a","seq":42},{"id":"journal_01b","type":"mail.sent","mailbox":"ag_01x","message":"message_01b","seq":43,"data":{"message_id":"abc@example.com"}}]}"#;

fn client(url: &str) -> Client {
    Client::builder()
        .key("test-token")
        .base_url(url)
        .retry(NoRetry)
        .build()
        .expect("client")
}

/// What is wrong with a request, measured against what the server accepts:
/// a JSON POST to /v1/watch asking for an event stream, with the
/// credential and this SDK's user agent.
fn misshapen(ex: &Exchange) -> Option<String> {
    let r = &ex.request;
    if r.method != "POST" || r.path != "/v1/watch" {
        return Some(format!(
            "request {} {}, want POST /v1/watch",
            r.method, r.path
        ));
    }
    let checks = [
        ("accept", "text/event-stream"),
        ("content-type", "application/json"),
        ("authorization", "Bearer test-token"),
    ];
    for (name, want) in checks {
        if r.header(name) != Some(want) {
            return Some(format!("{name} = {:?}, want {want}", r.header(name)));
        }
    }
    if !r
        .header("user-agent")
        .unwrap_or_default()
        .starts_with("noetive-sdk-rust/")
    {
        return Some(format!("user-agent = {:?}", r.header("user-agent")));
    }
    if serde_json::from_slice::<serde_json::Value>(&r.body).is_err() {
        return Some("the body is not JSON".into());
    }
    None
}

/// A server answering /v1/watch with a 200 event stream and running
/// `script` on it. A misshapen request is answered 400, naming the problem,
/// so the test reading it fails with the reason.
fn watch_server<F>(script: F) -> ScriptedServer
where
    F: Fn(&mut Exchange) + Send + Sync + 'static,
{
    ScriptedServer::start(move |mut ex| {
        if let Some(problem) = misshapen(&ex) {
            let body = serde_json::json!({"error": {"code": "invalid", "message": problem}});
            ex.respond(400, &[], body.to_string().as_bytes());
            return;
        }
        ex.open_stream();
        script(&mut ex);
    })
}

/// A server answering every request with `body` at `status`.
fn refusing_server(status: u16, body: &'static str) -> ScriptedServer {
    ScriptedServer::start(move |mut ex| {
        ex.respond(
            status,
            &[
                ("Content-Type", "application/json"),
                ("X-Request-Id", "request_01refused"),
            ],
            body.as_bytes(),
        );
    })
}

fn wait_for(seconds: u32) -> WaitInput {
    WaitInput {
        timeout_seconds: seconds,
        ..WaitInput::default()
    }
}

/// Whether the server saw the client let its connection go within `within`.
/// Polled rather than blocked on, so the runtime stays free to run the
/// connection task that does the closing.
async fn released(rx: &mpsc::Receiver<()>, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if rx.try_recv().is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn code(f: &Failure) -> Option<Code> {
    f.code().cloned()
}

#[tokio::test]
async fn watch_reads_the_stream_as_the_server_writes_it() {
    let srv = watch_server(|ex| {
        ex.open("41");
        ex.keepalive();
        ex.frame("presence", "not json; a frame this version does not know");
        ex.frame("batch", TWO_EVENTS);
    });

    let mut st = client(&srv.url)
        .watch(WaitInput::default())
        .await
        .expect("watch");
    assert_eq!(
        st.cursor(),
        "41",
        "the open frame says where the stream starts"
    );
    assert_eq!(st.request_id(), "request_01stream");

    let mut ids = Vec::new();
    while let Some(ev) = st.next().await {
        ids.push(ev.expect("a clean stream").id);
    }
    assert_eq!(ids, ["journal_01a", "journal_01b"]);
    assert_eq!(st.cursor(), "43");
}

#[tokio::test]
async fn watch_ends_on_a_refusal_in_flight() {
    let srv = watch_server(|ex| {
        ex.open("41");
        ex.frame(
            "batch",
            r#"{"cursor":"","error":{"code":"internal","message":"the request could not be completed"}}"#,
        );
        ex.hold(HOLD);
    });

    let mut st = client(&srv.url)
        .watch(WaitInput::default())
        .await
        .expect("watch");
    let failure = st.next().await.expect("an item").unwrap_err();
    let refusal = failure.refusal().expect("the server's refusal");
    assert_eq!(refusal.code, Code::Internal);
    assert_eq!(refusal.request_id, "request_01stream");
    assert_eq!(
        refusal.http_status, 200,
        "status 0 is reserved for refusals before sending"
    );
    assert!(st.next().await.is_none(), "the stream ends after a refusal");
    assert_eq!(
        st.cursor(),
        "41",
        "an error frame without a cursor keeps the position"
    );
}

#[tokio::test]
async fn watch_refuses_before_the_stream_opens() {
    let srv = refusing_server(
        403,
        r#"{"error":{"code":"forbidden_scope","message":"this key does not reach an agent on this service"}}"#,
    );
    let failure = client(&srv.url)
        .watch(WaitInput::default())
        .await
        .unwrap_err();
    let refusal = failure.refusal().expect("the server's refusal");
    assert_eq!(refusal.code, Code::ForbiddenScope);
    assert_eq!(refusal.http_status, 403);
    assert_eq!(refusal.request_id, "request_01refused");
}

#[tokio::test]
async fn wait_returns_the_first_batch_whole() {
    let srv = watch_server(|ex| {
        ex.open("41");
        ex.keepalive();
        ex.frame("batch", TWO_EVENTS);
        ex.frame(
            "batch",
            r#"{"cursor":"44","events":[{"id":"journal_01c","type":"mail.read"}]}"#,
        );
        ex.hold(HOLD);
    });

    let start = Instant::now();
    let out = client(&srv.url)
        .wait(WaitInput {
            cursor: "41".into(),
            ..WaitInput::default()
        })
        .await
        .expect("wait");
    assert!(out.error.is_none(), "{:?}", out.error);
    let ids: Vec<_> = out.events.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["journal_01a", "journal_01b"], "the whole first batch");
    assert_eq!(out.events[1].data.message_id, "abc@example.com");
    assert_eq!(out.cursor, "43");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "it returns on the first batch, not at the end of the window"
    );
}

#[tokio::test]
async fn a_quiet_window_is_success() {
    let srv = watch_server(|ex| {
        ex.open("41");
        while !ex.hold(Duration::from_millis(200)) {
            ex.keepalive();
        }
    });

    let start = Instant::now();
    let out = client(&srv.url)
        .wait(wait_for(1))
        .await
        .expect("a quiet window is not a failure");
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(900) && elapsed <= Duration::from_secs(3),
        "a one-second window took {elapsed:?}"
    );
    assert!(out.is_empty(), "{out:?}");
    assert_eq!(out.cursor, "41", "the position survives a quiet window");
}

#[tokio::test]
async fn wait_returns_a_refusal_before_the_stream_opens_as_a_value() {
    let srv = refusing_server(
        400,
        r#"{"error":{"code":"invalid","message":"the cursor does not parse","field":"/cursor"}}"#,
    );
    let out = client(&srv.url)
        .wait(WaitInput {
            cursor: "not-a-cursor".into(),
            ..WaitInput::default()
        })
        .await
        .expect("a refusal is a value");
    let refusal = out.error.as_ref().expect("the refusal");
    assert_eq!(
        (refusal.code.clone(), refusal.field.as_str()),
        (Code::Invalid, "/cursor")
    );
    assert_eq!(refusal.http_status, 400);
    assert_eq!(refusal.request_id, "request_01refused");
    assert_eq!(out.cursor, "not-a-cursor", "the caller's own cursor back");
}

#[tokio::test]
async fn wait_returns_a_refusal_in_flight_as_a_value() {
    let srv = watch_server(|ex| {
        ex.open("41");
        ex.frame(
            "batch",
            r#"{"cursor":"","error":{"code":"upstream_unavailable","message":"a dependency did not answer","request_id":"request_01frame"}}"#,
        );
        ex.hold(HOLD);
    });
    let out = client(&srv.url)
        .wait(WaitInput::default())
        .await
        .expect("a refusal is a value");
    let refusal = out.error.as_ref().expect("the refusal");
    assert_eq!(refusal.code, Code::UpstreamUnavailable);
    assert_eq!(
        refusal.http_status, 200,
        "status 0 is reserved for refusals before sending"
    );
    assert_eq!(refusal.request_id, "request_01frame", "the frame's own id");
    assert_eq!(
        out.cursor, "41",
        "the stream's position, not the frame's empty one"
    );
    assert!(!out.is_empty(), "a refusal is not a quiet window");
}

#[tokio::test]
async fn the_callers_own_deadline_ends_a_wait() {
    let srv = watch_server(|ex| {
        ex.open("41");
        ex.hold(HOLD);
    });
    let start = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_millis(200),
        client(&srv.url).wait(wait_for(5)),
    )
    .await;
    assert!(
        result.is_err(),
        "the caller's deadline, never an empty success"
    );
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn wait_does_not_read_a_closed_stream_as_quiet() {
    let srv = watch_server(|ex| ex.open("41"));
    let failure = client(&srv.url).wait(wait_for(5)).await.unwrap_err();
    assert!(matches!(failure, Failure::Closed), "{failure}");
}

#[tokio::test]
async fn a_slow_body_is_not_cut_short() {
    let srv = watch_server(|ex| {
        ex.open("41");
        std::thread::sleep(Duration::from_millis(300));
        ex.frame(
            "batch",
            r#"{"cursor":"42","events":[{"id":"journal_01a"}]}"#,
        );
        ex.hold(HOLD);
    });
    let out = client(&srv.url)
        .wait(wait_for(3))
        .await
        .expect("a slow body");
    assert_eq!(out.cursor, "42");
}

#[tokio::test]
async fn a_malformed_stream_is_not_an_answer() {
    type Script = fn(&mut Exchange);
    let cases: [(&str, Script); 3] = [
        ("no open frame", |ex| ex.frame("batch", TWO_EVENTS)),
        ("an open that is not JSON", |ex| ex.frame("open", "41")),
        ("a batch that is not JSON", |ex| {
            ex.open("41");
            ex.frame("batch", "{events");
        }),
    ];
    for (name, script) in cases {
        let srv = watch_server(script);
        let failure = client(&srv.url).wait(wait_for(5)).await.unwrap_err();
        assert_eq!(
            code(&failure),
            Some(Code::MalformedResponse),
            "{name}: {failure}"
        );
    }
}

#[tokio::test]
async fn a_200_that_is_not_an_event_stream_is_malformed() {
    let srv = ScriptedServer::start(|mut ex| {
        ex.respond(
            200,
            &[("Content-Type", "application/json")],
            br#"{"cursor":"41"}"#,
        );
    });
    let failure = client(&srv.url)
        .watch(WaitInput::default())
        .await
        .unwrap_err();
    assert_eq!(code(&failure), Some(Code::MalformedResponse), "{failure}");
}

#[tokio::test]
async fn json_that_is_not_an_envelope_is_malformed() {
    let srv = refusing_server(500, r#"{"cursor":"41"}"#);
    let failure = client(&srv.url)
        .watch(WaitInput::default())
        .await
        .unwrap_err();
    assert_eq!(code(&failure), Some(Code::MalformedResponse), "{failure}");
}

#[tokio::test]
async fn a_gateways_page_is_not_the_servers_refusal() {
    let srv = refusing_server(502, "<html>502 Bad Gateway</html>");
    let failure = client(&srv.url)
        .wait(WaitInput::default())
        .await
        .expect_err("words the server never wrote are not its refusal");
    let e = failure.refusal().expect("a malformed_response");
    assert_eq!(
        (e.code.clone(), e.http_status),
        (Code::MalformedResponse, 502)
    );
}

#[tokio::test]
async fn a_refusal_keeps_the_request_id_it_was_sent_with() {
    let srv = refusing_server(
        403,
        r#"{"error":{"code":"forbidden_scope","message":"no mail scope","request_id":"request_01body"}}"#,
    );
    let out = client(&srv.url)
        .wait(WaitInput::default())
        .await
        .expect("wait");
    assert_eq!(out.error.expect("refusal").request_id, "request_01body");
}

#[tokio::test]
async fn dropping_a_watch_ends_it() {
    let (released_tx, released_rx) = mpsc::channel();
    let released_tx = std::sync::Mutex::new(released_tx);
    let srv = watch_server(move |ex| {
        ex.open("41");
        if ex.hold(HOLD) {
            let _ = released_tx.lock().unwrap().send(());
        }
    });

    let mut st = client(&srv.url)
        .watch(WaitInput::default())
        .await
        .expect("watch");
    let waited = tokio::time::timeout(Duration::from_millis(100), st.next()).await;
    assert!(
        waited.is_err(),
        "a stream that sent nothing yielded something"
    );
    drop(st);
    assert!(
        released(&released_rx, Duration::from_secs(2)).await,
        "the server still holds the stream after it was dropped"
    );
}

#[tokio::test]
async fn wait_releases_its_connection() {
    let (released_tx, released_rx) = mpsc::channel();
    let released_tx = std::sync::Mutex::new(released_tx);
    let srv = watch_server(move |ex| {
        ex.open("41");
        ex.frame("batch", TWO_EVENTS);
        if ex.hold(HOLD) {
            let _ = released_tx.lock().unwrap().send(());
        }
    });

    client(&srv.url).wait(wait_for(5)).await.expect("wait");
    // An agent loop waits for ever; a wait that left its stream open would
    // run the process out of connections one window at a time.
    assert!(
        released(&released_rx, Duration::from_secs(1)).await,
        "the server still holds the stream a second after wait returned"
    );
}

#[tokio::test]
async fn wait_keeps_waiting_past_a_batch_that_only_moves_the_cursor() {
    let srv = watch_server(|ex| {
        ex.open("41");
        ex.frame("batch", r#"{"cursor":"42"}"#);
        ex.hold(HOLD);
    });
    let start = Instant::now();
    let out = client(&srv.url).wait(wait_for(1)).await.expect("wait");
    assert!(out.is_empty() && out.cursor == "42", "{out:?}");
    assert!(
        start.elapsed() >= Duration::from_millis(900),
        "an empty batch is not an answer to hand back"
    );
}

#[tokio::test]
async fn watch_skips_a_batch_that_only_moves_the_cursor() {
    let srv = watch_server(|ex| {
        ex.open("41");
        ex.frame("batch", r#"{"cursor":"42"}"#);
        ex.frame(
            "batch",
            r#"{"cursor":"43","events":[{"id":"journal_01c"}]}"#,
        );
    });
    let mut st = client(&srv.url)
        .watch(WaitInput::default())
        .await
        .expect("watch");
    let ev = st.next().await.expect("an event").expect("no failure");
    assert_eq!(ev.id, "journal_01c");
    assert!(st.next().await.is_none(), "one event only");
}

#[tokio::test]
async fn a_large_batch_arrives_whole() {
    // Subjects a stranger wrote, escaped as the server sends them, run well
    // past a small frame bound. A client that refused the frame would refuse
    // it again from the same cursor for ever.
    let subject = r"\u003c".repeat(120);
    let events: Vec<String> = (0..100)
        .map(|i| {
            format!(
                r#"{{"id":"journal_{i:03}","type":"mail.received","data":{{"subject":"{subject}","from":"{subject}"}}}}"#
            )
        })
        .collect();
    let batch = format!(r#"{{"cursor":"141","events":[{}]}}"#, events.join(","));
    assert!(
        batch.len() > 128 << 10,
        "the fixture must exceed the 64 KiB default bound"
    );

    let srv = watch_server(move |ex| {
        ex.open("41");
        ex.frame("batch", &batch);
        ex.hold(HOLD);
    });
    let out = client(&srv.url)
        .wait(WaitInput::default())
        .await
        .expect("wait");
    assert_eq!((out.events.len(), out.cursor.as_str()), (100, "141"));
    assert_eq!(out.events[0].data.subject, "<".repeat(120));
}

#[tokio::test]
async fn an_oversized_frame_is_malformed() {
    let srv = watch_server(|ex| {
        ex.open("41");
        let pad = "x".repeat(5 << 20);
        ex.frame("batch", &format!(r#"{{"cursor":"42","pad":"{pad}"}}"#));
        ex.hold(HOLD);
    });
    let failure = client(&srv.url).wait(wait_for(5)).await.unwrap_err();
    let e = failure.refusal().expect("a malformed_response");
    assert_eq!(e.code, Code::MalformedResponse, "{failure}");
    assert_eq!(e.request_id, "request_01stream", "it names the stream");
}

#[tokio::test]
async fn a_slow_handshake_does_not_spend_the_window() {
    // The handshake takes longer than the whole window, and a batch follows
    // the opening frame by a fraction of it. A window that started with the
    // request would already be over; one that starts at the opening frame
    // receives the batch.
    let srv = ScriptedServer::start(|mut ex| {
        std::thread::sleep(Duration::from_millis(1200));
        ex.open_stream();
        ex.open("41");
        std::thread::sleep(Duration::from_millis(300));
        ex.frame("batch", TWO_EVENTS);
        ex.hold(HOLD);
    });
    let out = client(&srv.url)
        .wait(wait_for(1))
        .await
        .expect("a slow handshake spent the window");
    assert_eq!(
        (out.events.len(), out.cursor.as_str()),
        (2, "43"),
        "the batch after the opening frame arrives within the window: {out:?}"
    );
}

#[tokio::test]
async fn a_stream_that_never_opens_is_bounded() {
    // Headers alone do not open a stream. A server or proxy that answers 200
    // and then holds the body would otherwise leave the handshake waiting on
    // the caller alone.
    let srv = ScriptedServer::start(|mut ex| {
        ex.open_stream();
        ex.hold(HOLD);
    });
    let c = Client::builder()
        .key("test-token")
        .base_url(&srv.url)
        .retry(NoRetry)
        .response_timeout(Duration::from_millis(200))
        .build()
        .unwrap();
    let start = Instant::now();
    let failure = c
        .wait(wait_for(1))
        .await
        .expect_err("a stream that never opened");
    assert!(matches!(failure, Failure::Timeout(_)), "{failure}");
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn the_cursor_covers_only_what_was_handed_out() {
    // A consumer that stops part-way through a batch and resumes from the
    // cursor must get the rest of that batch again, not skip it.
    let srv = watch_server(|ex| {
        ex.open("41");
        ex.frame("batch", TWO_EVENTS);
        ex.hold(HOLD);
    });
    let mut st = client(&srv.url)
        .watch(WaitInput::default())
        .await
        .expect("watch");
    st.next().await.expect("an event").expect("no failure");
    assert_eq!(
        st.cursor(),
        "41",
        "resuming after one of two events would skip the second"
    );
    st.next().await.expect("an event").expect("no failure");
    assert_eq!(st.cursor(), "43", "the whole batch has been handed out");
}

#[tokio::test]
async fn a_redirected_watch_is_refused_and_named() {
    let srv = ScriptedServer::start(|mut ex| {
        ex.respond(
            307,
            &[("Location", "https://elsewhere.example:8443/v1/watch")],
            b"",
        );
    });
    let c = Client::builder()
        .key("test-token")
        .base_url(&srv.url)
        .retry(TransientRetry::new(3).with_backoff(vec![Duration::from_millis(1)]))
        .build()
        .unwrap();
    let failure = c.watch(WaitInput::default()).await.unwrap_err();
    match &failure {
        Failure::Redirected { status, host } => {
            assert_eq!((*status, host.as_str()), (307, "elsewhere.example:8443"));
        }
        other => panic!("want a refused redirect, got {other}"),
    }
    assert!(
        failure.to_string().contains("elsewhere.example"),
        "{failure}"
    );
    assert_eq!(
        srv.requests(),
        1,
        "a refused redirect is an answer, not retried"
    );
}

/// A client whose policy allows one retry, quickly.
fn retrying_client(url: &str) -> Client {
    Client::builder()
        .key("test-token")
        .base_url(url)
        .retry(TransientRetry::new(1).with_backoff(vec![Duration::from_millis(1)]))
        .build()
        .expect("client")
}

#[tokio::test]
async fn a_dropped_handshake_is_retried() {
    // The first connection is closed before any response; opening a stream
    // writes nothing, so the handshake is safe to repeat.
    let srv = ScriptedServer::start(|mut ex| {
        if ex.index == 0 {
            return;
        }
        ex.open_stream();
        ex.open("41");
        ex.frame("batch", TWO_EVENTS);
        ex.hold(HOLD);
    });
    let out = retrying_client(&srv.url)
        .wait(WaitInput::default())
        .await
        .expect("one dropped connection is retried");
    assert_eq!(out.events.len(), 2);
    assert_eq!(srv.requests(), 2);
}

#[tokio::test]
async fn a_connection_that_stays_down_is_retried_only_as_the_policy_allows() {
    let srv = ScriptedServer::start(|_ex| {});
    let failure = retrying_client(&srv.url)
        .wait(WaitInput::default())
        .await
        .unwrap_err();
    assert!(matches!(failure, Failure::Transport(_)), "{failure}");
    assert_eq!(srv.requests(), 2, "one retry, not more");
}

#[tokio::test]
async fn a_refused_handshake_is_not_retried() {
    let srv = refusing_server(
        503,
        r#"{"error":{"code":"upstream_unavailable","message":"a dependency did not answer"}}"#,
    );
    let out = retrying_client(&srv.url)
        .wait(WaitInput::default())
        .await
        .expect("the refusal as a value");
    assert!(out.error.is_some());
    assert_eq!(srv.requests(), 1, "a refusal is the caller's to retry");
}

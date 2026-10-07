//! Integration tests against the live Bud endpoint
//! (`https://bud.noetive.io`).
//!
//! They exist to catch drift between this client and the real service, the
//! one bug class a mock cannot surface, so the production base URL is
//! hardcoded ([`PROD_BASE_URL`]) and only the key comes from the
//! environment. `NOETIVE_BUD_BASE_URL` is deliberately ignored, so a
//! misconfigured environment cannot point the drift detector elsewhere.
//!
//! The suite reads, waits and refuses; it never delivers mail. Sending is
//! exercised only through requests the service refuses before anything is
//! queued, and the one write, marking a message read or unread, sets the
//! message to the state it already has. That write still emits a `mail.read`
//! event, which anyone watching the mailbox sees.
//!
//! Every test returns early with a note when `NOETIVE_KEY_SECRET` is unset,
//! so a plain `cargo test` stays green without network access. Run with:
//!
//! ```text
//! NOETIVE_KEY_SECRET=keya_... cargo test --test bud_integration -- --nocapture
//! ```

use std::future::Future;
use std::time::{Duration, Instant};

use noetive::bud::{
    ById, Change, Client, Code, Conversation, Correspondents, Error, In, Listing, MailboxView, Me,
    PartInfo, PartMap, PartView, SendInput, Summary, WaitInput, CONTRACT_VERSION, FOLDER_INBOX,
    FOLDER_QUARANTINE, FOLDER_SENT, PART_MODE_BYTES, PREFIX_AGENT, PREFIX_MESSAGE, RENDER_PARTS,
    SENDING_PAUSED, SENDING_PROVISIONING, SENDING_READY,
};

/// The production endpoint. Not overridable; see the module doc.
const PROD_BASE_URL: &str = "https://bud.noetive.io";

/// Bounds each call so one slow request cannot stall the suite.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The key under test, or `None` with a note when the suite is disabled.
fn key() -> Option<String> {
    let key = std::env::var("NOETIVE_KEY_SECRET").unwrap_or_default();
    if key.trim().is_empty() {
        eprintln!("skipping: NOETIVE_KEY_SECRET unset");
        return None;
    }
    Some(key.trim().to_string())
}

/// Await `fut`, failing the test if it outlives [`REQUEST_TIMEOUT`].
async fn bounded<T>(op: &str, fut: impl Future<Output = T>) -> T {
    tokio::time::timeout(REQUEST_TIMEOUT, fut)
        .await
        .unwrap_or_else(|_| panic!("{op}: no answer within {REQUEST_TIMEOUT:?}"))
}

/// A client whose key the service accepts, and who it is.
///
/// A key the service refuses fails here, once and by name, rather than as a
/// refusal in the middle of every test that follows.
async fn setup() -> Option<(Client, Me)> {
    let key = key()?;
    let c = Client::builder()
        .key(key)
        .base_url(PROD_BASE_URL)
        .build()
        .expect("build the client");
    let out = bounded("describe_me", c.describe_me())
        .await
        .unwrap_or_else(|e| panic!("describe_me: {e}"));
    if let Some(refusal) = out.error {
        panic!(
            "the service refused NOETIVE_KEY_SECRET: {refusal}\n\
             Bud needs a current Noetive key (keya_...); legacy keys are not accepted"
        );
    }
    let me: Me = out.into().expect("decode Me");
    assert!(
        me.mailbox.starts_with(PREFIX_AGENT),
        "me.mailbox = {:?}, want an agent identifier",
        me.mailbox
    );
    Some((c, me))
}

/// Fails unless the call came back as a refusal with `code`.
fn refused<T: std::fmt::Debug>(
    op: &str,
    result: Result<T, noetive::bud::Failure>,
    refusal: impl FnOnce(&T) -> Option<&Error>,
    code: Code,
) -> Error {
    let out = result.unwrap_or_else(|e| panic!("{op}: a refusal arrived as an error: {e}"));
    let e = refusal(&out)
        .unwrap_or_else(|| panic!("{op}: want a {code} refusal, got an answer: {out:?}"))
        .clone();
    assert_eq!(e.code, code, "{op}: {e}");
    assert!(
        !e.request_id.is_empty(),
        "{op}: the refusal carries no request id to quote"
    );
    e
}

/// Fails unless the call succeeded, and returns its output.
fn answered<T>(
    op: &str,
    result: Result<T, noetive::bud::Failure>,
    refusal: impl FnOnce(&T) -> Option<&Error>,
) -> T {
    let out = result.unwrap_or_else(|e| panic!("{op}: {e}"));
    if let Some(e) = refusal(&out) {
        panic!("{op}: refused: {e}");
    }
    out
}

#[tokio::test]
async fn an_unknown_key_is_refused_as_a_value() {
    // Needs no valid key: the refusal is the answer.
    if key().is_none() {
        return;
    }
    let c = Client::builder()
        .key("keya_not_a_real_key_0000000000000000")
        .base_url(PROD_BASE_URL)
        .build()
        .unwrap();
    let e = refused(
        "health",
        bounded("health", c.health()).await,
        |h| h.error.as_ref(),
        Code::Unauthorized,
    );
    assert_eq!(e.http_status, 401);
}

#[tokio::test]
async fn health_names_the_agent() {
    let Some((c, me)) = setup().await else { return };
    let h = answered("health", bounded("health", c.health()).await, |h| {
        h.error.as_ref()
    });
    assert_eq!(
        h.contract, CONTRACT_VERSION,
        "the contract this client implements"
    );
    assert_eq!(
        (h.agent.as_str(), h.tenant.as_str()),
        (me.agent.as_str(), me.tenant.as_str())
    );
}

#[tokio::test]
async fn the_catalog_serves_every_operation_this_client_calls() {
    let Some((c, _)) = setup().await else { return };
    let cat = answered(
        "describe_catalog",
        bounded("describe_catalog", c.describe_catalog()).await,
        |c| c.error.as_ref(),
    );
    for op in [
        "me.describe",
        "health",
        "catalog.describe",
        "folder.list",
        "message.describe",
        "thread.describe",
        "part.describe",
        "mailbox.describe",
        "correspondent.list",
        "message.update",
        "send",
        "watch",
    ] {
        assert!(cat.served(op), "the catalog does not serve {op:?}");
    }
}

#[tokio::test]
async fn the_mailbox_describes_itself() {
    let Some((c, me)) = setup().await else { return };
    let out = answered(
        "describe_mailbox",
        bounded(
            "describe_mailbox",
            c.describe_mailbox(ById {
                id: me.mailbox.clone(),
                ..ById::default()
            }),
        )
        .await,
        |o| o.error.as_ref(),
    );
    let mv: MailboxView = out.into().expect("decode MailboxView");
    assert_eq!(mv.mailbox, me.mailbox);
    if let Some(sending) = mv.sending {
        assert!(
            [SENDING_READY, SENDING_PROVISIONING, SENDING_PAUSED]
                .contains(&sending.status.as_str()),
            "sending status {:?} is not one this client knows",
            sending.status
        );
    }
}

#[tokio::test]
async fn every_folder_lists() {
    let Some((c, me)) = setup().await else { return };
    for folder in [FOLDER_INBOX, FOLDER_SENT, FOLDER_QUARANTINE] {
        let input = In {
            mailbox: me.mailbox.clone(),
            folder: folder.into(),
            limit: 5,
            ..In::default()
        };
        let out = answered(
            folder,
            bounded("list_folder", c.list_folder(input.clone())).await,
            |o| o.error.as_ref(),
        );
        let cursor = out.cursor.clone();
        let listing: Listing = out.into().expect("decode Listing");
        for s in &listing.messages {
            assert!(
                s.message.starts_with(PREFIX_MESSAGE),
                "{folder} lists {:?}, want a message identifier",
                s.message
            );
        }
        if cursor.is_empty() {
            continue;
        }
        answered(
            folder,
            bounded("list_folder page 2", c.list_folder(In { cursor, ..input })).await,
            |o| o.error.as_ref(),
        );
    }
}

#[tokio::test]
async fn correspondents_list() {
    let Some((c, me)) = setup().await else { return };
    let out = answered(
        "list_correspondents",
        bounded(
            "list_correspondents",
            c.list_correspondents(In {
                mailbox: me.mailbox.clone(),
                limit: 5,
                ..In::default()
            }),
        )
        .await,
        |o| o.error.as_ref(),
    );
    let cv: Correspondents = out.into().expect("decode Correspondents");
    for cr in &cv.correspondents {
        assert!(
            !cr.addr.is_empty() && !cr.first_seen.is_empty(),
            "correspondent {cr:?} is missing its address or first sighting"
        );
    }
}

#[tokio::test]
async fn a_message_reads_every_way() {
    let Some((c, me)) = setup().await else { return };
    let Some(s) = any_message(&c, &me).await else {
        return;
    };

    let text = answered(
        "describe_message",
        bounded(
            "describe_message",
            c.describe_message(ById {
                id: s.message.clone(),
                ..ById::default()
            }),
        )
        .await,
        |o| o.error.as_ref(),
    );
    assert!(
        noetive::bud::has_banner(&text.text),
        "the rendering does not begin with its banner: {:?}",
        text.text.chars().take(60).collect::<String>()
    );
    assert!(text.provenance.is_some(), "a message carries no provenance");

    let parts = answered(
        "describe_message parts",
        bounded(
            "describe_message parts",
            c.describe_message(ById {
                id: s.message.clone(),
                render: RENDER_PARTS.into(),
                ..ById::default()
            }),
        )
        .await,
        |o| o.error.as_ref(),
    );
    let pm: PartMap = parts.into().expect("decode PartMap");
    assert!(!pm.parts.is_empty(), "a stored message lists no parts");

    if let Some(n) = first_text(&pm.parts) {
        let part = answered(
            "describe_part",
            bounded(
                "describe_part",
                c.describe_part(ById {
                    id: s.message.clone(),
                    part: n.clone(),
                    mode: PART_MODE_BYTES.into(),
                    ..ById::default()
                }),
            )
            .await,
            |o| o.error.as_ref(),
        );
        let pv: PartView = part.into().expect("decode PartView");
        assert!(
            pv.part == n && pv.bytes.len() as u64 == pv.size,
            "part {n}: got part {:?} with {} bytes of a declared {}",
            pv.part,
            pv.bytes.len(),
            pv.size
        );
    }

    if !s.thread.is_empty() {
        let thread = answered(
            "describe_thread",
            bounded(
                "describe_thread",
                c.describe_thread(ById {
                    id: s.thread.clone(),
                    max_messages: 3,
                    ..ById::default()
                }),
            )
            .await,
            |o| o.error.as_ref(),
        );
        let conv: Conversation = thread.into().expect("decode Conversation");
        assert!(
            conv.thread == s.thread && !conv.messages.is_empty(),
            "thread {:?} with {} messages, want {:?}",
            conv.thread,
            conv.messages.len(),
            s.thread
        );
    }
}

#[tokio::test]
async fn marking_a_message_as_it_is_changes_nothing() {
    let Some((c, me)) = setup().await else { return };
    let Some(s) = any_message(&c, &me).await else {
        return;
    };

    // The read state it already has: the one write this suite makes.
    answered(
        "update_message",
        bounded(
            "update_message",
            c.update_message(Change {
                id: s.message.clone(),
                changes: serde_json::json!({"read": !s.unread}),
                ..Change::default()
            }),
        )
        .await,
        |o| o.error.as_ref(),
    );
    let after = find_message(&c, &me, &s.message).await;
    assert_eq!(
        after.unread, s.unread,
        "the update was meant to change nothing"
    );
}

#[tokio::test]
async fn a_quiet_wait_is_success() {
    let Some((c, _)) = setup().await else { return };
    let start = Instant::now();
    let out = answered(
        "wait",
        bounded(
            "wait",
            c.wait(WaitInput {
                timeout_seconds: 2,
                ..WaitInput::default()
            }),
        )
        .await,
        |o| o.error.as_ref(),
    );
    assert!(
        !out.cursor.is_empty(),
        "wait returned no cursor to resume from"
    );
    assert!(
        start.elapsed() < Duration::from_secs(15),
        "a two-second window took {:?}",
        start.elapsed()
    );
    answered(
        "wait from the cursor",
        bounded(
            "wait from the cursor",
            c.wait(WaitInput {
                cursor: out.cursor,
                timeout_seconds: 1,
                ..WaitInput::default()
            }),
        )
        .await,
        |o| o.error.as_ref(),
    );
}

#[tokio::test]
async fn a_watch_opens_at_a_cursor() {
    let Some((c, _)) = setup().await else { return };
    let st = bounded(
        "watch",
        c.watch(WaitInput {
            timeout_seconds: 1,
            ..WaitInput::default()
        }),
    )
    .await
    .unwrap_or_else(|e| panic!("watch: {e}"));
    assert!(
        !st.request_id().is_empty(),
        "the stream carries no request id"
    );
    assert!(
        !st.cursor().is_empty(),
        "the stream opened without saying where it starts"
    );
}

#[tokio::test]
async fn refusals_carry_what_to_do_next() {
    let Some((c, me)) = setup().await else { return };

    refused(
        "describe_message of a message that does not exist",
        bounded(
            "describe_message",
            c.describe_message(ById {
                id: "message_01aaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                ..ById::default()
            }),
        )
        .await,
        |o| o.error.as_ref(),
        Code::NotFound,
    );

    refused(
        "list_folder of a folder that does not exist",
        bounded(
            "list_folder",
            c.list_folder(In {
                mailbox: me.mailbox.clone(),
                folder: "no-such-folder".into(),
                ..In::default()
            }),
        )
        .await,
        |o| o.error.as_ref(),
        Code::Invalid,
    );

    let Some(to) = me.addresses.first() else {
        eprintln!("skipping the refused send: the agent has no address to address it to");
        return;
    };
    // Refused before anything is queued: an attachment is not served.
    let e = refused(
        "send with an attachment",
        bounded(
            "send",
            c.send(SendInput {
                to: vec![to.clone()],
                subject: "integration: refused".into(),
                text: "never sent".into(),
                attach: vec!["message_01aaaaaaaaaaaaaaaaaaaaaaaaaa/part/2".into()],
                idempotency_key: "integration-refused-attach".into(),
                ..SendInput::default()
            }),
        )
        .await,
        |o| o.error.as_ref(),
        Code::Invalid,
    );
    assert_eq!(e.field, "/attach");
}

/// One stored message, or `None` with a note when the mailbox is empty.
async fn any_message(c: &Client, me: &Me) -> Option<Summary> {
    for folder in [FOLDER_INBOX, FOLDER_SENT] {
        let out = answered(
            folder,
            bounded(
                "list_folder",
                c.list_folder(In {
                    mailbox: me.mailbox.clone(),
                    folder: folder.into(),
                    limit: 20,
                    ..In::default()
                }),
            )
            .await,
            |o| o.error.as_ref(),
        );
        let listing: Listing = out.into().expect("decode Listing");
        if let Some(first) = listing.messages.into_iter().next() {
            return Some(first);
        }
    }
    eprintln!("skipping: the mailbox holds no mail to read");
    None
}

/// A message's current summary.
async fn find_message(c: &Client, me: &Me, id: &str) -> Summary {
    for folder in [FOLDER_INBOX, FOLDER_SENT] {
        let mut cursor = String::new();
        for _page in 0..20 {
            let out = answered(
                folder,
                bounded(
                    "list_folder",
                    c.list_folder(In {
                        mailbox: me.mailbox.clone(),
                        folder: folder.into(),
                        limit: 200,
                        cursor: cursor.clone(),
                        ..In::default()
                    }),
                )
                .await,
                |o| o.error.as_ref(),
            );
            cursor = out.cursor.clone();
            let listing: Listing = out.into().expect("decode Listing");
            if let Some(s) = listing.messages.into_iter().find(|s| s.message == id) {
                return s;
            }
            if cursor.is_empty() {
                break;
            }
        }
    }
    panic!("message {id} is no longer listed");
}

/// The number of the first leaf text part small enough to read whole.
fn first_text(parts: &[PartInfo]) -> Option<String> {
    for p in parts {
        if !p.children.is_empty() {
            if let Some(n) = first_text(&p.children) {
                return Some(n);
            }
            continue;
        }
        if p.content_type.starts_with("text/") && p.size > 0 && p.size < 1 << 20 {
            return Some(p.n.clone());
        }
    }
    None
}

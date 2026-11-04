//! The Bud wire types against the service's own description of them.
//!
//! The service declares the same eighteen shapes in code this crate cannot
//! import, and two hand-written definitions of one contract drift — the bad
//! kind of drift, where both sides compile and a field silently stops
//! arriving. So the encoding is compared: the service's description is
//! vendored at `tests/testdata/bud/contract.golden` (one line per field:
//! JSON name, wire kind, and whether it is omitted when empty), and for
//! every type this checks that a fully populated value encodes exactly the
//! golden's field names with the golden's kinds, and that a zero value omits
//! exactly the fields the golden marks omitempty.
//!
//! Refresh the golden by copying the service's description, and decide
//! which side is right before changing either: a client that encodes a
//! field the server does not read loses it silently.

use std::collections::{BTreeMap, BTreeSet};

use noetive::bud::Code;
use noetive::bud::{
    Actor, Conversation, Effect, Error, EventData, JournalEvent, Listing, Me, Provenance, PutInput,
    PutOutput, ReadInput, ReadOutput, SendInput, SendOutput, SendingStatus, Summary, WaitInput,
    WaitOutput, CONTRACT_VERSION,
};
use serde::Serialize;
use serde_json::{json, Value};

const GOLDEN: &str = include_str!("testdata/bud/contract.golden");

/// One field as the golden describes it.
#[derive(Debug, Clone)]
struct Field {
    kind: String,
    omitempty: bool,
}

/// The golden's generation and its types, by name.
fn golden() -> (String, BTreeMap<String, BTreeMap<String, Field>>) {
    let mut lines = GOLDEN.lines();
    let version = lines.next().expect("a version line").to_string();
    let mut types: BTreeMap<String, BTreeMap<String, Field>> = BTreeMap::new();
    let mut current = String::new();
    for line in lines.filter(|l| !l.trim().is_empty()) {
        if let Some(field) = line.strip_prefix("  ") {
            let (name, rest) = field.split_once(' ').expect("name and kind");
            let (kind, omitempty) = match rest.strip_suffix(",omitempty") {
                Some(kind) => (kind, true),
                None => (rest, false),
            };
            types.entry(current.clone()).or_default().insert(
                name.to_string(),
                Field {
                    kind: kind.to_string(),
                    omitempty,
                },
            );
        } else {
            current = line.to_string();
            types.entry(current.clone()).or_default();
        }
    }
    (version, types)
}

fn keys(v: &Value) -> BTreeSet<String> {
    v.as_object().expect("an object").keys().cloned().collect()
}

/// Whether a JSON value is of the golden's wire kind.
fn is_kind(kind: &str, v: &Value) -> bool {
    match kind {
        "string" => v.is_string(),
        "number" => v.is_number(),
        "bool" => v.is_boolean(),
        "object" => v.is_object(),
        "json" => true,
        k if k.starts_with("[]") => v.is_array(),
        k if k.starts_with("map[") => v.is_object(),
        other => panic!("the golden names a wire kind this test does not know: {other}"),
    }
}

fn s(v: &str) -> String {
    v.to_string()
}

fn strings() -> Vec<String> {
    vec![s("x")]
}

fn counts() -> BTreeMap<String, i64> {
    BTreeMap::from([(s("per_hour"), 1)])
}

fn full_error() -> Error {
    Error {
        code: Code::RateLimited,
        message: s("m"),
        hint: s("h"),
        request_id: s("request_01x"),
        retry_after_ms: 1,
        current: Some(json!({"read": true})),
        version: s("v"),
        field: s("/f"),
        guard: s("g"),
        counter: s("c"),
        http_status: 429,
    }
}

fn full_provenance() -> Provenance {
    Provenance {
        spf: s("pass"),
        dkim: s("pass"),
        dmarc: s("pass"),
        aligned: true,
        in_workspace: true,
        known: true,
        thread_join: s("new"),
        folder: s("inbox"),
        removed: counts(),
    }
}

fn full_effect() -> Effect {
    Effect {
        kind: s("mail.read"),
        count: 1,
        reference: s("message_01x"),
    }
}

fn full_actor() -> Actor {
    Actor {
        kind: s("agent"),
        agent: s("ag_01x"),
    }
}

fn full_event_data() -> EventData {
    EventData {
        from: s("a@b"),
        subject: s("s"),
        aligned: true,
        in_workspace: true,
        known: true,
        agent_part: true,
        folder: s("inbox"),
        guard: s("g"),
        counter: s("c"),
        reason: s("r"),
        state: s("sent"),
        provider: s("p"),
        message_id: s("id@b"),
    }
}

fn full_event() -> JournalEvent {
    JournalEvent {
        id: s("journal_01x"),
        workspace: s("workspace_01x"),
        at: s("2026-01-02T03:04:05Z"),
        seq: 1,
        kind: s("mail.received"),
        mailbox: s("ag_01x"),
        message: s("message_01x"),
        thread: s("thread_01x"),
        actor: full_actor(),
        corr: s("request_01x"),
        data: full_event_data(),
    }
}

fn full_summary() -> Summary {
    Summary {
        message: s("message_01x"),
        thread: s("thread_01x"),
        subject: s("s"),
        date: s("2026-01-02T03:04:05Z"),
        from: s("a@b"),
        from_name: s("A"),
        aligned: true,
        in_workspace: true,
        known: true,
        join: s("new"),
        attachments: 1,
        unread: true,
        incomplete: true,
    }
}

/// Every contract type, fully populated and zero, under its golden name.
fn values() -> Vec<(&'static str, Value, Value)> {
    fn pair<T: Serialize + Default>(name: &'static str, full: T) -> (&'static str, Value, Value) {
        (
            name,
            serde_json::to_value(full).unwrap(),
            serde_json::to_value(T::default()).unwrap(),
        )
    }
    vec![
        pair(
            "WaitInput",
            WaitInput {
                cursor: s("41"),
                mailbox: s("ag_01x"),
                timeout_seconds: 5,
                types: strings(),
                agent_part: Some(false),
            },
        ),
        pair(
            "WaitOutput",
            WaitOutput {
                events: vec![full_event()],
                cursor: s("41"),
                error: Some(full_error()),
            },
        ),
        pair(
            "ReadInput",
            ReadInput {
                reference: s("message_01x"),
                max_chars: 1,
                cursor: s("c"),
            },
        ),
        pair(
            "ReadOutput",
            ReadOutput {
                reference: s("message_01x"),
                kind: s("message"),
                text: s("t"),
                content: Some(json!({})),
                provenance: Some(full_provenance()),
                cursor: s("c"),
                truncated: true,
                version: s("v"),
                error: Some(full_error()),
            },
        ),
        pair(
            "PutInput",
            PutInput {
                reference: s("message_01x"),
                body: json!({"read": true}),
                version: s("v"),
                idempotency_key: s("k"),
            },
        ),
        pair(
            "PutOutput",
            PutOutput {
                reference: s("message_01x"),
                version: s("v"),
                created: true,
                effects: vec![full_effect()],
                error: Some(full_error()),
            },
        ),
        pair(
            "SendInput",
            SendInput {
                to: strings(),
                subject: s("s"),
                text: s("t"),
                html: s("h"),
                cc: strings(),
                bcc: strings(),
                from: s("a@b"),
                on_behalf_of: s("ag_01y"),
                in_reply_to: s("message_01p"),
                reply_all: true,
                attach: strings(),
                agent: Some(json!({"intent": "x"})),
                idempotency_key: s("k"),
            },
        ),
        pair(
            "SendOutput",
            SendOutput {
                message: s("message_01x"),
                thread: s("thread_01x"),
                state: s("queued"),
                approval_id: s("approval_01x"),
                reason: s("r"),
                remaining: counts(),
                error: Some(full_error()),
            },
        ),
        pair("Error", full_error()),
        pair("Provenance", full_provenance()),
        pair("Effect", full_effect()),
        pair("JournalEvent", full_event()),
        pair("EventData", full_event_data()),
        pair("Actor", full_actor()),
        pair(
            "Me",
            Me {
                agent: s("ag_01x"),
                workspace: s("workspace_01x"),
                kind: s("agent"),
                name: s("n"),
                addresses: strings(),
                mailbox: s("ag_01x"),
                readable_mailboxes: strings(),
                scopes: strings(),
                granted: strings(),
                limits: counts(),
                sending: Some(SendingStatus {
                    status: s("ready"),
                    reason: s("r"),
                }),
            },
        ),
        pair(
            "Listing",
            Listing {
                mailbox: s("ag_01x"),
                folder: s("inbox"),
                messages: vec![full_summary()],
            },
        ),
        pair(
            "Conversation",
            Conversation {
                thread: s("thread_01x"),
                messages: vec![full_summary()],
            },
        ),
        pair("Summary", full_summary()),
    ]
}

#[test]
fn the_vendored_contract_is_the_generation_this_crate_implements() {
    // The shapes can match line for line while the generation does not.
    let (version, _) = golden();
    assert_eq!(version, CONTRACT_VERSION);
}

#[test]
fn every_contract_type_encodes_as_the_server_describes_it() {
    let (_, types) = golden();
    let values = values();

    let tested: BTreeSet<&str> = values.iter().map(|(name, _, _)| *name).collect();
    let described: BTreeSet<&str> = types.keys().map(String::as_str).collect();
    assert_eq!(
        tested, described,
        "every type the server describes is checked, and no other"
    );

    for (name, full, zero) in values {
        let fields = &types[name];
        let all: BTreeSet<String> = fields.keys().cloned().collect();
        assert_eq!(
            keys(&full),
            all,
            "{name}: a fully populated value encodes exactly the server's field names"
        );
        for (field, spec) in fields {
            assert!(
                is_kind(&spec.kind, &full[field]),
                "{name}.{field}: the server describes a {}, this encodes {}",
                spec.kind,
                full[field]
            );
        }
        let required: BTreeSet<String> = fields
            .iter()
            .filter(|(_, f)| !f.omitempty)
            .map(|(k, _)| k.clone())
            .collect();
        assert_eq!(
            keys(&zero),
            required,
            "{name}: a zero value omits exactly the server's omitempty fields"
        );
    }
}

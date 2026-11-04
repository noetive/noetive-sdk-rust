//! The wire types, hand-written to match a contract the server also
//! declares.
//!
//! What is checked rather than hoped for is the encoding: the service
//! describes its own shapes, that description is vendored under
//! `tests/testdata/bud/contract.golden`, and the contract test fails when a
//! field name or its omission rule disagrees. A renamed field is a red test,
//! not a field that silently stops arriving.
//!
//! Two rules that look like style and are not. An identifier field is never
//! skipped when empty: the server's zero identifier encodes as `""`, so
//! omitting the field produces a document the server never sends.
//! `WaitOutput::cursor` is never skipped for the same reason and one more:
//! the server declares it required even on a refusal.
//!
//! Timestamps are RFC 3339 strings, as the server sends them; this crate
//! takes no date-time dependency for the caller.

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::bud::error::{preflight, Code, Error, Failure};
use crate::bud::vocab::{
    KIND_CORR, KIND_FOLDER, KIND_MAILBOX, KIND_ME, KIND_MESSAGE, KIND_PART, KIND_THREAD,
};

pub(crate) fn is_false(v: &bool) -> bool {
    !*v
}

pub(crate) fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

/// Asks for whatever a reference names.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReadInput {
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub max_chars: u32,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub cursor: String,
}

/// What a read returns.
///
/// `text` and `content` carry the same object two ways: `text` is the
/// rendering a model reads, `content` the structure a program branches on.
/// A caller that gates on provenance reads [`ReadOutput::provenance`]
/// rather than parsing `text` — that is why the two travel together.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReadOutput {
    #[serde(rename = "ref", skip_serializing_if = "String::is_empty")]
    pub reference: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub kind: String,

    /// The rendering. For a message or a thread it begins with a banner
    /// naming what follows as data rather than instructions; see
    /// [`has_banner`](crate::bud::has_banner).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub text: String,

    /// The structure; decode it with [`ReadOutput::into`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<serde_json::Value>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,

    /// Continues a truncated answer. A thread that left out older messages
    /// and a part cut at its read limit have none.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub cursor: String,

    /// There is more than this answer carries.
    #[serde(skip_serializing_if = "is_false")]
    pub truncated: bool,

    /// Declared by the wire and not sent: no update takes a version.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub version: String,

    /// A refusal. A refused read is still `Ok`: branch on this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

/// A type [`ReadOutput::into`] decodes `content` into.
///
/// This crate's views name the kind whose content they decode, and a
/// mismatch is refused rather than decoded into zero values that would read
/// as an empty mailbox. A caller's own type implements this with the default
/// `kind`, which decodes any kind as asked.
pub trait View: DeserializeOwned {
    /// The kind whose content this decodes, or `None` for any.
    fn kind() -> Option<&'static str> {
        None
    }
}

impl View for serde_json::Value {}

impl ReadOutput {
    /// Decode `content` into a view.
    ///
    /// A refusal comes back as itself. A view of the wrong kind, or an
    /// answer with no content (such as help, whose text is the whole of
    /// it), is refused as [`Code::Invalid`] with no `http_status`. Content
    /// that does not decode is [`Code::MalformedResponse`].
    ///
    /// Everything content carries about mail was written by somebody else;
    /// decoding it here means a caller does not pick its own decoder for
    /// stranger-written bytes.
    pub fn into<T: View>(self) -> Result<T, Failure> {
        if let Some(refusal) = self.error {
            return Err(refusal.into());
        }
        if let Some(want) = T::kind() {
            if self.kind != want {
                return Err(preflight(format!(
                    "a {} answer does not decode into a {} view",
                    quoted(&self.kind),
                    quoted(want)
                ))
                .into());
            }
        }
        let Some(content) = self.content else {
            return Err(preflight(format!(
                "this {} answer carries no content; read its text",
                quoted(&self.kind)
            ))
            .into());
        };
        serde_json::from_value(content).map_err(|e| {
            Error {
                code: Code::MalformedResponse,
                message: format!("the content did not decode: {e}"),
                ..Error::default()
            }
            .into()
        })
    }
}

/// Renders a name for a message without letting an empty one read as a
/// missing word.
pub(crate) fn quoted(s: &str) -> String {
    if s.is_empty() {
        "(unnamed)".to_string()
    } else {
        format!("\"{s}\"")
    }
}

/// How much a message's claims about itself can be trusted.
///
/// Separate from the content so a caller can refuse to act on an
/// unauthenticated message without reading a word of it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Provenance {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub spf: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub dkim: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub dmarc: String,

    /// The one to branch on: the authenticated domain and the visible From
    /// agree. SPF passing alone says only that somebody was allowed to send
    /// from that server.
    pub aligned: bool,
    pub in_workspace: bool,
    pub known: bool,

    /// How the message joined its conversation: `JOIN_NEW`,
    /// `JOIN_VERIFIED`, or `JOIN_CLAIMED` for one that only claims to be a
    /// reply.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub thread_join: String,

    /// Declared by the wire and not sent.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub folder: String,
    /// Declared by the wire and not sent.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub removed: BTreeMap<String, i64>,
}

/// Writes to whatever a reference names.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PutInput {
    #[serde(rename = "ref")]
    pub reference: String,
    pub body: serde_json::Value,
    /// Declared by the wire; no write takes one, and one sent is refused as
    /// invalid.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub idempotency_key: String,
}

/// What a write returns.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PutOutput {
    #[serde(rename = "ref", skip_serializing_if = "String::is_empty")]
    pub reference: String,
    /// Declared by the wire and not sent.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(skip_serializing_if = "is_false")]
    pub created: bool,
    /// What the write changed: `EFFECT_MAIL_READ`, `EFFECT_LABELLED`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<Effect>,
    /// A refusal. A refused write is still `Ok`: branch on this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

/// One consequence of a write.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Effect {
    pub kind: String,
    #[serde(skip_serializing_if = "is_zero_u64")]
    pub count: u64,
    #[serde(rename = "ref", skip_serializing_if = "String::is_empty")]
    pub reference: String,
}

/// Composes and sends a message.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SendInput {
    /// May be empty when `cc` or `bcc` names someone, or on a reply, which
    /// takes its recipients from the message replied to.
    pub to: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub subject: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub html: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cc: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bcc: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub from: String,

    /// Writes as another agent. It needs a send-on-behalf grant from that
    /// agent's owner, the organization's permission to send on behalf, and
    /// a `from` domain verified for the workspace; without all three the
    /// send is refused as `policy_refused`. The message names both, with
    /// the sending agent as Sender, so a recipient sees who wrote it and
    /// who it is from.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub on_behalf_of: String,

    #[serde(skip_serializing_if = "String::is_empty")]
    pub in_reply_to: String,
    #[serde(skip_serializing_if = "is_false")]
    pub reply_all: bool,

    /// Declared by the wire; a send that sets it is refused as invalid.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attach: Vec<String>,

    /// The machine-readable part, for an exchange between two agents: an
    /// object with `bud`, `intent`, `corr`, `in_reply_to_corr`, `schema`,
    /// `hops` and `payload`. Other keys are dropped and `expect` is
    /// refused. Send `hops` as the count on the message being answered; the
    /// server adds one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<serde_json::Value>,

    /// Makes a retried send return the first one's result for seven days
    /// instead of sending again. A different message under the same key
    /// also returns the first result and sends nothing, so never reuse one.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub idempotency_key: String,
}

/// What a send returns.
///
/// A new send is `STATE_QUEUED`, or `STATE_HELD` with an `approval_id` when
/// a person has to approve it first; a repeat with the same idempotency key
/// reports the message's state now.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SendOutput {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub thread: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub state: String,
    /// The approval a `STATE_HELD` message waits on. Absent until the
    /// request for approval has been filed, which happens in the
    /// background; the message is held either way, and only a person can
    /// release it.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub approval_id: String,
    /// Why a queued message waits instead of going now — "provisioning"
    /// while the sender's domain is verified, or "paused" — and, on a repeat
    /// for a message that failed, why it failed.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub reason: String,
    /// What is left of each sending limit after this message.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub remaining: BTreeMap<String, i64>,
    /// A refusal, naming the guard that fired. A refused send is still
    /// `Ok`: branch on this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

/// What `watch` and `wait` follow, and from where.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WaitInput {
    /// Where to resume. Empty starts at the tail.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub cursor: String,

    /// One mailbox; empty means every mailbox the key can read, and one it
    /// cannot is refused as forbidden_scope. Always sent: an identifier.
    pub mailbox: String,

    /// How long a quiet interval lasts before the server sends a keepalive,
    /// and for `wait`, how long the window is. At most
    /// [`MAX_WAIT_SECONDS`](crate::bud::MAX_WAIT_SECONDS); zero takes it.
    /// It never ends a `watch`.
    #[serde(rename = "timeout_s", skip_serializing_if = "is_zero_u32")]
    pub timeout_seconds: u32,

    /// Narrows to these event types (the `EVENT_*` constants). Empty matches
    /// everything; an unknown type is refused before the stream opens.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<String>,

    /// Narrows to messages carrying a machine-readable part (`Some(true)`)
    /// or without one (`Some(false)`). `None` matches both.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_part: Option<bool>,
}

/// What `wait` returns, and what each batch on a stream carries.
///
/// No events and no error is success: nothing happened within the window.
/// `cursor` is where to resume, including after a refusal.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WaitOutput {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<JournalEvent>,
    pub cursor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

impl WaitOutput {
    /// The window closed and nothing happened.
    ///
    /// Not a failure and nothing to retry: wait again from the returned
    /// cursor. Treating it as an error — backing off, alerting, discarding
    /// the cursor — is the mistake this exists to prevent.
    pub fn is_empty(&self) -> bool {
        self.error.is_none() && self.events.is_empty()
    }
}

/// One thing that happened.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JournalEvent {
    /// Dedupe on this: delivery is at least once.
    pub id: String,
    pub workspace: String,
    /// When, RFC 3339.
    #[serde(rename = "ts")]
    pub at: String,
    /// Orders events within a mailbox.
    pub seq: u64,
    /// An `EVENT_*` constant.
    #[serde(rename = "type")]
    pub kind: String,
    pub mailbox: String,
    pub message: String,
    pub thread: String,
    pub actor: Actor,
    /// Ties this event to the request that caused it, and to the other
    /// events that request caused: the same token the response carried as
    /// `X-Request-Id`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub corr: String,
    pub data: EventData,
}

/// Who caused an event.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Actor {
    /// `ACTOR_SYSTEM`, `ACTOR_AGENT` or `ACTOR_OPERATOR`.
    pub kind: String,
    /// The agent that acted, when `kind` is `ACTOR_AGENT`.
    pub agent: String,
}

/// The little an event carries about itself: enough to triage on without
/// opening anything, and no more, because every field here is one somebody
/// else chose.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EventData {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub from: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub subject: String,
    #[serde(skip_serializing_if = "is_false")]
    pub aligned: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub in_workspace: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub known: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub agent_part: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub folder: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub guard: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub counter: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub state: String,
    /// An opaque delivery reference for a message that was handed on.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub provider: String,
    /// The RFC 5322 Message-ID, without angle brackets, on `mail.sent`: the
    /// provider assigns it after the send answered, so `SendOutput` cannot
    /// carry it.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message_id: String,
}

/// Who the caller is and what it can reach. Content for `KIND_ME`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Me {
    pub agent: String,
    pub workspace: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// What this agent sends as. The first is the default.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub mailbox: String,
    /// Every mailbox this caller may read.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub readable_mailboxes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub granted: Vec<String>,
    /// What is left of each sending budget.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub limits: BTreeMap<String, i64>,
    /// Whether mail sent from these addresses goes out now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sending: Option<SendingStatus>,
}

/// A folder's contents. Content for `KIND_FOLDER`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Listing {
    pub mailbox: String,
    pub folder: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<Summary>,
}

/// A thread's messages in order. Content for `KIND_THREAD`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Conversation {
    pub thread: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<Summary>,
}

/// One message, as much as a listing shows.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Summary {
    pub message: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub thread: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub subject: String,
    /// RFC 3339.
    pub date: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub from: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub from_name: String,
    /// The provenance to triage on without opening anything.
    pub aligned: bool,
    pub in_workspace: bool,
    pub known: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub join: String,
    #[serde(skip_serializing_if = "is_zero_u64")]
    pub attachments: u64,
    #[serde(skip_serializing_if = "is_false")]
    pub unread: bool,
    /// The stored message is missing parts — a rendering that failed, a
    /// blob still uploading. What is here is true; it is not all of it.
    #[serde(skip_serializing_if = "is_false")]
    pub incomplete: bool,
}

// The views below are not in the contract: they follow the published API
// description field for field.

/// Whether mail an agent sends now goes out now, and if not, why. `status`
/// is `SENDING_READY`, `SENDING_PROVISIONING` or `SENDING_PAUSED`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SendingStatus {
    pub status: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

/// A mailbox's own state. Content for `KIND_MAILBOX`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MailboxView {
    pub mailbox: String,
    /// What this mailbox sends as. The first is the default.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<String>,
    /// An operator stopped this mailbox sending; `reason` says why.
    #[serde(skip_serializing_if = "is_false")]
    pub paused: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub reason: String,
    /// How many messages are in each folder.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub folders: BTreeMap<String, i64>,
    #[serde(skip_serializing_if = "is_zero_u64")]
    pub correspondents: u64,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub limits: BTreeMap<String, i64>,
    /// A count hit its bound and the real number is at least what is
    /// reported.
    #[serde(skip_serializing_if = "is_false")]
    pub approximate: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sending: Option<SendingStatus>,
}

/// Who a mailbox has exchanged mail with. Content for `KIND_CORR`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Correspondents {
    pub mailbox: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub correspondents: Vec<CorrespondentView>,
}

/// One address and the traffic with it: facts about traffic, not a curated
/// contact. `addr` is whatever a sender put on the wire.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CorrespondentView {
    pub addr: String,
    /// RFC 3339.
    pub first_seen: String,
    /// RFC 3339.
    pub last_seen: String,
    #[serde(skip_serializing_if = "is_zero_u64")]
    pub inbound: u64,
    #[serde(skip_serializing_if = "is_zero_u64")]
    pub outbound: u64,
    /// The last verdict rather than a summary: an address that passed a
    /// hundred times and fails now is the shape of an account takeover.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub last_auth: String,
    #[serde(skip_serializing_if = "is_false")]
    pub last_aligned: bool,
}

/// One part of a message in the mode that was asked for. Content for
/// `KIND_PART`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PartView {
    pub message: String,
    pub part: String,
    pub mode: String,
    /// What the sender declared: a claim, not a fact.
    #[serde(rename = "type", skip_serializing_if = "String::is_empty")]
    pub content_type: String,
    /// What the sender declared: a claim, not a fact.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub filename: String,
    #[serde(skip_serializing_if = "is_zero_u64")]
    pub size: u64,
    /// The content, for `PART_MODE_BYTES`. The server sends standard
    /// base64; it is decoded here so the caller never handles the encoding.
    #[serde(
        rename = "bytes_base64",
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "crate::bud::base64::serialize",
        deserialize_with = "crate::bud::base64::deserialize"
    )]
    pub bytes: Vec<u8>,
}

/// A message's structure: content for `KIND_MESSAGE` read with
/// `RENDER_PARTS`. Each part's `n` is what `describe_part` takes as
/// `ById::part`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PartMap {
    pub message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<PartInfo>,
}

/// One part of a message, as a parts reading lists it. `content_type`,
/// `charset`, `filename` and `disposition` are what the sender declared.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PartInfo {
    pub n: String,
    #[serde(rename = "type")]
    pub content_type: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub charset: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub filename: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub disposition: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<PartInfo>,
}

/// What a health probe returns.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Health {
    pub contract: String,
    pub agent: String,
    pub workspace: String,
    /// A refusal: a key that reaches no agent here is forbidden_scope, an
    /// account that cannot be charged is not_billable. Either way the key
    /// itself works; only unauthorized means it does not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

/// The operations a deployment serves and the reference shapes beneath them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Catalog {
    pub contract: String,
    pub operations: Vec<CatalogOperation>,
    pub kinds: Vec<CatalogKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

impl Catalog {
    /// Whether this deployment implements an operation, named either as the
    /// catalog names it (`"ListFolder"`) or by its path (`"folder.list"`).
    pub fn served(&self, name: &str) -> bool {
        self.operations
            .iter()
            .find(|op| op.name == name || op.path == name)
            .is_some_and(|op| op.served)
    }
}

/// One operation, as a client discovers it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CatalogOperation {
    pub name: String,
    pub path: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub kind: String,
    pub served: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub doc: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub body: String,
}

/// One reference shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CatalogKind {
    pub kind: String,
    pub pattern: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub doc: String,
    pub readable: bool,
    pub writable: bool,
    pub served: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub verb: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub action: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<String>,
}

macro_rules! view_of_kind {
    ($($ty:ty => $kind:expr),* $(,)?) => {
        $(impl View for $ty {
            fn kind() -> Option<&'static str> {
                Some($kind)
            }
        })*
    };
}

view_of_kind! {
    Me => KIND_ME,
    MailboxView => KIND_MAILBOX,
    Listing => KIND_FOLDER,
    Correspondents => KIND_CORR,
    Conversation => KIND_THREAD,
    PartView => KIND_PART,
    PartMap => KIND_MESSAGE,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bud::vocab::KIND_HELP;

    #[test]
    fn into_returns_the_refusal_or_says_there_is_nothing() {
        let refused = ReadOutput {
            error: Some(Error {
                code: Code::NotFound,
                ..Error::default()
            }),
            ..ReadOutput::default()
        };
        assert_eq!(
            refused.into::<MailboxView>().unwrap_err().code(),
            Some(&Code::NotFound)
        );

        let text_only = ReadOutput {
            kind: KIND_HELP.into(),
            text: "the grammar".into(),
            ..ReadOutput::default()
        };
        let err = text_only.into::<serde_json::Value>().unwrap_err();
        assert!(err.is_preflight(), "{err}");
    }

    #[test]
    fn a_parts_reading_lists_what_describe_part_takes() {
        let content: serde_json::Value = serde_json::from_str(
            r#"{"message":"message_01x","parts":[
                {"n":"1","type":"multipart/alternative","size":0,"range":[0,9],"children":[
                    {"n":"1.1","type":"text/plain","charset":"utf-8","size":12},
                    {"n":"1.2","type":"text/html","size":40}]},
                {"n":"2","type":"application/pdf","filename":"invoice.pdf","disposition":"attachment","size":2048}]}"#,
        )
        .unwrap();
        let out = ReadOutput {
            kind: KIND_MESSAGE.into(),
            content: Some(content.clone()),
            ..ReadOutput::default()
        };
        let pm: PartMap = out.into().unwrap();
        assert_eq!(pm.parts.len(), 2);
        assert_eq!(pm.parts[0].children.len(), 2);
        assert_eq!(pm.parts[0].children[0].n, "1.1");
        let p = &pm.parts[1];
        assert_eq!(
            (p.n.as_str(), p.filename.as_str(), p.size),
            ("2", "invoice.pdf", 2048)
        );

        let wrong = ReadOutput {
            kind: KIND_PART.into(),
            content: Some(content),
            ..ReadOutput::default()
        };
        let err = wrong.into::<PartMap>().unwrap_err();
        assert!(
            err.is_preflight(),
            "a part answer decoded into a PartMap: {err}"
        );
    }

    #[test]
    fn the_catalog_answers_by_name_or_path() {
        // The server lists each operation by name and by path; a caller may
        // ask by either.
        let c: Catalog = serde_json::from_str(
            r#"{"contract":"bud/v1","operations":[
                {"name":"ListFolder","path":"folder.list","served":true},
                {"name":"Send","path":"send","served":true},
                {"name":"DescribeHold","path":"hold.describe","served":false}]}"#,
        )
        .unwrap();
        for op in ["ListFolder", "folder.list", "Send", "send"] {
            assert!(c.served(op), "served({op:?}) = false");
        }
        for op in ["DescribeHold", "hold.describe", "nothing.at.all"] {
            assert!(!c.served(op), "served({op:?}) = true");
        }
    }
}

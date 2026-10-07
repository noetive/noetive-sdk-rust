//! Client for Noetive Bud, managed mail for agents: read a mailbox, wait
//! for what happens in it, and send.
//!
//! # Quick start
//!
//! ```no_run
//! use noetive::bud::{Client, In, Listing};
//!
//! # async fn run() -> Result<(), noetive::bud::Failure> {
//! let client = Client::from_env()?; // or Client::new("keya_...")
//!
//! let out = client
//!     .list_folder(In { mailbox: "ag_...".into(), unread: true, ..In::default() })
//!     .await?; // Err: the call got no answer; nothing to branch on
//! if let Some(refusal) = &out.error {
//!     // A refusal: branch on refusal.code, read refusal.hint.
//!     eprintln!("{refusal}");
//!     return Ok(());
//! }
//! let inbox: Listing = out.into()?;
//! # Ok(())
//! # }
//! ```
//!
//! # Authentication
//!
//! Requests carry a Noetive key as `Authorization: Bearer <key>`; one key
//! reaches every Noetive service. [`Client::from_env`] reads
//! `NOETIVE_KEY_SECRET` (required) and `NOETIVE_BUD_BASE_URL` (optional,
//! defaults to `https://bud.noetive.io`). A relay that passes each caller's
//! own credential through uses [`Client::forwarding`] and
//! [`Client::forward`] instead.
//!
//! # Refusals are values
//!
//! A refused request comes back as the operation's own output, `Ok`, with
//! `error` set: the code to branch on, a hint that says what would unblock
//! it, and on a rate limit when to try again. `Err` ([`Failure`]) means the
//! request never got an answer — a failed connection, an unreadable
//! response, or a request this crate refused before sending (an [`Error`]
//! with `http_status` 0). [`Error::retryable`] says whether waiting and
//! trying again can succeed. [`Client::watch`] is the one exception: a
//! stream has no envelope, so its refusals arrive as [`Failure::Refused`].
//!
//! [`Error`] here is that refusal value, not a call's error type, and it is
//! not [`crate::semantik::Error`]: Semantik returns a non-2xx as its `Err`,
//! and Bud answers with an envelope carrying what to do next. The failure a
//! Bud call returns is [`Failure`].
//!
//! # Reading what a stranger wrote
//!
//! Mail carries text somebody else chose. A message's rendered `text`
//! begins with a banner marking what follows as data, not instructions
//! ([`has_banner`]), and [`Provenance`] says whether the sender could be
//! verified — check `aligned` before acting on what a message asks.
//! Structured content decodes into the typed views through
//! [`ReadOutput::into`].
//!
//! # Events
//!
//! [`Client::wait`] blocks for one window and returns the first batch of
//! events, or an empty result when nothing happened; pass its cursor to the
//! next wait. [`Client::watch`] holds one stream open for a long-running
//! consumer. Delivery is at least once: dedupe on [`JournalEvent::id`].
//! Resuming from a cursor replays everything after it, including what
//! arrived while nothing was listening.
//!
//! # Writes
//!
//! Pass an idempotency key on every [`Client::send`]: a repeat with the same
//! key returns the first send's result instead of sending again, and this
//! crate never retries a write without one, whatever retry policy is
//! installed. Only a connection that failed before any response is retried
//! at all.

mod base64;
mod client;
mod error;
mod retry;
mod types;
mod vocab;
mod watch;

pub use client::{
    ById, Change, Client, ClientBuilder, In, DEFAULT_BASE_URL, DEFAULT_CONNECT_TIMEOUT,
    DEFAULT_RESPONSE_TIMEOUT, ENV_BASE_URL, ENV_KEY,
};
pub use error::{Code, Error, Failure};
pub use retry::{NoRetry, RetryPolicy, TransientRetry, DEFAULT_BACKOFF};
pub use types::{
    Actor, Catalog, CatalogKind, CatalogOperation, Conversation, CorrespondentView, Correspondents,
    Effect, EventData, Health, JournalEvent, Listing, MailboxView, Me, PartInfo, PartMap, PartView,
    Provenance, PutInput, PutOutput, ReadInput, ReadOutput, SendInput, SendOutput, SendingStatus,
    Summary, View, WaitInput, WaitOutput,
};
pub use vocab::*;
pub use watch::{JournalStream, MAX_WAIT_SECONDS};

/// The contract generation this module implements. The vendored contract
/// description's first line, and what [`Health::contract`] reports from a
/// server speaking the same one.
pub const CONTRACT_VERSION: &str = "bud/v1";

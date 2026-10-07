//! The client, and the one decision in it worth arguing about.
//!
//! A refusal is a value. Bud answers a rejected request with the operation's
//! own output type — a populated `error` inside a well-formed envelope — and
//! this client returns that as `Ok`, because the envelope is what the caller
//! acts on: the code to branch on, the hint that says what would unblock it,
//! the limit that fired and when it clears. `Err` is reserved for what is not
//! about the request: a failed connection, an unreadable answer, or this
//! crate refusing to send at all.
//!
//! This differs from [`crate::semantik`], which returns a non-2xx as an
//! error. Deliberately: that service's envelope is four flat fields, and
//! bud's carries what a caller needs in order to recover.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use reqwest::header::{
    HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, LOCATION, USER_AGENT,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::bud::error::{error_from, preflight, Error, Failure};
use crate::bud::retry::{repeatable, RetryPolicy, TransientRetry};
use crate::bud::types::{
    is_false, is_zero_u32, Catalog, Health, PutOutput, ReadOutput, SendInput, SendOutput,
};
use crate::bud::vocab::{PREFIX_AGENT, PREFIX_MESSAGE, PREFIX_THREAD};
use crate::semantik::user_agent;

/// The endpoint a client uses unless told otherwise: the geo-routed apex
/// name, because a base URL compiled into other people's programs is the
/// one string that cannot move.
pub const DEFAULT_BASE_URL: &str = "https://bud.noetive.io";

/// The agent's Noetive key, read by [`Client::from_env`]. The same variable
/// every Noetive SDK reads, because one key reaches every product.
pub const ENV_KEY: &str = "NOETIVE_KEY_SECRET";

/// Points [`Client::from_env`] somewhere other than production. Each
/// product has its own endpoint, so each has its own variable. These two are
/// the whole environment surface; everything else is a builder option.
pub const ENV_BASE_URL: &str = "NOETIVE_BUD_BASE_URL";

/// Bounds reaching the host, on the default transport.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Bounds waiting for a response's headers and, on a stream, for its opening
/// frame, and stops there: it never cuts the stream `wait` and `watch` hold
/// open. Generous, because it is what bounds the handshake of a `wait`, whose
/// own window starts only after the stream opens.
///
/// There is deliberately no whole-request timeout: `wait` holds a response
/// for up to 25 seconds and `watch` for as long as the caller stays
/// attached, and a timeout covering the body would sever exactly those.
pub const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(45);

/// Bounds a reply: generous, because a rendered thread is legitimately
/// large, and bounded, so a misbehaving proxy does not decide how much this
/// process allocates.
const MAX_RESPONSE_BYTES: usize = 32 << 20;

pub(crate) const MIME_JSON: &str = "application/json";
pub(crate) const MIME_EVENT_STREAM: &str = "text/event-stream";

/// A client for one Bud deployment.
///
/// Cheap to clone and safe to share across tasks. It holds a base URL, a
/// transport and at most one credential; everything per request lives on
/// the request. `Debug` never prints the credential.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
    credential: Credential,
}

struct Inner {
    http: reqwest::Client,
    base_url: String,
    retry: Box<dyn RetryPolicy>,
    response_timeout: Option<Duration>,
}

/// Where a request's `Authorization` comes from.
#[derive(Clone)]
enum Credential {
    /// The client's own key.
    Key(HeaderValue),
    /// A relay's: whatever [`Client::forward`] was given for this call, and
    /// nothing at all when it was given nothing.
    Forwarded(Option<HeaderValue>),
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("base_url", &self.inner.base_url)
            .field("credential", &"[redacted]")
            .finish()
    }
}

impl Client {
    /// A client carrying one Noetive key (`keya_...`). Bud mints no
    /// credential of its own: this key is what an agent authenticates with.
    pub fn new(key: impl Into<String>) -> Result<Self, Failure> {
        Self::builder().key(key).build()
    }

    /// A client from [`ENV_KEY`] (required) and [`ENV_BASE_URL`] (optional).
    pub fn from_env() -> Result<Self, Failure> {
        Self::builder().environment()?.build()
    }

    /// A relay's client: it holds no credential, and takes one per call
    /// through [`Client::forward`].
    pub fn forwarding() -> Result<Self, Failure> {
        Self::builder().forwarding().build()
    }

    /// Start building a client with non-default configuration.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    /// The base URL requests go to, trailing slash trimmed.
    pub fn base_url(&self) -> &str {
        &self.inner.base_url
    }

    /// This relay client, carrying `authorization` — the caller's header
    /// value, verbatim — for the calls made through the returned handle.
    ///
    /// The header belongs to the request being relayed and must not outlive
    /// it, so it lives on a cheap per-call handle rather than on the shared
    /// client. Nothing is validated, parsed or cached. An empty value sends
    /// no `Authorization` at all, and the server answers its own
    /// unauthorized envelope — the one the caller should relay, rather than
    /// a refusal manufactured here in a shape bud never produces.
    ///
    /// Refused on a client that carries its own key: forwarding over it
    /// would silently keep sending the key.
    pub fn forward(&self, authorization: &str) -> Result<Self, Failure> {
        if let Credential::Key(_) = self.credential {
            return Err(preflight(
                "forward needs a client built with Client::forwarding; this one carries its own key",
            )
            .into());
        }
        let header = if authorization.is_empty() {
            None
        } else {
            let mut v = HeaderValue::from_str(authorization).map_err(|_| {
                preflight("the forwarded authorization is not a valid header value")
            })?;
            v.set_sensitive(true);
            Some(v)
        };
        Ok(Self {
            inner: Arc::clone(&self.inner),
            credential: Credential::Forwarded(header),
        })
    }
}

/// Builder for [`Client`]. Construct via [`Client::builder`].
#[derive(Default)]
pub struct ClientBuilder {
    key: Option<String>,
    forwarding: bool,
    base_url: Option<String>,
    env_base_url: Option<String>,
    http_client: Option<reqwest::Client>,
    retry: Option<Box<dyn RetryPolicy>>,
    connect_timeout: Option<Duration>,
    response_timeout: Option<Duration>,
}

impl ClientBuilder {
    /// The Noetive key to send as `Authorization: Bearer <key>`.
    pub fn key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    /// Hold no credential; see [`Client::forward`].
    pub fn forwarding(mut self) -> Self {
        self.forwarding = true;
        self
    }

    /// Read [`ENV_KEY`] and [`ENV_BASE_URL`].
    ///
    /// An explicit [`base_url`](Self::base_url) wins over the environment
    /// whichever is called first: an argument is a decision, the
    /// environment a fallback. Refuses an unset key, and one that is an
    /// unexpanded `${...}` placeholder — an editor launched from a desktop
    /// icon often never reads a shell profile, and sending the placeholder
    /// would come back unauthorized and send somebody to check an account
    /// that is fine.
    pub fn environment(mut self) -> Result<Self, Failure> {
        let key = std::env::var(ENV_KEY).unwrap_or_default();
        let key = key.trim();
        if key.is_empty() {
            return Err(preflight(format!("{ENV_KEY} is not set")).into());
        }
        if key.starts_with("${") && key.ends_with('}') {
            return Err(preflight(format!(
                "{ENV_KEY} contains an unexpanded ${{...}} placeholder rather than a token"
            ))
            .into());
        }
        self.key = Some(key.to_string());
        let base = std::env::var(ENV_BASE_URL).unwrap_or_default();
        if !base.trim().is_empty() {
            self.env_base_url = Some(base.trim().to_string());
        }
        Ok(self)
    }

    /// Point the client at another deployment.
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    /// Replace the transport.
    ///
    /// Do not give it a whole-request `timeout`: it would cut `wait`'s
    /// window and every stream. Build it with
    /// `redirect(reqwest::redirect::Policy::none())`, as the default
    /// transport is — following a redirect re-attaches the credential to a
    /// host the caller never chose. The response timeout still applies only
    /// when set with [`response_timeout`](Self::response_timeout).
    pub fn http_client(mut self, http: reqwest::Client) -> Self {
        self.http_client = Some(http);
        self
    }

    /// Install a retry policy; [`NoRetry`](crate::bud::NoRetry) opts out.
    /// Default: [`TransientRetry::default`], one retry. No policy can make an
    /// unkeyed write repeatable.
    pub fn retry<P: RetryPolicy>(mut self, policy: P) -> Self {
        self.retry = Some(Box::new(policy));
        self
    }

    /// How long to wait to reach the host. Default
    /// [`DEFAULT_CONNECT_TIMEOUT`]; ignored with a custom transport.
    pub fn connect_timeout(mut self, t: Duration) -> Self {
        self.connect_timeout = Some(t);
        self
    }

    /// How long to wait for a response's headers. Default
    /// [`DEFAULT_RESPONSE_TIMEOUT`] on the default transport, none on a
    /// custom one unless set here. A stream's opening frame is bounded by
    /// this, or by the default, on any transport. Never bounds a body.
    pub fn response_timeout(mut self, t: Duration) -> Self {
        self.response_timeout = Some(t);
        self
    }

    /// Finalise the client.
    pub fn build(self) -> Result<Client, Failure> {
        let credential = if self.forwarding {
            Credential::Forwarded(None)
        } else {
            let key = self.key.unwrap_or_default();
            if key.trim().is_empty() {
                return Err(preflight("a key is required; pass one or use forwarding").into());
            }
            let mut v = HeaderValue::from_str(&format!("Bearer {key}"))
                .map_err(|_| preflight("the key is not a valid header value"))?;
            v.set_sensitive(true);
            Credential::Key(v)
        };

        let base_url = self
            .base_url
            .or(self.env_base_url)
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();

        let (http, default_response_timeout) = match self.http_client {
            Some(h) => (h, None),
            None => {
                let http = reqwest::Client::builder()
                    .connect_timeout(self.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT))
                    .pool_idle_timeout(Duration::from_secs(90))
                    // A redirect would re-attach the credential to whatever
                    // Location names, a host the caller never chose.
                    .redirect(reqwest::redirect::Policy::none())
                    .build()?;
                (http, Some(DEFAULT_RESPONSE_TIMEOUT))
            }
        };

        Ok(Client {
            inner: Arc::new(Inner {
                http,
                base_url,
                retry: self
                    .retry
                    .unwrap_or_else(|| Box::new(TransientRetry::default())),
                response_timeout: self.response_timeout.or(default_response_timeout),
            }),
            credential,
        })
    }
}

// --- The requests ----------------------------------------------------------

/// Names one object by its self-describing identifier.
///
/// One field, because the identifier's prefix says which kind it is. Shared
/// by the describe operations, and each takes only its own fields: a field
/// another operation reads — `max_chars` on `describe_thread`, `mode` on
/// `describe_message` — is refused as invalid. `part` is ignored outside
/// `describe_part`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ById {
    pub id: String,

    /// Bounds `describe_message`'s rendering: at most, and by default,
    /// 20000. A longer one comes back truncated with a cursor. Ignored with
    /// `RENDER_RAW`.
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub max_chars: u32,

    /// How `describe_message` renders: `RENDER_TEXT`, `RENDER_RAW` or
    /// `RENDER_PARTS`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub render: String,

    /// Continues a truncated rendering.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub cursor: String,

    /// One part of a message, by the number a `RENDER_PARTS` reading lists
    /// it under. Only `describe_part` reads it.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub part: String,

    /// How `describe_part` returns the part: `PART_MODE_TEXT` or
    /// `PART_MODE_BYTES`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub mode: String,

    /// How many of a thread's most recent messages `describe_thread`
    /// renders: 20 by default, at most 50.
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub max_messages: u32,
}

/// Names a collection by the identifier of whatever holds it, with typed
/// filters: a schema a caller can see is the difference between a filter
/// that works and one that is silently ignored.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct In {
    /// The mailbox, by its agent's identifier. Sent as `in`.
    #[serde(rename = "in")]
    pub mailbox: String,

    /// `FOLDER_INBOX` (the default), `FOLDER_SENT` or `FOLDER_QUARANTINE`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub folder: String,

    /// Only unread messages. False is no filter.
    #[serde(skip_serializing_if = "is_false")]
    pub unread: bool,

    /// Only messages carrying a machine-readable part. False is no filter.
    #[serde(skip_serializing_if = "is_false")]
    pub agent: bool,

    /// Bound the message date, RFC 3339; `since` is inclusive. Anything
    /// else is refused rather than read as a different window.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub since: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub before: String,

    #[serde(skip_serializing_if = "String::is_empty")]
    pub from: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub thread: String,

    /// Narrows correspondents to addresses containing it, ignoring case.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub q: String,

    /// How many entries to examine for this page, not how many match: 50 by
    /// default, at most 200. With a filter a page can come back short or
    /// empty and still carry a cursor; keep paging until there is none.
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub limit: u32,

    /// Resumes a listing. Pass back unchanged the one the previous page of
    /// the same operation returned.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub cursor: String,
}

/// Writes to an existing object.
///
/// Not compare-and-swap: the last write wins. A body carrying a version is
/// refused as invalid at `/version`, which is why there is no field for one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Change {
    pub id: String,

    /// `{"read": bool, "labels": [string]}`, at least one. Labels replace
    /// the message's labels whole, so an empty list removes them all.
    pub changes: serde_json::Value,

    /// Remembered on a best-effort basis for up to an hour. Reusing one for
    /// a different change is refused as invalid.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub idempotency_key: String,
}

// --- The operations --------------------------------------------------------

impl Client {
    /// Who this caller is and what it can reach. The first call worth
    /// making: every reference needs an identifier, and this is where they
    /// come from. Content decodes into [`Me`](crate::bud::Me).
    pub async fn describe_me(&self) -> Result<ReadOutput, Failure> {
        self.call("me.describe", &Empty {}, "").await
    }

    /// Whether this credential is accepted, and as which agent. Smaller than
    /// [`describe_me`](Self::describe_me) on purpose: it answers the one
    /// question a probe asks.
    pub async fn health(&self) -> Result<Health, Failure> {
        self.call("health", &Empty {}, "").await
    }

    /// The operations this deployment serves and the shapes beneath them.
    /// Worth one call at startup: it says which operations this build
    /// implements, which is otherwise discovered as a refusal mid-task.
    pub async fn describe_catalog(&self) -> Result<Catalog, Failure> {
        self.call("catalog.describe", &Empty {}, "").await
    }

    /// A folder's messages, oldest first, in the order they were filed. The
    /// summaries carry subject, sender, provenance and unread, so triage is
    /// one call. Content decodes into [`Listing`](crate::bud::Listing).
    pub async fn list_folder(&self, input: In) -> Result<ReadOutput, Failure> {
        require_id(&input.mailbox, PREFIX_AGENT, "list_folder")?;
        self.call("folder.list", &input, "").await
    }

    /// One message, rendered and structured. With `RENDER_PARTS`, content
    /// decodes into [`PartMap`](crate::bud::PartMap).
    pub async fn describe_message(&self, input: ById) -> Result<ReadOutput, Failure> {
        require_id(&input.id, PREFIX_MESSAGE, "describe_message")?;
        self.call("message.describe", &input, "").await
    }

    /// A conversation's most recent messages, in the order they were sent,
    /// each cut at 4000 characters. Truncated means older messages were left
    /// out, not that one was cut. Content decodes into
    /// [`Conversation`](crate::bud::Conversation).
    pub async fn describe_thread(&self, input: ById) -> Result<ReadOutput, Failure> {
        require_id(&input.id, PREFIX_THREAD, "describe_thread")?;
        self.call("thread.describe", &input, "").await
    }

    /// One part of a message: an attachment, or a body alternative.
    ///
    /// `PART_MODE_TEXT`, the default, renders a part that is already text
    /// into `text` under the same banner as a message body; any other type
    /// is refused as unavailable in that mode. At most the first 2 MiB is
    /// read, and a longer part comes back truncated with no cursor.
    /// `PART_MODE_BYTES` puts the whole content in
    /// [`PartView::bytes`](crate::bud::PartView); a part too large to return
    /// whole is refused as policy_refused, guard `part_size`, rather than
    /// cut. A message withheld for malware refuses every part, guard
    /// `virus`.
    pub async fn describe_part(&self, input: ById) -> Result<ReadOutput, Failure> {
        require_id(&input.id, PREFIX_MESSAGE, "describe_part")?;
        if input.part.is_empty() {
            return Err(preflight("describe_part needs a part").into());
        }
        self.call("part.describe", &input, "").await
    }

    /// A mailbox's own state: what waits in each folder, whether it may
    /// send, and what is left of its limits. Named by the agent's
    /// identifier, since an agent and its mailbox are one. Content decodes
    /// into [`MailboxView`](crate::bud::MailboxView).
    pub async fn describe_mailbox(&self, input: ById) -> Result<ReadOutput, Failure> {
        require_id(&input.id, PREFIX_AGENT, "describe_mailbox")?;
        self.call("mailbox.describe", &input, "").await
    }

    /// Who a mailbox has exchanged mail with. `q`, `limit` and `cursor` are
    /// the filters it accepts; the server ignores `folder` and refuses the
    /// rest. Content decodes into
    /// [`Correspondents`](crate::bud::Correspondents).
    pub async fn list_correspondents(&self, input: In) -> Result<ReadOutput, Failure> {
        require_id(&input.mailbox, PREFIX_AGENT, "list_correspondents")?;
        self.call("correspondent.list", &input, "").await
    }

    /// Mark a message read or unread, or replace its labels.
    ///
    /// A precondition_failed means other label writes kept landing first. A
    /// read change in the same request may already have applied; the request
    /// is safe to repeat as it was. Retried on a failed connection only with
    /// an idempotency key.
    pub async fn update_message(&self, input: Change) -> Result<PutOutput, Failure> {
        require_id(&input.id, PREFIX_MESSAGE, "update_message")?;
        if input.changes.is_null() {
            return Err(preflight("update_message needs changes").into());
        }
        self.call("message.update", &input, &input.idempotency_key)
            .await
    }

    /// Compose and send a message.
    ///
    /// The server either queues it or refuses it, naming the guard; nothing
    /// waits for approval. Pass an idempotency key on every send: without
    /// one, a timeout retried is a second message, so this client never
    /// retries a send without one. A repeat with the same key returns the
    /// first send's result for seven days and sends nothing; one arriving
    /// while the first is still running is refused as rate_limited with a
    /// wait.
    pub async fn send(&self, input: SendInput) -> Result<SendOutput, Failure> {
        if input.to.is_empty()
            && input.cc.is_empty()
            && input.bcc.is_empty()
            && input.in_reply_to.is_empty()
        {
            return Err(preflight(
                "send needs a recipient in to, cc or bcc, or in_reply_to for a reply",
            )
            .into());
        }
        self.call("send", &input, &input.idempotency_key).await
    }
}

/// The body of an operation that takes no arguments.
#[derive(Serialize)]
struct Empty {}

/// Refuse an identifier of the wrong kind before sending.
///
/// The prefix is the kind, so this is checkable here and turns a round trip
/// into an immediate mistake. The rest is the server parser's job;
/// duplicating it would be a second grammar.
fn require_id(id: &str, prefix: &str, op: &str) -> Result<(), Failure> {
    if id.is_empty() {
        return Err(preflight(format!("{op} needs an id")).into());
    }
    if !id.starts_with(prefix) {
        return Err(preflight(format!(
            "{op} needs an identifier beginning {prefix:?}, got {:?}",
            first_prefix(id)
        ))
        .into());
    }
    Ok(())
}

/// An identifier up to and including its underscore, so a refusal names
/// what arrived without quoting the whole thing.
fn first_prefix(id: &str) -> String {
    match id.find('_') {
        Some(i) => id[..=i].to_string(),
        None => id.chars().take(16).collect(),
    }
}

// --- Plumbing --------------------------------------------------------------

/// The outputs that carry a refusal in their envelope.
pub(crate) trait Envelope: DeserializeOwned {
    fn refusal(&mut self) -> Option<&mut Error>;
}

macro_rules! envelope {
    ($($ty:ty),*) => {
        $(impl Envelope for $ty {
            fn refusal(&mut self) -> Option<&mut Error> {
                self.error.as_mut()
            }
        })*
    };
}

envelope!(ReadOutput, PutOutput, SendOutput, Health, Catalog);

impl Client {
    /// Send one operation and decode its envelope.
    async fn call<I: Serialize, O: Envelope>(
        &self,
        op: &'static str,
        input: &I,
        idempotency_key: &str,
    ) -> Result<O, Failure> {
        let body = encode(input)?;
        let resp = self
            .post_with_retry(op, body, None, idempotency_key)
            .await?;
        let status = resp.status().as_u16();
        let request_id = request_id_of(resp.headers());
        let raw = read_capped(resp, MAX_RESPONSE_BYTES).await?;
        decode_envelope(status, &raw, &request_id)
    }

    /// POST `body` to `/v1/{op}`, retrying under the client's policy when —
    /// and only when — the connection failed before any response arrived,
    /// and the idempotency gate allows it. A response of any status is an
    /// answer and is returned as it is.
    pub(crate) async fn post_with_retry(
        &self,
        op: &'static str,
        body: Bytes,
        accept: Option<&'static str>,
        idempotency_key: &str,
    ) -> Result<reqwest::Response, Failure> {
        let gate = repeatable(op, idempotency_key);
        let mut attempt: u32 = 0;
        loop {
            match self.post(op, body.clone(), accept).await {
                Ok(resp) => {
                    // Checked here, the one place every response passes, so no
                    // operation can follow a redirect the others refuse.
                    if let Some(refusal) = refused_redirect(&resp) {
                        return Err(refusal);
                    }
                    return Ok(resp);
                }
                Err(failure) => {
                    let connection_failed =
                        matches!(failure, Failure::Transport(_) | Failure::Timeout(_));
                    if !(gate && connection_failed && self.inner.retry.should_retry(attempt, op)) {
                        return Err(failure);
                    }
                    tokio::time::sleep(self.inner.retry.backoff(attempt)).await;
                    attempt += 1;
                }
            }
        }
    }

    /// What bounds the wait for a stream's opening frame once its headers
    /// arrived: the response timeout, or its default on a custom transport,
    /// which bounds the headers itself but not the frame after them.
    pub(crate) fn open_timeout(&self) -> Duration {
        self.inner
            .response_timeout
            .unwrap_or(DEFAULT_RESPONSE_TIMEOUT)
    }

    /// One request, up to its response headers.
    async fn post(
        &self,
        op: &str,
        body: Bytes,
        accept: Option<&'static str>,
    ) -> Result<reqwest::Response, Failure> {
        let url = format!("{}/v1/{op}", self.inner.base_url);
        let mut req = self
            .inner
            .http
            .post(url)
            .header(CONTENT_TYPE, MIME_JSON)
            .header(USER_AGENT, user_agent())
            .body(body);
        if let Some(accept) = accept {
            req = req.header(ACCEPT, accept);
        }
        let authorization = match &self.credential {
            Credential::Key(v) => Some(v),
            Credential::Forwarded(v) => v.as_ref(),
        };
        if let Some(v) = authorization {
            req = req.header(AUTHORIZATION, v.clone());
        }

        let sent = match self.inner.response_timeout {
            Some(limit) => tokio::time::timeout(limit, req.send())
                .await
                .map_err(|_| Failure::Timeout(limit))?,
            None => req.send().await,
        };
        sent.map_err(|e| {
            if e.is_builder() {
                // Not a connection that failed: a request that could not be
                // built, which no retry will mend.
                Failure::from(preflight(format!("the request could not be built: {e}")))
            } else {
                Failure::Transport(e)
            }
        })
    }
}

/// A 3xx, named by the host it pointed at. Only the default transport stops
/// at a redirect; one that follows it never shows this crate a 3xx.
fn refused_redirect(resp: &reqwest::Response) -> Option<Failure> {
    let status = resp.status();
    if !status.is_redirection() {
        return None;
    }
    let target = resp
        .headers()
        .get(LOCATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|location| resp.url().join(location).ok());
    let host = match target.as_ref().map(|u| (u.host_str(), u.port())) {
        Some((Some(host), Some(port))) => format!("{host}:{port}"),
        Some((Some(host), None)) => host.to_string(),
        _ => "an unnamed host".to_string(),
    };
    Some(Failure::Redirected {
        status: status.as_u16(),
        host,
    })
}

pub(crate) fn encode<I: Serialize>(input: &I) -> Result<Bytes, Failure> {
    serde_json::to_vec(input)
        .map(Bytes::from)
        .map_err(|e| preflight(format!("the request could not be encoded: {e}")).into())
}

pub(crate) fn request_id_of(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// Read a body up to `cap` bytes. Past the cap the body is cut, and what
/// remains does not decode, so an oversized answer surfaces as malformed.
pub(crate) async fn read_capped(
    mut resp: reqwest::Response,
    cap: usize,
) -> Result<Vec<u8>, Failure> {
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        let room = cap - buf.len();
        if chunk.len() >= room {
            buf.extend_from_slice(&chunk[..room]);
            break;
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Read the operation's own output out of a response.
///
/// An empty or undecodable body is not the envelope (a load balancer's HTML 502, a
/// proxy's plain-text 413). A refusal is a value: everything the server
/// said is already in it, and this fills only what the envelope cannot
/// carry. A success at a failing status cannot be believed — either the
/// body or the status is wrong, and guessing which would hide a real
/// disagreement.
pub(crate) fn decode_envelope<O: Envelope>(
    status: u16,
    raw: &[u8],
    request_id: &str,
) -> Result<O, Failure> {
    if raw.iter().all(u8::is_ascii_whitespace) {
        return Err(error_from(status, raw, request_id).into());
    }
    let Ok(mut out) = serde_json::from_slice::<O>(raw) else {
        return Err(error_from(status, raw, request_id).into());
    };
    if let Some(refusal) = out.refusal() {
        refusal.http_status = status;
        if refusal.request_id.is_empty() {
            refusal.request_id = request_id.to_string();
        }
        return Ok(out);
    }
    if !(200..=299).contains(&status) {
        return Err(error_from(status, raw, request_id).into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bud::error::Code;

    fn code_of<O: Envelope + fmt::Debug>(r: Result<O, Failure>) -> Code {
        r.unwrap_err().code().cloned().expect("a refusal")
    }

    #[test]
    fn decode_envelope_follows_the_rules() {
        // Empty and undecodable bodies are not envelopes.
        assert_eq!(
            code_of(decode_envelope::<ReadOutput>(200, b"  ", "r")),
            Code::MalformedResponse
        );
        assert_eq!(
            code_of(decode_envelope::<ReadOutput>(502, b"<html>", "r")),
            Code::MalformedResponse
        );
        // A bare 401 names the credential.
        assert_eq!(
            code_of(decode_envelope::<ReadOutput>(401, b"", "r")),
            Code::Unauthorized
        );
        // A success body at a failing status cannot be believed.
        assert_eq!(
            code_of(decode_envelope::<ReadOutput>(500, br#"{"kind":"me"}"#, "r")),
            Code::MalformedResponse
        );
        // A refusal is a value, with status and the header's request id.
        let out: ReadOutput = decode_envelope(
            403,
            br#"{"error":{"code":"forbidden_scope","message":"m"}}"#,
            "r1",
        )
        .unwrap();
        let e = out.error.unwrap();
        assert_eq!((e.http_status, e.request_id.as_str()), (403, "r1"));
        // The body's own request id is kept.
        let out: ReadOutput = decode_envelope(
            403,
            br#"{"error":{"code":"forbidden_scope","request_id":"body"}}"#,
            "header",
        )
        .unwrap();
        assert_eq!(out.error.unwrap().request_id, "body");
    }

    #[test]
    fn a_client_needs_a_key_or_to_forward() {
        assert!(Client::new("").is_err());
        assert!(Client::new("   ").is_err());
        assert!(Client::forwarding().is_ok());
        let keyed = Client::new("keya_secret").unwrap();
        assert!(keyed.forward("Bearer other").is_err());
        let s = format!("{keyed:?}");
        assert!(!s.contains("keya_secret") && s.contains("redacted"), "{s}");
    }

    #[test]
    fn the_base_url_prefers_an_argument() {
        let c = Client::builder()
            .key("k")
            .base_url("https://example.test/")
            .build()
            .unwrap();
        assert_eq!(c.base_url(), "https://example.test");
        assert_eq!(Client::new("k").unwrap().base_url(), DEFAULT_BASE_URL);
    }

    #[test]
    fn first_prefix_names_the_kind() {
        assert_eq!(first_prefix("thread_01x"), "thread_");
        assert_eq!(
            first_prefix("abcdefghijklmnopqrstuvwxyz"),
            "abcdefghijklmnop"
        );
    }
}

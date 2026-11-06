//! The closed vocabularies, and the banner.
//!
//! Every value here is one the server chose and will keep choosing. They
//! are constants rather than documentation because a caller branches on
//! them, and a string literal at a branch is a typo nothing catches.

// The send states, which `SendOutput::state` names. A new send is queued;
// a repeat with the same idempotency key reports any of these.

/// Durably queued and not yet handed on.
pub const STATE_QUEUED: &str = "queued";
/// It is being handed on.
pub const STATE_SENDING: &str = "sending";
/// The provider accepted it.
pub const STATE_SENT: &str = "sent";
/// The receiving side accepted it.
pub const STATE_DELIVERED: &str = "delivered";
/// The receiving side refused it for good.
pub const STATE_BOUNCED: &str = "bounced";
/// It could not be handed on; `reason` says why.
pub const STATE_FAILED: &str = "failed";
/// A rule of the workspace's policy needs a person to approve the message
/// before it goes; `SendOutput::approval_id` names the approval. It becomes
/// `STATE_QUEUED` when approved and `STATE_REJECTED` when a person refuses
/// it or the wait lapses. Nothing a caller sends releases it, so never send
/// it again: that is a second message, held in its turn.
pub const STATE_HELD: &str = "held";
/// A person refused a held message, or the wait for approval lapsed.
pub const STATE_REJECTED: &str = "rejected";

// The event types, which `JournalEvent::kind` names and
// `WaitInput::types` filters on.

/// Mail arriving.
pub const EVENT_RECEIVED: &str = "mail.received";
/// Mail arriving and filed in quarantine.
pub const EVENT_QUARANTINED: &str = "mail.quarantined";
/// A send was accepted.
pub const EVENT_QUEUED: &str = "mail.queued";
/// A send was held for a person to approve.
pub const EVENT_HELD: &str = "mail.held";
/// A person approved a held send.
pub const EVENT_APPROVED: &str = "mail.approved";
/// A held send was refused or lapsed; its sending budget is given back.
pub const EVENT_REJECTED: &str = "mail.rejected";
/// A send was handed to the provider.
pub const EVENT_SENT: &str = "mail.sent";
/// A send was accepted by the receiving side.
pub const EVENT_DELIVERED: &str = "mail.delivered";
/// A send was refused for good.
pub const EVENT_BOUNCED: &str = "mail.bounced";
/// A send was reported as unwanted.
pub const EVENT_COMPLAINED: &str = "mail.complained";
/// A message was marked read or unread.
pub const EVENT_READ: &str = "mail.read";
/// A rendering was withheld as unsafe and a degraded one shown instead.
pub const EVENT_RENDER_FAILED: &str = "render.failed";

// The actor kinds, which `Actor::kind` names.

/// The service itself.
pub const ACTOR_SYSTEM: &str = "system";
/// An agent; `Actor::agent` names which.
pub const ACTOR_AGENT: &str = "agent";
/// A person operating the service.
pub const ACTOR_OPERATOR: &str = "operator";

// The folders, which `In::folder` names.

/// The default folder.
pub const FOLDER_INBOX: &str = "inbox";
/// What this mailbox sent.
pub const FOLDER_SENT: &str = "sent";
/// Mail filed apart on arrival.
pub const FOLDER_QUARANTINE: &str = "quarantine";

// How a message joined its conversation, which `Provenance::thread_join`
// names.

/// It started one.
pub const JOIN_NEW: &str = "new";
/// A reply from someone the conversation already involves.
pub const JOIN_VERIFIED: &str = "verified";
/// It only claims to be a reply.
pub const JOIN_CLAIMED: &str = "claimed";

// The sending statuses, which `SendingStatus::status` names.

/// Mail goes out now.
pub const SENDING_READY: &str = "ready";
/// The sending domain is not ready yet. Mail is accepted and queued, and
/// goes out once it is.
pub const SENDING_PROVISIONING: &str = "provisioning";
/// An operator stopped sending. A send is refused until it is released.
pub const SENDING_PAUSED: &str = "paused";

// The modes `describe_part` reads a part in.

/// Renders a part that is already text into `ReadOutput::text`. The
/// default; any other type is refused as unavailable in this mode.
pub const PART_MODE_TEXT: &str = "text";
/// Returns the part's content in `PartView::bytes`. A part larger than the
/// server's cap is refused with the guard that fired.
pub const PART_MODE_BYTES: &str = "bytes";

// The renderings `describe_message` offers, which `ById::render` names.

/// The safe rendering, under a banner. The default.
pub const RENDER_TEXT: &str = "text";
/// The original message as it arrived.
pub const RENDER_RAW: &str = "raw";
/// The message's structure, with the part numbers `describe_part` takes.
pub const RENDER_PARTS: &str = "parts";

// The object kinds a read can return, which `ReadOutput::kind` names.

/// Who the caller is.
pub const KIND_ME: &str = "me";
/// The grammar; its text is the whole answer.
pub const KIND_HELP: &str = "help";
/// A mailbox's own state.
pub const KIND_MAILBOX: &str = "mailbox";
/// A folder listing.
pub const KIND_FOLDER: &str = "folder";
/// Correspondents.
pub const KIND_CORR: &str = "corr";
/// The workspace addressbook.
pub const KIND_ADDRESSBOOK: &str = "addressbook";
/// One message.
pub const KIND_MESSAGE: &str = "message";
/// One part of a message.
pub const KIND_PART: &str = "part";
/// One conversation.
pub const KIND_THREAD: &str = "thread";

// The identifier prefixes. An identifier is self-describing: the prefix
// names the kind, so one opaque string is the only argument a read or a
// write needs. Exposed so a caller can check a string before spending a
// round trip on it.

/// A workspace.
pub const PREFIX_WORKSPACE: &str = "workspace_";
/// An agent, and its mailbox: the two are one identifier, not a pair that
/// could disagree.
pub const PREFIX_AGENT: &str = "ag_";
/// A message.
pub const PREFIX_MESSAGE: &str = "message_";
/// A thread.
pub const PREFIX_THREAD: &str = "thread_";
/// A journal event.
pub const PREFIX_JOURNAL: &str = "journal_";
/// A request.
pub const PREFIX_REQUEST: &str = "request_";

// The effect kinds a write reports.

/// A message marked read or unread.
pub const EFFECT_MAIL_READ: &str = "mail.read";
/// A message whose labels were set.
pub const EFFECT_LABELLED: &str = "labelled";

/// Begins the rendering of anything carrying content somebody else wrote.
///
/// The full line names the object and says, in the server's own voice, that
/// what follows is data rather than instructions. A mail server cannot stop
/// a message containing "ignore your previous instructions"; the defence is
/// to make the provenance impossible to miss.
pub const BANNER_PREFIX: &str = "[bud message ";

/// Whether a rendering carries its provenance banner.
///
/// Worth checking rather than assuming: the banner is what makes
/// `ReadOutput::text` safe to put in a model's context, and a relay that
/// dropped it would hand a stranger's words to a model with nothing marking
/// them as a stranger's. Gate on this and fall back to the structured
/// content otherwise.
pub fn has_banner(text: &str) -> bool {
    text.starts_with(BANNER_PREFIX)
}

/// Whether a kind's rendering can contain text somebody else wrote, and
/// therefore must carry a banner. A listing is the server's own table and
/// does not; a message, a thread and a part do.
pub fn carries_sender_content(kind: &str) -> bool {
    matches!(kind, KIND_MESSAGE | KIND_THREAD | KIND_PART)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_banner_marks_sender_content() {
        assert!(has_banner(
            "[bud message message_01x from a@b] data follows"
        ));
        assert!(!has_banner("hello [bud message "));
        assert!(carries_sender_content(KIND_PART));
        assert!(!carries_sender_content(KIND_FOLDER));
    }
}

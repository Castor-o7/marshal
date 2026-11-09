//! `MessageOffer` — a direct message handed to a session that hasn't yet
//! shown it reached the model (MAR-82, designed in docs/two-phase-delivery.md).
//!
//! A Claude session's background `claude-hook wait` hands mail to Claude Code,
//! which holds it until the running turn ends. Marking it read at that point
//! lost it whenever the session ended first. So the waiter acks `offered`,
//! which writes this row and no `MessageRead`; the session's next hooks look
//! for the message in its transcript and ack `seen`, which writes the
//! `MessageRead` and removes this row.
//!
//! While an offer stands, the inbox doesn't hand that message to that session
//! again, until it's older than the offer timeout (6 h), when it's offered
//! afresh. The message stays unread throughout, so a successor gets it by
//! MAR-42 like any unread mail, and a sender sees it as not yet read.
//!
//! Composite id is `"<message_id>::<session_id>"`, as `MessageRead`'s, so
//! offering again replaces the row.

use myko::myko_item;
use myko::prelude::{EventPublishing as _, Querying as _, RegistryScoped as _, RequestScoped as _};

use crate::message::Message;

#[myko_item]
pub struct MessageOffer {
    /// Dies with its message, like a `MessageRead`.
    #[belongs_to(Message)]
    pub message_id: crate::message::MessageId,

    /// Not a cascade on the session, for the reason `MessageRead::session_id`
    /// gives: a session row is DEL'd and SET again on a reconnect.
    pub session_id: crate::session::SessionId,

    /// Wall-clock millis when the waiter handed the message on.
    pub offered_at: i64,
}

impl MessageOffer {
    /// The composite id for a (message, session) pair.
    pub fn make_id(message_id: &str, session_id: &str) -> String {
        format!("{message_id}::{session_id}")
    }
}

/// How long an offer stands before its message is offered again (MAR-82):
/// Josh's 6 h, overridable with `MARSHAL_OFFER_SECS` for tests.
pub fn offer_timeout_ms() -> i64 {
    std::env::var("MARSHAL_OFFER_SECS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(6 * 60 * 60)
        * 1000
}

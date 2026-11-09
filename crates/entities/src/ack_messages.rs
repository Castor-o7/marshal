//! `AckMessages` — write tool that marks messages read for the calling
//! session.
//!
//! Split from `ReadMessages` so the read path stays a pure resource
//! (no side effects) and the mutation lives in its own tool. Idempotent:
//! re-acking a message that's already been acked is a no-op.

use std::sync::Arc;

use chrono::Utc;
use myko::prelude::EventPublishing as _;
use myko::{
    command::{CommandContext, CommandError, CommandHandler},
    myko_command,
};
use serde::{Deserialize, Serialize};

use crate::{
    message::MessageId,
    message_offer::{GetAllMessageOffers, MessageOffer, MessageOfferId},
    message_read::{GetAllMessageReads, MessageRead, MessageReadId},
    session::{GetAllSessions, Session, SessionId, resolve_caller},
};

#[myko_command(AckMessagesResult)]
pub struct AckMessages {
    /// Messages to mark read. Order doesn't matter; missing/unknown
    /// ids are silently skipped (idempotent — re-acking is fine).
    pub message_ids: Vec<MessageId>,

    /// Self-identified caller for connectionless paths (HTTP-MCP agents,
    /// the daemon's `/hook/*` handlers acking on a session's behalf).
    /// WS shim callers omit it. See `resolve_caller`.
    #[serde(default, rename = "asSession", skip_serializing_if = "Option::is_none")]
    pub as_session: Option<SessionId>,

    /// `"offered"`: the caller handed the mail on, but can't yet say the model
    /// has it (a Claude `asyncRewake` waiter), so record a `MessageOffer` and
    /// leave the message unread (MAR-82). Absent or `"seen"`: mark it read, as
    /// before, and drop any offer for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
}

/// `AckMessages::phase` for mail handed on but not yet seen (MAR-82).
pub const ACK_OFFERED: &str = "offered";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(myko::TS), ts(export))]
pub struct AckMessagesResult {
    /// Number of `MessageRead` rows newly written. Re-acks of already-read
    /// messages don't count here.
    pub newly_acked: u32,
    /// Number of input ids that were already acked before this call.
    pub already_acked: u32,
    /// With `phase: "offered"`, the offers written (MAR-82).
    #[serde(default)]
    pub newly_offered: u32,
}

impl CommandHandler for AckMessages {
    #[cfg(target_arch = "wasm32")]
    fn execute(self, _ctx: CommandContext) -> Result<Self::Result, CommandError> {
        unreachable!("AckMessages::execute is server-only");
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn execute(self, ctx: CommandContext) -> Result<Self::Result, CommandError> {
        let sessions: Vec<Arc<Session>> = ctx.exec_query(GetAllSessions {})?;
        let me = resolve_caller(&ctx, &sessions, self.as_session.as_ref())?;

        let reads: Vec<Arc<MessageRead>> = ctx.exec_query(GetAllMessageReads {})?;
        let already: std::collections::HashSet<MessageId> = reads
            .iter()
            .filter(|r| r.session_id == me.id)
            .map(|r| r.message_id.clone())
            .collect();

        let now = Utc::now().timestamp_millis();
        if self.phase.as_deref() == Some(ACK_OFFERED) {
            let mut newly_offered = 0u32;
            let mut already_acked = 0u32;
            for message_id in &self.message_ids {
                if already.contains(message_id) {
                    already_acked += 1;
                    continue;
                }
                let id_str = MessageOffer::make_id(message_id.0.as_ref(), me.id.0.as_ref());
                ctx.emit_set(&MessageOffer {
                    id: MessageOfferId(Arc::from(id_str.as_str())),
                    message_id: message_id.clone(),
                    session_id: me.id.clone(),
                    offered_at: now,
                })?;
                newly_offered += 1;
            }
            return Ok(AckMessagesResult {
                newly_acked: 0,
                already_acked,
                newly_offered,
            });
        }
        let acked: std::collections::HashSet<&MessageId> = self.message_ids.iter().collect();
        let offers: Vec<Arc<MessageOffer>> = ctx.exec_query(GetAllMessageOffers {})?;
        for offer in offers
            .iter()
            .filter(|o| o.session_id == me.id && acked.contains(&o.message_id))
        {
            ctx.emit_del(&**offer)?;
        }
        let mut newly_acked = 0u32;
        let mut already_acked = 0u32;
        for message_id in &self.message_ids {
            if already.contains(message_id) {
                already_acked += 1;
                continue;
            }
            let id_str = MessageRead::make_id(message_id.0.as_ref(), me.id.0.as_ref());
            ctx.emit_set(&MessageRead {
                id: MessageReadId(Arc::from(id_str.as_str())),
                message_id: message_id.clone(),
                session_id: me.id.clone(),
                read_at: now,
            })?;
            newly_acked += 1;
        }

        Ok(AckMessagesResult {
            newly_acked,
            already_acked,
            newly_offered: 0,
        })
    }
}

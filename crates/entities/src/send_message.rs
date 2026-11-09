//! Server-handled `SendMessage` command.
//!
//! Persistence is primary; live push is a best-effort overlay. The pull
//! model decouples acceptance from delivery — the recipient reads on its
//! next turn via the hook, online or not. The handler:
//!   1. resolves the sender — `asSession` for connectionless HTTP-MCP
//!      callers, or the WS `client_id` for shim callers (`resolve_caller`),
//!   2. validates the recipient `Session` exists on the roster (it need
//!      NOT be online),
//!   3. persists the `Message` SET unconditionally,
//!   4. if the recipient has a live WS client, best-effort dispatches a
//!      `NotifyChannel` to surface it immediately (the Track-2 live path);
//!      otherwise the recipient pulls it next turn,
//!   5. returns separate status for the synchronous live-channel push and
//!      asynchronous wake boundary. The daemon cannot observe a later Codex
//!      bridge `turn/start`, so it never claims that wake succeeded.
//!
//! Only an unresolvable sender or unknown recipient returns a
//! `CommandError`; an offline recipient is success.

use std::sync::Arc;

use chrono::Utc;
use myko::prelude::{EventPublishing as _, RequestScoped as _};
use myko::{
    command::{CommandContext, CommandError, CommandHandler},
    myko_command,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    message::{CONTEXT_BODY_MAX_CHARS, GetMessagesByIds, Message, MessageId, context_preview},
    message_read::{MessageRead, MessageReadId},
    notify::NotifyChannel,
    session::{GetAllSessions, Session, SessionId, resolve_caller},
    wake_watcher::{GetAllWakeWatchers, WakeWatcher},
};

/// Namespace for message ids derived from an idempotency key. Marshal's own,
/// so these ids can't coincide with those of any other UUIDv5 scheme.
const IDEMPOTENT_ID_NAMESPACE: Uuid = Uuid::from_u128(0x7a4776dd_d70f_405c_898c_8432b782d05f);

/// Longest accepted idempotency key. The shim sends a 36-character UUID.
const IDEMPOTENCY_KEY_MAX_BYTES: usize = 128;

/// Held by a keyed send from looking up its first attempt until it has stored
/// its own message. See `SendMessage::execute`.
#[cfg(not(target_arch = "wasm32"))]
static KEYED_SENDS: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[myko_command(SendMessageResult)]
pub struct SendMessage {
    pub to_session_id: SessionId,
    #[serde(default)]
    pub response_required: bool,

    pub body: String,

    /// Self-identified sender, for callers with no connection identity —
    /// an agent calling this tool over stock myko's HTTP-MCP transport
    /// (no WS `client_id`). The id is the agent's own `session_id`, which
    /// it learned from its SessionStart hook output. WS shim callers omit
    /// this; the server derives the sender from the live connection.
    #[serde(default, rename = "asSession", skip_serializing_if = "Option::is_none")]
    pub as_session: Option<SessionId>,

    /// Makes a retry safe. Sending again with the key of a send whose outcome
    /// wasn't seen, such as one that timed out, stores and delivers nothing new.
    #[serde(
        default,
        rename = "idempotencyKey",
        skip_serializing_if = "Option::is_none"
    )]
    pub idempotency_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(myko::TS), ts(export))]
pub struct SendMessageResult {
    pub message_id: MessageId,
    /// Recipient's session id, echoed back so callers can confirm what
    /// was actually addressed (the daemon's caller resolution can
    /// disambiguate by host+cwd in edge cases the client couldn't see).
    pub to_session_id: SessionId,
    pub sent_at: i64,
    /// Outcome of the synchronous `NotifyChannel` path. This says nothing
    /// about a later wake bridge attempt.
    #[serde(default)]
    pub live_push: LivePushStatus,
    /// Whether this synchronous response observed an asynchronous wake.
    ///
    /// A durable unread message may be noticed by a Codex bridge after this
    /// command returns. Marshal currently has no bridge acknowledgement path,
    /// so an inbox result is `Unobserved`, never a false claim of wake failure.
    #[serde(default)]
    pub wake: WakeStatus,
    /// Compatibility alias for clients predating `live_push`.
    ///
    /// This is `true` exactly when `live_push == Delivered`. It describes only
    /// the synchronous channel push; it does not describe asynchronous wake.
    #[serde(default)]
    pub delivered_live: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(myko::TS), ts(export))]
pub enum LivePushStatus {
    /// Compatibility default when a newer client reads an older daemon result.
    #[default]
    Unknown,
    /// The recipient's live channel accepted the push.
    Delivered,
    /// The recipient had no render-capable live channel, so no push was tried.
    Unavailable,
    /// A render-capable live channel was advertised, but dispatch failed.
    Failed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(myko::TS), ts(export))]
pub enum WakeStatus {
    /// The live push landed. The durable inbox copy is marked read then, or,
    /// for a recipient that acks its own pushes, when it acks.
    NotNeeded,
    /// No live push landed, but a waiter holds `/hook/wait` for the recipient,
    /// so the waiter will hand it the message and start a turn: the roster's
    /// `wake: waiting`. States that a path EXISTS, never that a wake happened.
    ///
    /// It exists because `live_push: unavailable` alone can't tell a sender
    /// whether the recipient will wake or will only see the message at its next
    /// turn. A live client alone doesn't count.
    Expected,
    /// Wake handling, if configured, happens after this response and is not
    /// observed by the sender.
    #[default]
    Unobserved,
}

impl SendMessageResult {
    /// Normalize a response from an older daemon that only populated
    /// `delivered_live`.
    pub fn effective_live_push(&self) -> LivePushStatus {
        match (self.live_push, self.delivered_live) {
            (LivePushStatus::Unknown, true) => LivePushStatus::Delivered,
            (status, _) => status,
        }
    }

    /// Normalize wake status across mixed daemon/client versions.
    pub fn effective_wake(&self) -> WakeStatus {
        if self.effective_live_push() == LivePushStatus::Delivered {
            WakeStatus::NotNeeded
        } else {
            self.wake
        }
    }
}

impl CommandHandler for SendMessage {
    #[cfg(target_arch = "wasm32")]
    fn execute(self, _ctx: CommandContext) -> Result<Self::Result, CommandError> {
        // SendMessage requires the live `client_registry`, which only the
        // server build has. Wasm clients never reach this — the macro skips
        // `register_command_handler!` on wasm — so the body is inert.
        unreachable!("SendMessage::execute is server-only");
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn execute(self, ctx: CommandContext) -> Result<Self::Result, CommandError> {
        let sessions: Vec<Arc<Session>> = ctx.exec_query(GetAllSessions {})?;

        let sender = resolve_caller(&ctx, &sessions, self.as_session.as_ref())?;

        // Resolve the recipient token server-side so every harness (Claude via
        // the shim, the opencode plugin, raw HTTP-MCP) shares ONE resolution
        // policy — resolution used to live only in the shim, so plugin/HTTP
        // callers could address by exact id alone. Accepts id / nickname /
        // operator (human-via-agent) / id-prefix; see `resolve_recipient`.
        // The id comes from the sender and key, so a retry finds the first send.
        let id = match &self.idempotency_key {
            Some(key) => {
                if key.trim().is_empty() {
                    return Err(err(
                        &ctx,
                        "`idempotencyKey` is empty; omit it or send a key",
                    ));
                }
                if key.len() > IDEMPOTENCY_KEY_MAX_BYTES {
                    return Err(err(
                        &ctx,
                        &format!(
                            "`idempotencyKey` is longer than {IDEMPOTENCY_KEY_MAX_BYTES} bytes"
                        ),
                    ));
                }
                Uuid::new_v5(
                    &IDEMPOTENT_ID_NAMESPACE,
                    format!("{}\0{key}", sender.id.0).as_bytes(),
                )
            }
            None => Uuid::now_v7(),
        };
        let id = MessageId(Arc::from(id.to_string()));

        // A keyed send looks for its first attempt and stores itself under one
        // lock, so two attempts racing (a retry sent while a timed-out first is
        // still running) can't both find nothing and both store and push. The
        // store insert in `emit_set` is synchronous, so the second attempt sees
        // the first's message. Sends without a key don't take the lock.
        let keyed_guard = match self.idempotency_key {
            Some(_) => {
                let guard = KEYED_SENDS.lock().unwrap_or_else(|e| e.into_inner());
                let found: Vec<Arc<Message>> = ctx.exec_query(GetMessagesByIds {
                    ids: vec![id.clone()],
                })?;
                if let Some(first) = found.first() {
                    // Checked before the recipient is resolved: a retry to a
                    // nickname or operator email can resolve to another session
                    // by now, or to none, and is still the same send. The
                    // stored message doesn't keep the `to` token, so the
                    // recipient isn't compared; the key is the sender's own.
                    if first.body != self.body || first.response_required != self.response_required
                    {
                        return Err(err(
                            &ctx,
                            "this idempotency key was already used for a different message",
                        ));
                    }
                    // Already stored, and delivered or waiting in the inbox. What
                    // happened to its live push wasn't recorded, so don't claim any.
                    return Ok(SendMessageResult {
                        message_id: first.id.clone(),
                        to_session_id: first
                            .to_session_id
                            .clone()
                            .unwrap_or_else(|| self.to_session_id.clone()),
                        sent_at: first.sent_at,
                        live_push: LivePushStatus::Unknown,
                        wake: WakeStatus::Unobserved,
                        delivered_live: false,
                    });
                }
                Some(guard)
            }
            None => None,
        };

        let (recipient, to_operator) =
            resolve_recipient(&ctx, &sessions, self.to_session_id.0.as_ref())?;
        let now = Utc::now().timestamp_millis();
        let msg = Message {
            response_required: self.response_required,
            id,
            from_session_id: sender.id.clone(),
            to_session_id: Some(recipient.id.clone()),
            to_room_id: None,
            to_operator: to_operator.clone(),
            body: self.body.clone(),
            sent_at: now,
            from_kind: sender.kind.clone(),
            from_machine: sender.host.as_ref().map(|h| h.name.clone()),
        };

        // Persist unconditionally: in the pull model the recipient reads
        // this on its next turn via the hook, whether or not it has a live
        // connection right now. Delivery is decoupled from acceptance.
        ctx.emit_set(&msg)?;
        // Stored: a racing retry now finds it. Push without holding the lock.
        drop(keyed_guard);

        // Best-effort live push: only when the recipient has a live WS client
        // AND its session can actually render channel pushes. A flag-off
        // recipient (channels_enabled == Some(false)) has a live client but
        // Claude silently drops `notifications/claude/channel`, so a push is a
        // no-op the daemon would otherwise mis-report as delivered_live=true.
        // Treat it as offline: skip the push, report delivered_live=false, and
        // let it pull from the inbox next turn. `None` (legacy shim that
        // doesn't report channel state) keeps prior best-effort behavior.
        let recipient_can_render = recipient.channels_enabled != Some(false);
        // Surface the sender by its memorable nickname so the recipient sees who
        // it's from at a glance. Read the daemon-assigned, frozen handle
        // (`SessionNickname`); fall back to the computed candidate for the brief
        // window before the assignment saga has run. The full id stays in `meta`
        // for exact reply routing; `from_nickname` lets an agent reply by name.
        let from_nickname = crate::nickname_for(&ctx, sender.id.0.as_ref())?;
        let (context_body, body_truncated) = context_preview(&msg.body, CONTEXT_BODY_MAX_CHARS);
        let live_push = match recipient.client_id.as_ref() {
            Some(cid) if recipient_can_render => {
                if push_to_client(
                    cid.0.as_ref(),
                    // Concise origin ping only — no leading "marshal:" (Claude
                    // already prefixes the channel name, else it reads "marshal:
                    // marshal:") and no body (it would just be a truncated
                    // banner). `meta.body` is a bounded model-context preview;
                    // the persisted Message retains the complete body.
                    direct_push_content(
                        &msg.sender_label(&from_nickname),
                        to_operator.as_deref(),
                        body_truncated.then_some(msg.id.0.as_ref()),
                    ),
                    serde_json::json!({
                        "source": "marshal",
                        "kind": "new_message",
                        "message_id": msg.id.0.as_ref(),
                        "from_session": sender.id.0.as_ref(),
                        "from_nickname": from_nickname,
                        "to_session": recipient.id.0.as_ref(),
                        // Present when addressed to a human via their operator
                        // identity: the receiving agent should surface it to that
                        // operator, not treat it as ordinary peer chatter.
                        "to_operator": to_operator,
                        "response_required": self.response_required,
                        "body": crate::message::push_body(&context_body),
                        "body_truncated": body_truncated,
                        "sent_at": now,
                    }),
                ) {
                    LivePushStatus::Delivered
                } else {
                    LivePushStatus::Failed
                }
            }
            _ => LivePushStatus::Unavailable,
        };
        let delivered_live = live_push == LivePushStatus::Delivered;
        // Report a wake path only when one exists: the same rule as the
        // roster's `wake_state`. A live push that landed needs no wake.
        // Otherwise the message wakes the recipient only through a waiter
        // holding `/hook/wait` (a `WakeWatcher`).
        //
        // A live client alone isn't a wake path. A Claude session's MCP shim
        // without the channels flag is connected but can't wake it, and the
        // roster calls that session `none`; reporting `expected` there told
        // senders a session would wake when it wouldn't. Codex's MCP shim
        // registers no session, so its rows are hook-owned and its bridge's
        // wake leaves no trace here: those report `unobserved`, as the roster
        // reports `none`.
        let waiting = || {
            ctx.exec_query(GetAllWakeWatchers {})
                .unwrap_or_default()
                .iter()
                .any(|w: &Arc<WakeWatcher>| w.session() == recipient.id.0.as_ref() && w.until > now)
        };
        let wake = match live_push {
            LivePushStatus::Delivered => WakeStatus::NotNeeded,
            _ if waiting() => WakeStatus::Expected,
            _ => WakeStatus::Unobserved,
        };

        // Dedup the pull inbox against a landed live push. A channels-ON
        // recipient already rendered this message via the NotifyChannel, so
        // mark it read for them — the per-turn `<marshal_inbox>` reads only
        // UNREAD direct messages, so it won't re-surface it. Without this the
        // recipient sees every direct message TWICE (once live, once in the
        // inbox). Best-effort: a failed write just falls back to that harmless
        // double-surface.
        //
        // "Delivered" here only means the push was queued on the connection,
        // so a connection that stops being read and then dies loses it, already
        // marked read. A recipient that acks its own pushes (`acks_pushes`)
        // is left unread until that ack instead.
        if delivered_live && recipient.acks_pushes != Some(true) {
            let read_id = MessageRead::make_id(msg.id.0.as_ref(), recipient.id.0.as_ref());
            if let Err(e) = ctx.emit_set(&MessageRead {
                id: MessageReadId(Arc::from(read_id.as_str())),
                message_id: msg.id.clone(),
                session_id: recipient.id.clone(),
                read_at: now,
            }) {
                log::warn!("[send_message] live-push inbox dedup write failed: {e:?}");
            }
        }

        Ok(SendMessageResult {
            message_id: msg.id,
            to_session_id: recipient.id.clone(),
            sent_at: now,
            live_push,
            wake,
            delivered_live,
        })
    }
}

fn err(ctx: &CommandContext, message: &str) -> CommandError {
    CommandError {
        tx: ctx.tx().to_string(),
        command_id: ctx.command_id.to_string(),
        message: message.to_string(),
    }
}

/// Resolve a caller-supplied recipient token against the live roster. Runs
/// server-side (inside the command) rather than in each client, so the shim,
/// the opencode plugin, and raw HTTP-MCP callers all share one policy.
///
/// Priority order:
///   1. exact session id;
///   2. a unique nickname (`swift-falcon`, what the statusline shows);
///   3. an OPERATOR identity — the human, addressed by their resolved operator
///      string (an email like `max@lucid.rocks`, optionally `op:`/`human:`
///      prefixed). This is human-via-agent routing: it delivers to that
///      operator's most-recently-active session, preferring one with a live,
///      render-capable client, so the sender addresses the *person* and the
///      daemon picks which of their agents currently has the floor. If none of
///      the operator's sessions is live the freshest still takes it and reads
///      from its inbox next turn — delivery is decoupled from liveness;
///   4. a unique session-id prefix.
///
/// Ambiguous or unknown tokens error with the candidates listed, so a caller
/// can never silently mis-route.
///
/// A resolved recipient session plus the operator identity it was addressed as:
/// `Some` only when resolution went through the operator (human-via-agent) tier,
/// which marks the message human-addressed (`Message::to_operator`); `None` for
/// id / nickname / prefix resolution (agent-to-agent).
#[cfg(not(target_arch = "wasm32"))]
type ResolvedRecipient = (Arc<Session>, Option<String>);

/// Returns the resolved session plus, when resolution went through the operator
/// tier (3), the operator identity that was addressed — so the caller can mark
/// the message human-addressed (`Message::to_operator`). `None` for the id /
/// nickname / prefix tiers (agent-addressed).
#[cfg(not(target_arch = "wasm32"))]
fn resolve_recipient(
    ctx: &CommandContext,
    sessions: &[Arc<Session>],
    token: &str,
) -> Result<ResolvedRecipient, CommandError> {
    // 1. Exact session id.
    if let Some(s) = sessions.iter().find(|s| s.id.0.as_ref() == token) {
        return Ok((s.clone(), None));
    }
    // 2. Unique nickname.
    let mut by_nick: Vec<Arc<Session>> = Vec::new();
    for s in sessions {
        if crate::nickname_for(ctx, s.id.0.as_ref())? == token {
            by_nick.push(s.clone());
        }
    }
    if by_nick.len() == 1 {
        return Ok((by_nick.remove(0), None));
    }
    if by_nick.len() > 1 {
        return Err(err(ctx, &ambiguous(ctx, "nickname", token, &by_nick)));
    }
    // MUS-3: a session that restarted keeps reaching whoever took it over.
    if let Some(s) = resolve_succeeded(ctx, sessions, token)? {
        return Ok((s, None));
    }
    // 3. Operator identity — human-via-agent routing. Record the operator we
    // addressed (the resolved session's canonical operator string, falling back
    // to the bare token) so the message is marked human-addressed downstream.
    let op = token
        .strip_prefix("op:")
        .or_else(|| token.strip_prefix("human:"))
        .unwrap_or(token);
    if let Some(best) = pick_operator_session(sessions, op) {
        let addressed = best.operator.clone().unwrap_or_else(|| op.to_string());
        return Ok((best, Some(addressed)));
    }
    // 4. Unique session-id prefix.
    let by_prefix: Vec<Arc<Session>> = sessions
        .iter()
        .filter(|s| s.id.0.as_ref().starts_with(token))
        .cloned()
        .collect();
    match by_prefix.len() {
        1 => Ok((by_prefix.into_iter().next().unwrap(), None)),
        0 => Err(err(
            ctx,
            &format!(
                "no session matches '{token}' (by id, nickname, operator, or id-prefix) — check marshal://roster"
            ),
        )),
        _ => Err(err(ctx, &ambiguous(ctx, "id-prefix", token, &by_prefix))),
    }
}

/// MUS-3: a token that names a session through a succession or a role, after
/// the nickname and id tiers found nothing.
///
/// - A nickname a session gave up in a late takeover reaches it until the
///   alias lapses, since peers may have written to it meanwhile.
/// - An old session id reaches the session that took it over.
/// - A declared role (`MARSHAL_IDENTITY=watchman`) reaches the one session
///   that claims it. It's a claim, not attested; see `Session::identity`.
#[cfg(not(target_arch = "wasm32"))]
fn resolve_succeeded(
    ctx: &CommandContext,
    sessions: &[Arc<Session>],
    token: &str,
) -> Result<Option<Arc<Session>>, CommandError> {
    let successions: Vec<Arc<crate::Succession>> = ctx.exec_query(crate::GetAllSuccessions {})?;
    let now = chrono::Utc::now().timestamp_millis();
    let find = |id: &str| sessions.iter().find(|s| s.id.0.as_ref() == id).cloned();
    if let Some(s) = successions.iter().find(|s| {
        s.alias.as_deref() == Some(token) && s.alias_until.is_some_and(|until| until > now)
    }) && let Some(found) = find(&s.to_session_id)
    {
        return Ok(Some(found));
    }
    if let Some(to) = crate::successor_of(&successions, token)
        && let Some(found) = find(&to)
    {
        return Ok(Some(found));
    }
    let by_role: Vec<&Arc<Session>> = sessions
        .iter()
        .filter(|s| s.identity.as_deref() == Some(token) && token != "none")
        .collect();
    match by_role.as_slice() {
        [one] => Ok(Some((*one).clone())),
        [] => Ok(None),
        many => Err(err(
            ctx,
            &format!(
                "role '{token}' is claimed by {} sessions; name one by nickname: {}",
                many.len(),
                many.iter()
                    .map(|s| crate::nickname_for(ctx, s.id.0.as_ref()).unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )),
    }
}

/// Best-effort recipient resolution for `@mentions` in a broadcast body:
/// exact id, unique nickname, or operator identity (human-via-agent,
/// most-active). Returns `None` for unknown / ambiguous tokens — a mention is
/// an explicit handle, so we never guess or error on it; an unresolvable
/// `@token` is just prose. Shared with `BroadcastMessage`'s @mention escape
/// hatch. Mirrors tiers 1–3 of `resolve_recipient` minus the prefix tier and
/// the hard errors.
///
/// Like `resolve_recipient`, the second tuple element is the addressed operator
/// identity when resolution went through the operator tier (so a human
/// `@mention` produces a human-addressed DM), else `None`.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn resolve_mention(
    ctx: &CommandContext,
    sessions: &[Arc<Session>],
    token: &str,
) -> Result<Option<ResolvedRecipient>, CommandError> {
    // 1. Exact session id.
    if let Some(s) = sessions.iter().find(|s| s.id.0.as_ref() == token) {
        return Ok(Some((s.clone(), None)));
    }
    // 2. Unique nickname (ambiguous → treat as prose, not a hard error).
    let mut by_nick: Vec<Arc<Session>> = Vec::new();
    for s in sessions {
        if crate::nickname_for(ctx, s.id.0.as_ref())? == token {
            by_nick.push(s.clone());
        }
    }
    if by_nick.len() == 1 {
        return Ok(Some((by_nick.remove(0), None)));
    }
    if by_nick.len() > 1 {
        return Ok(None);
    }
    // 3. Operator identity — human-via-agent.
    let op = token
        .strip_prefix("op:")
        .or_else(|| token.strip_prefix("human:"))
        .unwrap_or(token);
    Ok(pick_operator_session(sessions, op).map(|best| {
        let addressed = best.operator.clone().unwrap_or_else(|| op.to_string());
        (best, Some(addressed))
    }))
}

/// Choose which of an operator's sessions receives a human-via-agent message:
/// the most-recently-active one, with live + render-capable sessions ranked
/// above idle/disconnected ones (the human's current focus). `None` when the
/// operator has no session on the roster. Pure over the roster so the routing
/// policy is unit-testable without a live command context.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn pick_operator_session(sessions: &[Arc<Session>], op: &str) -> Option<Arc<Session>> {
    pick_operator_session_at(sessions, op, Utc::now().timestamp_millis())
}

/// How far ahead of the daemon's clock a client's own can run and its `lastActivityAt` still count
/// as activity (MAR-55): a machine whose clock is a few seconds fast, as another machine's may be
/// against Murph's, is still live.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) const CLOCK_TOLERANCE_MS: i64 = 60_000;

/// [`pick_operator_session`] at `now_ms`. Clients report `lastActivityAt` themselves, so one later
/// than now isn't activity: it ranks as none, or it would win the operator's mail for as long as
/// it stays in the future (MAR-51, from muster's MUS-33). One less than [`CLOCK_TOLERANCE_MS`]
/// ahead is a fast clock, and counts as activity now (MAR-55).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn pick_operator_session_at(
    sessions: &[Arc<Session>],
    op: &str,
    now_ms: i64,
) -> Option<Arc<Session>> {
    sessions
        .iter()
        .filter(|s| {
            s.operator
                .as_deref()
                .is_some_and(|o| o.eq_ignore_ascii_case(op))
        })
        .cloned()
        .max_by_key(|s| {
            let live = s.client_id.is_some() && s.channels_enabled != Some(false);
            let active = s.last_activity_at.unwrap_or(s.connected_at);
            let active = if active > now_ms + CLOCK_TOLERANCE_MS {
                0
            } else {
                active.min(now_ms)
            };
            (live as i64, active)
        })
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod operator_routing_tests {
    use super::*;

    /// Built from JSON so only the fields routing reads are named.
    fn session(id: &str, last_activity_at: i64) -> Arc<Session> {
        Arc::new(
            serde_json::from_value(serde_json::json!({
                "id": id,
                "pid": 0,
                "cwd": "/repo",
                "connectedAt": 0,
                "lastActivityAt": last_activity_at,
                "operator": "op@example.test",
            }))
            .expect("a session"),
        )
    }

    #[test]
    fn a_future_activity_time_does_not_win_operator_mail() {
        let now = 10_000_000;
        let sessions = [
            session("honest", now - 1_000),
            session("liar", now + 86_400_000),
        ];
        let best = pick_operator_session_at(&sessions, "op@example.test", now).expect("a session");
        assert_eq!(best.id.0.as_ref(), "honest");
        // Without the future one, the most recently active still wins, as before.
        let sessions = [session("older", now - 5_000), session("newer", now - 1_000)];
        let best = pick_operator_session_at(&sessions, "op@example.test", now).expect("a session");
        assert_eq!(best.id.0.as_ref(), "newer");
        // A clock a little fast is still live: it counts as activity now, ahead of an older one.
        let sessions = [
            session("older", now - 5_000),
            session("fast-clock", now + 30_000),
        ];
        let best = pick_operator_session_at(&sessions, "op@example.test", now).expect("a session");
        assert_eq!(best.id.0.as_ref(), "fast-clock");
        // Past the tolerance it's no activity at all.
        let sessions = [
            session("older", now - 5_000),
            session("liar", now + CLOCK_TOLERANCE_MS + 1),
        ];
        let best = pick_operator_session_at(&sessions, "op@example.test", now).expect("a session");
        assert_eq!(best.id.0.as_ref(), "older");
        // A time exactly now is real activity.
        let sessions = [session("older", now - 5_000), session("now", now)];
        let best = pick_operator_session_at(&sessions, "op@example.test", now).expect("a session");
        assert_eq!(best.id.0.as_ref(), "now");
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn ambiguous(ctx: &CommandContext, kind: &str, token: &str, matches: &[Arc<Session>]) -> String {
    let list = matches
        .iter()
        .map(|s| {
            let nick = crate::nickname_for(ctx, s.id.0.as_ref()).unwrap_or_default();
            format!("{nick} ({})", s.id.0.as_ref())
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "'{token}' is an ambiguous {kind} matching {} sessions [{list}] — address by full session_id",
        matches.len()
    )
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn push_to_client(client_id: &str, content: String, meta: serde_json::Value) -> bool {
    use myko::{command::CommandRequest, server::try_client_registry};
    let Some(registry) = try_client_registry() else {
        log::warn!("[send_message] client_registry not initialized; cannot deliver");
        return false;
    };
    let cmd = NotifyChannel { content, meta };
    let request = CommandRequest::new(cmd);
    registry.send_command_request_to(client_id, &request)
}

/// The channel banner for a direct message. Agent-addressed mail stays a
/// concise origin ping; human-addressed mail opens with the relay-to-operator
/// contract so the recipient agent puts it in front of the person instead of
/// acting on it. Pure so it can be asserted without a live client registry.
/// `truncated_id` is the message's id when its `meta.body` preview was cut,
/// so the line the model reads says where the rest is.
pub(crate) fn direct_push_content(
    from_nickname: &str,
    to_operator: Option<&str>,
    truncated_id: Option<&str>,
) -> String {
    let banner = match to_operator {
        Some(op) => format!(
            "{} New message from {from_nickname}.",
            crate::message::operator_relay_notice(op)
        ),
        None => format!("new message from {from_nickname}"),
    };
    with_truncated_notice(banner, truncated_id)
}

pub(crate) fn with_truncated_notice(banner: String, truncated_id: Option<&str>) -> String {
    match truncated_id {
        Some(id) => format!("{banner} {}", crate::message::truncated_notice(id)),
        None => banner,
    }
}

#[cfg(test)]
mod push_content_tests {
    use super::direct_push_content;

    #[test]
    fn agent_addressed_banner_is_unchanged() {
        assert_eq!(
            direct_push_content("fleet-wolf", None, None),
            "new message from fleet-wolf"
        );
    }

    #[test]
    fn human_addressed_banner_opens_with_the_relay_contract() {
        let content = direct_push_content("fleet-wolf", Some("max@lucid.rocks"), None);
        assert!(content.starts_with("For operator (max@lucid.rocks): relay to them;"));
        assert!(content.ends_with("New message from fleet-wolf."));
    }

    #[test]
    fn a_cut_preview_says_where_the_full_message_is() {
        assert_eq!(
            direct_push_content("fleet-wolf", None, Some("m-1")),
            "new message from fleet-wolf [truncated; full message m-1 remains in marshal://messages]"
        );
        let content = direct_push_content("fleet-wolf", Some("max@lucid.rocks"), Some("m-1"));
        assert!(content.starts_with("For operator (max@lucid.rocks): relay to them;"));
        assert!(content.ends_with("[truncated; full message m-1 remains in marshal://messages]"));
    }
}

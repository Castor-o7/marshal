//! Daemon-side sticky nickname assignment.
//!
//! Every `Session` SET fires this saga. The first time a session is seen it
//! assigns a frozen handle — the candidate from the current wordlists
//! (`marshal_entities::nickname`), re-hashed on the rare collision with a
//! live-or-past assignment — and stores it as a `SessionNickname`. Once
//! assigned it is NEVER touched again, so editing/expanding the wordlists only
//! affects sessions that register afterward; existing sessions keep their
//! handle. That's the grandfather: `session_nickname.rs`.
//!
//! Idempotent + cheap: on a session that already has a handle the command is
//! a single query + early return, so it's safe to fire on every Session SET
//! (the client re-SETs its Session constantly, but never touches the separate
//! `SessionNickname`, so it can't clobber the assignment).

use std::{collections::HashSet, sync::Arc};

use marshal_entities::{
    GetAllSessionNicknames, GetAllSessions, Session, SessionNickname, SessionNicknameId, nickname,
};
use myko::prelude::EventPublishing as _;
use myko::{
    command::{CommandContext, CommandError, CommandHandler},
    myko_command,
    prelude::myko_saga,
    saga::{SagaContext, SagaHandler},
    wire::{MEvent, MEventType},
};

/// Force-link the saga/command registrations against dead-code elimination.
pub fn link() {}

// ─── Saga ───────────────────────────────────────────────────────────────────

#[myko_saga]
pub struct AssignNicknameSaga;

impl SagaHandler for AssignNicknameSaga {
    type EventItem = Session;
    type Command = AssignNickname;
    const EVENT_TYPE: MEventType = MEventType::SET;

    fn handle(session: Session, _event: MEvent, _ctx: Arc<SagaContext>) -> Option<Self::Command> {
        Some(AssignNickname {
            session_id: session.id.0.as_ref().to_string(),
        })
    }
}

// ─── Command ────────────────────────────────────────────────────────────────

/// Server-internal: assign a frozen handle to `session_id` if it has none yet.
#[myko_command]
pub struct AssignNickname {
    pub session_id: String,
}

impl CommandHandler for AssignNickname {
    fn execute(self, ctx: CommandContext) -> Result<(), CommandError> {
        ensure_assigned(&ctx, &self.session_id)?;
        Ok(())
    }
}

/// Reserve and return the sticky nickname for a registered session.
///
/// Hook registration calls this synchronously so callers can learn the
/// collision-safe identity before the asynchronous assignment saga runs.
pub fn ensure_assigned(ctx: &CommandContext, session_id: &str) -> Result<String, CommandError> {
    // One naming at a time, so a saga and a hook naming the same session at
    // once can't both take a predecessor over (MUS-3).
    let _naming = crate::succession::NAMING
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    // The session may have DEL'd between the SET and our run — that's fine,
    // a stray assignment is harmless, but skip it to avoid reserving a name
    // for a gone session.
    let sessions: Vec<Arc<Session>> = ctx.exec_query(GetAllSessions {})?;
    let Some(session) = sessions.iter().find(|s| s.id.0.as_ref() == session_id) else {
        return Ok(nickname(session_id));
    };

    let assigned: Vec<Arc<SessionNickname>> = ctx.exec_query(GetAllSessionNicknames {})?;
    let now = chrono::Utc::now().timestamp_millis();

    // Already has a handle → FROZEN. This early return is the whole
    // grandfather: once a session is named, no wordlist change moves it.
    // Only a succession re-keys it (MUS-3).
    if let Some(existing) = assigned.iter().find(|n| n.id.0.as_ref() == session_id) {
        crate::succession::record_identity(ctx, session, now)?;
        return Ok(existing.nickname.clone());
    }

    // MUS-3: a dead predecessor with the same identity hands its handle on.
    if let Some(inherited) = crate::succession::name_from_predecessor(ctx, session, now)? {
        return Ok(inherited);
    }

    // Assign: the current-wordlist candidate, re-hashed with a salt on the
    // rare collision so two sessions never share a handle. The un-salted
    // candidate equals the old pure-function output, so at rollout every
    // live session is assigned exactly the handle it already had. A handle
    // still reaching a session as an alias after a swap is taken too.
    let aliases = crate::succession::live_aliases(ctx, now)?;
    let taken: HashSet<&str> = assigned
        .iter()
        .map(|n| n.nickname.as_str())
        .chain(aliases.iter().map(String::as_str))
        .collect();
    let mut handle = nickname(session_id);
    let mut salt = 0u32;
    while taken.contains(handle.as_str()) {
        salt += 1;
        handle = nickname(&format!("{session_id}#{salt}"));
    }

    ctx.emit_set(&SessionNickname {
        id: SessionNicknameId(Arc::from(session_id)),
        nickname: handle.clone(),
    })?;
    Ok(handle)
}

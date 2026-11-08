//! Stable identity across restarts (MUS-3): hand a dead predecessor's nickname
//! to the session that replaces it.
//!
//! A session's identity key is its harness and working directory, or a role
//! it declares with `MARSHAL_IDENTITY`, on the same machine for the same
//! operator ([`identity_key`]). When a session with a key is first named, the
//! daemon looks for an earlier session with the same key that's dead, and
//! gives the new one its nickname. The predecessor's row is retired, and a
//! [`Succession`] record says who took over from whom and why the predecessor
//! counted as dead.
//!
//! A false succession hands a live session's mail to another, while a missed
//! one only costs a fresh nickname, so "dead" is strict ([`verdict`]):
//!
//! - **Observed death.** The row's client left the daemon's Client store at
//!   least the grace ago (`MARSHAL_SUCCESSION_GRACE_SECS`, 30 s), it has no
//!   live waiter, and it hasn't hooked since its client dropped. That's a
//!   Claude session whose process ended.
//! - **Unobservable rows**, which never had a client (Codex hooks, or a Claude
//!   session whose MCP shim never attached), also need no activity for the
//!   quiet time (`MARSHAL_SUCCESSION_QUIET_SECS`, 600 s). A Codex row still on
//!   the roster counts as dead only once this daemon saw its bridge's wake
//!   record end, at least the bridge grace (`MARSHAL_SUCCESSION_BRIDGE_GRACE_SECS`,
//!   120 s) ago, with no hook since (MAR-54). One never seen with a bridge
//!   record never counts as dead.
//! - **A removed row** counts as dead only if this daemon saw it go (a session
//!   end or a reap). After a daemon restart every row is gone, and nothing
//!   counts as dead until it's seen again.
//!
//! A new session that registers while its predecessor's client dropped only
//! moments ago, inside the grace, gets a fresh nickname first. The sweeper
//! checks again each tick for the swap window (10 min), and once the
//! predecessor counts as dead the new session takes its nickname. Its fresh
//! nickname stays an alias for it for the alias time (10 min), and its next
//! hook tells it the new name.
//!
//! A role is a claim: on a machine where every session shares one credential,
//! any of them can declare `watchman`. So a role is shown as declared, and is
//! never taken over while any session holding it is live.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock, Mutex},
};

use marshal_entities::{
    GetAllMessageReads, GetAllMessages, GetAllRoomMembers, GetAllRooms, GetAllSessionIdentitys,
    GetAllSessionNicknames, GetAllSessions, GetAllSuccessions, GetAllWakeWatchers, Message,
    MessageRead, Room, RoomKind, RoomMember, RoomMemberId, Session, SessionId, SessionIdentity,
    SessionIdentityId, SessionNickname, SessionNicknameId, Succession, SuccessionId,
};
use myko::command::{CommandContext, CommandError};
use myko::prelude::EventPublishing as _;

// MAR-54: a Codex row has no client and gets activity only on hooks, so an
// idle, live Codex thread looks like a dead one by those alone. The leading
// app-server bridge refreshes a `WakeWatcher` for each thread it can wake
// every 30 s (MAR-12), and the record lasts 2 min. So a present Codex row
// counts as dead only once this daemon saw such a record end, at least the
// bridge grace ago (no shorter than the record's lifetime, since a bridge
// leadership handover or an app-server restart can leave a gap of a refresh
// or more before the next record), with no waiter and no hook since. A Codex
// row never seen with a bridge record (a thread with no bridge, or a bridge
// that died before this daemon started) never counts as dead; only its row
// going (its session end, or a reap) frees its nickname.

/// The timings, in ms.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// How long a session's client must have been gone (observed death).
    pub grace_ms: i64,
    /// How long a row that never had a client must have been silent.
    pub quiet_ms: i64,
    /// How long after registering a session is checked again for a late swap.
    pub window_ms: i64,
    /// How long a nickname given up in a late swap still reaches its session.
    pub alias_ms: i64,
    /// How long after registering a session is checked again when its
    /// predecessor still looked live then, only because its client was still
    /// connected or its waiter still registered (MUS-92).
    pub settle_ms: i64,
    /// How long a Codex row's bridge wake record must have been gone before
    /// the row counts as dead (MAR-54). At least the record's 2 min lifetime.
    pub bridge_grace_ms: i64,
}

impl Config {
    pub fn from_env() -> Self {
        let secs = |name: &str, default: i64| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse::<i64>().ok())
                .filter(|v| *v >= 0)
                .unwrap_or(default)
                * 1000
        };
        Self {
            grace_ms: secs("MARSHAL_SUCCESSION_GRACE_SECS", 30),
            quiet_ms: secs("MARSHAL_SUCCESSION_QUIET_SECS", 600),
            window_ms: secs("MARSHAL_SUCCESSION_WINDOW_SECS", 600),
            alias_ms: secs("MARSHAL_SUCCESSION_ALIAS_SECS", 600),
            settle_ms: secs("MARSHAL_SUCCESSION_SETTLE_SECS", 60),
            bridge_grace_ms: secs("MARSHAL_SUCCESSION_BRIDGE_GRACE_SECS", 120),
        }
    }
}

/// The identity key a session is matched on, and whether it's a declared
/// role. `None` when it has none: `MARSHAL_IDENTITY=none`, or no harness or
/// working directory to infer one from.
pub fn identity_key(s: &Session) -> Option<(String, bool)> {
    let operator = s.operator.as_deref().unwrap_or("");
    let host = s.host.as_ref().map(|h| h.name.as_str()).unwrap_or("");
    match s.identity.as_deref().map(str::trim) {
        Some("none") => None,
        Some(role) if !role.is_empty() => Some((format!("role:{role}|{operator}|{host}"), true)),
        _ => {
            let harness = s.harness.as_deref().filter(|h| !h.is_empty())?;
            if s.cwd.is_empty() {
                return None;
            }
            Some((format!("cwd:{harness}|{}|{operator}|{host}", s.cwd), false))
        }
    }
}

/// Whether a predecessor counts as dead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Dead, and why.
    Dead(String),
    /// Its client dropped, but inside the grace: dead soon, unless it returns.
    Pending(String),
    /// Live, or can't be told from live.
    Live(String),
}

/// What the daemon knows about one predecessor.
#[derive(Clone, Copy, Debug, Default)]
pub struct Facts<'a> {
    /// Its row, if it's still on the roster.
    pub row: Option<&'a Session>,
    /// It has a live `WakeWatcher`.
    pub waiting: bool,
    /// Its `client_id` is in the live Client store.
    pub client_live: bool,
    /// When the daemon first saw its row without a live client.
    pub client_gone_at: Option<i64>,
    /// When the daemon saw its row go.
    pub removed_at: Option<i64>,
    /// Its row was on the roster at the sweeper's last tick.
    pub known: bool,
    /// This daemon has seen a Codex bridge's wake record for it (MAR-54).
    pub bridge_seen: bool,
    /// When the daemon first saw that bridge record gone, if it's gone now.
    pub bridge_gone_at: Option<i64>,
}

/// Whether a predecessor is dead. Pure, so it can be tested on its own.
pub fn verdict(f: &Facts, now: i64, cfg: &Config) -> Verdict {
    let Some(row) = f.row else {
        return match f.removed_at {
            Some(at) => Verdict::Dead(format!(
                "its row was removed {}s ago (session end or reap)",
                (now - at) / 1000
            )),
            // Gone since the last tick: the next one sees it go.
            None if f.known => Verdict::Pending("its row went since the last sweep".into()),
            None => Verdict::Live(
                "its row is gone, but this daemon didn't see it go (a restart?)".into(),
            ),
        };
    };
    if f.waiting {
        return Verdict::Live("it's waiting on /hook/wait".into());
    }
    if row.client_id.is_some() && f.client_live {
        return Verdict::Live("its client is connected".into());
    }
    let last_signal = [
        row.last_activity_at,
        row.last_tool_at,
        Some(row.connected_at),
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(0);
    if row.harness.as_deref() == Some("codex") {
        return codex_verdict(f, last_signal, now, cfg);
    }
    match f.client_gone_at {
        Some(gone_at) => {
            // The hooked_since guard: a session still hooking after its client
            // dropped is alive (its MCP shim crashed, say).
            if last_signal > gone_at {
                return Verdict::Live(format!(
                    "it hooked {}s after its client dropped",
                    (last_signal - gone_at) / 1000
                ));
            }
            let gone_for = now - gone_at;
            if gone_for < cfg.grace_ms {
                return Verdict::Pending(format!(
                    "its client dropped {}s ago, inside the {}s grace",
                    gone_for / 1000,
                    cfg.grace_ms / 1000
                ));
            }
            Verdict::Dead(format!(
                "its client dropped {}s ago, with no waiter and no hook since",
                gone_for / 1000
            ))
        }
        // Its client is gone, but the sweeper hasn't timed the drop yet (it
        // ticks every few seconds). Pending, so a quick restart is checked
        // again once the drop is timed.
        None if row.client_id.is_some() => Verdict::Pending(
            "its client isn't connected, and the daemon hasn't timed the drop yet".into(),
        ),
        None => {
            let quiet_for = now - last_signal;
            if quiet_for < cfg.quiet_ms {
                Verdict::Live(format!(
                    "it never had a client, and was active {}s ago (quiet time {}s)",
                    quiet_for / 1000,
                    cfg.quiet_ms / 1000
                ))
            } else {
                Verdict::Dead(format!(
                    "it never had a client, has no waiter, and has been quiet for {}s",
                    quiet_for / 1000
                ))
            }
        }
    }
}

/// A present Codex row with no waiter and no live client (MAR-54): dead only
/// once its bridge's wake record has been gone for the bridge grace, with no
/// hook since.
fn codex_verdict(f: &Facts, last_signal: i64, now: i64, cfg: &Config) -> Verdict {
    let Some(gone_at) = f.bridge_gone_at else {
        return Verdict::Live(if f.bridge_seen {
            "its Codex bridge can wake it".into()
        } else {
            "a Codex row never seen with a bridge record can't be told from an idle live thread"
                .into()
        });
    };
    if last_signal > gone_at {
        return Verdict::Live(format!(
            "it hooked {}s after its bridge record ended",
            (last_signal - gone_at) / 1000
        ));
    }
    let gone_for = now - gone_at;
    if gone_for < cfg.bridge_grace_ms {
        return Verdict::Pending(format!(
            "its bridge record ended {}s ago, inside the {}s bridge grace",
            gone_for / 1000,
            cfg.bridge_grace_ms / 1000
        ));
    }
    // A bridge can die while its thread's TUI lives on, so the row must also
    // be as quiet as any row with no client.
    let quiet_for = now - last_signal;
    if quiet_for < cfg.quiet_ms {
        return Verdict::Pending(format!(
            "its bridge record ended {}s ago, but it was active {}s ago (quiet time {}s)",
            gone_for / 1000,
            quiet_for / 1000,
            cfg.quiet_ms / 1000
        ));
    }
    Verdict::Dead(format!(
        "its bridge record ended {}s ago, with no waiter and no hook since, and it has been \
         quiet for {}s",
        gone_for / 1000,
        quiet_for / 1000
    ))
}

// ─── What the sweeper observes ──────────────────────────────────────────────

#[derive(Default)]
struct Observed {
    live_clients: HashSet<Arc<str>>,
    client_gone_at: HashMap<Arc<str>, i64>,
    known_rows: HashSet<Arc<str>>,
    removed_at: HashMap<Arc<str>, i64>,
    /// Sessions a Codex bridge's wake record has named (MAR-54).
    bridge_seen: HashSet<Arc<str>>,
    /// When each of those was first seen without a current bridge record.
    bridge_gone_at: HashMap<Arc<str>, i64>,
    /// Sessions to check again for a late takeover, and until when (ms).
    pending: HashMap<Arc<str>, i64>,
}

static OBSERVED: LazyLock<Mutex<Observed>> = LazyLock::new(Default::default);

/// Serializes naming, so a saga and a hook naming the same session at once
/// can't both take a predecessor over.
pub static NAMING: Mutex<()> = Mutex::new(());

/// How long a removed row is remembered as one this daemon saw go.
const REMEMBER_REMOVED_MS: i64 = 24 * 60 * 60 * 1000;

/// The sweeper's view each tick: every row's client id, and the live clients.
pub fn observe(rows: &[(Arc<str>, Option<Arc<str>>)], live_clients: &HashSet<Arc<str>>, now: i64) {
    let mut o = OBSERVED.lock().unwrap_or_else(|e| e.into_inner());
    o.live_clients = live_clients.clone();
    let current: HashSet<Arc<str>> = rows.iter().map(|(id, _)| id.clone()).collect();
    for (id, cid) in rows {
        match cid {
            Some(cid) if live_clients.contains(cid) => {
                o.client_gone_at.remove(id);
            }
            Some(_) => {
                o.client_gone_at.entry(id.clone()).or_insert(now);
            }
            // No client: a demoted row keeps the drop it was seen with.
            None => {}
        }
    }
    let gone: Vec<Arc<str>> = o.known_rows.difference(&current).cloned().collect();
    for id in gone {
        o.removed_at.insert(id.clone(), now);
        o.client_gone_at.remove(&id);
        o.bridge_seen.remove(&id);
        o.bridge_gone_at.remove(&id);
    }
    o.removed_at.retain(|_, at| now - *at < REMEMBER_REMOVED_MS);
    o.known_rows = current;
}

/// The sweeper's view each tick of which sessions a Codex bridge's wake
/// record currently names (MAR-54). A session seen before and missing now is
/// timed from the first tick it's missing.
pub fn observe_bridges(current: &HashSet<Arc<str>>, now: i64) {
    let mut o = OBSERVED.lock().unwrap_or_else(|e| e.into_inner());
    for id in current {
        o.bridge_seen.insert(id.clone());
        o.bridge_gone_at.remove(id);
    }
    let lapsed: Vec<Arc<str>> = o
        .bridge_seen
        .iter()
        .filter(|id| !current.contains(*id))
        .cloned()
        .collect();
    for id in lapsed {
        o.bridge_gone_at.entry(id).or_insert(now);
    }
}

/// A session's row was removed by its own session end (Codex's SessionEnd
/// hook). Recorded at once, since the row may not have lasted a sweeper tick.
pub fn record_removed(session_id: &str, now: i64) {
    let mut o = OBSERVED.lock().unwrap_or_else(|e| e.into_inner());
    o.removed_at.insert(Arc::from(session_id), now);
    o.client_gone_at.remove(session_id);
}

fn facts_for<'a>(
    o: &Observed,
    id: &str,
    row: Option<&'a Session>,
    watching: &HashSet<&str>,
) -> Facts<'a> {
    Facts {
        row,
        waiting: watching.contains(id),
        client_live: row
            .and_then(|r| r.client_id.as_ref())
            .is_some_and(|cid| o.live_clients.contains(&cid.0)),
        client_gone_at: o.client_gone_at.get(id).copied(),
        removed_at: o.removed_at.get(id).copied(),
        known: o.known_rows.contains(id),
        bridge_seen: o.bridge_seen.contains(id),
        bridge_gone_at: o.bridge_gone_at.get(id).copied(),
    }
}

// ─── Taking over ────────────────────────────────────────────────────────────

/// A predecessor a session can take over.
struct Found {
    predecessor: String,
    nickname: String,
    reason: String,
}

/// Look for a dead predecessor of `session` with the same key, named before
/// `before`. Records a pending predecessor for the sweeper's late check.
fn find_predecessor(
    ctx: &CommandContext,
    session: &Session,
    key: &str,
    role: bool,
    before: i64,
    now: i64,
    cfg: &Config,
) -> Result<Option<Found>, CommandError> {
    let identities: Vec<Arc<SessionIdentity>> = ctx.exec_query(GetAllSessionIdentitys {})?;
    let nicknames: Vec<Arc<SessionNickname>> = ctx.exec_query(GetAllSessionNicknames {})?;
    let sessions: Vec<Arc<Session>> = ctx.exec_query(GetAllSessions {})?;
    let watchers = ctx.exec_query(GetAllWakeWatchers {})?;
    let watching: HashSet<&str> = watchers
        .iter()
        .filter(|w| w.until > now)
        .map(|w| w.session())
        .collect();
    let me = session.id.0.as_ref();
    let o = OBSERVED.lock().unwrap_or_else(|e| e.into_inner());

    let mut dead: Vec<(i64, Found)> = Vec::new();
    // Until when to check this session again, if a predecessor may yet die.
    let mut check_until: Option<i64> = None;
    // A live holder of the role: no takeover now, whatever else is dead.
    let mut role_held = false;
    let mut check_again =
        |until: i64| check_until = Some(check_until.map_or(until, |u| u.max(until)));
    for ident in identities
        .iter()
        .filter(|i| i.key == key && i.id.0.as_ref() != me && i.assigned_at <= before)
    {
        let id = ident.id.0.as_ref();
        let Some(nick) = nicknames.iter().find(|n| n.id.0.as_ref() == id) else {
            continue;
        };
        let row = sessions
            .iter()
            .find(|s| s.id.0.as_ref() == id)
            .map(|s| &**s);
        let facts = facts_for(&o, id, row, &watching);
        let v = verdict(&facts, now, cfg);
        log::debug!("[identity] {me}: predecessor {id} is {v:?}");
        match v {
            Verdict::Dead(reason) => dead.push((
                ident.assigned_at,
                Found {
                    predecessor: id.to_string(),
                    nickname: nick.nickname.clone(),
                    reason,
                },
            )),
            Verdict::Pending(why) => {
                log::info!("[identity] {me} may take over from {id} later: {why}");
                check_again(before + cfg.window_ms);
            }
            Verdict::Live(why) => {
                // MUS-92: live only because its client hasn't closed yet, or
                // its waiter's record hasn't ended, as when a new session
                // starts before the old one's socket closes. Check again for a
                // short while, so a restart that races its own exit still
                // takes over. A sibling that stays live past it keeps its name.
                if facts.client_live || facts.waiting {
                    log::info!(
                        "[identity] {me} may take over from {id} if it ends within {}s: {why}",
                        cfg.settle_ms / 1000
                    );
                    check_again(before + cfg.settle_ms);
                }
                if role {
                    log::info!("[identity] {me} won't take role {key} over: {id} holds it ({why})");
                    role_held = true;
                    break;
                }
            }
        }
    }
    // Release the observations before recording a check, which takes them too.
    drop(o);
    if role_held {
        record_check(session, check_until);
        return Ok(None);
    }
    let found = dead.into_iter().max_by_key(|(at, _)| *at).map(|(_, f)| f);
    if found.is_none() {
        record_check(session, check_until);
    }
    Ok(found)
}

/// Note that `session` is to be checked again until `until`, if at all.
fn record_check(session: &Session, until: Option<i64>) {
    if let Some(until) = until {
        let mut o = OBSERVED.lock().unwrap_or_else(|e| e.into_inner());
        o.pending.insert(session.id.0.clone(), until);
    }
}

/// Give `successor` the predecessor's nickname: re-key it, retire the
/// predecessor, and record the succession.
fn take_over(
    ctx: &CommandContext,
    successor: &str,
    found: &Found,
    key: &str,
    role: bool,
    alias: Option<(String, i64)>,
    now: i64,
) -> Result<(), CommandError> {
    let pred = found.predecessor.as_str();
    ctx.emit_set(&SessionNickname {
        id: SessionNicknameId(Arc::from(successor)),
        nickname: found.nickname.clone(),
    })?;
    ctx.emit_del(&SessionNickname {
        id: SessionNicknameId(Arc::from(pred)),
        nickname: String::new(),
    })?;
    ctx.emit_del(&SessionIdentity {
        id: SessionIdentityId(Arc::from(pred)),
        key: String::new(),
        role,
        assigned_at: 0,
    })?;
    // Move its ad hoc room memberships over before the row goes: retiring the
    // row removes them, and the auto-room saga gives the successor back only
    // its auto-rooms. `joined_at` is kept, so the successor sees the room's
    // history from when its predecessor joined.
    move_adhoc_memberships(ctx, pred, successor)?;
    // MUS-47: its unread direct mail is the successor's now.
    let handed_over = hand_over_unread(ctx, pred, successor)?;
    // Retire the predecessor's row if it's still there, so its fallback
    // nickname doesn't name it too. Its mail is kept (MUS-46).
    let sessions: Vec<Arc<Session>> = ctx.exec_query(GetAllSessions {})?;
    if let Some(row) = sessions.iter().find(|s| s.id.0.as_ref() == pred) {
        ctx.emit_del(&**row)?;
    }
    let (alias, alias_until) = match alias {
        Some((a, until)) => (Some(a), Some(until)),
        None => (None, None),
    };
    log::info!(
        "[identity] {successor} took over {} from {pred} ({key}): {}{}",
        found.nickname,
        found.reason,
        alias
            .as_deref()
            .map(|a| format!("; {a} stays an alias"))
            .unwrap_or_default()
    );
    ctx.emit_set(&Succession {
        id: SuccessionId(Arc::from(pred)),
        to_session_id: successor.to_string(),
        nickname: found.nickname.clone(),
        key: key.to_string(),
        reason: found.reason.clone(),
        at: now,
        alias,
        alias_until,
        told: false,
        handed_over,
    })?;
    Ok(())
}

/// Readdress `from`'s unread direct messages to `to` (MUS-47), so the
/// successor's inbox shows each once, as its own. Read mail stays with the
/// predecessor's history. Returns the ids moved.
fn hand_over_unread(
    ctx: &CommandContext,
    from: &str,
    to: &str,
) -> Result<Vec<String>, CommandError> {
    let messages: Vec<Arc<Message>> = ctx.exec_query(GetAllMessages {})?;
    let reads: Vec<Arc<MessageRead>> = ctx.exec_query(GetAllMessageReads {})?;
    let read: HashSet<&str> = reads
        .iter()
        .filter(|r| r.session_id.0.as_ref() == from)
        .map(|r| r.message_id.0.as_ref())
        .collect();
    let mut moved = Vec::new();
    for m in messages.iter().filter(|m| {
        m.to_session_id
            .as_ref()
            .is_some_and(|t| t.0.as_ref() == from)
            && !read.contains(m.id.0.as_ref())
    }) {
        let mut readdressed = (**m).clone();
        readdressed.to_session_id = Some(SessionId(Arc::from(to)));
        ctx.emit_set(&readdressed)?;
        moved.push(m.id.0.to_string());
    }
    if !moved.is_empty() {
        log::info!(
            "[identity] handed {} unread message(s) from {from} to {to}",
            moved.len()
        );
    }
    Ok(moved)
}

/// Give `to` each ad hoc room `from` is a member of, keeping when it joined.
fn move_adhoc_memberships(ctx: &CommandContext, from: &str, to: &str) -> Result<(), CommandError> {
    let rooms: Vec<Arc<Room>> = ctx.exec_query(GetAllRooms {})?;
    let members: Vec<Arc<RoomMember>> = ctx.exec_query(GetAllRoomMembers {})?;
    let adhoc: HashSet<&str> = rooms
        .iter()
        .filter(|r| matches!(r.kind, RoomKind::Adhoc))
        .map(|r| r.id.0.as_ref())
        .collect();
    for m in members
        .iter()
        .filter(|m| m.session_id.0.as_ref() == from && adhoc.contains(m.room_id.0.as_ref()))
    {
        let room = m.room_id.0.as_ref();
        let joined_at = members
            .iter()
            .find(|e| e.session_id.0.as_ref() == to && e.room_id.0.as_ref() == room)
            .map_or(m.joined_at, |e| e.joined_at.min(m.joined_at));
        ctx.emit_set(&RoomMember {
            id: RoomMemberId(Arc::from(RoomMember::make_id(room, to))),
            room_id: m.room_id.clone(),
            session_id: SessionId(Arc::from(to)),
            joined_at,
        })?;
    }
    Ok(())
}

/// Nicknames held as aliases after a late swap, which a new session mustn't get.
pub fn live_aliases(ctx: &CommandContext, now: i64) -> Result<HashSet<String>, CommandError> {
    let successions: Vec<Arc<Succession>> = ctx.exec_query(GetAllSuccessions {})?;
    Ok(successions
        .iter()
        .filter(|s| s.alias_until.is_some_and(|until| until > now))
        .filter_map(|s| s.alias.clone())
        .collect())
}

/// For a session being named for the first time: take a dead predecessor's
/// nickname over if there is one, and record its identity either way. Returns
/// the nickname taken over. The caller holds [`NAMING`].
pub fn name_from_predecessor(
    ctx: &CommandContext,
    session: &Session,
    now: i64,
) -> Result<Option<String>, CommandError> {
    let Some((key, role)) = identity_key(session) else {
        return Ok(None);
    };
    let cfg = Config::from_env();
    let me = session.id.0.as_ref();
    let found = find_predecessor(ctx, session, &key, role, now, now, &cfg)?;
    if let Some(found) = &found {
        take_over(ctx, me, found, &key, role, None, now)?;
    }
    ctx.emit_set(&SessionIdentity {
        id: SessionIdentityId(Arc::from(me)),
        key,
        role,
        assigned_at: now,
    })?;
    Ok(found.map(|f| f.nickname))
}

/// For a session that already has a nickname: record the identity key it
/// has now if it has none, as a session named before MUS-3 hasn't. A key once
/// recorded is frozen, like the nickname: the hooks and the MCP shim both
/// write the row, and a key that followed every write could flap between them.
pub fn record_identity(
    ctx: &CommandContext,
    session: &Session,
    now: i64,
) -> Result<(), CommandError> {
    let me = session.id.0.as_ref();
    let identities: Vec<Arc<SessionIdentity>> = ctx.exec_query(GetAllSessionIdentitys {})?;
    if identities.iter().any(|i| i.id.0.as_ref() == me) {
        return Ok(());
    }
    let Some((key, role)) = identity_key(session) else {
        return Ok(());
    };
    ctx.emit_set(&SessionIdentity {
        id: SessionIdentityId(Arc::from(me)),
        key,
        role,
        assigned_at: now,
    })
}

/// The sweeper's late check: a session named while its predecessor was
/// pending takes the predecessor's nickname over once it counts as dead,
/// within the swap window.
pub fn recheck_pending(ctx: &CommandContext, now: i64) {
    let cfg = Config::from_env();
    let pending: Vec<(Arc<str>, i64)> = {
        let o = OBSERVED.lock().unwrap_or_else(|e| e.into_inner());
        o.pending
            .iter()
            .map(|(a, until)| (a.clone(), *until))
            .collect()
    };
    if pending.is_empty() {
        return;
    }
    let _naming = NAMING.lock().unwrap_or_else(|e| e.into_inner());
    let (Ok(sessions), Ok(identities), Ok(nicknames)) = (
        ctx.exec_query(GetAllSessions {}),
        ctx.exec_query(GetAllSessionIdentitys {}),
        ctx.exec_query(GetAllSessionNicknames {}),
    ) else {
        return;
    };
    for (successor, until) in pending {
        let done = |o: &mut Observed| {
            o.pending.remove(&successor);
        };
        let row = sessions.iter().find(|s| s.id.0 == successor);
        let ident = identities.iter().find(|i| i.id.0 == successor);
        let (Some(row), Some(ident)) = (row, ident) else {
            done(&mut OBSERVED.lock().unwrap_or_else(|e| e.into_inner()));
            continue;
        };
        if now > until || now - ident.assigned_at > cfg.window_ms {
            done(&mut OBSERVED.lock().unwrap_or_else(|e| e.into_inner()));
            continue;
        }
        let Some(fresh) = nicknames.iter().find(|n| n.id.0 == successor) else {
            continue;
        };
        // Recorded again, with the same deadline (it's measured from when the
        // session registered), if a predecessor may still die.
        done(&mut OBSERVED.lock().unwrap_or_else(|e| e.into_inner()));
        let found = match find_predecessor(
            ctx,
            row,
            &ident.key,
            ident.role,
            ident.assigned_at,
            now,
            &cfg,
        ) {
            Ok(Some(found)) => found,
            Ok(None) => continue,
            Err(e) => {
                log::warn!("[identity] late check for {successor} failed: {e:?}");
                continue;
            }
        };
        let alias = Some((fresh.nickname.clone(), now + cfg.alias_ms));
        if let Err(e) = take_over(ctx, &successor, &found, &ident.key, ident.role, alias, now) {
            log::warn!("[identity] late takeover for {successor} failed: {e:?}");
        }
    }
}

/// A command context for the daemon's own writes, as the hook listener uses.
pub fn internal_ctx(ctx: &myko::server::MykoServerContext) -> CommandContext {
    let tx: Arc<str> = uuid::Uuid::new_v4().to_string().into();
    let req = myko::request::RequestContext::internal(tx, ctx.host_id, "succession");
    CommandContext::new(
        Arc::from("succession"),
        Arc::new(req),
        Arc::new(ctx.clone()),
    )
}

/// A line telling `sid` its nickname changed in a late swap, once. `None` when
/// there's nothing to tell.
pub fn take_notice(ctx: &CommandContext, sid: &str) -> Option<String> {
    let successions: Vec<Arc<Succession>> = ctx.exec_query(GetAllSuccessions {}).ok()?;
    let s = successions
        .iter()
        .find(|s| s.to_session_id == sid && s.alias.is_some() && !s.told)?;
    let mut told = (**s).clone();
    told.told = true;
    if let Err(e) = ctx.emit_set(&told) {
        log::warn!("[identity] marking {sid}'s notice told failed: {e:?}");
    }
    Some(format!(
        "<marshal_notice>Your Marshal nickname is now {} (you took over from session {}, which ended). \
         Your earlier nickname {} still reaches you for a while.</marshal_notice>\n",
        s.nickname,
        s.id.0,
        s.alias.as_deref().unwrap_or("")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use marshal_entities::HostInfo;

    fn cfg() -> Config {
        Config {
            grace_ms: 30_000,
            quiet_ms: 600_000,
            window_ms: 600_000,
            alias_ms: 600_000,
            settle_ms: 60_000,
            bridge_grace_ms: 120_000,
        }
    }

    fn row(harness: &str, client: Option<&str>, last: i64) -> Session {
        Session {
            id: SessionId(Arc::from("pred")),
            client_id: client.map(|c| myko::entities::client::ClientId(Arc::from(c))),
            agent_id: None,
            pid: 1,
            cwd: "/work".into(),
            git_branch: None,
            current_task: None,
            session_name: None,
            activity: None,
            kind: None,
            connected_at: 0,
            last_activity_at: Some(last),
            last_tool: None,
            last_tool_at: None,
            operator: Some("op@example.com".into()),
            host: Some(HostInfo {
                name: "a1".into(),
                os: String::new(),
                arch: String::new(),
            }),
            project: None,
            channels_enabled: None,
            acks_pushes: None,
            harness: Some(harness.into()),
            identity: None,
        }
    }

    fn dead(v: Verdict) -> bool {
        matches!(v, Verdict::Dead(_))
    }

    #[test]
    fn a_claude_whose_client_dropped_past_the_grace_is_dead() {
        let r = row("claude", Some("c1"), 1_000);
        let f = Facts {
            row: Some(&r),
            client_gone_at: Some(2_000),
            ..Default::default()
        };
        assert!(dead(verdict(&f, 2_000 + 30_000, &cfg())));
    }

    #[test]
    fn inside_the_grace_it_is_pending() {
        let r = row("claude", Some("c1"), 1_000);
        let f = Facts {
            row: Some(&r),
            client_gone_at: Some(2_000),
            ..Default::default()
        };
        assert!(matches!(
            verdict(&f, 2_000 + 29_999, &cfg()),
            Verdict::Pending(_)
        ));
    }

    #[test]
    fn a_session_that_hooked_after_its_client_dropped_is_live() {
        // The hooked_since guard: its MCP shim crashed, the session didn't.
        let r = row("claude", None, 5_000);
        let f = Facts {
            row: Some(&r),
            client_gone_at: Some(2_000),
            ..Default::default()
        };
        assert!(matches!(verdict(&f, 1_000_000, &cfg()), Verdict::Live(_)));
    }

    #[test]
    fn a_connected_client_or_a_waiter_is_live() {
        let r = row("claude", Some("c1"), 0);
        let connected = Facts {
            row: Some(&r),
            client_live: true,
            ..Default::default()
        };
        assert!(matches!(
            verdict(&connected, 10_000_000, &cfg()),
            Verdict::Live(_)
        ));
        let waiting = Facts {
            row: Some(&r),
            waiting: true,
            client_gone_at: Some(0),
            ..Default::default()
        };
        assert!(matches!(
            verdict(&waiting, 10_000_000, &cfg()),
            Verdict::Live(_)
        ));
    }

    #[test]
    fn a_client_gone_before_the_sweeper_timed_it_is_pending_not_dead() {
        let r = row("claude", Some("c1"), 0);
        let f = Facts {
            row: Some(&r),
            ..Default::default()
        };
        assert!(matches!(
            verdict(&f, 10_000_000, &cfg()),
            Verdict::Pending(_)
        ));
    }

    #[test]
    fn a_row_that_never_had_a_client_needs_the_quiet_time() {
        let r = row("claude", None, 1_000);
        let f = Facts {
            row: Some(&r),
            ..Default::default()
        };
        assert!(matches!(
            verdict(&f, 1_000 + 599_999, &cfg()),
            Verdict::Live(_)
        ));
        assert!(dead(verdict(&f, 1_000 + 600_000, &cfg())));
    }

    /// MAR-54: a present Codex row's verdict follows its bridge's wake record.
    #[test]
    fn a_codex_row_is_dead_only_after_its_bridge_record_has_been_gone_the_grace() {
        let r = row("codex", None, 0);
        let at = |bridge_seen, bridge_gone_at| Facts {
            row: Some(&r),
            bridge_seen,
            bridge_gone_at,
            ..Default::default()
        };
        let late = 100_000_000;
        assert!(
            matches!(verdict(&at(false, None), late, &cfg()), Verdict::Live(_)),
            "never seen with a bridge record: never dead"
        );
        assert!(
            matches!(verdict(&at(true, None), late, &cfg()), Verdict::Live(_)),
            "its bridge record is current: live"
        );
        let gone = late - 60_000;
        assert!(
            matches!(
                verdict(&at(true, Some(gone)), late, &cfg()),
                Verdict::Pending(_)
            ),
            "inside the bridge grace: pending"
        );
        let gone = late - 120_000;
        assert!(
            dead(verdict(&at(true, Some(gone)), late, &cfg())),
            "gone the grace, no hook since, quiet: dead"
        );
        // A hook after the record ended: it's alive.
        let hooked = row("codex", None, gone + 1_000);
        let f = Facts {
            row: Some(&hooked),
            bridge_seen: true,
            bridge_gone_at: Some(gone),
            ..Default::default()
        };
        assert!(matches!(verdict(&f, late, &cfg()), Verdict::Live(_)));
        // Gone the grace but active inside the quiet time: not yet.
        let recent = row("codex", None, late - 300_000);
        let f = Facts {
            row: Some(&recent),
            bridge_seen: true,
            bridge_gone_at: Some(late - 200_000),
            ..Default::default()
        };
        assert!(matches!(verdict(&f, late, &cfg()), Verdict::Pending(_)));
        // A waiter beats everything.
        let f = Facts {
            waiting: true,
            ..at(true, Some(gone))
        };
        assert!(matches!(verdict(&f, late, &cfg()), Verdict::Live(_)));
    }

    /// MAR-54: a bridge leadership handover (MAR-4) ends the old leader's
    /// record before the new leader's appears. A 45 s gap never makes the row
    /// dead.
    #[test]
    fn a_bridge_handover_gap_keeps_a_codex_row_live() {
        let id: Arc<str> = Arc::from(format!("handover-{}", uuid::Uuid::new_v4()).as_str());
        let r = row("codex", None, 0);
        let none: HashSet<Arc<str>> = HashSet::new();
        let some: HashSet<Arc<str>> = [id.clone()].into_iter().collect();
        let check = |now: i64| {
            let o = OBSERVED.lock().unwrap_or_else(|e| e.into_inner());
            let watching = HashSet::new();
            verdict(&facts_for(&o, &id, Some(&r), &watching), now, &cfg())
        };
        let t0 = 50_000_000;
        observe_bridges(&some, t0 - 30_000);
        assert!(matches!(check(t0 - 30_000), Verdict::Live(_)));
        // The old leader's record ends; ticks every 3 s through the gap.
        for t in (t0..t0 + 45_000).step_by(3_000) {
            observe_bridges(&none, t);
            assert!(
                !dead(check(t)),
                "dead {}s into the handover gap",
                (t - t0) / 1000
            );
        }
        // The new leader's record appears 45 s later.
        observe_bridges(&some, t0 + 45_000);
        for t in (t0 + 45_000..t0 + 400_000).step_by(30_000) {
            observe_bridges(&some, t);
            assert!(
                matches!(check(t), Verdict::Live(_)),
                "live after the handover"
            );
        }
    }

    #[test]
    fn a_removed_row_is_dead_only_if_this_daemon_saw_it_go() {
        let seen = Facts {
            removed_at: Some(5),
            ..Default::default()
        };
        assert!(dead(verdict(&seen, 10, &cfg())));
        let unseen = Facts::default();
        assert!(matches!(verdict(&unseen, 10, &cfg()), Verdict::Live(_)));
        let since_last_sweep = Facts {
            known: true,
            ..Default::default()
        };
        assert!(matches!(
            verdict(&since_last_sweep, 10, &cfg()),
            Verdict::Pending(_)
        ));
    }

    #[test]
    fn the_key_is_harness_and_cwd_or_a_declared_role() {
        let mut s = row("claude", None, 0);
        assert_eq!(
            identity_key(&s),
            Some(("cwd:claude|/work|op@example.com|a1".into(), false))
        );
        s.identity = Some("watchman".into());
        assert_eq!(
            identity_key(&s),
            Some(("role:watchman|op@example.com|a1".into(), true))
        );
        s.identity = Some("none".into());
        assert_eq!(identity_key(&s), None);
        s.identity = None;
        s.harness = None;
        assert_eq!(identity_key(&s), None, "no harness, nothing to infer from");
    }

    #[test]
    fn an_old_id_follows_the_chain() {
        let rec = |from: &str, to: &str| {
            Arc::new(Succession {
                id: SuccessionId(Arc::from(from)),
                to_session_id: to.into(),
                nickname: "n".into(),
                key: "k".into(),
                reason: "r".into(),
                at: 0,
                alias: None,
                alias_until: None,
                told: false,
                handed_over: Vec::new(),
            })
        };
        let chain = vec![rec("a", "b"), rec("b", "c")];
        assert_eq!(
            marshal_entities::successor_of(&chain, "a").as_deref(),
            Some("c")
        );
        assert_eq!(marshal_entities::successor_of(&chain, "c"), None);
    }
}

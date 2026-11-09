//! `WakeWatcher`: the daemon's record that a session can be woken: a held
//! `/hook/wait`, or a Codex bridge that can start a turn for it.
//!
//! This is separate from `Session`, which each session writes for itself. The
//! daemon's hook listener writes this one, but any connected client can also
//! write one, so it's the daemon's record, not proof.
//!
//! A session can have more than one wake path, and each path has a record of
//! its own (MAR-4), so ending one path leaves the others. A held `/hook/wait`'s
//! record has the session id as its `id` and no `session_id`, as all records
//! did before, so older readers still see Claude's waiters. A Codex bridge's
//! record has its own `id` (see [`WakeWatcher::bridge_id`]) and names its
//! session in `session_id`. Use [`WakeWatcher::session`] to find the session.

use std::sync::Arc;

use myko::myko_item;
use myko::prelude::{EventPublishing as _, Querying as _, RegistryScoped as _, RequestScoped as _};

use crate::Session;

#[myko_item]
pub struct WakeWatcher {
    /// When this record stops counting, in ms since the epoch. A waiter that
    /// vanishes without the daemon noticing stops counting by itself. Records
    /// aren't persisted, because a wait doesn't outlive the daemon.
    pub until: i64,
    /// The session this path can wake, when the record's `id` isn't that
    /// session's id: a Codex bridge's record (MAR-4). Absent on a held
    /// `/hook/wait`'s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<crate::SessionId>,
}

impl WakeWatcher {
    /// The session this record says can be woken.
    pub fn session(&self) -> &str {
        self.session_id
            .as_ref()
            .map_or(self.id.0.as_ref(), |s| s.0.as_ref())
    }

    /// The record id for one Codex bridge's wake path to `session`. `bridge`
    /// names the bridge process, so a bridge that has handed over wake
    /// leadership ends only its own record, not the new leader's.
    pub fn bridge_id(session: &str, bridge: &str) -> String {
        format!("{session}#bridge:{bridge}")
    }
}

/// Whether a message would wake a session, as far as Marshal can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeState {
    /// Observed: the daemon has a current waiter for this session.
    Waiting,
    /// Declared: the session says it has a live channel. Not observed.
    Channel,
    /// Declared: it reports a turn in progress. Claude Code's wait hook starts
    /// again when the turn ends, so having no waiter now isn't a fault.
    Busy,
    /// No sign that a message would wake it. It'll see the mail on its next turn,
    /// if it has one.
    None,
}

impl WakeState {
    pub fn as_str(self) -> &'static str {
        match self {
            WakeState::Waiting => "waiting",
            WakeState::Channel => "channel",
            WakeState::Busy => "busy",
            WakeState::None => "none",
        }
    }
}

/// Work out a session's wake state. Pure, so each reader gets the same answer.
pub fn wake_state(session: &Session, watchers: &[Arc<WakeWatcher>], now_ms: i64) -> WakeState {
    let waiting = watchers
        .iter()
        .any(|w| w.session() == session.id.0.as_ref() && w.until > now_ms);
    if waiting {
        WakeState::Waiting
    } else if session.client_id.is_some() && session.channels_enabled == Some(true) {
        WakeState::Channel
    } else if session.activity.as_deref() == Some("busy") {
        WakeState::Busy
    } else {
        WakeState::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionId;
    use myko::entities::client::ClientId;

    fn session(id: &str) -> Session {
        Session {
            id: SessionId(Arc::from(id)),
            client_id: None,
            agent_id: None,
            pid: 0,
            cwd: String::new(),
            git_branch: None,
            current_task: None,
            session_name: None,
            activity: None,
            activity_at: None,
            kind: None,
            connected_at: 0,
            last_activity_at: None,
            last_tool: None,
            last_tool_at: None,
            operator: None,
            host: None,
            project: None,
            channels_enabled: None,
            acks_pushes: None,
            harness: None,
            identity: None,
        }
    }

    fn watcher(id: &str, until: i64) -> Arc<WakeWatcher> {
        Arc::new(WakeWatcher {
            id: WakeWatcherId(Arc::from(id)),
            until,
            session_id: None,
        })
    }

    fn bridge_watcher(session: &str, bridge: &str, until: i64) -> Arc<WakeWatcher> {
        Arc::new(WakeWatcher {
            id: WakeWatcherId(Arc::from(WakeWatcher::bridge_id(session, bridge))),
            until,
            session_id: Some(crate::SessionId(Arc::from(session))),
        })
    }

    #[test]
    fn a_bridge_record_counts_for_the_session_it_names() {
        let w = bridge_watcher("a", "b1", 101);
        assert_eq!(w.session(), "a");
        assert_eq!(
            wake_state(&session("a"), std::slice::from_ref(&w), 100),
            WakeState::Waiting
        );
        assert_eq!(wake_state(&session("b1"), &[w], 100), WakeState::None);
    }

    #[test]
    fn a_hook_wait_record_is_keyed_by_its_session_as_before() {
        let w = watcher("a", 101);
        assert_eq!(w.session(), "a");
        let json = serde_json::to_value(&*w).unwrap();
        assert!(json.get("sessionId").is_none() && json.get("session_id").is_none());
    }

    #[test]
    fn a_current_waiter_counts_and_an_expired_one_does_not() {
        let s = session("a");
        assert_eq!(
            wake_state(&s, &[watcher("a", 101)], 100),
            WakeState::Waiting
        );
        assert_eq!(wake_state(&s, &[watcher("a", 100)], 100), WakeState::None);
    }

    #[test]
    fn another_sessions_waiter_does_not_count() {
        assert_eq!(
            wake_state(&session("a"), &[watcher("b", 101)], 100),
            WakeState::None
        );
    }

    #[test]
    fn a_channel_needs_a_connection() {
        let mut s = session("a");
        s.channels_enabled = Some(true);
        assert_eq!(wake_state(&s, &[], 0), WakeState::None);
        s.client_id = Some(ClientId(Arc::from("c")));
        assert_eq!(wake_state(&s, &[], 0), WakeState::Channel);
    }

    #[test]
    fn a_busy_session_is_not_reported_as_unwakeable() {
        let mut s = session("a");
        s.activity = Some("busy".into());
        assert_eq!(wake_state(&s, &[], 0), WakeState::Busy);
    }
}

//! `marshal-shim roster`: print the roster as JSON for other programs.
//!
//! It only subscribes to queries. It registers no session, so reading the roster
//! doesn't change it and the reader doesn't appear on it.
//!
//! Each session's facts are split by who set them:
//!
//! - `recorded`: the daemon's record. myko fills `client_id` from the
//!   connection, and the daemon's hook listener writes a `WakeWatcher` while
//!   `/hook/wait` is held. But any connected client can also write either: it
//!   can SET a `WakeWatcher`, or SET another session's id and rebind it. So
//!   these are the daemon's record, not proof, and even when true they show a
//!   connection or a wait, not that the session behind it is working.
//! - `declared`: what the session's own software reports, including its machine,
//!   operator and `last_activity_at`. Any client can set these to anything.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use chrono::Utc;
use hyphae::Gettable;
use marshal_entities::{
    GetAllSessionNicknames, GetAllSessions, GetAllWakeWatchers, Session, SessionNickname,
    WakeWatcher, wake_state,
};
use myko::client::{ConnectionStatus, MykoClient, QueryMapWatch};
use serde_json::{Value, json};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(100);

fn view(
    sessions: &[Arc<Session>],
    nicknames: &[Arc<SessionNickname>],
    watchers: &[Arc<WakeWatcher>],
    now_ms: i64,
) -> Value {
    let rows: Vec<Value> = sessions
        .iter()
        .map(|s| {
            let id = s.id.0.as_ref();
            let nickname = nicknames
                .iter()
                .find(|n| n.id.0.as_ref() == id)
                .map(|n| n.nickname.clone())
                .unwrap_or_else(|| marshal_entities::nickname(id));
            let waiting_until = watchers
                .iter()
                .find(|w| w.session() == id && w.until > now_ms)
                .map(|w| w.until);
            json!({
                "session_id": id,
                "nickname": nickname,
                "wake": wake_state(s, watchers, now_ms).as_str(),
                "recorded": {
                    // The row is bound to a connection. It can outlast that
                    // connection until the daemon's sweep reaps it.
                    "bound_to_connection": s.client_id.is_some(),
                    "waiting": waiting_until.is_some(),
                    "waiting_until": waiting_until,
                },
                "declared": {
                    "machine": s.host.as_ref().map(|h| h.name.as_str()),
                    "operator": s.operator,
                    "activity": s.activity,
                    "activity_at": s.activity_at,
                    "last_activity_at": s.last_activity_at,
                    "last_tool": s.last_tool,
                    "last_tool_at": s.last_tool_at,
                    "connected_at": s.connected_at,
                    "current_task": s.current_task,
                    "session_name": s.session_name,
                    "kind": s.kind,
                    "project": s.project,
                    "cwd": s.cwd,
                    "git_branch": s.git_branch,
                    "pid": s.pid,
                    "channels_enabled": s.channels_enabled,
                    "harness": s.harness,
                    // MUS-3: a claimed role, which reaches this session by name.
                    "role": s.identity.as_deref().filter(|i| *i != "none"),
                },
            })
        })
        .collect();
    json!({ "read_at": now_ms, "sessions": rows })
}

pub async fn run() -> Result<()> {
    marshal_entities::link();
    let client = MykoClient::new();
    // `_state` watches carry the daemon's first answer as its own signal, so an
    // empty roster is a fact once it's arrived, not a wait (MAR-57).
    let sessions = client.watch_query_map_state::<GetAllSessions>(GetAllSessions {});
    let nicknames =
        client.watch_query_map_state::<GetAllSessionNicknames>(GetAllSessionNicknames {});
    let watchers = client.watch_query_map_state::<GetAllWakeWatchers>(GetAllWakeWatchers {});
    let address = crate::daemon_address();
    crate::tls_link::connect(&client, address.clone());

    let started = Instant::now();
    while !(sessions.ready().get() && nicknames.ready().get() && watchers.ready().get()) {
        if started.elapsed() > CONNECT_TIMEOUT {
            if matches!(
                client.connection_status_sync(),
                ConnectionStatus::Connected(_)
            ) {
                bail!("connected to {address}, but it sent no roster within {CONNECT_TIMEOUT:?}");
            }
            bail!("couldn't reach the daemon at {address} within {CONNECT_TIMEOUT:?}");
        }
        tokio::time::sleep(POLL).await;
    }

    let mut sessions = values(&sessions);
    // A map has no order of its own: list sessions oldest connection first.
    sessions
        .sort_by(|a, b| (a.connected_at, a.id.0.as_ref()).cmp(&(b.connected_at, b.id.0.as_ref())));
    let output = view(
        &sessions,
        &values(&nicknames),
        &values(&watchers),
        Utc::now().timestamp_millis(),
    );
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn values<T: hyphae::CellValue>(watch: &QueryMapWatch<T>) -> Vec<Arc<T>> {
    watch.map().snapshot().into_iter().map(|(_, v)| v).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use marshal_entities::{HostInfo, SessionId, WakeWatcherId};

    fn session() -> Arc<Session> {
        Arc::new(Session {
            id: SessionId(Arc::from("a")),
            client_id: None,
            pid: 7,
            cwd: "/w".into(),
            git_branch: None,
            current_task: Some("anything at all".into()),
            session_name: None,
            activity: Some("idle".into()),
            activity_at: Some(3),
            kind: None,
            connected_at: 1,
            last_activity_at: Some(2),
            last_tool: None,
            last_tool_at: None,
            operator: Some("op".into()),
            host: Some(HostInfo {
                name: "host-a".into(),
                os: String::new(),
                arch: String::new(),
            }),
            project: None,
            channels_enabled: None,
            acks_pushes: None,
            agent_id: None,
            harness: None,
            identity: None,
        })
    }

    #[test]
    fn claims_are_kept_apart_from_the_daemons_record() {
        let v = view(&[session()], &[], &[], 100);
        let row = &v["sessions"][0];
        assert_eq!(v["read_at"], 100);
        assert_eq!(row["declared"]["machine"], "host-a");
        assert!(row["recorded"].get("machine").is_none());
        assert!(row["recorded"].get("operator").is_none());
        assert_eq!(row["declared"]["current_task"], "anything at all");
        assert_eq!(row["declared"]["last_activity_at"], 2);
        // MAR-58: when the activity last changed sits beside it, as a claim.
        assert_eq!(row["declared"]["activity"], "idle");
        assert_eq!(row["declared"]["activity_at"], 3);
        assert!(row["recorded"].get("activity_at").is_none());
        assert!(row["recorded"].get("current_task").is_none());
        assert!(row["recorded"].get("last_activity_at").is_none());
    }

    #[test]
    fn waiting_comes_from_a_current_watcher() {
        let watcher = |until| {
            Arc::new(WakeWatcher {
                id: WakeWatcherId(Arc::from("a")),
                until,
                session_id: None,
            })
        };
        let v = view(&[session()], &[], &[watcher(101)], 100);
        assert_eq!(v["sessions"][0]["wake"], "waiting");
        assert_eq!(v["sessions"][0]["recorded"]["waiting_until"], 101);
        let v = view(&[session()], &[], &[watcher(100)], 100);
        assert_eq!(v["sessions"][0]["wake"], "none");
        assert_eq!(v["sessions"][0]["recorded"]["waiting"], false);
    }
}

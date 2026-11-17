//! The unattended first task, with a real Claude and nobody at the keyboard.
//!
//! A launched Claude has no channels flag (rule 1 adds no flags), so its mail
//! arrives through its hooks: `marshal-shim claude-hook wait` holds a
//! long-poll that wakes the idle session when a message comes. This test
//! launches a real `claude` through the built-in tmux adapter, lets
//! `marshal_launch::launch` send the first task, and never types into the
//! pane. It passes when the launched session marks the task read and does
//! what it says: send a token back to an observer row this test holds.
//!
//! It is `#[ignore]`d: it needs a real daemon (the hook port is the daemon's,
//! not the in-process cell's in launch_e2e.rs), a logged-in Claude with
//! marshal-shim wired (the plugin, or by hand with the `claude-hook wait`
//! hook), and it spends real model tokens. Run it with
//!
//! ```text
//! MARSHAL_E2E_DAEMON=ws://<host>:6155 \
//! MARSHAL_SHIM_BIN=<dir>/marshal-shim \
//! cargo test -p marshal-launch --test first_task_e2e -- --ignored --nocapture
//! ```
//!
//! - `MARSHAL_E2E_DAEMON` (required): the daemon, at this tree's version.
//! - `MARSHAL_SHIM_BIN`: the shim the launch checks, and whose folder goes
//!   first on the agent's PATH, so Claude's `marshal-shim` hooks and MCP
//!   server run it. Point it at a shim built from this tree when the
//!   installed one is another major.minor.
//! - `MARSHAL_E2E_CWD`: the folder to launch in. It must already be trusted
//!   by Claude, or Claude stops at the trust prompt with nobody to answer.
//!   Defaults to this crate's folder.
//! - `MARSHAL_E2E_CLAUDE_CONFIG_DIR`: a `CLAUDE_CONFIG_DIR` for the agent
//!   (a prepared login and wiring), instead of the operator's own.
//! - `MARSHAL_E2E_TIMEOUT_SECS`: how long the agent has to act (default 300).

#![cfg(unix)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hyphae::Gettable;
use marshal_entities::{
    DeleteSession, DeleteSessionResult, GetAllMessageReads, GetAllMessages, HostInfo,
    RegisterSession, Session, SessionId,
};
use marshal_launch::join::Roster;
use marshal_launch::tmux::{Tmux, this_host};
use marshal_launch::{Adapter, Plan};

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

/// A plain roster row the agent can address, held by this test's connection.
fn observer(id: &str) -> Session {
    let mut s: Session = serde_json::from_value(serde_json::json!({
        "id": id,
        "pid": std::process::id(),
        "cwd": env!("CARGO_MANIFEST_DIR"),
        "connectedAt": 1,
        "sessionName": "first-task-e2e observer",
    }))
    .unwrap();
    s.host = Some(HostInfo {
        name: this_host(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
    });
    s
}

async fn wait_for<T>(
    what: &str,
    timeout: Duration,
    mut found: impl FnMut() -> Option<T>,
    pane: impl Fn() -> String,
) -> T {
    let started = Instant::now();
    loop {
        if let Some(t) = found() {
            return t;
        }
        if started.elapsed() > timeout {
            panic!(
                "{what} didn't happen within {}s. The agent's pane:\n{}",
                timeout.as_secs(),
                pane()
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[test]
#[ignore = "needs a running daemon and a logged-in Claude with marshal-shim wired; see the module docs"]
fn an_unattended_claude_wakes_for_its_first_task_and_acts_on_it() {
    let daemon = env("MARSHAL_E2E_DAEMON")
        .expect("set MARSHAL_E2E_DAEMON to a running daemon's ws:// address (see the module docs)");
    let Some(server) = common::PrivateTmux::new("first-task") else {
        return;
    };
    let cwd =
        PathBuf::from(env("MARSHAL_E2E_CWD").unwrap_or_else(|| env!("CARGO_MANIFEST_DIR").into()));
    let cwd = std::fs::canonicalize(&cwd).expect("MARSHAL_E2E_CWD must exist");
    let act_within = Duration::from_secs(
        env("MARSHAL_E2E_TIMEOUT_SECS")
            .and_then(|s| s.parse().ok())
            .unwrap_or(300),
    );

    // The agent's PATH: the checked shim's folder first, so its hooks and MCP
    // server run the same shim.
    let mut path = std::env::var("PATH").unwrap_or_default();
    if let Some(dir) = env("MARSHAL_SHIM_BIN")
        .map(PathBuf::from)
        .and_then(|b| b.parent().map(PathBuf::from))
    {
        path = format!("{}:{path}", dir.display());
    }
    let mut tmux = Tmux::new("tmux")
        .with_socket(&server.socket)
        .with_client_env("PATH", path);
    if let Some(dir) = env("MARSHAL_E2E_CLAUDE_CONFIG_DIR") {
        tmux = tmux.with_client_env("CLAUDE_CONFIG_DIR", dir);
    }

    let name = format!("first-task-e2e-{}", std::process::id());
    let observer_id = uuid::Uuid::new_v4().to_string();
    let token = format!("first-task-{}", uuid::Uuid::new_v4().simple());
    let task = format!(
        "This is an automated test of unattended task delivery. Use the marshal \
         send_message tool once: send the text {token} to session {observer_id}. \
         Do nothing else, and don't wait for a reply."
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let roster = Roster::connect(&daemon)
            .await
            .expect("connect to the daemon");
        let client = roster.client();
        let messages = client.watch_query::<GetAllMessages>(GetAllMessages {});
        let reads = client.watch_query::<GetAllMessageReads>(GetAllMessageReads {});
        client.send_command::<RegisterSession, ()>(&RegisterSession {
            session: observer(&observer_id),
        });
        let pane = || server.pane(&name);
        wait_for(
            "the observer row appearing",
            Duration::from_secs(10),
            || {
                roster
                    .sessions()
                    .iter()
                    .any(|s| s.id.0.as_ref() == observer_id)
                    .then_some(())
            },
            String::new,
        )
        .await;

        let plan = Plan {
            harness: "claude".into(),
            cwd: cwd.display().to_string(),
            name: name.clone(),
            launch_address: daemon.clone(),
            daemon_address: daemon.clone(),
            timeout: Duration::from_secs(120),
            check: true,
            options: vec![],
            task: Some(task),
            records: Some(
                PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}/launches.json")),
            ),
        };
        let outcome = match marshal_launch::launch(&tmux, &plan).await {
            Ok(o) => o,
            Err(e) => panic!("launch failed: {e:#}\nThe agent's pane:\n{}", pane()),
        };
        let agent = outcome.joined.session_id.clone();
        let task_id = outcome
            .task_message_id
            .clone()
            .expect("the first task was sent");

        // Nobody types: the waiter has to wake the idle session.
        let read_at = wait_for(
            "the launched session marking its first task read",
            act_within,
            || {
                reads.get().iter().find_map(|r| {
                    (r.message_id.0.as_ref() == task_id && r.session_id.0.as_ref() == agent)
                        .then_some(r.read_at)
                })
            },
            pane,
        )
        .await;
        let reply = wait_for(
            "the launched agent sending the token back",
            act_within,
            || {
                messages.get().iter().find_map(|m| {
                    (m.from_session_id.0.as_ref() == agent
                        && m.to_session_id
                            .as_ref()
                            .is_some_and(|t| t.0.as_ref() == observer_id)
                        && m.body.contains(&token))
                    .then(|| m.body.clone())
                })
            },
            pane,
        )
        .await;
        eprintln!("first task read at {read_at}; the agent answered: {reply}");

        tmux.stop(&outcome.launched.handle).unwrap();
        let _ = client.send_command::<DeleteSession, DeleteSessionResult>(&DeleteSession {
            id: SessionId(Arc::from(observer_id.as_str())),
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
    });
}

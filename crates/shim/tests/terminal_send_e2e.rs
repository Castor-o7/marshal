//! `marshal-shim tell` from two terminals at once: each run registers its own
//! short-lived Session, so the first to finish can't take the other's sender
//! off the roster.
//!
//! These launch the real `marshal-shim` against a real in-process Myko cell.

#![cfg(unix)]

mod support;

use std::{
    net::SocketAddr,
    path::Path,
    process::Output,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use marshal_entities::{GetAllMessages, GetAllSessions, HostInfo, Session, SessionId};
use myko::prelude::EventPublishing as _;
use myko::{
    command::CommandContext,
    request::RequestContext,
    server::{MykoServerContext, Persister},
};
use myko_server::{BlackholePersister, MykoServer};

const RECIPIENT: &str = "terminal-e2e-recipient-0001";
const WAIT: Duration = Duration::from_secs(10);

struct Cell {
    address: String,
    ctx: Arc<MykoServerContext>,
    shutdown: Option<std::sync::mpsc::Sender<()>>,
    join: Option<thread::JoinHandle<()>>,
}

impl Cell {
    /// A cell on a free port. The port is picked by binding port 0 and
    /// closing it, so another process can take it first: then it's picked
    /// again (MAR-21).
    fn spawn() -> Self {
        marshal_entities::link();
        let mut lost = Vec::new();
        for _ in 0..5 {
            let bind = std::net::TcpListener::bind(("127.0.0.1", 0))
                .and_then(|listener| listener.local_addr())
                .expect("pick a free port");
            match Self::try_spawn(bind) {
                Ok(cell) => return cell,
                Err(why) => lost.push(format!("{bind}: {why}")),
            }
        }
        panic!("no free port held for the test cell: {lost:?}");
    }

    fn try_spawn(bind: SocketAddr) -> Result<Self, String> {
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (failed_tx, failed_rx) = std::sync::mpsc::channel::<String>();
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
        let join = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("build cell runtime");
            runtime.block_on(async move {
                let blackhole: Arc<dyn Persister> = Arc::new(BlackholePersister);
                let server = Arc::new(
                    MykoServer::builder()
                        .with_bind_addr(bind)
                        .with_default_persister(blackhole)
                        .build(),
                );
                ready_tx.send(server.ctx()).expect("publish cell context");
                tokio::select! {
                    result = server.run() => {
                        if let Err(e) = result {
                            let _ = failed_tx.send(e.to_string());
                        }
                    }
                    _ = tokio::task::spawn_blocking(move || shutdown_rx.recv()) => {}
                }
            });
        });
        let ctx = Arc::new(ready_rx.recv_timeout(WAIT).expect("cell context"));
        // A port another process holds accepts connections too; the cell's
        // own bind failure comes at once, so it's waited for a moment.
        let deadline = Instant::now() + WAIT;
        while std::net::TcpStream::connect(bind).is_err() {
            if let Ok(why) = failed_rx.try_recv() {
                // The thread's runtime waits on the shutdown channel as it ends.
                drop(shutdown_tx);
                let _ = join.join();
                return Err(why);
            }
            if Instant::now() >= deadline {
                return Err(format!("nothing listening on {bind}"));
            }
            thread::sleep(Duration::from_millis(25));
        }
        if let Ok(why) = failed_rx.recv_timeout(Duration::from_millis(200)) {
            // The thread's runtime waits on the shutdown channel as it ends.
            drop(shutdown_tx);
            let _ = join.join();
            return Err(why);
        }
        Ok(Self {
            address: format!("ws://{bind}"),
            ctx,
            shutdown: Some(shutdown_tx),
            join: Some(join),
        })
    }

    fn context(&self) -> CommandContext {
        let request = RequestContext::internal(
            uuid::Uuid::new_v4().to_string().into(),
            self.ctx.host_id,
            "terminal-send",
        );
        CommandContext::new(
            Arc::from("terminal-send"),
            Arc::new(request),
            Arc::clone(&self.ctx),
        )
    }

    /// A session to send to. It has no live client, so the mail just waits.
    fn add_recipient(&self) {
        self.context()
            .emit_set(&Session {
                id: SessionId(Arc::from(RECIPIENT)),
                client_id: None,
                agent_id: None,
                pid: 0,
                cwd: "/".into(),
                git_branch: None,
                current_task: None,
                session_name: None,
                activity: None,
                kind: None,
                connected_at: 1,
                last_activity_at: Some(1),
                last_tool: None,
                last_tool_at: None,
                operator: Some("terminal-send".into()),
                host: Some(HostInfo {
                    name: "terminal-send".into(),
                    os: std::env::consts::OS.into(),
                    arch: std::env::consts::ARCH.into(),
                }),
                project: None,
                channels_enabled: None,
                acks_pushes: None,
                harness: None,
                identity: None,
            })
            .expect("register recipient");
    }
}

impl Drop for Cell {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// `marshal-shim tell` with its own config directory, so no address file from
/// this machine is read: the cell's address comes from the environment.
fn tell(address: &str, home: &Path, body: &str) -> Output {
    support::shim(support::shared_home())
        .args(["tell", RECIPIENT, body])
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("APPDATA", home.join("appdata"))
        .env("MARSHAL_DAEMON_ADDRESS", address)
        .env_remove("MYKO_ADDRESS")
        .output()
        .expect("run marshal-shim tell")
}

/// `marshal-shim <args>` against `address`, isolated like [`tell`], with how
/// long it took.
fn shim_timed(address: &str, home: &Path, args: &[&str]) -> (Output, Duration) {
    let started = Instant::now();
    let out = support::shim(home)
        .args(args)
        .env("MARSHAL_DAEMON_ADDRESS", address)
        .output()
        .expect("run marshal-shim");
    (out, started.elapsed())
}

/// Well under the 10 s an unreachable daemon is given: an answer, not a timeout.
const PROMPT: Duration = Duration::from_secs(5);

#[test]
fn the_roster_of_a_daemon_with_no_sessions_is_an_empty_list_at_once() {
    // MAR-57: an empty roster used to look like one that hadn't arrived, so
    // `roster` waited the full timeout and exited 1.
    let cell = Cell::spawn();
    let home = tempfile::tempdir().expect("terminal home");
    let (out, took) = shim_timed(&cell.address, home.path(), &["roster"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{:?}: {stderr}", out.status);
    assert!(took < PROMPT, "took {took:?}");
    let roster: serde_json::Value = serde_json::from_slice(&out.stdout).expect("roster JSON");
    assert_eq!(roster["sessions"], serde_json::json!([]));
}

#[test]
fn the_roster_still_lists_the_sessions_there_are() {
    let cell = Cell::spawn();
    cell.add_recipient();
    let home = tempfile::tempdir().expect("terminal home");
    let (out, took) = shim_timed(&cell.address, home.path(), &["roster"]);
    assert!(out.status.success(), "{:?}", out.status);
    assert!(took < PROMPT, "took {took:?}");
    let roster: serde_json::Value = serde_json::from_slice(&out.stdout).expect("roster JSON");
    let ids: Vec<&str> = roster["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .filter_map(|s| s["session_id"].as_str())
        .collect();
    assert_eq!(ids, [RECIPIENT]);
}

#[test]
fn tell_to_an_unknown_name_on_an_empty_daemon_fails_at_once() {
    let cell = Cell::spawn();
    let home = tempfile::tempdir().expect("terminal home");
    let (out, took) = shim_timed(&cell.address, home.path(), &["tell", "nobody-here", "hi"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a send to nobody can't succeed");
    assert!(
        stderr.contains("no team or session named `nobody-here`"),
        "{stderr}"
    );
    assert!(took < PROMPT, "took {took:?}");
}

#[test]
fn two_terminal_runs_at_once_each_send_from_their_own_session() {
    let cell = Cell::spawn();
    cell.add_recipient();
    // Both runs share one config directory, as two terminals on one account do.
    let home = tempfile::tempdir().expect("terminal home");

    let runs: Vec<_> = ["first terminal", "second terminal"]
        .into_iter()
        .map(|body| {
            let address = cell.address.clone();
            let home = home.path().to_path_buf();
            thread::spawn(move || tell(&address, &home, body))
        })
        .collect();
    for run in runs {
        let out = run.join().expect("terminal thread");
        assert!(
            out.status.success(),
            "a terminal run failed: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let messages = cell
        .context()
        .exec_query(GetAllMessages {})
        .expect("read messages");
    let senders: Vec<&str> = ["first terminal", "second terminal"]
        .iter()
        .map(|body| {
            messages
                .iter()
                .find(|m| m.body.ends_with(body))
                .unwrap_or_else(|| panic!("no message ending {body:?}: {messages:?}"))
                .from_session_id
                .0
                .as_ref()
        })
        .collect();
    assert_ne!(
        senders[0], senders[1],
        "both runs sent from one session, so the first to exit removes the other's"
    );

    // Each run removes its own row on exit; none is left behind.
    let terminals: Vec<_> = cell
        .context()
        .exec_query(GetAllSessions {})
        .expect("read roster")
        .into_iter()
        .filter(|s| s.kind.as_deref() == Some("terminal"))
        .collect();
    assert!(terminals.is_empty(), "terminal rows left: {terminals:?}");
}

#[test]
fn a_later_run_from_the_same_terminal_gets_a_new_session() {
    let cell = Cell::spawn();
    cell.add_recipient();
    let home = tempfile::tempdir().expect("terminal home");
    for body in ["run one", "run two"] {
        let out = tell(&cell.address, home.path(), body);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let messages = cell
        .context()
        .exec_query(GetAllMessages {})
        .expect("read messages");
    let from = |body: &str| {
        messages
            .iter()
            .find(|m| m.body.ends_with(body))
            .map(|m| m.from_session_id.0.to_string())
            .unwrap_or_else(|| panic!("no message ending {body:?}"))
    };
    assert_ne!(from("run one"), from("run two"));
    // Nothing is written to keep an id between runs.
    assert!(
        !home.path().join("config/marshal/terminal-session").exists(),
        "a per-machine terminal id file was written"
    );
}

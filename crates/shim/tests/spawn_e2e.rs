//! `marshal-shim spawn` end to end (MAR-26): the real binary starts a harness
//! in a real, private tmux server, and reports it once its session joins a real
//! in-process Myko cell.
//!
//! The harness is a script standing in for `claude` (`MARSHAL_CLAUDE_BIN`). It
//! writes its working directory to a file, and the test registers the session
//! the way the daemon's hook listener would, so what's checked is spawn: that
//! tmux ran the harness in the right directory, and that spawn told the new
//! session from the ones already there.
//!
//! It needs tmux. Without it the tests say so and pass, except under CI, where
//! they fail.

#![cfg(unix)]

use std::{
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use marshal_entities::{Session, SessionId};
use myko::prelude::EventPublishing as _;
use myko::{
    command::CommandContext,
    request::RequestContext,
    server::{MykoServerContext, Persister},
};
use myko_server::{BlackholePersister, MykoServer};

const SHIM: &str = env!("CARGO_BIN_EXE_marshal-shim");
const WAIT: Duration = Duration::from_secs(10);

fn have_tmux() -> bool {
    let ok = Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        assert!(
            std::env::var_os("CI").is_none(),
            "tmux is missing, and CI must run the spawn tests"
        );
        eprintln!("tmux isn't installed: skipping the spawn end-to-end test");
    }
    ok
}

struct Cell {
    address: String,
    ctx: Arc<MykoServerContext>,
    shutdown: Option<std::sync::mpsc::Sender<()>>,
    join: Option<thread::JoinHandle<()>>,
}

impl Cell {
    fn spawn() -> Self {
        marshal_entities::link();
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .and_then(|l| l.local_addr())
            .expect("reserve port")
            .port();
        let bind: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
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
                    result = server.run() => result.expect("run test cell"),
                    _ = tokio::task::spawn_blocking(move || shutdown_rx.recv()) => {}
                }
            });
        });
        let ctx = Arc::new(ready_rx.recv_timeout(WAIT).expect("cell context"));
        Self {
            address: format!("ws://{bind}"),
            ctx,
            shutdown: Some(shutdown_tx),
            join: Some(join),
        }
    }

    /// Register a session as the hook listener does for a new one.
    fn register(&self, id: &str, cwd: &str, connected_at: i64) {
        let request = RequestContext::internal(
            uuid::Uuid::new_v4().to_string().into(),
            self.ctx.host_id,
            "spawn-e2e",
        );
        CommandContext::new(
            Arc::from("spawn-e2e"),
            Arc::new(request),
            Arc::clone(&self.ctx),
        )
        .emit_set(&Session {
            id: SessionId(Arc::from(id)),
            client_id: None,
            agent_id: None,
            pid: 0,
            cwd: cwd.into(),
            git_branch: None,
            current_task: None,
            session_name: None,
            activity: None,
            kind: None,
            connected_at,
            last_activity_at: None,
            last_tool: None,
            last_tool_at: None,
            operator: None,
            host: None,
            project: None,
            channels_enabled: None,
            acks_pushes: None,
        })
        .expect("register session");
    }
}

impl Drop for Cell {
    fn drop(&mut self) {
        if let Some(s) = self.shutdown.take() {
            let _ = s.send(());
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// A private tmux server (its own `TMUX_TMPDIR`), a work directory, and a fake
/// `claude` that records where it ran and then stays up.
struct Rig {
    dir: tempfile::TempDir,
}

impl Rig {
    fn new(harness_body: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tmux")).unwrap();
        std::fs::create_dir_all(dir.path().join("home/config")).unwrap();
        std::fs::create_dir_all(dir.path().join("work dir")).unwrap();
        let script = dir.path().join("fake-claude");
        std::fs::write(&script, format!("#!/bin/sh\n{harness_body}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    fn work(&self) -> PathBuf {
        std::fs::canonicalize(self.dir.path().join("work dir")).unwrap()
    }

    fn marker(&self) -> PathBuf {
        self.dir.path().join("ran-in")
    }

    fn tmux(&self) -> Command {
        let mut c = Command::new("tmux");
        c.env_remove("TMUX")
            .env("TMUX_TMPDIR", self.dir.path().join("tmux"));
        c
    }

    fn spawn(&self, address: &str, extra: &[&str]) -> std::process::Child {
        let home = self.dir.path().join("home");
        Command::new(SHIM)
            .arg("spawn")
            .arg("claude")
            .arg("--cwd")
            .arg(self.work())
            .args(extra)
            .env_remove("TMUX")
            .env("TMUX_TMPDIR", self.dir.path().join("tmux"))
            .env("MARSHAL_CLAUDE_BIN", self.dir.path().join("fake-claude"))
            .env("SPAWN_E2E_MARKER", self.marker())
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("MARSHAL_DAEMON_ADDRESS", address)
            .env_remove("MYKO_ADDRESS")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("run marshal-shim spawn")
    }

    fn has_session(&self, name: &str) -> bool {
        self.tmux()
            .args(["has-session", "-t", &format!("={name}")])
            .output()
            .is_ok_and(|o| o.status.success())
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.tmux().arg("kill-server").output();
    }
}

fn finish(child: std::process::Child) -> Output {
    child.wait_with_output().unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "status {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn wait_for_file(path: &Path) -> String {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if let Ok(s) = std::fs::read_to_string(path)
            && !s.is_empty()
        {
            return s.trim().to_string();
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("the harness never ran: {} wasn't written", path.display());
}

#[test]
fn spawn_reports_the_new_session_and_not_one_already_in_that_directory() {
    if !have_tmux() {
        return;
    }
    let cell = Cell::spawn();
    let rig = Rig::new(r#"pwd > "$SPAWN_E2E_MARKER"; exec sleep 120"#);
    let work = rig.work();
    let now = chrono::Utc::now().timestamp_millis();
    // Already working in that directory before the launch: not the new one.
    cell.register("spawn-e2e-old", work.to_str().unwrap(), now - 10 * 60_000);

    let child = rig.spawn(&cell.address, &["--name", "e2e-spawn", "--timeout", "20"]);
    let ran_in = wait_for_file(&rig.marker());
    assert_eq!(Path::new(&ran_in), work, "tmux ran the harness elsewhere");
    assert!(rig.has_session("e2e-spawn"));
    cell.register(
        "spawn-e2e-new",
        &ran_in,
        chrono::Utc::now().timestamp_millis(),
    );

    let out = finish(child);
    assert!(out.status.success(), "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("(spawn-e2e-new) joined from tmux session e2e-spawn"),
        "{}",
        text(&out)
    );
    assert!(!stdout.contains("spawn-e2e-old"), "{}", text(&out));
    // Spawn returns, and the session stays up for someone to attach to.
    assert!(rig.has_session("e2e-spawn"));

    // A second spawn with the same name is refused, and starts nothing.
    let again = finish(rig.spawn(&cell.address, &["--name", "e2e-spawn"]));
    assert!(!again.status.success(), "{}", text(&again));
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("already exists"),
        "{}",
        text(&again)
    );
}

#[test]
fn a_harness_that_exits_at_once_is_reported_without_waiting_out_the_timeout() {
    if !have_tmux() {
        return;
    }
    let cell = Cell::spawn();
    let rig = Rig::new("exit 3");
    let started = Instant::now();
    let out = finish(rig.spawn(&cell.address, &["--name", "e2e-exits", "--timeout", "25"]));
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("exited before joining"),
        "{}",
        text(&out)
    );
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "it waited out the timeout"
    );
}

#[test]
fn a_session_that_never_joins_times_out_and_is_left_running() {
    if !have_tmux() {
        return;
    }
    let cell = Cell::spawn();
    let rig = Rig::new("exec sleep 120");
    let out = finish(rig.spawn(&cell.address, &["--name", "e2e-quiet", "--timeout", "2"]));
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("nothing joined the roster"),
        "{}",
        text(&out)
    );
    assert!(
        rig.has_session("e2e-quiet"),
        "the session should be left for the operator"
    );
}

#[test]
fn nothing_is_started_when_the_daemon_cant_be_reached() {
    if !have_tmux() {
        return;
    }
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let rig = Rig::new(r#"pwd > "$SPAWN_E2E_MARKER"; exec sleep 120"#);
    let out = finish(rig.spawn(
        &format!("ws://127.0.0.1:{port}"),
        &["--name", "e2e-nodaemon"],
    ));
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("nothing was started"),
        "{}",
        text(&out)
    );
    assert!(!rig.has_session("e2e-nodaemon"));
    assert!(!rig.marker().exists());
}

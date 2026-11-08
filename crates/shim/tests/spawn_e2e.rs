//! `marshal-shim spawn` end to end (MAR-26, MAR-27): the real binary starts a
//! harness in a real, private tmux server, and reports it once its session
//! joins a real in-process Myko cell.
//!
//! The harness is a script standing in for `claude` (`MARSHAL_CLAUDE_BIN`). It
//! writes down where it ran and what it was given, and the test registers the
//! session the way the daemon's hook listener would, with the `agent_id` the
//! harness got as `MARSHAL_AGENT_ID`. So what's checked is spawn: that tmux ran
//! the harness in the right directory with this launch's token, and that spawn
//! reports the session carrying it and no other.
//!
//! It needs tmux. Without it the tests say so and pass, except under CI, where
//! they fail.

#![cfg(unix)]

mod support;

use std::{
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use marshal_entities::{GetAllMessages, Message, Session, SessionId};
use myko::prelude::EventPublishing as _;
use myko::{
    command::CommandContext,
    request::RequestContext,
    server::{MykoServerContext, Persister},
};
use myko_server::{BlackholePersister, MykoServer};

const WAIT: Duration = Duration::from_secs(10);
const SPAWNER_ID: &str = "the-spawner";
/// A harness that records its directory, its `MARSHAL_AGENT_ID` and its daemon
/// address, one per line, then stays up.
const RECORDING_HARNESS: &str = r#"printf '%s\n%s\n%s\n' "$(pwd)" "$MARSHAL_AGENT_ID" "$MARSHAL_DAEMON_ADDRESS" > "$SPAWN_E2E_MARKER.tmp"; mv "$SPAWN_E2E_MARKER.tmp" "$SPAWN_E2E_MARKER"; exec sleep 120"#;

/// What the recording harness wrote down.
struct Seen {
    cwd: String,
    agent_id: String,
    daemon: String,
}

fn seen(marker: &Path) -> Seen {
    let all = wait_for_file(marker);
    let mut lines = all.lines().map(String::from);
    let mut next = || lines.next().unwrap_or_default();
    Seen {
        cwd: next(),
        agent_id: next(),
        daemon: next(),
    }
}

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

    /// Every message the cell holds.
    fn messages(&self) -> Vec<Arc<Message>> {
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
        .exec_query(GetAllMessages {})
        .expect("read messages")
    }

    /// Register a session as the hook listener does for a new one.
    fn register(&self, id: &str, cwd: &str, agent_id: Option<&str>) {
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
            agent_id: agent_id.map(String::from),
            pid: 0,
            cwd: cwd.into(),
            git_branch: None,
            current_task: None,
            session_name: None,
            activity: None,
            kind: None,
            connected_at: chrono::Utc::now().timestamp_millis(),
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
        // A space and a `;` in its path: run through `sh -c`, it would never start.
        let bin = dir.path().join("bin dir;x");
        std::fs::create_dir_all(&bin).unwrap();
        let script = bin.join("fake-claude");
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
        let work = self.work();
        let mut args = vec!["spawn", "claude", "--cwd", work.to_str().unwrap()];
        args.extend_from_slice(extra);
        self.shim(address, &args)
    }

    /// Run `marshal-shim <args>` with this rig's tmux, harness and home.
    fn shim(&self, address: &str, args: &[&str]) -> std::process::Child {
        self.shim_env(address, args, &[])
    }

    fn shim_env(&self, address: &str, args: &[&str], env: &[(&str, &Path)]) -> std::process::Child {
        let home = self.dir.path().join("home");
        support::shim(support::shared_home())
            .args(args)
            .envs(env.iter().map(|(k, v)| (*k, *v)))
            .env_remove("TMUX")
            .env("TMUX_TMPDIR", self.dir.path().join("tmux"))
            .env(
                "MARSHAL_CLAUDE_BIN",
                self.dir.path().join("bin dir;x/fake-claude"),
            )
            .env("SPAWN_E2E_MARKER", self.marker())
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("MARSHAL_DAEMON_ADDRESS", address)
            .env_remove("MYKO_ADDRESS")
            // The spawner's own identity, which the harness must not get.
            .env("MARSHAL_AGENT_ID", SPAWNER_ID)
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
fn spawn_reports_the_session_carrying_its_token_and_no_other_in_that_directory() {
    if !have_tmux() {
        return;
    }
    let cell = Cell::spawn();
    let rig = Rig::new(RECORDING_HARNESS);
    let work = rig.work();
    let work_s = work.to_str().unwrap();
    // Already working in that directory before the launch.
    cell.register("spawn-e2e-old", work_s, None);

    let child = rig.spawn(&cell.address, &["--name", "e2e-spawn", "--timeout", "20"]);
    let got = seen(&rig.marker());
    assert_eq!(Path::new(&got.cwd), work, "tmux ran the harness elsewhere");
    assert!(
        got.agent_id.starts_with("spawn-"),
        "token: {:?}",
        got.agent_id
    );
    assert_ne!(
        got.agent_id, SPAWNER_ID,
        "the harness got the spawner's identity"
    );
    assert!(rig.has_session("e2e-spawn"));

    // Other sessions start in the same directory after the launch, before the
    // spawned one joins. MAR-26's directory rule would have taken the first.
    cell.register("spawn-e2e-decoy", work_s, None);
    cell.register("spawn-e2e-other-launch", work_s, Some("spawn-someone-else"));
    thread::sleep(Duration::from_millis(800));
    cell.register("spawn-e2e-new", &got.cwd, Some(&got.agent_id));

    let out = finish(child);
    assert!(out.status.success(), "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("(spawn-e2e-new) joined from tmux session e2e-spawn"),
        "{}",
        text(&out)
    );
    for other in ["spawn-e2e-old", "spawn-e2e-decoy", "spawn-e2e-other-launch"] {
        assert!(!stdout.contains(other), "{}", text(&out));
    }
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
fn a_session_in_the_launch_directory_without_the_token_never_counts() {
    if !have_tmux() {
        return;
    }
    let cell = Cell::spawn();
    let rig = Rig::new(RECORDING_HARNESS);
    let child = rig.spawn(&cell.address, &["--name", "e2e-decoy", "--timeout", "3"]);
    let got = seen(&rig.marker());
    cell.register("spawn-e2e-decoy-only", &got.cwd, None);
    let out = finish(child);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no session carrying this launch's token"),
        "{}",
        text(&out)
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
        String::from_utf8_lossy(&out.stderr).contains("no session carrying this launch's token"),
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
    let rig = Rig::new(RECORDING_HARNESS);
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

#[test]
fn a_running_tmux_server_doesnt_give_the_harness_its_own_daemon_address() {
    if !have_tmux() {
        return;
    }
    let cell = Cell::spawn();
    let rig = Rig::new(RECORDING_HARNESS);
    // A server already up, started with another daemon's address.
    let up = rig
        .tmux()
        .args(["new-session", "-d", "-s", "already-running", "sleep", "120"])
        .env("MARSHAL_DAEMON_ADDRESS", "ws://127.0.0.1:1")
        .env("MARSHAL_AGENT_ID", "the-servers-identity")
        .env("SPAWN_E2E_MARKER", rig.marker())
        .output()
        .unwrap();
    assert!(up.status.success(), "{}", text(&up));

    let child = rig.spawn(&cell.address, &["--name", "e2e-env", "--timeout", "20"]);
    let got = seen(&rig.marker());
    assert_eq!(
        got.daemon, cell.address,
        "the harness got the server's address, not spawn's"
    );
    assert!(
        got.agent_id.starts_with("spawn-"),
        "token: {:?}",
        got.agent_id
    );
    cell.register("spawn-e2e-env", &got.cwd, Some(&got.agent_id));
    let out = finish(child);
    assert!(out.status.success(), "{}", text(&out));
}

#[test]
fn the_first_task_goes_to_the_joined_session_alone_as_a_direct_message() {
    if !have_tmux() {
        return;
    }
    let cell = Cell::spawn();
    let rig = Rig::new(RECORDING_HARNESS);
    let task = "MAR-28 first task: reply with the word ready";
    let child = rig.spawn(
        &cell.address,
        &["--name", "e2e-task", "--timeout", "20", "--task", task],
    );
    let got = seen(&rig.marker());
    // Another session in the same directory, which mustn't get the task.
    cell.register("spawn-e2e-task-decoy", &got.cwd, None);
    cell.register("spawn-e2e-task-new", &got.cwd, Some(&got.agent_id));

    let out = finish(child);
    assert!(out.status.success(), "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("(spawn-e2e-task-new) joined"),
        "{}",
        text(&out)
    );
    assert!(stdout.contains("1 sent, 0 not sent"), "{}", text(&out));

    let messages = cell.messages();
    let with_task: Vec<&Message> = messages
        .iter()
        .map(|m| m.as_ref())
        .filter(|m| m.body.ends_with(task))
        .collect();
    assert_eq!(with_task.len(), 1, "{messages:?}");
    assert_eq!(
        with_task[0].to_session_id.as_ref().map(|id| id.0.as_ref()),
        Some("spawn-e2e-task-new"),
        "the task went to another session"
    );
}

#[test]
fn without_a_task_nothing_is_sent() {
    if !have_tmux() {
        return;
    }
    let cell = Cell::spawn();
    let rig = Rig::new(RECORDING_HARNESS);
    let child = rig.spawn(&cell.address, &["--name", "e2e-notask", "--timeout", "20"]);
    let got = seen(&rig.marker());
    cell.register("spawn-e2e-notask", &got.cwd, Some(&got.agent_id));
    let out = finish(child);
    assert!(out.status.success(), "{}", text(&out));
    assert!(cell.messages().is_empty(), "{:?}", cell.messages());
}

/// For a team: each member writes its directory and token to a file named
/// after its directory, and a member working in `quitter` exits at once.
const TEAM_HARNESS: &str = r#"d=$(basename "$(pwd)"); [ "$d" = quitter ] && exit 3; printf '%s\n%s\n' "$(pwd)" "$MARSHAL_AGENT_ID" > "$SPAWN_E2E_MARKER-$d.tmp"; mv "$SPAWN_E2E_MARKER-$d.tmp" "$SPAWN_E2E_MARKER-$d"; exec sleep 120"#;

fn team_file(rig: &Rig, members: &[(&str, &str, Option<&str>)]) -> PathBuf {
    let mut text = String::from("team = \"e2e\"\n");
    for (role, dir, task) in members {
        std::fs::create_dir_all(rig.dir.path().join(dir)).unwrap();
        text.push_str(&format!(
            "[[member]]\nrole = \"{role}\"\nharness = \"claude\"\ncwd = \"{dir}\"\n"
        ));
        if let Some(task) = task {
            text.push_str(&format!("task = \"{task}\"\n"));
        }
    }
    let file = rig.dir.path().join("team.toml");
    std::fs::write(&file, text).unwrap();
    file
}

fn member_marker(rig: &Rig, dir: &str) -> PathBuf {
    PathBuf::from(format!("{}-{dir}", rig.marker().display()))
}

#[test]
fn a_team_starts_together_each_member_gets_its_own_task_and_failures_are_reported() {
    if !have_tmux() {
        return;
    }
    let cell = Cell::spawn();
    let rig = Rig::new(TEAM_HARNESS);
    let file = team_file(
        &rig,
        &[
            ("backend", "backend-dir", Some("take the next story")),
            ("reviewer", "reviewer-dir", None),
            ("quitter", "quitter", Some("never read")),
        ],
    );
    let child = rig.shim(
        &cell.address,
        &["spawn-team", file.to_str().unwrap(), "--timeout", "20"],
    );
    let backend = seen(&member_marker(&rig, "backend-dir"));
    let reviewer = seen(&member_marker(&rig, "reviewer-dir"));
    assert_ne!(backend.agent_id, reviewer.agent_id, "members share a token");
    for token in [&backend.agent_id, &reviewer.agent_id] {
        assert!(token.starts_with("spawn-"), "token: {token:?}");
    }
    // Registered in the other order from the file's.
    cell.register("team-e2e-reviewer", &reviewer.cwd, Some(&reviewer.agent_id));
    cell.register("team-e2e-backend", &backend.cwd, Some(&backend.agent_id));

    let out = finish(child);
    assert!(
        !out.status.success(),
        "the quitter should fail the run: {}",
        text(&out)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in [
        "backend: joined as",
        "(team-e2e-backend) in tmux e2e-backend; task sent",
        "(team-e2e-reviewer) in tmux e2e-reviewer; no task",
        "quitter: DIDN'T JOIN: the harness exited",
        "--- 2 of 3 joined and set up, 1 not",
    ] {
        assert!(stdout.contains(line), "missing {line:?}: {}", text(&out));
    }
    assert!(rig.has_session("e2e-backend") && rig.has_session("e2e-reviewer"));

    let messages = cell.messages();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(
        messages[0]
            .body
            .ends_with("You are the backend member of team e2e. take the next story"),
        "{messages:?}"
    );
    assert_eq!(
        messages[0].to_session_id.as_ref().map(|id| id.0.as_ref()),
        Some("team-e2e-backend")
    );
}

#[test]
fn a_team_with_a_taken_tmux_name_starts_no_one() {
    if !have_tmux() {
        return;
    }
    let cell = Cell::spawn();
    let rig = Rig::new(TEAM_HARNESS);
    let file = team_file(&rig, &[("one", "one-dir", None), ("two", "two-dir", None)]);
    let up = rig
        .tmux()
        .args(["new-session", "-d", "-s", "e2e-two", "sleep", "120"])
        .output()
        .unwrap();
    assert!(up.status.success(), "{}", text(&up));
    let out = finish(rig.shim(&cell.address, &["spawn-team", file.to_str().unwrap()]));
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("so no one was started"),
        "{}",
        text(&out)
    );
    assert!(!rig.has_session("e2e-one"));
    assert!(!member_marker(&rig, "one-dir").exists());
}

/// A stand-in desktop terminal: records its arguments, then runs what follows
/// `-e` under script(1), so `tmux attach` gets the pty a window would give it.
/// Like a real terminal, it sets TERM: with TERM unset or `dumb`, as on a CI
/// runner, `tmux attach` fails with "open terminal failed: terminal does not
/// support clear". It also keeps script's stdin open, as a precaution only,
/// since spawn starts the terminal with stdin at /dev/null. That was never
/// shown to matter. script's output goes to `terminal-out`, for the failure
/// message.
fn fake_terminal(rig: &Rig) -> PathBuf {
    let path = rig.dir.path().join("fake-terminal");
    std::fs::write(
        &path,
        format!(
            "#!/bin/bash\nprintf '%s\\n' \"$@\" > {args}\n[ \"$1\" = -e ] || exit 9\nshift\nexport TERM=xterm-256color\nsleep 300 | script -qfc \"$(printf '%q ' \"$@\")\" /dev/null > {out} 2>&1\n",
            args = rig.dir.path().join("terminal-args").display(),
            out = rig.dir.path().join("terminal-out").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn window_opens_a_terminal_attached_to_the_tmux_session() {
    if !have_tmux() {
        return;
    }
    if Command::new("script").arg("--version").output().is_err() {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI needs script(1) for the window test"
        );
        eprintln!("script(1) isn't installed: skipping the window test");
        return;
    }
    let cell = Cell::spawn();
    let rig = Rig::new(RECORDING_HARNESS);
    let terminal = fake_terminal(&rig);
    let work = rig.work();
    let child = rig.shim_env(
        &cell.address,
        &[
            "spawn",
            "claude",
            "--cwd",
            work.to_str().unwrap(),
            "--name",
            "e2e-window",
            "--window",
            "--timeout",
            "20",
        ],
        &[("TERMINAL", terminal.as_path())],
    );
    let got = seen(&rig.marker());
    cell.register("spawn-e2e-window", &got.cwd, Some(&got.agent_id));
    let out = finish(child);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("opened a "),
        "{}",
        text(&out)
    );

    let args_file = rig.dir.path().join("terminal-args");
    let deadline = Instant::now() + WAIT;
    let clients = || {
        rig.tmux()
            .args(["list-clients", "-t", "=e2e-window"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
            .unwrap_or(0)
    };
    while clients() == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        clients(),
        1,
        "the window isn't attached to the tmux session; the terminal said: {:?}",
        std::fs::read_to_string(rig.dir.path().join("terminal-out")).unwrap_or_default()
    );
    let args = std::fs::read_to_string(&args_file).unwrap();
    let args: Vec<&str> = args.lines().collect();
    assert_eq!(args[0], "-e");
    assert!(args[1].ends_with("tmux"), "{args:?}");
    assert_eq!(args[2..], ["attach-session", "-t", "=e2e-window"]);

    // Closing the window leaves the session running.
    let detached = rig
        .tmux()
        .args(["detach-client", "-s", "=e2e-window"])
        .output()
        .unwrap();
    assert!(detached.status.success(), "{}", text(&detached));
    thread::sleep(Duration::from_millis(500));
    assert!(rig.has_session("e2e-window"));
}

#[test]
fn window_with_no_terminal_starts_nothing() {
    if !have_tmux() {
        return;
    }
    let cell = Cell::spawn();
    let rig = Rig::new(RECORDING_HARNESS);
    let work = rig.work();
    let missing = rig.dir.path().join("no-such-terminal");
    let out = finish(rig.shim_env(
        &cell.address,
        &[
            "spawn",
            "claude",
            "--cwd",
            work.to_str().unwrap(),
            "--name",
            "e2e-nowindow",
            "--window",
        ],
        &[("TERMINAL", missing.as_path())],
    ));
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("nothing was started"),
        "{}",
        text(&out)
    );
    assert!(!rig.has_session("e2e-nowindow"));
    assert!(!rig.marker().exists());
}

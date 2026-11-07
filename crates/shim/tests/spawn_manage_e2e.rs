//! `marshal-shim spawn list | stop` end to end (MAR-30), against the real
//! `marshal-daemon`, a private tmux server, and a stand-in harness that joins
//! through the real `marshal-shim claude-hook session-start`, so its roster row
//! and its auto-rooms are the daemon's own.
//!
//! It needs tmux. Without it the tests say so and pass, except under CI, where
//! they fail.

#![cfg(unix)]

use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use hyphae::Gettable;
use marshal_entities::{GetAllRoomMembers, GetAllSessions, RoomMember, Session};
use myko::client::MykoClient;

const SHIM: &str = env!("CARGO_BIN_EXE_marshal-shim");
const WAIT: Duration = Duration::from_secs(15);

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
        eprintln!("tmux isn't installed: skipping the spawn-manage end-to-end test");
    }
    ok
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_until(within: Duration, mut ok: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if ok() {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    ok()
}

/// A real daemon, in memory, on free loopback ports, and a private tmux server.
struct Rig {
    dir: tempfile::TempDir,
    ws: u16,
    hook: u16,
    daemon: Child,
}

impl Rig {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["tmux", "home/config", "work", "bin"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        let (ws, hook) = (free_port(), free_port());
        let bin = Path::new(SHIM).with_file_name("marshal-daemon");
        assert!(
            bin.exists(),
            "{} is missing: build the workspace first",
            bin.display()
        );
        let log = std::fs::File::create(dir.path().join("daemon.log")).unwrap();
        let p = |s: &str| dir.path().join(s);
        let daemon = Command::new(bin)
            .env_remove("MARSHAL_BASE_URL")
            .env_remove("MARSHAL_DAEMON_ADDRESS")
            .env_remove("MYKO_ADDRESS")
            .env_remove("MYKO_POSTGRES_URL")
            .env_remove("MARSHAL_PERSIST")
            .env("XDG_CONFIG_HOME", p("home/config"))
            .env("HOME", p("home"))
            .env("MARSHAL_STATE_DIR", p("state"))
            .env("MARSHAL_HOST", "spawn-manage")
            .env("MARSHAL_TLS", "0")
            .env("MARSHAL_BIND", format!("127.0.0.1:{ws}"))
            .env("MARSHAL_HOOK_BIND", format!("127.0.0.1:{hook}"))
            .env("MARSHAL_MYKO_BIND", format!("127.0.0.1:{}", free_port()))
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .expect("start marshal-daemon");
        let rig = Self {
            dir,
            ws,
            hook,
            daemon,
        };
        let up = |port| TcpStream::connect(("127.0.0.1", port)).is_ok();
        assert!(
            wait_until(Duration::from_secs(20), || up(rig.ws) && up(rig.hook)),
            "marshal-daemon didn't come up"
        );
        // Joins through the real hook, with whatever MARSHAL_AGENT_ID and hook
        // address tmux gave it, then records its session id and stays up.
        let script = rig.dir.path().join("bin/fake-claude");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nsid=\"manage-$(basename \"$(pwd)\")-$$\"\nprintf '{{\"session_id\":\"%s\",\"cwd\":\"%s\"}}' \"$sid\" \"$(pwd)\" | {SHIM} claude-hook session-start >/dev/null 2>&1\nprintf '%s' \"$sid\" > \"$SPAWN_E2E_MARKER.tmp\"; mv \"$SPAWN_E2E_MARKER.tmp\" \"$SPAWN_E2E_MARKER\"\nexec sleep 120\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        rig
    }

    fn marker(&self) -> PathBuf {
        self.dir.path().join("joined")
    }

    fn address(&self) -> String {
        format!("ws://127.0.0.1:{}", self.ws)
    }

    fn env(&self, c: &mut Command) {
        let home = self.dir.path().join("home");
        c.env_remove("TMUX")
            .env_remove("MYKO_ADDRESS")
            .env_remove("MARSHAL_AGENT_ID")
            .env("TMUX_TMPDIR", self.dir.path().join("tmux"))
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("MARSHAL_DAEMON_ADDRESS", self.address())
            .env(
                "MARSHAL_BASE_URL",
                format!("http://127.0.0.1:{}", self.hook),
            )
            .env(
                "MARSHAL_CLAUDE_BIN",
                self.dir.path().join("bin/fake-claude"),
            )
            .env("SPAWN_E2E_MARKER", self.marker());
    }

    fn shim(&self, args: &[&str]) -> Output {
        let mut c = Command::new(SHIM);
        c.args(args);
        self.env(&mut c);
        c.output().unwrap()
    }

    fn tmux(&self, args: &[&str]) -> Output {
        let mut c = Command::new("tmux");
        c.args(args);
        self.env(&mut c);
        c.output().unwrap()
    }

    fn has_session(&self, name: &str) -> bool {
        self.tmux(&["has-session", "-t", &format!("={name}")])
            .status
            .success()
    }

    /// The roster and every room membership, as the daemon has them now.
    fn state(&self) -> (Vec<Arc<Session>>, Vec<Arc<RoomMember>>) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            marshal_entities::link();
            let client = MykoClient::new();
            let sessions = client.watch_query::<GetAllSessions>(GetAllSessions {});
            let members = client.watch_query::<GetAllRoomMembers>(GetAllRoomMembers {});
            client.set_address(Some(self.address()));
            let started = Instant::now();
            // Both may really be empty, so wait for a little, not for rows.
            while (sessions.get().is_empty() || members.get().is_empty())
                && started.elapsed() < Duration::from_secs(3)
            {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            (sessions.get(), members.get())
        })
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.tmux(&["kill-server"]);
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

fn text(out: &Output) -> String {
    format!(
        "status {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn joined_sid(rig: &Rig) -> String {
    let marker = rig.marker();
    assert!(
        wait_until(WAIT, || std::fs::read_to_string(&marker)
            .is_ok_and(|s| !s.is_empty())),
        "the harness never joined"
    );
    std::fs::read_to_string(&marker).unwrap()
}

#[test]
fn stop_ends_a_spawned_session_and_clears_its_row_and_rooms() {
    if !have_tmux() {
        return;
    }
    let rig = Rig::new();
    let work = rig.dir.path().join("work");
    let out = rig.shim(&[
        "spawn",
        "claude",
        "--cwd",
        work.to_str().unwrap(),
        "--name",
        "m30-worker",
    ]);
    assert!(out.status.success(), "{}", text(&out));
    let sid = joined_sid(&rig);
    // A tmux session spawn didn't start, which must be left alone.
    assert!(
        rig.tmux(&["new-session", "-d", "-s", "m30-mine", "sleep", "120"])
            .status
            .success()
    );

    let (rows, members) = rig.state();
    assert!(
        rows.iter().any(|s| s.id.0.as_ref() == sid),
        "{sid} not on the roster"
    );
    let rooms: Vec<&str> = members
        .iter()
        .filter(|m| m.session_id.0.as_ref() == sid)
        .map(|m| m.room_id.0.as_ref())
        .collect();
    assert!(
        !rooms.is_empty(),
        "{sid} is in no rooms, so clearing them proves nothing"
    );

    let list = rig.shim(&["spawn", "list"]);
    assert!(list.status.success(), "{}", text(&list));
    let listed = String::from_utf8_lossy(&list.stdout);
    assert!(
        listed.contains("m30-worker\t") && listed.contains(&format!("({sid})")),
        "{}",
        text(&list)
    );
    assert!(!listed.contains("m30-mine"), "{}", text(&list));

    let stop = rig.shim(&["spawn", "stop", "m30-worker"]);
    assert!(stop.status.success(), "{}", text(&stop));
    assert!(
        String::from_utf8_lossy(&stop.stdout).contains("its roster row and rooms are cleared"),
        "{}",
        text(&stop)
    );
    assert!(!rig.has_session("m30-worker"));
    let (rows, members) = rig.state();
    assert!(
        !rows.iter().any(|s| s.id.0.as_ref() == sid),
        "{sid} still on the roster"
    );
    assert!(
        !members.iter().any(|m| m.session_id.0.as_ref() == sid),
        "{sid} is still in rooms: {members:?}"
    );
    assert!(
        rig.has_session("m30-mine"),
        "stop touched a session spawn didn't start"
    );

    let again = rig.shim(&["spawn", "stop", "m30-worker"]);
    assert!(!again.status.success(), "{}", text(&again));
    assert!(String::from_utf8_lossy(&again.stderr).contains("no tmux session named m30-worker"));
}

#[test]
fn stop_refuses_tmux_sessions_spawn_didnt_start() {
    if !have_tmux() {
        return;
    }
    let rig = Rig::new();
    assert!(
        rig.tmux(&["new-session", "-d", "-s", "plain", "sleep", "120"])
            .status
            .success()
    );
    // An embedder's own agent id is not a spawn token.
    assert!(
        rig.tmux(&[
            "new-session",
            "-d",
            "-s",
            "embedded",
            "-e",
            "MARSHAL_AGENT_ID=my-embedder",
            "sleep",
            "120"
        ])
        .status
        .success()
    );
    for name in ["plain", "embedded"] {
        let out = rig.shim(&["spawn", "stop", name]);
        assert!(!out.status.success(), "{}", text(&out));
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("wasn't started by spawn"),
            "{}",
            text(&out)
        );
        assert!(rig.has_session(name), "{name} was stopped");
    }
    let list = rig.shim(&["spawn", "list"]);
    assert!(list.status.success(), "{}", text(&list));
    assert!(
        String::from_utf8_lossy(&list.stdout).contains("no tmux sessions started by spawn"),
        "{}",
        text(&list)
    );
}

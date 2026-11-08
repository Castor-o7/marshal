//! Delivery conformance (MUS-69): one scenario table, run against every
//! harness, with the real `marshal-daemon` and `marshal-shim` binaries and no TUI
//! or model.
//!
//! Each harness is a fake that drives the real binaries the way its TUI does
//! (`FakeHarness`). What each fake assumes about its TUI, and the harness
//! version it's pinned to, is in `docs/delivery-conformance.md`. Messages are
//! sent by a real `marshal-shim` speaking MCP, so the send result is checked as
//! well as what the recipient sees.
//!
//! The suite needs the daemon binary, so it doesn't run in plain
//! `cargo test`. Build first, then run it:
//!
//! ```sh
//! cargo build --workspace
//! cargo test -p marshal-shim --test conformance -- --ignored --nocapture
//! ```
//!
//! The restart cells in `POSTGRES_SCENARIOS` need a Postgres database, given
//! as `CONFORMANCE_POSTGRES_URL` (each daemon gets its own events table in it).
//! Without one they show `no postgres`; CI sets `CONFORMANCE_REQUIRE_POSTGRES`,
//! so there that's a failure.
//!
//! It prints a harness × scenario grid. A cell in `KNOWN_FAILURES` is expected
//! to fail and is reported as such; if one starts passing, the suite fails so
//! the list gets updated.

mod support;

use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tempfile::TempDir;

const SHIM: &str = env!("CARGO_BIN_EXE_marshal-shim");
/// Short, so a dropped ack is redelivered within a test (default 60 s).
const ACK_LEASE_SECS: u64 = 2;
/// How long an idle session gets to be woken.
const WAKE_WITHIN: Duration = Duration::from_secs(10);
/// How long to watch for a wake that must not happen.
const QUIET_FOR: Duration = Duration::from_secs(4);

/// Cells that pass or fail from run to run, each with its story and why it's
/// listed. They're reported as `flaky` and never fail the suite either way,
/// and they're checked before `KNOWN_FAILURES`, which may list them too.
const FLAKY: &[(&str, &str, &str, &str)] = &[
    // Muster's entries were identity-takeover cells on claude-tls (MUS-93),
    // n/a here until Marshal has a TLS gate (MAR-23); MAR-8 tracks them.
];

/// Cells expected to fail today, each with the story that fixes it. The
/// suite reports them as known failures, and fails if one passes.
const KNOWN_FAILURES: &[(&str, &str, &str)] = &[
    // Muster's entries were all push-plugin cells, whose columns come with
    // stage 2 (MAR-10, MAR-11).
];

/// Harnesses in the grid that have no fake yet, with the stage that adds one.
const NOT_YET: &[(&str, &str)] = &[];

// ---------------------------------------------------------------------------
// The daemon

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Plain,
    Tls,
}

fn daemon_bin() -> PathBuf {
    let bin = Path::new(SHIM).with_file_name("marshal-daemon");
    assert!(
        bin.exists(),
        "{} is missing: run `cargo build --workspace` before the conformance suite",
        bin.display()
    );
    bin
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

/// A real `marshal-daemon` in throwaway dirs on free loopback ports, in memory
/// or, given a database, persisting to its own table there.
struct Daemon {
    dir: TempDir,
    mode: Mode,
    /// `MYKO_POSTGRES_URL`, and the events table this daemon alone uses.
    postgres: Option<(String, String)>,
    /// `MARSHAL_PERSIST=file`: keep the store in the state directory (MUS-17).
    persist_file: bool,
    bind: u16,
    hook: u16,
    myko: u16,
    child: Option<Child>,
}

impl Daemon {
    fn start(mode: Mode, postgres: Option<&str>, persist_file: bool) -> Self {
        let table = format!(
            "conformance_{}",
            &uuid::Uuid::new_v4().simple().to_string()[..16]
        );
        let mut d = Self {
            dir: tempfile::tempdir().unwrap(),
            mode,
            postgres: postgres.map(|url| (url.to_string(), table)),
            persist_file,
            bind: free_port(),
            hook: free_port(),
            myko: free_port(),
            child: None,
        };
        // Its ports are picked by binding port 0 and closing it, so another
        // process can take one before the daemon binds it. The daemon then
        // exits saying so, and it's started again on new ports (MAR-21).
        let mut lost = Vec::new();
        for _ in 0..5 {
            match d.try_spawn() {
                Ok(()) => return d,
                Err(log) if log.contains("in use") => {
                    lost.push(format!("{} {} {}", d.bind, d.hook, d.myko));
                    (d.bind, d.hook, d.myko) = (free_port(), free_port(), free_port());
                }
                Err(log) => panic!("marshal-daemon didn't come up; log:\n{log}"),
            }
        }
        panic!(
            "marshal-daemon lost its ports 5 times: {lost:?}; log:\n{}",
            d.log()
        );
    }

    fn config_dir(&self) -> PathBuf {
        self.dir.path().join("config")
    }

    fn env(&self) -> Vec<(String, String)> {
        let p = |name: &str| self.dir.path().join(name).display().to_string();
        let addr = |port: u16| format!("127.0.0.1:{port}");
        vec![
            ("XDG_CONFIG_HOME".into(), p("config")),
            ("HOME".into(), p("home")),
            ("MARSHAL_STATE_DIR".into(), p("state")),
            ("MARSHAL_HOST".into(), "conformance".into()),
            (
                "MARSHAL_TLS".into(),
                if self.mode == Mode::Tls { "1" } else { "0" }.into(),
            ),
            ("MARSHAL_ACK_LEASE_SECS".into(), ACK_LEASE_SECS.to_string()),
            // MUS-3: short, so a restarted session takes over within a test.
            ("MARSHAL_SUCCESSION_GRACE_SECS".into(), "1".into()),
            ("MARSHAL_SUCCESSION_QUIET_SECS".into(), "2".into()),
            ("MARSHAL_BIND".into(), addr(self.bind)),
            ("MARSHAL_HOOK_BIND".into(), addr(self.hook)),
            ("MARSHAL_MYKO_BIND".into(), addr(self.myko)),
        ]
        .into_iter()
        .chain(
            self.persist_file
                .then(|| ("MARSHAL_PERSIST".to_string(), "file".to_string())),
        )
        .chain(self.postgres.iter().flat_map(|(url, table)| {
            [
                ("MYKO_POSTGRES_URL".to_string(), url.clone()),
                ("MYKO_POSTGRES_TABLE".to_string(), table.clone()),
                (
                    "MYKO_POSTGRES_CHANNEL".to_string(),
                    format!("{table}_notify"),
                ),
            ]
        }))
        .collect()
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(daemon_bin());
        for var in [
            "MARSHAL_BASE_URL",
            "MARSHAL_DAEMON_ADDRESS",
            "MYKO_ADDRESS",
            "MYKO_POSTGRES_URL",
            "MARSHAL_PERSIST",
        ] {
            cmd.env_remove(var);
        }
        cmd.envs(self.env());
        cmd
    }

    /// Start it again on the same ports, as a restart does.
    fn spawn(&mut self) {
        if let Err(log) = self.try_spawn() {
            panic!("marshal-daemon didn't come up; log:\n{log}");
        }
    }

    /// Start it, and wait for it to listen. `Err` with this start's log if it
    /// exited or never listened.
    fn try_spawn(&mut self) -> Result<(), String> {
        let before = self.log().len();
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.path().join("daemon.log"))
            .unwrap();
        let child = self
            .command()
            // Debug for succession, so a missed takeover shows each verdict.
            .env("RUST_LOG", "info,daemon::succession=debug")
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .expect("spawn marshal-daemon");
        let mut child = child;
        let up = |port| TcpStream::connect(("127.0.0.1", port)).is_ok();
        let exited = |c: &mut Child| c.try_wait().ok().flatten().is_some();
        let listening = wait_until(Duration::from_secs(20), || {
            exited(&mut child) || (up(self.hook) && up(self.bind))
        });
        // A port another process holds answers too. A daemon that lost its
        // WebSocket bind exits at once; one that lost its hook bind logs it
        // and keeps running, so its log is read as well.
        if listening && !exited(&mut child) {
            thread::sleep(Duration::from_millis(300));
        }
        let hook_lost = || {
            self.log()
                .get(before..)
                .is_some_and(|log| log.contains("[hook] listener exited"))
        };
        if exited(&mut child) || !listening || hook_lost() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(self.log().get(before..).unwrap_or_default().to_string());
        }
        self.child = Some(child);
        Ok(())
    }

    /// Stop it the way systemd does (SIGTERM, which ends held waits first), and
    /// start it again on the same ports and dirs.
    fn restart(&mut self) {
        self.stop();
        self.spawn();
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .status();
            let exited = wait_until(Duration::from_secs(10), || {
                child.try_wait().ok().flatten().is_some()
            });
            if !exited {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }

    /// Run a `marshal-daemon` subcommand (`pause`, `resume`, …) against it.
    fn cli(&self, args: &[&str]) {
        let out = self.command().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "marshal-daemon {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn hook_addr(&self) -> SocketAddr {
        format!("127.0.0.1:{}", self.hook).parse().unwrap()
    }

    fn ws_address(&self) -> String {
        format!("ws://127.0.0.1:{}", self.bind)
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("daemon.log")).unwrap_or_default()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ---------------------------------------------------------------------------
// A fault-injecting proxy in front of the hook listener

/// Where a delivery's acknowledgement gets lost, which depends on how the
/// harness acks.
#[derive(Clone, Copy, Debug, PartialEq)]
enum AckFault {
    /// The harness acks with its own `/hook/ack` request (Claude's waiter):
    /// that request never reaches the daemon.
    DropAckRequests,
    /// The daemon acks once it has flushed a hook's reply (`deferred_ack`):
    /// the reply to a request starting with this path is read from the
    /// daemon, then lost before the harness gets it.
    DropReplies(&'static str),
}

/// Forwards hook requests to the daemon, and can lose a delivery's ack on the
/// way (`AckFault`). Every shim request is `Connection: close`.
struct HookProxy {
    base: String,
    fault: Arc<std::sync::Mutex<Option<AckFault>>>,
    faulted: Arc<AtomicUsize>,
}

impl HookProxy {
    fn start(upstream: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let fault = Arc::new(std::sync::Mutex::new(None));
        let faulted = Arc::new(AtomicUsize::new(0));
        let (f, n) = (fault.clone(), faulted.clone());
        thread::spawn(move || {
            for client in listener.incoming().flatten() {
                let (f, n) = (f.clone(), n.clone());
                thread::spawn(move || {
                    let fault = *f.lock().unwrap();
                    let _ = forward(client, upstream, fault, &n);
                });
            }
        });
        Self {
            base,
            fault,
            faulted,
        }
    }

    fn set_fault(&self, fault: Option<AckFault>) {
        *self.fault.lock().unwrap() = fault;
    }

    /// How many acks the fault has lost so far.
    fn faulted(&self) -> usize {
        self.faulted.load(Ordering::SeqCst)
    }
}

fn forward(
    mut client: TcpStream,
    upstream: SocketAddr,
    fault: Option<AckFault>,
    faulted: &AtomicUsize,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(client.try_clone()?);
    let mut head = String::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        if let Some((k, v)) = line.split_once(':')
            && k.eq_ignore_ascii_case("content-length")
        {
            content_length = v.trim().parse().unwrap_or(0);
        }
        let end = line == "\r\n";
        head.push_str(&line);
        if end {
            break;
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body)?;
    if fault == Some(AckFault::DropAckRequests) && head.starts_with("POST /hook/ack") {
        // The ack is lost on the way: the daemon never hears it.
        faulted.fetch_add(1, Ordering::SeqCst);
        return Ok(());
    }
    let mut up = TcpStream::connect(upstream)?;
    up.write_all(head.as_bytes())?;
    up.write_all(&body)?;
    if let Some(AckFault::DropReplies(prefix)) = fault
        && head.starts_with(&format!("POST {prefix}"))
    {
        // Take the whole reply, so the daemon has flushed it, then lose it.
        let mut reply = Vec::new();
        up.read_to_end(&mut reply)?;
        let tag = b"<marshal_inbox";
        if reply.windows(tag.len()).any(|w| w == tag) {
            faulted.fetch_add(1, Ordering::SeqCst);
        }
        return Ok(());
    }
    std::io::copy(&mut up, &mut client)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// A real marshal-shim speaking MCP

/// `marshal-shim` as an MCP server, as a harness spawns it. Used for the sender,
/// and for a Claude recipient, whose TUI runs one too.
struct Mcp {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    next_id: u64,
}

impl Mcp {
    fn start(daemon: &Daemon, session_id: &str) -> Self {
        let mut child = support::shim(support::shared_home())
            .env_remove("MARSHAL_BASE_URL")
            .env_remove("MYKO_ADDRESS")
            .env("XDG_CONFIG_HOME", daemon.config_dir())
            .env("HOME", daemon.dir.path().join("home"))
            .env("MARSHAL_HOST", "conformance")
            .env("MARSHAL_DAEMON_ADDRESS", daemon.ws_address())
            .env("CLAUDE_CODE_SESSION_ID", session_id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn marshal-shim");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut mcp = Self {
            child,
            stdin,
            lines,
            next_id: 0,
        };
        mcp.rpc(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "conformance", "version": "0"}
            }),
        )
        .expect("MCP initialize");
        mcp
    }

    fn rpc(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{req}").map_err(|e| e.to_string())?;
        self.stdin.flush().map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(20);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let line = self
                .lines
                .recv_timeout(left)
                .map_err(|_| format!("no reply to {method}"))?;
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if msg["id"] == json!(id) {
                if let Some(err) = msg.get("error") {
                    return Err(format!("{method}: {err}"));
                }
                return Ok(msg["result"].clone());
            }
        }
        Err(format!("no reply to {method}"))
    }

    /// Call a tool and return its JSON result.
    fn tool(&mut self, name: &str, args: Value) -> Result<Value, String> {
        let r = self.rpc("tools/call", json!({"name": name, "arguments": args}))?;
        let text = r["content"][0]["text"].as_str().unwrap_or_default();
        if r["isError"] == json!(true) {
            return Err(format!("{name}: {text}"));
        }
        serde_json::from_str(text).map_err(|e| format!("{name} result isn't JSON ({e}): {text}"))
    }

    fn send(&mut self, to: &str, body: &str, key: Option<&str>) -> Result<Value, String> {
        let mut args = json!({"to": to, "body": body});
        if let Some(key) = key {
            args["idempotency_key"] = json!(key);
        }
        self.tool("send_message", args)
    }

    fn broadcast(&mut self, room: &str, body: &str) -> Result<Value, String> {
        self.tool("broadcast", json!({"to_room": room, "body": body}))
    }

    fn roster(&mut self) -> Result<Value, String> {
        let r = self.rpc("resources/read", json!({"uri": "marshal://roster"}))?;
        let text = r["contents"][0]["text"].as_str().unwrap_or_default();
        serde_json::from_str(text).map_err(|e| format!("roster isn't JSON: {e}"))
    }

    /// The roster row for `session_id`, once it's there.
    fn row(&mut self, session_id: &str, within: Duration) -> Option<Value> {
        let deadline = Instant::now() + within;
        loop {
            if let Ok(roster) = self.roster()
                && let Some(row) = roster["sessions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|s| s["session_id"] == json!(session_id))
            {
                return Some(row.clone());
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(250));
        }
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Fake harnesses

/// What a harness's TUI does, as far as delivery goes, driven against the real
/// binaries. Text returned is what the model would see.
trait FakeHarness {
    /// The session starts: whatever the TUI runs at session start.
    fn start(&mut self, daemon: &Daemon, proxy: &HookProxy);
    /// The session goes idle: arm whatever wakes it.
    fn arm_idle(&mut self);
    /// What an idle session was woken with, if it was woken within `within`.
    fn wait_for_wake(&mut self, within: Duration) -> Option<String>;
    /// Whether the armed wake path has fired yet, without consuming it.
    fn woken_yet(&mut self) -> bool;
    /// A busy session reaches its next delivery point (a hook or a turn
    /// boundary): what was injected, if anything.
    fn busy_boundary(&mut self) -> Option<String>;
    fn session_id(&self) -> &str;
    /// Lose this harness's delivery acks, or stop losing them, wherever it
    /// acks: a hook harness through the proxy, a push harness in the harness.
    fn lose_acks(&mut self, proxy: &HookProxy, on: bool);
    /// How many acks have been lost so far.
    fn acks_lost(&mut self, proxy: &HookProxy) -> usize;
    /// Whether the harness's MCP shim attaches to the session's roster row
    /// (Claude's does; Codex's registers no session, `MARSHAL_HARNESS=codex`).
    fn mcp_attaches(&self) -> bool;
    /// Cut the harness's daemon connection, or restore it, and say whether
    /// it's open afterwards. Only a push plugin runs the scenarios that call it.
    fn set_online(&mut self, _on: bool) -> bool {
        unreachable!("only a push plugin cuts its connection")
    }
    /// Make the harness's API hang taking a push, or let the hung ones go
    /// (they're taken after all). Only a push plugin runs it.
    fn hang_handovers(&mut self, _on: bool) {
        unreachable!("only a push plugin hangs a handover")
    }
    /// How many handovers have hung so far.
    fn handovers_hung(&mut self) -> usize {
        unreachable!("only a push plugin hangs a handover")
    }
    /// The session ends as its TUI ends it, and a new one starts in the same
    /// working directory with a new session id (MUS-3). Only Claude and Codex
    /// run the scenarios that call it.
    fn restart(&mut self, _daemon: &Daemon, _proxy: &HookProxy) {
        unreachable!("only Claude and Codex restart")
    }
    /// Start a second session in the same working directory while this one
    /// stays live, and return its id. It lives as long as the harness.
    fn sibling(&mut self, _daemon: &Daemon, _proxy: &HookProxy) -> String {
        unreachable!("only Claude and Codex start a sibling")
    }
}

/// Claude Code, as `plugins/marshal-shim/hooks/hooks.json` wires it: the
/// `claude-hook` commands with the hook JSON on stdin, an `asyncRewake` waiter
/// whose exit 2 starts a turn with its stderr, and the MCP shim Claude runs.
struct FakeClaude {
    session_id: String,
    cwd: String,
    env: Vec<(String, String)>,
    mcp: Option<Mcp>,
    waiter: Option<Child>,
    /// The first waiter comes from SessionStart, later ones from Stop.
    armed_before: bool,
    /// Other sessions in the same directory, for `sibling`.
    siblings: Vec<FakeClaude>,
}

/// The Claude Code version the fake's hook input is pinned to.
const CLAUDE_CODE_VERSION: &str = "2.1.318";

/// Claude Code's hook input for `event`, from the pinned fixtures.
fn claude_fixture(event: &str, session_id: &str, cwd: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("claude-code-{CLAUDE_CODE_VERSION}"))
        .join(format!("{event}.json"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("fixture {}: {e}", path.display()))
        .replace("{{session_id}}", session_id)
        .replace("{{cwd}}", cwd)
        .trim()
        .to_string()
}

impl FakeClaude {
    fn new() -> Self {
        Self {
            session_id: uuid::Uuid::new_v4().to_string(),
            cwd: "/tmp/conformance".into(),
            env: Vec::new(),
            mcp: None,
            waiter: None,
            armed_before: false,
            siblings: Vec::new(),
        }
    }

    fn hook_input(&self, fixture: &str) -> String {
        claude_fixture(fixture, &self.session_id, &self.cwd)
    }

    fn hook(&self, event: &str) -> Command {
        let mut cmd = support::shim(support::shared_home());
        cmd.args(["claude-hook", event])
            .env_remove("MARSHAL_DAEMON_ADDRESS")
            .env_remove("MYKO_ADDRESS")
            .envs(self.env.iter().cloned());
        cmd
    }

    /// Run a command hook to completion, as Claude does, and return stdout.
    fn run_hook(&self, event: &str, fixture: &str) -> String {
        let mut child = self
            .hook(event)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn claude-hook");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(self.hook_input(fixture).as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl FakeHarness for FakeClaude {
    fn start(&mut self, daemon: &Daemon, proxy: &HookProxy) {
        self.env = vec![
            (
                "XDG_CONFIG_HOME".into(),
                daemon.config_dir().display().to_string(),
            ),
            (
                "HOME".into(),
                daemon.dir.path().join("home").display().to_string(),
            ),
            ("MARSHAL_HOST".into(), "conformance".into()),
            ("MARSHAL_BASE_URL".into(), proxy.base.clone()),
            ("MARSHAL_WAITER_LOCK_WAIT_SECS".into(), "2".into()),
        ];
        // SessionStart: register and inject identity; Claude also spawns the
        // MCP shim. The SessionStart waiter is `arm_idle`.
        self.run_hook("session-start", "session-start");
        self.mcp = Some(Mcp::start(daemon, &self.session_id));
    }

    fn arm_idle(&mut self) {
        // The SessionStart or Stop hook's `asyncRewake` waiter.
        let fixture = if self.armed_before {
            "stop"
        } else {
            "session-start"
        };
        self.armed_before = true;
        let input = self.hook_input(fixture);
        let mut child = self
            .hook("wait")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn claude-hook wait");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        self.waiter = Some(child);
        // Let it reach the daemon and register its WakeWatcher.
        thread::sleep(Duration::from_millis(1500));
    }

    fn wait_for_wake(&mut self, within: Duration) -> Option<String> {
        let mut child = self.waiter.take()?;
        let exited = wait_until(within, || child.try_wait().ok().flatten().is_some());
        if !exited {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        let status = child.wait().unwrap();
        let mut stderr = String::new();
        let _ = child.stderr.take().unwrap().read_to_string(&mut stderr);
        // asyncRewake: exit 2 starts a turn with stderr; anything else doesn't.
        (status.code() == Some(2)).then_some(stderr)
    }

    fn woken_yet(&mut self) -> bool {
        self.waiter
            .as_mut()
            .is_some_and(|w| w.try_wait().ok().flatten().is_some())
    }

    fn busy_boundary(&mut self) -> Option<String> {
        let out = self.run_hook("post-tool-use", "post-tool-use");
        let v: Value = serde_json::from_str(out.trim()).ok()?;
        let text = v["hookSpecificOutput"]["additionalContext"].as_str()?;
        (!text.is_empty()).then(|| text.to_string())
    }

    fn restart(&mut self, daemon: &Daemon, proxy: &HookProxy) {
        // Claude exits: its waiter and its MCP shim go with it. The plugin
        // installs no SessionEnd hook for Claude, so nothing else is said.
        if let Some(mut child) = self.waiter.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.mcp = None;
        self.session_id = uuid::Uuid::new_v4().to_string();
        self.armed_before = false;
        self.start(daemon, proxy);
    }

    fn sibling(&mut self, daemon: &Daemon, proxy: &HookProxy) -> String {
        let mut other = FakeClaude::new();
        other.start(daemon, proxy);
        other.arm_idle();
        let id = other.session_id.clone();
        self.siblings.push(other);
        id
    }

    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn lose_acks(&mut self, proxy: &HookProxy, on: bool) {
        proxy.set_fault(on.then_some(AckFault::DropAckRequests));
    }

    fn acks_lost(&mut self, proxy: &HookProxy) -> usize {
        proxy.faulted()
    }

    fn mcp_attaches(&self) -> bool {
        true
    }
}

impl Drop for FakeClaude {
    fn drop(&mut self) {
        if let Some(mut child) = self.waiter.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The Codex version the fake's fixtures are pinned to.
const CODEX_VERSION: &str = "0.163.0";

/// A Codex fixture, with the thread id and cwd filled in.
fn codex_fixture(name: &str, thread_id: &str, cwd: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("codex-{CODEX_VERSION}"))
        .join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("fixture {}: {e}", path.display()))
        .replace("{{thread_id}}", thread_id)
        .replace("{{cwd}}", cwd);
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("fixture {}: {e}", path.display()))
}

/// The one thread a fake app-server hosts.
struct AppThread {
    /// A turn is running, so `turn/start` is refused.
    busy: bool,
    /// `turn/start` requests accepted so far.
    turns: usize,
}

/// A Codex app-server with one thread, attached to a TUI: every connection
/// gets `thread/started` for it, and `turn/start` is accepted while the thread
/// is idle (and then a turn runs, so it's busy) and refused while it's busy.
struct FakeAppServer {
    endpoint: String,
    thread: Arc<std::sync::Mutex<AppThread>>,
    stop: Option<mpsc::Sender<()>>,
    join: Option<thread::JoinHandle<()>>,
}

impl FakeAppServer {
    fn start(thread_id: String, cwd: String) -> Self {
        let state = Arc::new(std::sync::Mutex::new(AppThread {
            busy: true,
            turns: 0,
        }));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let (stop, stopped) = mpsc::channel::<()>();
        let shared = state.clone();
        let join = thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let accept = async {
                    loop {
                        let Ok((stream, _)) = listener.accept().await else {
                            continue;
                        };
                        let (state, tid, cwd) = (shared.clone(), thread_id.clone(), cwd.clone());
                        tokio::spawn(async move {
                            // A bridge that's killed resets without a close
                            // handshake; that's cleanup, not a finding.
                            let _ = serve_app_server(stream, state, tid, cwd).await;
                        });
                    }
                };
                tokio::select! {
                    () = accept => {}
                    _ = tokio::task::spawn_blocking(move || stopped.recv()) => {}
                }
            });
        });
        Self {
            endpoint,
            thread: state,
            stop: Some(stop),
            join: Some(join),
        }
    }

    fn turns(&self) -> usize {
        self.thread.lock().unwrap().turns
    }

    fn set_busy(&self, busy: bool) {
        self.thread.lock().unwrap().busy = busy;
    }
}

impl Drop for FakeAppServer {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

async fn serve_app_server(
    stream: tokio::net::TcpStream,
    state: Arc<std::sync::Mutex<AppThread>>,
    thread_id: String,
    cwd: String,
) -> Result<(), String> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as Frame;

    let mut ws = tokio_tungstenite::accept_async(stream)
        .await
        .map_err(|e| e.to_string())?;
    let fixture = |name: &str| codex_fixture(name, &thread_id, &cwd);
    let mut announced = false;
    while let Some(frame) = ws.next().await {
        let Ok(Frame::Text(text)) = frame else {
            continue;
        };
        let Ok(req) = serde_json::from_str::<Value>(text.as_ref()) else {
            continue;
        };
        let method = req["method"].as_str().unwrap_or_default();
        let reply = match method {
            "initialize" => Some(json!({"id": req["id"], "result": fixture("initialize-result")})),
            "initialized" => {
                // An attached TUI's thread: every client hears about it.
                if !announced {
                    announced = true;
                    ws.send(Frame::Text(fixture("thread-started").to_string().into()))
                        .await
                        .map_err(|e| e.to_string())?;
                }
                None
            }
            "turn/start" if req["params"]["threadId"] == json!(thread_id) => {
                let mut t = state.lock().unwrap();
                if t.busy {
                    Some(json!({"id": req["id"], "error": fixture("turn-start-busy-error")}))
                } else {
                    t.busy = true;
                    t.turns += 1;
                    Some(json!({"id": req["id"], "result": fixture("turn-start-result")}))
                }
            }
            "turn/start" => Some(json!({
                "id": req["id"],
                "error": {"code": -32602, "message": "no such thread"}
            })),
            // Anything else the bridge asks gets an empty result, so it
            // never hangs waiting on the fake.
            _ if req.get("id").is_some() => Some(json!({"id": req["id"], "result": {}})),
            _ => None,
        };
        if let Some(reply) = reply {
            ws.send(Frame::Text(reply.to_string().into()))
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Codex, as `marshal-shim codex-run` sets it up: the real `codex-bridge`
/// watching the daemon and a (fake) shared app-server, and the `codex-hook`
/// commands Codex's hooks run. An idle thread is woken by the bridge's
/// `turn/start`; the turn's `UserPromptSubmit` hook then injects the inbox,
/// and the daemon acks it once that reply is flushed.
struct FakeCodex {
    thread_id: String,
    cwd: String,
    hook_base: String,
    env: Vec<(String, String)>,
    app: Option<FakeAppServer>,
    bridge: Option<Child>,
    turns_before: usize,
}

impl FakeCodex {
    fn new() -> Self {
        Self {
            thread_id: uuid::Uuid::new_v4().to_string(),
            cwd: "/tmp/conformance-codex".into(),
            hook_base: String::new(),
            env: Vec::new(),
            app: None,
            bridge: None,
            turns_before: 0,
        }
    }

    fn app(&self) -> &FakeAppServer {
        self.app.as_ref().expect("started")
    }

    /// Run a Codex command hook and return what it injected.
    fn run_hook(&self, event: &str, fixture: &str) -> Option<String> {
        let input = codex_fixture(fixture, &self.thread_id, &self.cwd).to_string();
        let mut child = support::shim(support::shared_home())
            .args(["codex-hook", event, &self.hook_base])
            .env_remove("MARSHAL_DAEMON_ADDRESS")
            .env_remove("MYKO_ADDRESS")
            .envs(self.env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn codex-hook");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let v: Value = serde_json::from_slice(&out.stdout).ok()?;
        let text = v["hookSpecificOutput"]["additionalContext"].as_str()?;
        (!text.is_empty()).then(|| text.to_string())
    }
}

impl FakeHarness for FakeCodex {
    fn start(&mut self, daemon: &Daemon, proxy: &HookProxy) {
        let tmp = daemon.dir.path().join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        self.hook_base = proxy.base.clone();
        self.env = vec![
            (
                "XDG_CONFIG_HOME".into(),
                daemon.config_dir().display().to_string(),
            ),
            (
                "HOME".into(),
                daemon.dir.path().join("home").display().to_string(),
            ),
            ("MARSHAL_HOST".into(), "conformance".into()),
            ("MARSHAL_BASE_URL".into(), proxy.base.clone()),
            ("TMPDIR".into(), tmp.display().to_string()),
        ];
        let app = FakeAppServer::start(self.thread_id.clone(), self.cwd.clone());
        let ready = daemon.dir.path().join("bridge.ready");
        let log = std::fs::File::create(daemon.dir.path().join("bridge.log")).unwrap();
        let bridge = support::shim(support::shared_home())
            .args([
                "codex-bridge",
                "--endpoint",
                &app.endpoint,
                "--daemon",
                &daemon.ws_address(),
                "--poll-ms",
                "100",
                "--ready-file",
            ])
            .arg(&ready)
            .env_remove("MARSHAL_DAEMON_ADDRESS")
            .env_remove("MYKO_ADDRESS")
            .envs(self.env.iter().cloned())
            .env("RUST_LOG", "marshal_shim=debug")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("spawn codex-bridge");
        self.app = Some(app);
        self.bridge = Some(bridge);
        assert!(
            wait_until(Duration::from_secs(20), || ready.is_file()),
            "codex-bridge never became ready; log:\n{}",
            std::fs::read_to_string(daemon.dir.path().join("bridge.log")).unwrap_or_default()
        );
    }

    fn arm_idle(&mut self) {
        // The turn ends and the thread sits idle in the TUI.
        self.turns_before = self.app().turns();
        self.app().set_busy(false);
    }

    fn wait_for_wake(&mut self, within: Duration) -> Option<String> {
        let before = self.turns_before;
        if !wait_until(within, || self.app().turns() > before) {
            return None;
        }
        // The bridge started a turn: Codex runs its UserPromptSubmit hook.
        Some(
            self.run_hook("prompt-submit", "user-prompt-submit")
                .unwrap_or_default(),
        )
    }

    fn woken_yet(&mut self) -> bool {
        self.app().turns() > self.turns_before
    }

    fn busy_boundary(&mut self) -> Option<String> {
        self.app().set_busy(true);
        self.run_hook("post-tool-use", "post-tool-use")
    }

    fn restart(&mut self, daemon: &Daemon, proxy: &HookProxy) {
        // The thread ends: Codex runs SessionEnd (the shim reads only the
        // ids and cwd), and the TUI and its app-server go.
        self.run_hook("session-end", "user-prompt-submit");
        if let Some(mut child) = self.bridge.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.app = None;
        let _ = std::fs::remove_file(daemon.dir.path().join("bridge.ready"));
        self.thread_id = uuid::Uuid::new_v4().to_string();
        self.start(daemon, proxy);
    }

    fn sibling(&mut self, _daemon: &Daemon, _proxy: &HookProxy) -> String {
        // A second thread in the same cwd, registered by its SessionStart
        // hook. Like the first, it's a hook-owned row with no client: a live
        // idle Codex thread, as far as the daemon can tell.
        let id = uuid::Uuid::new_v4().to_string();
        let input = codex_fixture("user-prompt-submit", &id, &self.cwd).to_string();
        let mut child = support::shim(support::shared_home())
            .args(["codex-hook", "session-start", &self.hook_base])
            .env_remove("MARSHAL_DAEMON_ADDRESS")
            .env_remove("MYKO_ADDRESS")
            .envs(self.env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn codex-hook");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let _ = child.wait();
        id
    }

    fn session_id(&self) -> &str {
        &self.thread_id
    }

    fn lose_acks(&mut self, proxy: &HookProxy, on: bool) {
        proxy.set_fault(on.then_some(AckFault::DropReplies("/hook/prompt-submit")));
    }

    fn acks_lost(&mut self, proxy: &HookProxy) -> usize {
        proxy.faulted()
    }

    fn mcp_attaches(&self) -> bool {
        false
    }
}

impl Drop for FakeCodex {
    fn drop(&mut self) {
        if let Some(mut child) = self.bridge.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// bun, for the TypeScript plugins' fakes: `MARSHAL_BUN`, else `bun` on PATH.
fn bun_bin() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("MARSHAL_BUN").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(path));
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("bun"))
            .find(|bun| bun.is_file())
    })
}

/// A TypeScript plugin, loaded by its `test/conformance-driver.ts` with a fake
/// host that stands in for the harness's API, and driven over the driver's
/// line protocol. The plugin connects to the daemon itself, as it does inside
/// its harness, and takes the live push. Its `node_modules` must be installed.
struct FakePlugin {
    plugin: &'static str,
    session_id: String,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    replies: Option<mpsc::Receiver<String>>,
}

/// The prefix on the driver's replies, which the plugin's own logging lacks.
const DRIVER_REPLY: &str = "@@conformance ";

impl FakePlugin {
    fn new(plugin: &'static str) -> Self {
        Self {
            plugin,
            session_id: String::new(),
            child: None,
            stdin: None,
            replies: None,
        }
    }

    fn call(&mut self, cmd: Value) -> Value {
        let stdin = self.stdin.as_mut().expect("started");
        writeln!(stdin, "{cmd}").expect("write to the plugin driver");
        stdin.flush().expect("flush the plugin driver");
        let wait = cmd["ms"].as_u64().map_or(0, |ms| ms / 1000) + 20;
        let line = self
            .replies
            .as_ref()
            .expect("started")
            .recv_timeout(Duration::from_secs(wait))
            .unwrap_or_else(|_| panic!("{} driver didn't answer {cmd}", self.plugin));
        let reply: Value = serde_json::from_str(&line).expect("driver reply is JSON");
        if let Some(e) = reply.get("error") {
            panic!("{} driver: {e}", self.plugin);
        }
        reply
    }

    fn text(reply: &Value) -> Option<String> {
        reply["text"].as_str().map(str::to_string)
    }
}

impl FakeHarness for FakePlugin {
    fn start(&mut self, daemon: &Daemon, _proxy: &HookProxy) {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins")
            .join(self.plugin);
        assert!(
            dir.join("node_modules").is_dir(),
            "{}/node_modules is missing: install it first (npm ci --workspaces=false)",
            dir.display()
        );
        let log =
            std::fs::File::create(daemon.dir.path().join(format!("{}.log", self.plugin))).unwrap();
        let mut child = Command::new(bun_bin().expect("bun"))
            .arg("run")
            .arg(dir.join("test/conformance-driver.ts"))
            .current_dir(daemon.dir.path())
            .env_remove("MYKO_ADDRESS")
            .env("MARSHAL_DAEMON_ADDRESS", daemon.ws_address())
            .env("MARSHAL_HOST", "conformance")
            .env("MARSHAL_OPERATOR", "conformance")
            // The plugin's hold on a push lapses after the same lease (MUS-81).
            .env("MARSHAL_ACK_LEASE_SECS", ACK_LEASE_SECS.to_string())
            .env("HOME", daemon.dir.path().join("home"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .expect("spawn the plugin driver under bun");
        let stdout = child.stdout.take().unwrap();
        let (tx, replies) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(reply) = line.strip_prefix(DRIVER_REPLY)
                    && tx.send(reply.to_string()).is_err()
                {
                    break;
                }
            }
        });
        self.stdin = child.stdin.take();
        self.child = Some(child);
        self.replies = Some(replies);
        let session_file = daemon
            .dir
            .path()
            .join(format!("{}-session.jsonl", self.plugin));
        let reply = self.call(json!({"cmd": "start", "session_file": session_file}));
        self.session_id = reply["session_id"]
            .as_str()
            .expect("session id")
            .to_string();
    }

    fn arm_idle(&mut self) {
        self.call(json!({"cmd": "arm_idle"}));
    }

    fn wait_for_wake(&mut self, within: Duration) -> Option<String> {
        let ms = within.as_millis() as u64;
        Self::text(&self.call(json!({"cmd": "wait_for_wake", "ms": ms})))
    }

    fn woken_yet(&mut self) -> bool {
        self.call(json!({"cmd": "woken_yet"}))["woken"] == json!(true)
    }

    fn busy_boundary(&mut self) -> Option<String> {
        Self::text(&self.call(json!({"cmd": "busy_boundary"})))
    }

    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn lose_acks(&mut self, _proxy: &HookProxy, on: bool) {
        // The daemon marks a pushed message read when the push reaches the
        // plugin's connection. Lose it after that: the harness's API throws.
        self.call(json!({"cmd": "lose_deliveries", "on": on}));
    }

    fn acks_lost(&mut self, _proxy: &HookProxy) -> usize {
        self.call(json!({"cmd": "lost"}))["lost"]
            .as_u64()
            .unwrap_or(0) as usize
    }

    fn mcp_attaches(&self) -> bool {
        false
    }

    fn set_online(&mut self, on: bool) -> bool {
        self.call(json!({"cmd": "set_online", "on": on}))["open"] == json!(true)
    }

    fn hang_handovers(&mut self, on: bool) {
        self.call(json!({"cmd": "hang_deliveries", "on": on}));
    }

    fn handovers_hung(&mut self) -> usize {
        self.call(json!({"cmd": "hung"}))["hung"]
            .as_u64()
            .unwrap_or(0) as usize
    }
}

impl Drop for FakePlugin {
    fn drop(&mut self) {
        drop(self.stdin.take());
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ---------------------------------------------------------------------------
// Scenarios

/// Everything one cell needs: a fresh daemon, the proxy, the harness under
/// test, and a sender.
struct Cell {
    daemon: Daemon,
    proxy: HookProxy,
    harness: Box<dyn FakeHarness>,
    sender: Mcp,
    sender_id: String,
}

impl Cell {
    fn new(
        mode: Mode,
        mut harness: Box<dyn FakeHarness>,
        postgres: Option<&str>,
        persist_file: bool,
    ) -> Self {
        let daemon = Daemon::start(mode, postgres, persist_file);
        let proxy = HookProxy::start(daemon.hook_addr());
        harness.start(&daemon, &proxy);
        let sender_id = uuid::Uuid::new_v4().to_string();
        let sender = Self::sender(&daemon, &sender_id);
        Self {
            daemon,
            proxy,
            harness,
            sender,
            sender_id,
        }
    }

    /// A sender session, registered as a Claude session's hook would, with
    /// its MCP shim.
    fn sender(daemon: &Daemon, sender_id: &str) -> Mcp {
        let mut hook = support::shim(support::shared_home())
            .args(["claude-hook", "session-start"])
            .env("XDG_CONFIG_HOME", daemon.config_dir())
            .env("HOME", daemon.dir.path().join("home"))
            .env("MARSHAL_HOST", "conformance")
            .env("MARSHAL_BASE_URL", format!("http://{}", daemon.hook_addr()))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        hook.stdin
            .take()
            .unwrap()
            .write_all(
                json!({"session_id": sender_id, "cwd": "/tmp/sender"})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        hook.wait().unwrap();
        Mcp::start(daemon, sender_id)
    }

    fn recipient(&self) -> String {
        self.harness.session_id().to_string()
    }

    /// Wait until the sender can see the recipient on the roster.
    fn recipient_on_roster(&mut self) -> Result<Value, String> {
        let id = self.recipient();
        self.sender
            .row(&id, Duration::from_secs(15))
            .ok_or_else(|| format!("recipient {id} never appeared on the roster"))
    }

    fn send(&mut self, body: &str) -> Result<Value, String> {
        self.recipient_on_roster()?;
        let to = self.recipient();
        self.sender.send(&to, body, None)
    }
}

fn token(label: &str) -> String {
    format!(
        "{label}-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    )
}

fn contains(seen: &Option<String>, token: &str) -> bool {
    seen.as_deref().is_some_and(|s| s.contains(token))
}

type Scenario = fn(&mut Cell) -> Result<(), String>;

const SCENARIOS: &[(&str, Scenario)] = &[
    ("idle-wakes-on-dm", idle_wakes_on_dm),
    ("busy-gets-it-at-next-hook", busy_gets_it_at_next_hook),
    ("dropped-ack-redelivers", dropped_ack_redelivers),
    ("lost-hook-reply-redelivers", lost_hook_reply_redelivers),
    ("duplicate-shown-at-most-once", duplicate_shown_at_most_once),
    ("burst-both-delivered", burst_both_delivered),
    ("daemon-restart-mid-wait", daemon_restart_mid_wait),
    ("daemon-restart-mid-wait-pg", daemon_restart_mid_wait),
    ("restart-keeps-unread-mail", restart_keeps_unread_mail),
    ("restart-burst-shown-once-pg", restart_burst_shown_once),
    (
        "restart-without-mail-stays-idle-pg",
        restart_without_mail_stays_idle,
    ),
    ("restart-keeps-unread-mail-file", restart_keeps_unread_mail),
    ("operator-pause-stops-wakes", operator_pause_stops_wakes),
    ("resume-wakes-held-mail", resume_wakes_held_mail),
    ("broadcast-never-wakes", broadcast_never_wakes),
    ("wake-none-not-expected", wake_none_not_expected),
    ("armed-shows-its-wake-path", armed_shows_its_wake_path),
    ("mail-waiting-at-reconnect", mail_waiting_at_reconnect),
    ("hung-handover-released", hung_handover_released),
    ("restart-keeps-identity", restart_keeps_identity),
    ("live-sibling-keeps-its-name", live_sibling_keeps_its_name),
    (
        "unread-mail-follows-a-restart",
        unread_mail_follows_a_restart,
    ),
];

/// Scenario 1: An idle session wakes on a direct message.
fn idle_wakes_on_dm(c: &mut Cell) -> Result<(), String> {
    c.harness.arm_idle();
    let t = token("idle");
    c.send(&t)?;
    let woke = c.harness.wait_for_wake(WAKE_WITHIN);
    if !contains(&woke, &t) {
        return Err(format!("not woken with the message; woke with {woke:?}"));
    }
    Ok(())
}

/// Scenario 2: A busy session gets the message at its next hook or turn, once.
fn busy_gets_it_at_next_hook(c: &mut Cell) -> Result<(), String> {
    let t = token("busy");
    c.send(&t)?;
    let seen = c.harness.busy_boundary();
    if !contains(&seen, &t) {
        return Err(format!("next hook didn't carry it; got {seen:?}"));
    }
    let again = c.harness.busy_boundary();
    if contains(&again, &t) {
        return Err("the hook after that showed it again".into());
    }
    Ok(())
}

/// Scenario 3: No loss: when a delivery's ack is lost (Claude: the waiter's
/// `/hook/ack`; Codex: the hook's reply, which the daemon acks on flush), the
/// message comes again at the session's next delivery point once the lease
/// lapses, and not after an ack that arrives.
fn dropped_ack_redelivers(c: &mut Cell) -> Result<(), String> {
    c.harness.lose_acks(&c.proxy, true);
    c.harness.arm_idle();
    let t = token("ack");
    c.send(&t)?;
    let _first = c.harness.wait_for_wake(WAKE_WITHIN);
    if c.harness.acks_lost(&c.proxy) == 0 {
        return Err("no delivery's ack was lost, so the scenario didn't run".into());
    }
    c.harness.lose_acks(&c.proxy, false);
    thread::sleep(Duration::from_secs(ACK_LEASE_SECS + 1));
    let again = c.harness.busy_boundary();
    if !contains(&again, &t) {
        return Err(format!(
            "not delivered again after its ack was lost (message lost); got {again:?}"
        ));
    }
    thread::sleep(Duration::from_secs(ACK_LEASE_SECS + 1));
    if contains(&c.harness.busy_boundary(), &t) {
        return Err("delivered again after a successful ack".into());
    }
    Ok(())
}

/// Scenario 3b: No loss on a busy session's hook either: when the reply to a
/// prompt hook is lost after the daemon sent it (as when the harness kills a
/// hook at its timeout), the message comes again at the next hook.
fn lost_hook_reply_redelivers(c: &mut Cell) -> Result<(), String> {
    c.proxy
        .set_fault(Some(AckFault::DropReplies("/hook/prompt-submit")));
    let t = token("reply");
    c.send(&t)?;
    let _lost = c.harness.busy_boundary();
    if c.proxy.faulted() == 0 {
        return Err("no hook reply carrying mail was lost, so the scenario didn't run".into());
    }
    c.proxy.set_fault(None);
    thread::sleep(Duration::from_secs(ACK_LEASE_SECS + 1));
    let again = c.harness.busy_boundary();
    if !contains(&again, &t) {
        return Err(format!(
            "not delivered again after the hook's reply was lost (message lost); got {again:?}"
        ));
    }
    Ok(())
}

/// Scenario 4: A duplicate is shown at most once: a send retried with the same
/// idempotency key is one message, and mail delivered by the waiter isn't
/// injected again by the next hook.
fn duplicate_shown_at_most_once(c: &mut Cell) -> Result<(), String> {
    c.recipient_on_roster()?;
    let to = c.recipient();
    let t = token("dup");
    let key = uuid::Uuid::new_v4().to_string();
    let a = c.sender.send(&to, &t, Some(&key))?;
    let b = c.sender.send(&to, &t, Some(&key))?;
    if a["message_id"] != b["message_id"] {
        return Err(format!(
            "a retried send made a second message: {} and {}",
            a["message_id"], b["message_id"]
        ));
    }
    let seen = c.harness.busy_boundary().unwrap_or_default();
    if seen.matches(&t).count() != 1 {
        return Err(format!(
            "shown {} times in one hook, not once",
            seen.matches(&t).count()
        ));
    }
    if contains(&c.harness.busy_boundary(), &t) {
        return Err("shown again at the next hook".into());
    }
    c.harness.arm_idle();
    let t2 = token("dup-wake");
    c.send(&t2)?;
    let woke = c.harness.wait_for_wake(WAKE_WITHIN).unwrap_or_default();
    match woke.matches(&t2).count() {
        0 => return Err("the wake didn't deliver the second message".into()),
        1 => {}
        // As when a push's text and the woken turn's inbox drain both carry it.
        n => {
            return Err(format!(
                "the wake showed the second message {n} times, not once"
            ));
        }
    }
    if contains(&c.harness.busy_boundary(), &t2) {
        return Err("mail the waiter delivered was injected again by the next hook".into());
    }
    Ok(())
}

/// Scenario 4b: Two direct messages sent back to back to an idle session both
/// reach the model, between the wake and the next delivery point. A harness
/// that drains the inbox while handling the first must show what it drains,
/// not just ack it.
fn burst_both_delivered(c: &mut Cell) -> Result<(), String> {
    c.harness.arm_idle();
    let (t1, t2) = (token("burst-a"), token("burst-b"));
    c.send(&t1)?;
    c.send(&t2)?;
    let woke = c.harness.wait_for_wake(WAKE_WITHIN).unwrap_or_default();
    // Whatever the wake didn't carry must come at the next delivery point.
    thread::sleep(Duration::from_millis(500));
    let next = c.harness.busy_boundary().unwrap_or_default();
    let seen = format!("{woke}\n{next}");
    let missing: Vec<&str> = [&t1, &t2]
        .into_iter()
        .filter(|t| !seen.contains(t.as_str()))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "never shown: {missing:?} (the wake showed {:?}, the next delivery point {:?})",
            woke, next
        ));
    }
    Ok(())
}

/// Scenario 5: The daemon restarts while a session waits; the session still wakes on
/// mail sent after the restart.
fn daemon_restart_mid_wait(c: &mut Cell) -> Result<(), String> {
    c.harness.arm_idle();
    c.daemon.restart();
    // The old sender's connection went with the old daemon, and without
    // Postgres so did the roster; register the sender again.
    c.sender = Cell::sender(&c.daemon, &c.sender_id);
    let t = token("restart");
    c.send(&t)?;
    let woke = c.harness.wait_for_wake(Duration::from_secs(25));
    if !contains(&woke, &t) {
        return Err(format!(
            "not woken after the restart; got {woke:?}; daemon log:\n{}",
            c.daemon.log()
        ));
    }
    Ok(())
}

/// Scenarios that run against Postgres (`CONFORMANCE_POSTGRES_URL`), so a restart
/// keeps the roster and the mail (MUS-76). Every other cell runs in memory.
const POSTGRES_SCENARIOS: &[&str] = &[
    "daemon-restart-mid-wait-pg",
    "restart-keeps-unread-mail",
    "restart-burst-shown-once-pg",
    "restart-without-mail-stays-idle-pg",
];

/// Scenario 5c (Postgres, MUS-90): Mail sent all through a plugin's reconnect
/// after a restart is each shown once. With Postgres the session's row is on the
/// roster from the start, so some of these are sent while the plugin is still
/// away, some race its reconnect, where a live push can land while the
/// reconnect's inbox pull is in flight, and some are pushed live after it.
fn restart_burst_shown_once(c: &mut Cell) -> Result<(), String> {
    c.harness.arm_idle();
    c.daemon.restart();
    c.sender = Cell::sender(&c.daemon, &c.sender_id);
    // Every 350 ms for about 5 s, spanning the plugin's reconnect (about 3 s),
    // and under the daemon's limit of 20 messages at once.
    let mut sent = Vec::new();
    for i in 0..14 {
        let t = token(&format!("burst{i}"));
        c.send(&t)?;
        sent.push(t);
        thread::sleep(Duration::from_millis(350));
    }
    let woke = c
        .harness
        .wait_for_wake(Duration::from_secs(25))
        .unwrap_or_default();
    thread::sleep(Duration::from_millis(500));
    let next = c.harness.busy_boundary().unwrap_or_default();
    // Past the hold lease, anything held but never acked would come back.
    thread::sleep(Duration::from_secs(ACK_LEASE_SECS + 1));
    let after = c.harness.busy_boundary().unwrap_or_default();
    let seen = format!("{woke}\n{next}\n{after}");
    let wrong: Vec<String> = sent
        .iter()
        .map(|t| (t, seen.matches(t.as_str()).count()))
        .filter(|(_, n)| *n != 1)
        .map(|(t, n)| format!("{t} x{n}"))
        .collect();
    if !wrong.is_empty() {
        return Err(format!(
            "not shown exactly once: {wrong:?}; the wake showed {woke:?}, then {next:?}, then {after:?}"
        ));
    }
    Ok(())
}

/// Scenario 5d (Postgres, MUS-90): A restart with no mail doesn't wake an idle
/// session. The reconnect pulls the inbox, and an empty one must stay quiet.
fn restart_without_mail_stays_idle(c: &mut Cell) -> Result<(), String> {
    c.harness.arm_idle();
    c.daemon.restart();
    // Long enough for the plugin to reconnect (about 3 s) and pull.
    let woke = c.harness.wait_for_wake(QUIET_FOR + Duration::from_secs(4));
    if woke.is_some() {
        return Err(format!("woken by a restart with no mail: {woke:?}"));
    }
    Ok(())
}

/// Scenarios whose daemon keeps its store in a file, with no Postgres (MUS-17).
const FILE_SCENARIOS: &[&str] = &["restart-keeps-unread-mail-file"];

/// Scenario 5b (Postgres), and 5e with the store in a file instead (MUS-17): Mail
/// sent before a restart, while the session was busy, is kept, and wakes the
/// session once it goes idle after the restart.
fn restart_keeps_unread_mail(c: &mut Cell) -> Result<(), String> {
    let t = token("before-restart");
    c.send(&t)?;
    c.daemon.restart();
    c.harness.arm_idle();
    let woke = c.harness.wait_for_wake(Duration::from_secs(25));
    if !contains(&woke, &t) {
        return Err(format!(
            "mail sent before the restart didn't wake it after; got {woke:?}; daemon log:\n{}",
            c.daemon.log()
        ));
    }
    Ok(())
}

/// Scenario 6: The operator's pause stops wakes. Mail still arrives at the next hook,
/// which doesn't wake anyone.
fn operator_pause_stops_wakes(c: &mut Cell) -> Result<(), String> {
    c.daemon.cli(&["pause"]);
    // The daemon reads its controls every 2 s.
    thread::sleep(Duration::from_millis(2500));
    c.harness.arm_idle();
    let t = token("paused");
    c.send(&t)?;
    let woke = c.harness.wait_for_wake(QUIET_FOR);
    if woke.is_some() {
        return Err(format!("woken while paused: {woke:?}"));
    }
    let seen = c.harness.busy_boundary();
    if !contains(&seen, &t) {
        return Err(format!(
            "the next hook didn't deliver it while paused; got {seen:?}"
        ));
    }
    Ok(())
}

/// Scenario 6b: Resuming alone wakes a session whose mail arrived while wakes
/// were paused. No new message is sent after the resume: the held, still
/// unread mail is enough.
fn resume_wakes_held_mail(c: &mut Cell) -> Result<(), String> {
    c.daemon.cli(&["pause"]);
    // The daemon reads its controls every 2 s.
    thread::sleep(Duration::from_millis(2500));
    c.harness.arm_idle();
    let t = token("held");
    c.send(&t)?;
    // Held through a pause long enough for any wake to have fired.
    thread::sleep(QUIET_FOR);
    if c.harness.woken_yet() {
        return Err("woken while paused, so resuming wasn't what woke it".into());
    }
    c.daemon.cli(&["resume"]);
    let woke = c.harness.wait_for_wake(WAKE_WITHIN);
    if !contains(&woke, &t) {
        return Err(format!(
            "resuming didn't wake the session with the mail held during the pause; got {woke:?}"
        ));
    }
    Ok(())
}

/// Scenario 7: Broadcasts never wake anyone, and aren't injected either.
fn broadcast_never_wakes(c: &mut Cell) -> Result<(), String> {
    c.recipient_on_roster()?;
    c.harness.arm_idle();
    let t = token("broadcast");
    c.sender.broadcast("everyone", &t)?;
    let woke = c.harness.wait_for_wake(QUIET_FOR);
    if woke.is_some() {
        return Err(format!("woken by a broadcast: {woke:?}"));
    }
    if contains(&c.harness.busy_boundary(), &t) {
        return Err("a broadcast was injected at the next hook".into());
    }
    Ok(())
}

/// Scenario 8: A session the roster calls `wake: none` must not be reported to the
/// sender as `wake: expected` (MUS-71). Here the session's MCP shim is
/// connected, but no waiter runs and it isn't busy, so nothing can wake it.
fn wake_none_not_expected(c: &mut Cell) -> Result<(), String> {
    let mut row = c.recipient_on_roster()?;
    if c.harness.mcp_attaches() {
        // A hook-registered row has pid 0; the session's MCP shim fills it in
        // when it attaches. Until then the roster says `none` for a different
        // reason, and the bug can't show.
        let id = c.recipient();
        let deadline = Instant::now() + Duration::from_secs(15);
        while row["pid"].as_u64().unwrap_or(0) == 0 {
            if Instant::now() >= deadline {
                return Err("precondition: the session's MCP shim never attached".into());
            }
            thread::sleep(Duration::from_millis(250));
            row = c
                .sender
                .row(&id, Duration::from_secs(5))
                .unwrap_or_default();
        }
    }
    if row["wake"] != json!("none") {
        return Err(format!(
            "precondition: the roster should say wake none, says {}",
            row["wake"]
        ));
    }
    let r = c.send(&token("none"))?;
    if r["wake"] == json!("expected") {
        return Err(format!(
            "send said wake expected for a session the roster says can't wake (live_push {})",
            r["live_push"]
        ));
    }
    Ok(())
}

/// Scenario 9: An idle session that something can wake is shown so, and it does wake. The roster
/// says `waiting` for a held wait (Claude's waiter; for Codex, its app-server bridge, which holds
/// none, MUS-77) and then a send says `expected`, or `channel` for a plugin that takes the live
/// push. Never `none`.
fn armed_shows_its_wake_path(c: &mut Cell) -> Result<(), String> {
    c.harness.arm_idle();
    let id = c.recipient();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut row = c.recipient_on_roster()?;
    while row["wake"] != json!("waiting") && row["wake"] != json!("channel") {
        if Instant::now() >= deadline {
            return Err(format!(
                "the roster says wake {} for an armed idle session",
                row["wake"]
            ));
        }
        thread::sleep(Duration::from_millis(250));
        row = c
            .sender
            .row(&id, Duration::from_secs(5))
            .unwrap_or_default();
    }
    let waiting = row["wake"] == json!("waiting");
    let t = token("armed");
    let r = c.send(&t)?;
    if waiting && r["wake"] != json!("expected") {
        return Err(format!(
            "send said wake {} for a session the roster says is waiting",
            r["wake"]
        ));
    }
    let woke = c.harness.wait_for_wake(WAKE_WITHIN);
    if !contains(&woke, &t) {
        return Err(format!("not woken with the message; woke with {woke:?}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The grid

/// One column of the grid.
struct Harness {
    name: &'static str,
    mode: Mode,
    make: fn() -> Box<dyn FakeHarness>,
    /// Scenarios that don't apply to this harness, and why. They show as
    /// `n/a` in the grid.
    not_applicable: &'static [(&'static str, &'static str)],
    /// A TypeScript plugin's fake, which needs bun (`bun_bin`).
    needs_bun: bool,
    /// Why this whole column doesn't run on Marshal yet, if it doesn't: every
    /// cell shows `n/a` with this reason (MAR-14 stage 1).
    excluded: Option<&'static str>,
}

/// Scenarios that don't apply on Marshal at all, for any harness, and why. The
/// suite came from muster, which has features Marshal doesn't yet.
const MARSHAL_NOT_APPLICABLE: &[(&str, &str)] = &[
    (
        "operator-pause-stops-wakes",
        "Marshal has no operator pause (muster's `daemon pause`, MUS-10)",
    ),
    (
        "resume-wakes-held-mail",
        "Marshal has no operator pause to resume (MUS-10)",
    ),
    (
        "restart-keeps-unread-mail-file",
        "Marshal has no file-backed store (muster's MUS-17); the Postgres cells cover restarts",
    ),
];

const TLS_EXCLUDED: &str = "Marshal has no TLS gate yet (MAR-23); runs as plain mode would";
const PLUGIN_EXCLUDED: &str =
    "the push plugins' columns come with MAR-10 and MAR-11 (stage 2 of MAR-14)";

/// Scenario 9 (MUS-81, upstream #134's case): Mail that waited out a
/// dropped connection is shown once, at the next turn, and a push after the
/// reconnect doesn't take it with it. While the plugin is offline the daemon
/// can't push, so the message stays unread. Then a second message is pushed
/// live: a plugin that drains the inbox after a push would ack the first one
/// there without showing it.
fn mail_waiting_at_reconnect(c: &mut Cell) -> Result<(), String> {
    c.recipient_on_roster()?;
    let to = c.recipient();
    if c.harness.set_online(false) {
        return Err("the plugin's connection never dropped, so the scenario didn't run".into());
    }
    // The plugin's side is closed; give the daemon a moment to handle the
    // close too, or the send below can still be queued on the dying
    // connection (seen once in a full grid run).
    thread::sleep(Duration::from_secs(1));
    let waiting = token("waiting");
    let sent = c.sender.send(&to, &waiting, None)?;
    if sent["live_push"] == json!("delivered") {
        return Err(format!(
            "pushed while the plugin was offline, so the scenario didn't run: {sent}"
        ));
    }
    if !c.harness.set_online(true) {
        return Err("the plugin never reconnected".into());
    }
    let pushed = token("pushed");
    let sent = c.sender.send(&to, &pushed, None)?;
    if sent["live_push"] != json!("delivered") {
        return Err(format!(
            "the message after the reconnect wasn't pushed, so the scenario didn't run: {sent}"
        ));
    }
    let seen = c.harness.busy_boundary().unwrap_or_default();
    for (what, t) in [("the mail that waited", &waiting), ("the push", &pushed)] {
        let n = seen.matches(t.as_str()).count();
        if n != 1 {
            return Err(format!(
                "{what} was shown {n} times at the next turn, not once; got {seen:?}"
            ));
        }
    }
    thread::sleep(Duration::from_secs(ACK_LEASE_SECS + 1));
    let again = c.harness.busy_boundary();
    if contains(&again, &waiting) || contains(&again, &pushed) {
        return Err(format!("shown again at the turn after; got {again:?}"));
    }
    Ok(())
}

/// Scenario 10 (MUS-81): A push whose handover hangs isn't lost behind it.
/// The harness's API never resolves taking the push. While the plugin holds
/// it, a turn's pull doesn't show it too; once the hold's lease lapses, the
/// next turn shows it. When the hung handover finally goes through, the
/// harness has it a second time (the design's accepted duplicate), but no
/// later pull shows it again.
fn hung_handover_released(c: &mut Cell) -> Result<(), String> {
    c.harness.hang_handovers(true);
    let t = token("hung");
    c.send(&t)?;
    let deadline = Instant::now() + WAKE_WITHIN;
    while c.harness.handovers_hung() == 0 {
        if Instant::now() >= deadline {
            return Err("no handover hung, so the scenario didn't run".into());
        }
        thread::sleep(Duration::from_millis(100));
    }
    let early = c.harness.busy_boundary();
    if contains(&early, &t) {
        return Err(format!(
            "a pull showed it while its push was still being handed over; got {early:?}"
        ));
    }
    thread::sleep(Duration::from_secs(ACK_LEASE_SECS + 1));
    let late = c.harness.busy_boundary();
    if !contains(&late, &t) {
        return Err(format!(
            "not shown once the hung handover's hold lapsed (message lost); got {late:?}"
        ));
    }
    c.harness.hang_handovers(false);
    // The late handover goes through now, and may show it a second time.
    let _late_handover = c.harness.busy_boundary();
    thread::sleep(Duration::from_secs(ACK_LEASE_SECS + 1));
    if contains(&c.harness.busy_boundary(), &t) {
        return Err("a later pull showed it again after it was acked".into());
    }
    Ok(())
}

/// The nickname the roster shows for `id`, once it has a row.
fn nickname_of(c: &mut Cell, id: &str) -> Option<String> {
    c.sender
        .row(id, Duration::from_secs(15))
        .and_then(|r| r["nickname"].as_str().map(str::to_string))
}

/// Scenario 11 (MUS-3): A session restarted in the same working directory
/// keeps its nickname. The old session ends as its TUI ends it, and a new one
/// with a new id starts in the same place. Once the daemon counts the old one
/// as dead, the new one holds its nickname, and mail to the old nickname or
/// the old id reaches it. If the new one first got a fresh nickname (it
/// registered before the old one counted as dead), mail to that still
/// reaches it after the swap, and its next hook tells it its new name.
fn restart_keeps_identity(c: &mut Cell) -> Result<(), String> {
    let old_id = c.recipient();
    let old_nick = nickname_of(c, &old_id).ok_or("the first session never got a nickname")?;
    c.harness.restart(&c.daemon, &c.proxy);
    let new_id = c.recipient();
    if new_id == old_id {
        return Err("the restart kept the session id, so the scenario didn't run".into());
    }
    let first_nick = nickname_of(c, &new_id).ok_or("the new session never reached the roster")?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if nickname_of(c, &new_id).as_deref() == Some(old_nick.as_str()) {
            break;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the new session never took over {old_nick}; it's {:?}",
                nickname_of(c, &new_id)
            ));
        }
        thread::sleep(Duration::from_millis(500));
    }
    let late = first_nick != old_nick;
    if late {
        let told = c.harness.busy_boundary().unwrap_or_default();
        if !told.contains(&format!("nickname is now {old_nick}")) {
            return Err(format!(
                "took over {old_nick} after starting as {first_nick}, but its next hook didn't say so; got {told:?}"
            ));
        }
    }
    let mut targets = vec![
        ("the old nickname", old_nick.clone()),
        ("the old id", old_id),
    ];
    if late {
        targets.push(("the nickname it gave up", first_nick));
    }
    for (what, to) in targets {
        let t = token("successor");
        c.sender.send(&to, &t, None)?;
        let seen = c.harness.busy_boundary();
        if !contains(&seen, &t) {
            return Err(format!(
                "mail to {what} ({to}) didn't reach the new session; got {seen:?}"
            ));
        }
    }
    Ok(())
}

/// Scenario 13 (MUS-47): Mail the old session never saw is shown once by the
/// session that takes over its nickname, and never again. It's sent before the
/// restart, while no hook runs, so it's still unread when the old session ends.
fn unread_mail_follows_a_restart(c: &mut Cell) -> Result<(), String> {
    let old_id = c.recipient();
    let old_nick = nickname_of(c, &old_id).ok_or("the first session never got a nickname")?;
    let t = token("before-restart");
    c.send(&t)?;
    c.harness.restart(&c.daemon, &c.proxy);
    let new_id = c.recipient();
    let deadline = Instant::now() + Duration::from_secs(20);
    while nickname_of(c, &new_id).as_deref() != Some(old_nick.as_str()) {
        if Instant::now() >= deadline {
            return Err(format!("the new session never took over {old_nick}"));
        }
        thread::sleep(Duration::from_millis(500));
    }
    let seen = c.harness.busy_boundary().unwrap_or_default();
    let n = seen.matches(t.as_str()).count();
    if n != 1 {
        return Err(format!(
            "the old session's unread mail was shown {n} times after the restart, not once; got {seen:?}"
        ));
    }
    thread::sleep(Duration::from_secs(ACK_LEASE_SECS + 1));
    if contains(&c.harness.busy_boundary(), &t) {
        return Err("shown again at the next hook".into());
    }
    Ok(())
}

/// Scenario 12 (MUS-3): A second session in the same working directory,
/// while the first is live, gets a nickname of its own, and the first keeps
/// its nickname past the quiet time. For Codex the first is an idle thread
/// with no client, which the daemon can't tell from a dead one until MUS-77,
/// so it must never be taken over.
fn live_sibling_keeps_its_name(c: &mut Cell) -> Result<(), String> {
    c.harness.arm_idle();
    let first = c.recipient();
    let first_nick = nickname_of(c, &first).ok_or("the first session never got a nickname")?;
    // Idle past the quiet time first, so a rule that counted a quiet idle
    // session as dead would hand its nickname to the sibling.
    thread::sleep(Duration::from_secs(4));
    let second = c.harness.sibling(&c.daemon, &c.proxy);
    let second_nick = nickname_of(c, &second).ok_or("the sibling never reached the roster")?;
    if second_nick == first_nick {
        return Err(format!("the sibling took {first_nick} from a live session"));
    }
    // Past the grace, the quiet time and a few sweeps.
    thread::sleep(Duration::from_secs(8));
    let now_first = nickname_of(c, &first);
    if now_first.as_deref() != Some(first_nick.as_str()) {
        return Err(format!(
            "the live session lost {first_nick}; it's now {now_first:?}"
        ));
    }
    if nickname_of(c, &second).as_deref() == Some(first_nick.as_str()) {
        return Err(format!("the sibling took {first_nick} over later"));
    }
    Ok(())
}

const HARNESSES: &[Harness] = &[
    Harness {
        name: "claude",
        mode: Mode::Plain,
        make: || Box::new(FakeClaude::new()),
        not_applicable: HOOK_HARNESS_NOT_APPLICABLE,
        needs_bun: false,
        excluded: None,
    },
    Harness {
        name: "claude-tls",
        mode: Mode::Tls,
        make: || Box::new(FakeClaude::new()),
        not_applicable: HOOK_HARNESS_NOT_APPLICABLE,
        needs_bun: false,
        excluded: Some(TLS_EXCLUDED),
    },
    Harness {
        name: "codex",
        mode: Mode::Plain,
        make: || Box::new(FakeCodex::new()),
        not_applicable: CODEX_NOT_APPLICABLE,
        needs_bun: false,
        excluded: None,
    },
    Harness {
        name: "codex-tls",
        mode: Mode::Tls,
        make: || Box::new(FakeCodex::new()),
        not_applicable: HOOK_HARNESS_NOT_APPLICABLE,
        needs_bun: false,
        excluded: Some(TLS_EXCLUDED),
    },
    Harness {
        name: "pi",
        mode: Mode::Plain,
        make: || Box::new(FakePlugin::new("marshal-pi")),
        not_applicable: PUSH_PLUGIN_NOT_APPLICABLE,
        needs_bun: true,
        excluded: Some(PLUGIN_EXCLUDED),
    },
    Harness {
        name: "opencode",
        mode: Mode::Plain,
        make: || Box::new(FakePlugin::new("marshal-opencode")),
        not_applicable: OPENCODE_NOT_APPLICABLE,
        needs_bun: true,
        excluded: Some(PLUGIN_EXCLUDED),
    },
    Harness {
        name: "prime-agent",
        mode: Mode::Plain,
        make: || Box::new(FakePlugin::new("marshal-prime-agent")),
        not_applicable: PUSH_PLUGIN_NOT_APPLICABLE,
        needs_bun: true,
        excluded: Some(PLUGIN_EXCLUDED),
    },
];

/// Scenarios that don't apply to a harness delivered by hooks, with no live
/// push to the session.
/// Codex's: a hook harness's, and its app-server bridge is a wake path from the start (MUS-77).
const CODEX_NOT_APPLICABLE: &[(&str, &str)] = &[
    HOOK_HARNESS_NOT_APPLICABLE[0],
    HOOK_HARNESS_NOT_APPLICABLE[1],
    (
        "wake-none-not-expected",
        "its app-server bridge is a wake path from the start, so the roster never says none \
         (MUS-77); armed-shows-its-wake-path covers it",
    ),
];

const HOOK_HARNESS_NOT_APPLICABLE: &[(&str, &str)] = &[
    (
        "mail-waiting-at-reconnect",
        "no live push: mail waits for the next hook, which pulls it",
    ),
    (
        "hung-handover-released",
        "no live push to hand over: the hook prints the inbox and then acks",
    ),
];

/// Scenarios that don't apply to a plugin that takes the live push.
const PUSH_PLUGIN_NOT_APPLICABLE: &[(&str, &str)] = &[
    (
        "restart-keeps-identity",
        "the plugins don't report a harness yet: MUS-3 covers Claude and Codex first",
    ),
    (
        "live-sibling-keeps-its-name",
        "the plugins don't report a harness yet: MUS-3 covers Claude and Codex first",
    ),
    (
        "unread-mail-follows-a-restart",
        "the plugins don't report a harness yet: MUS-3 covers Claude and Codex first",
    ),
    (
        "lost-hook-reply-redelivers",
        "no prompt hooks: delivery is the live push and a per-turn inbox drain",
    ),
    (
        "wake-none-not-expected",
        "a connected plugin renders pushes, so the roster says channel, never none",
    ),
    (
        "restart-keeps-unread-mail",
        "a busy plugin takes the push into its running turn, so nothing is unread at the restart",
    ),
    (
        "restart-keeps-unread-mail-file",
        "a busy plugin takes the push into its running turn, so nothing is unread at the restart",
    ),
];

/// OpenCode's, which differ from Pi's only in the reason given for why the roster can't say
/// none (MUS-89, fixed for Marshal by MAR-13).
/// Every push plugin's entry but `wake-none-not-expected` is taken by index, so
/// `opencode_skips_what_every_push_plugin_skips` checks the two lists agree.
const OPENCODE_NOT_APPLICABLE: &[(&str, &str)] = &[
    PUSH_PLUGIN_NOT_APPLICABLE[0],
    PUSH_PLUGIN_NOT_APPLICABLE[1],
    PUSH_PLUGIN_NOT_APPLICABLE[2],
    PUSH_PLUGIN_NOT_APPLICABLE[3],
    PUSH_PLUGIN_NOT_APPLICABLE[5],
    PUSH_PLUGIN_NOT_APPLICABLE[6],
    (
        "wake-none-not-expected",
        "it wakes by the live push, and its plugin declares that channel when it registers \
         (MAR-13), so the roster says channel, never none",
    ),
];

enum Outcome {
    Pass,
    Fail(String),
    KnownFailure(&'static str),
    /// A cell in `FLAKY`, with its story and whether it passed this time.
    Flaky(&'static str, bool),
    UnexpectedPass(&'static str),
    NotApplicable,
    NoBun,
    NoPostgres,
}

fn run_cell(harness: &Harness, scenario: Scenario, name: &str) -> Outcome {
    if harness.excluded.is_some()
        || MARSHAL_NOT_APPLICABLE.iter().any(|(s, _)| *s == name)
        || harness.not_applicable.iter().any(|(s, _)| *s == name)
    {
        return Outcome::NotApplicable;
    }
    if harness.needs_bun && bun_bin().is_none() {
        return Outcome::NoBun;
    }
    let postgres = std::env::var("CONFORMANCE_POSTGRES_URL")
        .ok()
        .filter(|url| !url.is_empty());
    let postgres = if POSTGRES_SCENARIOS.contains(&name) {
        match postgres {
            Some(url) => Some(url),
            None => return Outcome::NoPostgres,
        }
    } else {
        None
    };
    let flaky = FLAKY
        .iter()
        .find(|(h, s, _, _)| *h == harness.name && *s == name)
        .map(|(_, _, story, _)| *story);
    let known = KNOWN_FAILURES
        .iter()
        .find(|(h, s, _)| *h == harness.name && *s == name)
        .map(|(_, _, story)| *story);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut cell = Cell::new(
            harness.mode,
            (harness.make)(),
            postgres.as_deref(),
            FILE_SCENARIOS.contains(&name),
        );
        let result = scenario(&mut cell);
        // CONFORMANCE_DAEMON_LOG=1 prints it for passing cells too.
        if std::env::var_os("CONFORMANCE_DAEMON_LOG").is_some() && result.is_ok() {
            eprintln!(
                "--- {} / {name} passed\n--- daemon log:\n{}--- end of daemon log",
                harness.name,
                daemon_log_excerpt(&cell.daemon.log())
            );
        }
        if let Err(e) = &result {
            // CI keeps no daemon logs, so a failure (FLAKY ones too) prints
            // the daemon's identity and hook lines, where a missed takeover
            // or a lost delivery shows its cause (MUS-93).
            eprintln!(
                "--- {} / {name} failed: {e}\n--- daemon log ([identity], [hook], warnings):\n{}--- end of daemon log",
                harness.name,
                daemon_log_excerpt(&cell.daemon.log())
            );
        }
        result
    }))
    .unwrap_or_else(|panic| {
        Err(panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "panicked".into()))
    });
    if let Some(story) = flaky {
        return Outcome::Flaky(story, result.is_ok());
    }
    match (result, known) {
        (Ok(()), None) => Outcome::Pass,
        (Ok(()), Some(story)) => Outcome::UnexpectedPass(story),
        (Err(_), Some(story)) => Outcome::KnownFailure(story),
        (Err(e), None) => Outcome::Fail(e),
    }
}

/// OpenCode's n/a list is the push plugins' with one reason changed (MUS-89); a
/// scenario added to theirs must reach it too.
#[test]
fn opencode_skips_what_every_push_plugin_skips() {
    let names = |list: &[(&'static str, &'static str)]| {
        let mut names: Vec<&str> = list.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        names
    };
    assert_eq!(
        names(OPENCODE_NOT_APPLICABLE),
        names(PUSH_PLUGIN_NOT_APPLICABLE)
    );
}

/// The daemon log lines that explain a failed cell: identity decisions, hook
/// traffic and anything at warn or above, at most the last 200 of them.
fn daemon_log_excerpt(log: &str) -> String {
    let lines: Vec<&str> = log
        .lines()
        .filter(|l| {
            l.contains("[identity]")
                || l.contains("[hook]")
                || l.contains(" WARN ")
                || l.contains(" ERROR ")
        })
        .collect();
    let from = lines.len().saturating_sub(200);
    lines[from..].iter().map(|l| format!("{l}\n")).collect()
}

/// A `FLAKY` entry must name a real cell, or it quietly covers nothing.
#[test]
fn every_flaky_cell_is_in_the_grid() {
    for (harness, scenario, story, _) in FLAKY {
        assert!(
            HARNESSES.iter().any(|h| h.name == *harness),
            "{harness} ({story})"
        );
        assert!(
            SCENARIOS.iter().any(|(name, _)| name == scenario),
            "{scenario} ({story})"
        );
    }
}

#[test]
#[ignore = "needs `cargo build --workspace` first; run with --ignored"]
fn delivery_conformance() {
    let only = std::env::var("CONFORMANCE_ONLY").ok();
    let mut grid: BTreeMap<(&str, &str), String> = BTreeMap::new();
    let mut failures = Vec::new();
    for harness in HARNESSES {
        for (name, scenario) in SCENARIOS {
            if only
                .as_deref()
                .is_some_and(|o| !name.contains(o) && !harness.name.contains(o))
            {
                continue;
            }
            let started = Instant::now();
            let outcome = run_cell(harness, *scenario, name);
            let cell = match &outcome {
                Outcome::Pass => "pass".to_string(),
                Outcome::KnownFailure(story) => format!("known ({story})"),
                Outcome::Flaky(story, passed) => {
                    format!("flaky ({story}, {})", if *passed { "pass" } else { "fail" })
                }
                Outcome::Fail(_) => "FAIL".to_string(),
                Outcome::UnexpectedPass(story) => format!("PASS? ({story})"),
                Outcome::NotApplicable => "n/a".to_string(),
                Outcome::NoBun => "no bun".to_string(),
                Outcome::NoPostgres => "no postgres".to_string(),
            };
            eprintln!(
                "{:<12} {:<30} {:<14} {:.1}s",
                harness.name,
                name,
                cell,
                started.elapsed().as_secs_f32()
            );
            if let Outcome::Fail(e) = &outcome {
                eprintln!("    why: {e}");
            }
            match outcome {
                Outcome::Fail(e) => failures.push(format!("{} / {name}: {e}", harness.name)),
                Outcome::UnexpectedPass(story) => failures.push(format!(
                    "{} / {name} passes, but KNOWN_FAILURES lists it for {story}: remove it",
                    harness.name
                )),
                // CI sets CONFORMANCE_REQUIRE_BUN, so a missing bun there is a
                // failure, not a quiet gap.
                Outcome::NoPostgres
                    if std::env::var_os("CONFORMANCE_REQUIRE_POSTGRES").is_some() =>
                {
                    failures.push(format!(
                        "{} / {name}: no postgres (set CONFORMANCE_POSTGRES_URL)",
                        harness.name
                    ));
                }
                Outcome::NoBun if std::env::var_os("CONFORMANCE_REQUIRE_BUN").is_some() => {
                    failures.push(format!(
                        "{} / {name}: no bun (set MARSHAL_BUN or put bun on PATH)",
                        harness.name
                    ));
                }
                _ => {}
            }
            grid.insert((harness.name, name), cell);
        }
    }

    // The grid: one row per scenario, one column per harness.
    let mut columns: Vec<&str> = HARNESSES.iter().map(|h| h.name).collect();
    columns.extend(NOT_YET.iter().map(|(h, _)| *h));
    let mut table = format!("\n{:<30}", "scenario");
    for col in &columns {
        table.push_str(&format!(" | {col:<14}"));
    }
    table.push('\n');
    for (name, _) in SCENARIOS {
        table.push_str(&format!("{name:<30}"));
        for col in &columns {
            let cell = grid.get(&(*col, *name)).cloned().unwrap_or_else(|| {
                NOT_YET
                    .iter()
                    .find(|(h, _)| h == col)
                    .map_or("skipped".into(), |_| "not yet".into())
            });
            table.push_str(&format!(" | {cell:<14}"));
        }
        table.push('\n');
    }
    for (harness, stage) in NOT_YET {
        table.push_str(&format!("not yet: {harness} ({stage})\n"));
    }
    // Every n/a cell says why, so a cell left out never reads as one that ran.
    for harness in HARNESSES {
        if let Some(why) = harness.excluded {
            table.push_str(&format!("n/a: {} × every scenario: {why}\n", harness.name));
            continue;
        }
        for (scenario, why) in MARSHAL_NOT_APPLICABLE {
            table.push_str(&format!("n/a: {} × {scenario}: {why}\n", harness.name));
        }
        for (scenario, why) in harness.not_applicable {
            if !MARSHAL_NOT_APPLICABLE.iter().any(|(s, _)| s == scenario) {
                table.push_str(&format!("n/a: {} × {scenario}: {why}\n", harness.name));
            }
        }
    }
    eprintln!("{table}");
    if let Ok(path) = std::env::var("CONFORMANCE_GRID") {
        let _ = std::fs::write(path, &table);
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

/// The members of `room_id` as `marshal://rooms` shows them to `mcp`.
fn room_members(mcp: &mut Mcp, room_id: &str) -> Vec<String> {
    let Ok(r) = mcp.rpc("resources/read", json!({"uri": "marshal://rooms"})) else {
        return Vec::new();
    };
    let text = r["contents"][0]["text"].as_str().unwrap_or_default();
    let rooms: Value = serde_json::from_str(text).unwrap_or_default();
    rooms["rooms"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|room| room["room_id"] == json!(room_id))
        .flat_map(|room| room["members"].as_array().cloned().unwrap_or_default())
        .filter_map(|m| m["session_id"].as_str().map(str::to_string))
        .collect()
}

fn log_tail(log: &str) -> String {
    let lines: Vec<&str> = log.lines().collect();
    lines[lines.len().saturating_sub(80)..].join("\n")
}

/// MAR-19: an adhoc membership survives a restart on the same Postgres. The
/// sweeper drops memberships whose session isn't on the roster, so it must
/// start only once myko has replayed the store; started before that, a boot
/// sweep could find the membership before its session and drop it. That
/// window is too narrow to hit on purpose (myko fills its stores one entity
/// type at a time), so this pins the outcome; the order itself is pinned by
/// `cleanup::tests::the_sweeper_waits_for_the_store_to_load`.
#[test]
#[ignore = "needs `cargo build --workspace` and CONFORMANCE_POSTGRES_URL; run with --ignored"]
fn adhoc_membership_survives_a_postgres_restart() {
    let Some(url) = std::env::var("CONFORMANCE_POSTGRES_URL")
        .ok()
        .filter(|url| !url.is_empty())
    else {
        assert!(
            std::env::var_os("CONFORMANCE_REQUIRE_POSTGRES").is_none(),
            "no postgres (set CONFORMANCE_POSTGRES_URL)"
        );
        eprintln!("adhoc_membership_survives_a_postgres_restart: no postgres, skipped");
        return;
    };
    const ROOM: &str = "mar19-adhoc";
    let mut daemon = Daemon::start(Mode::Plain, Some(&url), false);
    let member_id = uuid::Uuid::new_v4().to_string();
    let mut member = Cell::sender(&daemon, &member_id);
    // Its shim's writes are refused until its session is on the roster.
    let joined = wait_until(Duration::from_secs(15), || {
        member.tool("join_room", json!({"name": ROOM})).is_ok()
    });
    assert!(joined, "join_room never succeeded");
    assert!(
        wait_until(Duration::from_secs(10), || room_members(&mut member, ROOM)
            .contains(&member_id)),
        "the join never showed in marshal://rooms"
    );

    daemon.restart();
    let loaded = "the store is loaded; starting the cleanup sweeper";
    // The member's shim redials. Wait for the sweeper, then past a few of its
    // ticks (3 s each): any boot sweep that was going to drop it has run.
    assert!(
        wait_until(Duration::from_secs(20), || daemon
            .log()
            .matches(loaded)
            .count()
            >= 2),
        "the restarted daemon never started its sweeper; log:\n{}",
        log_tail(&daemon.log())
    );
    thread::sleep(Duration::from_secs(10));
    let observer_id = uuid::Uuid::new_v4().to_string();
    let mut observer = Cell::sender(&daemon, &observer_id);
    let mut seen = Vec::new();
    let kept = wait_until(Duration::from_secs(10), || {
        seen = room_members(&mut observer, ROOM);
        seen.contains(&member_id)
    });
    assert!(
        kept,
        "the adhoc membership didn't survive the restart; {ROOM} has {seen:?}; log:\n{}",
        log_tail(&daemon.log())
    );
}

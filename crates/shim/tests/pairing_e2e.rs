//! Per-machine pairing (MAR-24), end to end against the real binaries: a
//! `--tls` daemon refuses a shim with the pin but no token; `marshal-daemon
//! pair-code` and `marshal-shim pair` let it in; `marshal-daemon unpair`
//! throws it out within a second; the daemon host's own shim gets in with the
//! local token and never pairs; and a daemon with machines paired refuses to
//! start untold, and runs open with `--plain`.

mod support;

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use support::SHIM;

/// The daemon binary beside the shim, if this test run built it. `cargo test
/// -p marshal-shim` alone (the Windows job) doesn't, and then these tests
/// skip; the workspace run does, and they run there.
fn daemon_bin() -> Option<PathBuf> {
    let bin =
        Path::new(SHIM).with_file_name(format!("marshal-daemon{}", std::env::consts::EXE_SUFFIX));
    if bin.exists() {
        Some(bin)
    } else {
        eprintln!(
            "skipped: {} isn't built (run the workspace tests)",
            bin.display()
        );
        None
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
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
        std::thread::sleep(Duration::from_millis(100));
    }
    ok()
}

/// Drive the MCP shim over stdio as a Codex harness would, and call
/// `set_status` for `session` with `text`. The tool answers ok whatever myko
/// then decides, so the effect is read back from the roster.
fn mcp_set_status(mut shim: Command, session: &str, text: &str) {
    use std::io::{BufRead as _, BufReader, Write as _};
    let mut child = shim
        .env("MARSHAL_HARNESS", "codex")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let messages = [
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                       "clientInfo": {"name": "pairing-e2e", "version": "0"}}}),
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "set_status",
                       "arguments": {"text": text, "asSession": session}}}),
    ];
    for m in &messages {
        writeln!(stdin, "{m}").unwrap();
    }
    let answered = std::sync::mpsc::channel::<bool>();
    std::thread::spawn(move || {
        for line in stdout.lines() {
            let Ok(line) = line else { break };
            if line.contains("\"id\":2") {
                let _ = answered.0.send(true);
                return;
            }
        }
        let _ = answered.0.send(false);
    });
    let got = answered
        .1
        .recv_timeout(Duration::from_secs(20))
        .unwrap_or(false);
    // The command is queued on the shim's connection; give it a moment to go.
    std::thread::sleep(Duration::from_secs(2));
    let _ = child.kill();
    let _ = child.wait();
    assert!(got, "the MCP shim never answered set_status");
}

/// `declared.machine` of `session` on the roster.
fn machine_of(rig: &Rig, session: &str) -> Option<String> {
    let out = roster_of(rig);
    let roster: serde_json::Value =
        serde_json::from_str(text(&out).trim_start_matches(|c| c != '{')).ok()?;
    fn find(v: &serde_json::Value, id: &str) -> Option<String> {
        match v {
            serde_json::Value::Object(map) => {
                if map.get("session_id").and_then(|v| v.as_str()) == Some(id) {
                    return map
                        .get("declared")?
                        .get("machine")?
                        .as_str()
                        .map(str::to_string);
                }
                map.values().find_map(|v| find(v, id))
            }
            serde_json::Value::Array(items) => items.iter().find_map(|v| find(v, id)),
            _ => None,
        }
    }
    find(&roster, session)
}

fn roster_of(rig: &Rig) -> Output {
    rig.local_shim(&["roster"]).output().unwrap()
}

/// `declared.current_task` of `session` on the roster, through the host's shim.
fn current_task_of(rig: &Rig, session: &str) -> Option<String> {
    let out = roster_of(rig);
    let roster: serde_json::Value =
        serde_json::from_str(text(&out).trim_start_matches(|c| c != '{')).ok()?;
    fn find(v: &serde_json::Value, id: &str) -> Option<String> {
        match v {
            serde_json::Value::Object(map) => {
                if map.get("session_id").and_then(|v| v.as_str()) == Some(id) {
                    return map
                        .get("declared")?
                        .get("current_task")?
                        .as_str()
                        .map(str::to_string);
                }
                map.values().find_map(|v| find(v, id))
            }
            serde_json::Value::Array(items) => items.iter().find_map(|v| find(v, id)),
            _ => None,
        }
    }
    find(&roster, session)
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// One temp directory holding the daemon's home (its config directory, with
/// the local token), its state directory and a log, and a running daemon.
struct Rig {
    dir: tempfile::TempDir,
    ws: u16,
    hook: u16,
    child: Option<Child>,
    fingerprint: String,
    /// Extra environment for the daemon (MAR-88's allowances).
    env: Vec<(String, String)>,
}

impl Rig {
    fn home(&self) -> PathBuf {
        self.dir.path().join("daemon-home")
    }

    fn state(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    /// A `marshal-daemon` command with this rig's directories.
    fn daemon_cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(daemon_bin().expect("checked by the test before starting"));
        c.args(args)
            .env_remove("MYKO_POSTGRES_URL")
            .env_remove("MARSHAL_PERSIST")
            .env_remove("MARSHAL_ADVERTISE")
            .env("HOME", self.home())
            .env("XDG_CONFIG_HOME", self.home().join("config"))
            .env("MARSHAL_STATE_DIR", self.state())
            .env("RUST_LOG", "info");
        c.envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        c
    }

    /// Start `marshal-daemon <flags>` on free ports. `--tls` daemons are
    /// waited for until myko answers; others until the WebSocket port does.
    fn start(flags: &[&str]) -> Self {
        Self::start_with(flags, &[])
    }

    fn start_with(flags: &[&str], env: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut rig = Self {
            dir,
            ws: 0,
            hook: 0,
            child: None,
            fingerprint: String::new(),
            env: env
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        };
        assert!(
            rig.restart(flags),
            "marshal-daemon {flags:?} didn't come up"
        );
        rig
    }

    /// Start (again) with `flags`. False if it exited instead of listening.
    fn restart(&mut self, flags: &[&str]) -> bool {
        self.stop();
        for _ in 0..5 {
            let (ws, hook, myko) = (free_port(), free_port(), free_port());
            let log = std::fs::File::create(self.log_path()).unwrap();
            let mut child = self
                .daemon_cmd(flags)
                .env("MARSHAL_BIND", format!("127.0.0.1:{ws}"))
                .env("MARSHAL_HOOK_BIND", format!("127.0.0.1:{hook}"))
                .env("MARSHAL_MYKO_BIND", format!("127.0.0.1:{myko}"))
                .stdin(Stdio::null())
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap();
            let tls = flags.contains(&"--tls");
            let probe = if tls { myko } else { ws };
            let exited = |c: &mut Child| c.try_wait().ok().flatten().is_some();
            let up = wait_until(Duration::from_secs(20), || {
                exited(&mut child) || TcpStream::connect(("127.0.0.1", probe)).is_ok()
            });
            if exited(&mut child) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(300));
            let log = self.log();
            if up && !exited(&mut child) && !log.contains("in use") {
                self.ws = ws;
                self.hook = hook;
                self.child = Some(child);
                if tls {
                    let fp = self.state().join("tls/fingerprint");
                    assert!(wait_until(Duration::from_secs(5), || fp.exists()));
                    self.fingerprint = std::fs::read_to_string(fp).unwrap().trim().to_string();
                }
                return true;
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        panic!("marshal-daemon lost its ports 5 times");
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn log_path(&self) -> PathBuf {
        self.dir.path().join("daemon.log")
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.log_path()).unwrap_or_default()
    }

    /// A shim in its own home (another machine), pointed at this daemon over
    /// TLS with the pin, and with `token` if given. Its pairing files, when
    /// `pair` writes them, win over these variables.
    fn remote_shim(&self, token: Option<&str>, args: &[&str]) -> Command {
        let home = self.dir.path().join("remote-home");
        let mut c = support::shim(&home);
        c.args(args)
            .env(
                "MARSHAL_DAEMON_ADDRESS",
                format!("wss://127.0.0.1:{}", self.ws),
            )
            .env("MARSHAL_TLS_PIN", &self.fingerprint)
            .env("MARSHAL_HOST", "remote");
        match token {
            Some(token) => {
                c.env("MARSHAL_TOKEN", token);
            }
            None => {
                c.env_remove("MARSHAL_TOKEN");
            }
        }
        c
    }

    /// A shim on the daemon's own host: the same home, so the same config
    /// directory and local token.
    fn local_shim(&self, args: &[&str]) -> Command {
        let mut c = support::shim(&self.home());
        c.args(args)
            .env(
                "MARSHAL_DAEMON_ADDRESS",
                format!("wss://127.0.0.1:{}", self.ws),
            )
            .env(
                "MARSHAL_BASE_URL",
                format!("https://127.0.0.1:{}", self.hook),
            )
            .env("MARSHAL_TLS_PIN", &self.fingerprint)
            .env_remove("MARSHAL_TOKEN");
        c
    }

    fn pair_code(&self, name: &str) -> String {
        let out = self
            .daemon_cmd(&[
                "pair-code",
                name,
                "--address",
                &format!("127.0.0.1:{}", self.ws),
                "--hooks",
                &format!("127.0.0.1:{}", self.hook),
            ])
            .output()
            .unwrap();
        assert!(out.status.success(), "pair-code: {}", text(&out));
        let code = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert!(code.starts_with("marshal-invite-v1:"), "{code}");
        code
    }

    fn hook(&self, mut cmd: Command, event: &str, session_id: &str) -> Output {
        use std::io::Write as _;
        let mut child = cmd
            .arg("claude-hook")
            .arg(event)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input = serde_json::json!({
            "session_id": session_id,
            "cwd": self.dir.path().join("work").display().to_string(),
        });
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop();
    }
}

#[test]
fn pairing_lets_a_machine_in_and_unpairing_throws_it_out() {
    let Some(_) = daemon_bin() else {
        return;
    };
    let rig = Rig::start(&["--tls"]);
    std::fs::create_dir_all(rig.dir.path().join("work")).unwrap();

    // The pin alone isn't enough: myko refuses a connection with no token,
    // and so does the hook listener (MAR-81), saying what it wants.
    let out = rig.remote_shim(None, &["roster"]).output().unwrap();
    assert!(
        !out.status.success(),
        "no token read the roster: {}",
        text(&out)
    );
    let mut no_token = rig.remote_shim(None, &[]);
    no_token.env(
        "MARSHAL_BASE_URL",
        format!("https://127.0.0.1:{}", rig.hook),
    );
    let out = rig.hook(no_token, "session-start", "unpaired-session");
    assert!(
        text(&out).contains("wants a token it knows"),
        "a tokenless hook wasn't told why: {}",
        text(&out)
    );
    let out = rig
        .remote_shim(Some("not-a-token"), &["roster"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "a made-up token read the roster: {}",
        text(&out)
    );

    // Pair: a code from the daemon's host, redeemed by the machine, once.
    let code = rig.pair_code("laptop");
    let out = rig.remote_shim(None, &["pair", &code]).output().unwrap();
    assert!(
        out.status.success(),
        "pair: {}\ndaemon log:\n{}",
        text(&out),
        rig.log()
    );
    assert!(text(&out).contains("paired as laptop"), "{}", text(&out));
    let again = rig.remote_shim(None, &["pair", &code]).output().unwrap();
    assert!(
        !again.status.success(),
        "the code worked twice: {}",
        text(&again)
    );
    assert!(text(&again).contains("already used"), "{}", text(&again));
    let listed = rig.daemon_cmd(&["paired"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&listed.stdout).trim(), "laptop");

    // Paired, the machine's shims get in with what `pair` wrote: no
    // MARSHAL_TOKEN here, the pairing file carries the token and the pin.
    let out = rig.remote_shim(None, &["roster"]).output().unwrap();
    assert!(out.status.success(), "paired roster: {}", text(&out));
    let out = rig.remote_shim(None, &["pairing"]).output().unwrap();
    assert!(text(&out).contains("paired as laptop"), "{}", text(&out));
    let joined = rig.hook(
        rig.remote_shim(None, &[]),
        "session-start",
        "paired-session",
    );
    assert!(joined.status.success(), "session-start: {}", text(&joined));
    let mut seen = String::new();
    assert!(
        wait_until(Duration::from_secs(10), || {
            let out = rig.remote_shim(None, &["roster"]).output().unwrap();
            seen = text(&out);
            out.status.success() && seen.contains("paired-session")
        }),
        "the paired machine's session never showed: {seen}"
    );

    // The session belongs to the machine whose token registered it (MAR-81):
    // the daemon host's own shim, with the local token, can't run its hooks,
    // and the tokenless hook above registered nothing.
    let poser = rig.hook(rig.local_shim(&[]), "prompt-submit", "paired-session");
    assert!(
        text(&poser).contains("belongs to another machine"),
        "the local shim ran a hook for the paired machine's session: {}",
        text(&poser)
    );
    assert!(
        !text(&rig.local_shim(&["roster"]).output().unwrap()).contains("unpaired-session"),
        "the tokenless hook's session joined"
    );
    // Nor can the host end it, or re-register it over the owner's row: the
    // row keeps the owner's declared host after both refusals.
    let ended = rig.hook(rig.local_shim(&[]), "session-end", "paired-session");
    assert!(
        text(&ended).contains("belongs to another machine"),
        "the host ended the paired machine's session: {}",
        text(&ended)
    );
    let overwrite = rig.hook(rig.local_shim(&[]), "session-start", "paired-session");
    assert!(
        text(&overwrite).contains("belongs to another machine"),
        "the host re-registered the paired machine's session: {}",
        text(&overwrite)
    );
    assert_eq!(
        machine_of(&rig, "paired-session").as_deref(),
        Some("laptop"),
        "the owner's row didn't survive the refused hooks"
    );

    // The machine declared MARSHAL_HOST=remote, but its sessions are recorded
    // under its pairing's name (MAR-85), on the hook path here and on the
    // RegisterSession path below (the terminal's "a terminal on laptop").
    assert_eq!(
        machine_of(&rig, "paired-session").as_deref(),
        Some("laptop"),
        "the paired machine's declared host wasn't replaced by its pairing's name"
    );

    // Acting *as* the hook-registered session (MAR-80 through MAR-81): the
    // paired machine's own MCP shim may set its status; the daemon host's,
    // with the local token, may not, since the session is the machine's.
    mcp_set_status(
        rig.remote_shim(None, &[]),
        "paired-session",
        "set by the laptop",
    );
    let mut task = None;
    assert!(
        wait_until(Duration::from_secs(10), || {
            task = current_task_of(&rig, "paired-session");
            task.as_deref() == Some("set by the laptop")
        }),
        "the paired machine couldn't act as its own session (current_task {task:?})"
    );
    mcp_set_status(rig.local_shim(&[]), "paired-session", "set by the host");
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        current_task_of(&rig, "paired-session").as_deref(),
        Some("set by the laptop"),
        "the daemon host acted as the paired machine's session"
    );

    // The paired machine writes as itself: its terminal's tell reaches its
    // session, and the session's next hook shows it.
    let told = rig
        .remote_shim(None, &["tell", "paired-session", "hello from the laptop"])
        .output()
        .unwrap();
    assert!(
        told.status.success(),
        "the paired machine's tell: {}",
        text(&told)
    );
    let mut shown = String::new();
    assert!(
        wait_until(Duration::from_secs(10), || {
            let out = rig.hook(
                rig.remote_shim(None, &[]),
                "prompt-submit",
                "paired-session",
            );
            shown = text(&out);
            shown.contains("hello from the laptop")
        }),
        "the tell never showed in the paired session's hook: {shown}"
    );
    assert!(
        shown.contains("a terminal on laptop"),
        "the terminal's sender label didn't carry the pairing's name: {shown}"
    );

    // A pasted pairing code or token never reaches a reader (MAR-86). The
    // spent code and the machine's live token both go in; neither comes out.
    let pairing_code =
        std::fs::read_to_string(rig.dir.path().join("remote-home/config/marshal/pairing")).unwrap();
    let token = marshal_entities::pairing::Pairing::from_code(&pairing_code)
        .unwrap()
        .token;
    let leak = format!("my code was {code} and my token is {token}, keep them safe");
    let told = rig
        .remote_shim(None, &["tell", "paired-session", &leak])
        .output()
        .unwrap();
    assert!(told.status.success(), "the leaking tell: {}", text(&told));
    let mut shown = String::new();
    assert!(
        wait_until(Duration::from_secs(10), || {
            let out = rig.hook(
                rig.remote_shim(None, &[]),
                "prompt-submit",
                "paired-session",
            );
            shown = text(&out);
            shown.contains("keep them safe")
        }),
        "the leaking tell never showed: {shown}"
    );
    assert!(
        !shown.contains(&token) && !shown.contains(&code[20..60]),
        "a secret reached the inbox: {shown}"
    );
    assert!(
        shown.contains("[marshal pairing code redacted]")
            && shown.contains("[marshal token redacted]"),
        "the secrets weren't marked as redacted: {shown}"
    );

    // Unpaired, it's out within the token cache's second, and stays out.
    let out = rig.daemon_cmd(&["unpair", "laptop"]).output().unwrap();
    assert!(out.status.success(), "unpair: {}", text(&out));
    assert!(
        wait_until(Duration::from_secs(5), || {
            !rig.remote_shim(None, &["roster"])
                .output()
                .unwrap()
                .status
                .success()
        }),
        "the unpaired machine still read the roster"
    );
    let listed = rig.daemon_cmd(&["paired"]).output().unwrap();
    assert!(text(&listed).contains("no machines are paired"));
    let out = rig.daemon_cmd(&["unpair", "laptop"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
}

#[test]
fn the_daemon_hosts_own_shim_uses_the_local_token() {
    let Some(_) = daemon_bin() else {
        return;
    };
    let rig = Rig::start(&["--tls"]);
    let token_file = rig.home().join("config/marshal/local-token");
    assert!(token_file.exists(), "the daemon wrote no local token");
    let out = rig.local_shim(&["roster"]).output().unwrap();
    assert!(out.status.success(), "local roster: {}", text(&out));
    let out = rig.local_shim(&["pairing"]).output().unwrap();
    assert!(text(&out).contains("isn't paired"), "{}", text(&out));
}

#[test]
fn with_machines_paired_the_daemon_refuses_to_start_untold_and_runs_open_with_plain() {
    let Some(_) = daemon_bin() else {
        return;
    };
    let mut rig = Rig::start(&["--tls"]);
    let code = rig.pair_code("laptop");
    let out = rig.remote_shim(None, &["pair", &code]).output().unwrap();
    assert!(out.status.success(), "pair: {}", text(&out));
    rig.stop();

    assert!(!rig.restart(&[]), "a daemon with pairings started untold");
    assert!(
        rig.log().contains("machines are paired"),
        "no refusal in the log: {}",
        rig.log()
    );

    assert!(rig.restart(&["--plain"]), "--plain didn't start");
    assert!(rig.log().contains("running open"), "{}", rig.log());
    assert!(TcpStream::connect(("127.0.0.1", rig.ws)).is_ok());
}

/// MAR-88: with a machine allowance of `2/60`, a paired machine's third
/// message in a minute is refused with a clear reason. Each `tell` is its own
/// terminal session, so the machine's allowance is the one that bites across
/// them; the session allowance is covered by the unit tests. Nothing is
/// limited otherwise: the other tests here send freely with no allowance set.
#[test]
fn a_send_allowance_refuses_a_flood_with_a_reason() {
    let Some(_) = daemon_bin() else {
        return;
    };
    let rig = Rig::start_with(
        &["--tls"],
        &[
            ("MARSHAL_SEND_ALLOWANCE", "2/60"),
            ("MARSHAL_MACHINE_SEND_ALLOWANCE", "2/60"),
        ],
    );
    std::fs::create_dir_all(rig.dir.path().join("work")).unwrap();
    let code = rig.pair_code("laptop");
    let out = rig.remote_shim(None, &["pair", &code]).output().unwrap();
    assert!(out.status.success(), "pair: {}", text(&out));
    let joined = rig.hook(rig.remote_shim(None, &[]), "session-start", "flood-target");
    assert!(joined.status.success(), "session-start: {}", text(&joined));
    assert!(wait_until(Duration::from_secs(10), || {
        text(&roster_of(&rig)).contains("flood-target")
    }));
    for n in 1..=2 {
        let out = rig
            .remote_shim(None, &["tell", "flood-target", &format!("message {n}")])
            .output()
            .unwrap();
        assert!(out.status.success(), "tell {n}: {}", text(&out));
    }
    let third = rig
        .remote_shim(None, &["tell", "flood-target", "message 3"])
        .output()
        .unwrap();
    let said = text(&third);
    assert!(
        !third.status.success() || said.contains("not sent"),
        "the third message in a minute went through: {said}"
    );
    assert!(
        said.contains("too many messages from this machine"),
        "the refusal didn't say why: {said}"
    );
}

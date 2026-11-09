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
        c
    }

    /// Start `marshal-daemon <flags>` on free ports. `--tls` daemons are
    /// waited for until myko answers; others until the WebSocket port does.
    fn start(flags: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut rig = Self {
            dir,
            ws: 0,
            hook: 0,
            child: None,
            fingerprint: String::new(),
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

    // The pin alone isn't enough: myko refuses a connection with no token.
    let out = rig.remote_shim(None, &["roster"]).output().unwrap();
    assert!(
        !out.status.success(),
        "no token read the roster: {}",
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
    assert!(out.status.success(), "pair: {}", text(&out));
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

    // The paired machine writes as itself: its terminal's tell reaches its
    // session, and the session's next hook shows it. (Acting *as* the
    // hook-registered session from that machine's MCP shim is what MAR-80
    // refuses until MAR-81 binds hook sessions to the machine that sent them;
    // no CLI command does that, so it isn't driven here.)
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

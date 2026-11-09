//! The shim against a `marshal-daemon --tls` (MAR-23 part 2): with the
//! daemon's fingerprint pinned, a session joins through the hooks over TLS, a
//! terminal `tell` reaches it through myko over TLS, and the next hook shows
//! the mail; with a wrong pin, or none, the shim is refused in the handshake
//! and sends nothing.

mod support;

use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use support::SHIM;

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

/// The daemon binary beside the shim, if this test run built it. `cargo test
/// -p marshal-shim` alone (the Windows job) doesn't, and then these tests
/// skip; the workspace run does, and they run there.
fn daemon_bin() -> Option<std::path::PathBuf> {
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

/// A real `marshal-daemon --tls` in a temp state directory, on free loopback
/// ports. Its ports are picked by binding 0 and closing, so a lost one means
/// a restart on new ports (MAR-21).
struct Daemon {
    dir: tempfile::TempDir,
    ws: u16,
    hook: u16,
    child: Child,
    fingerprint: String,
    /// The daemon host's own token (MAR-24): with `--tls`, myko admits
    /// nothing without a token, and these tests are about TLS, not pairing.
    local_token: String,
}

impl Daemon {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bin = daemon_bin().expect("checked by the test before starting");
        let log_path = dir.path().join("daemon.log");
        let mut lost = Vec::new();
        loop {
            assert!(lost.len() < 5, "lost ports 5 times: {lost:?}");
            let (ws, hook, myko) = (free_port(), free_port(), free_port());
            let log = std::fs::File::create(&log_path).unwrap();
            let mut child = Command::new(&bin)
                .arg("--tls")
                .env_remove("MYKO_POSTGRES_URL")
                .env_remove("MARSHAL_PERSIST")
                .env("HOME", dir.path().join("home"))
                .env("MARSHAL_STATE_DIR", dir.path().join("state"))
                // The local token (MAR-24) goes to the config dir: this
                // home's, not the runner's XDG_CONFIG_HOME.
                .env_remove("XDG_CONFIG_HOME")
                .env("MARSHAL_BIND", format!("127.0.0.1:{ws}"))
                .env("MARSHAL_HOOK_BIND", format!("127.0.0.1:{hook}"))
                .env("MARSHAL_MYKO_BIND", format!("127.0.0.1:{myko}"))
                .env("RUST_LOG", "info")
                .stdin(Stdio::null())
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap();
            let exited = |c: &mut Child| c.try_wait().ok().flatten().is_some();
            // myko binds its own port last; the TLS front forwards once it answers.
            let up = wait_until(Duration::from_secs(20), || {
                exited(&mut child) || TcpStream::connect(("127.0.0.1", myko)).is_ok()
            });
            let fp_path = dir.path().join("state/tls/fingerprint");
            let token_path = dir.path().join("home/.config/marshal/local-token");
            if up && !exited(&mut child) && fp_path.exists() && token_path.exists() {
                std::thread::sleep(Duration::from_millis(300));
                let log = std::fs::read_to_string(&log_path).unwrap_or_default();
                if !exited(&mut child) && !log.contains("in use") {
                    let fingerprint = std::fs::read_to_string(&fp_path)
                        .unwrap()
                        .trim()
                        .to_string();
                    let local_token = std::fs::read_to_string(&token_path)
                        .unwrap()
                        .trim()
                        .to_string();
                    return Self {
                        dir,
                        ws,
                        hook,
                        child,
                        fingerprint,
                        local_token,
                    };
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            lost.push(format!("{ws} {hook} {myko}"));
        }
    }

    /// A shim pointed at this daemon over TLS with `pin`, or with none.
    fn shim(&self, pin: Option<&str>, args: &[&str]) -> Command {
        let home = self.dir.path().join("shim-home");
        let mut c = support::shim(&home);
        c.args(args)
            .env(
                "MARSHAL_DAEMON_ADDRESS",
                format!("wss://127.0.0.1:{}", self.ws),
            )
            .env(
                "MARSHAL_BASE_URL",
                format!("https://127.0.0.1:{}", self.hook),
            )
            .env("MARSHAL_TOKEN", &self.local_token)
            .env("MARSHAL_HOST", "tls-e2e");
        match pin {
            Some(pin) => {
                c.env("MARSHAL_TLS_PIN", pin);
            }
            None => {
                c.env_remove("MARSHAL_TLS_PIN");
            }
        }
        c
    }

    fn hook(&self, pin: Option<&str>, event: &str, session_id: &str) -> Output {
        use std::io::Write as _;
        let mut child = self
            .shim(pin, &["claude-hook", event])
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

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The `nickname` of the roster row whose `session_id` is `id`, wherever the
/// row sits in the JSON.
fn nickname_of(value: &serde_json::Value, id: &str) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            if map.get("session_id").and_then(|v| v.as_str()) == Some(id) {
                return map.get("nickname")?.as_str().map(str::to_string);
            }
            map.values().find_map(|v| nickname_of(v, id))
        }
        serde_json::Value::Array(items) => items.iter().find_map(|v| nickname_of(v, id)),
        _ => None,
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn a_pinned_shim_joins_is_told_and_reads_its_mail_through_tls() {
    let Some(_) = daemon_bin() else {
        return;
    };
    let daemon = Daemon::start();
    let pin = daemon.fingerprint.clone();
    std::fs::create_dir_all(daemon.dir.path().join("work")).unwrap();

    // Joins through the hook listener over TLS.
    let joined = daemon.hook(Some(&pin), "session-start", "tls-session-1");
    assert!(joined.status.success(), "session-start: {}", text(&joined));

    // Seen through myko over TLS, by the loopback tunnel.
    let nickname = {
        let mut seen = String::new();
        let found = wait_until(Duration::from_secs(10), || {
            let out = daemon.shim(Some(&pin), &["roster"]).output().unwrap();
            seen = text(&out);
            out.status.success() && seen.contains("tls-session-1")
        });
        assert!(found, "roster never showed the session: {seen}");
        // The roster is JSON: find the row for this session and take its nickname.
        let roster: serde_json::Value = serde_json::from_str(seen.trim_start_matches(|c| c != '{'))
            .unwrap_or_else(|e| panic!("roster isn't JSON ({e}): {seen}"));
        nickname_of(&roster, "tls-session-1").expect("the session's nickname on the roster")
    };

    // A terminal tell reaches it through myko over TLS.
    let told = daemon
        .shim(Some(&pin), &["tell", &nickname, "hello over TLS"])
        .output()
        .unwrap();
    assert!(told.status.success(), "tell: {}", text(&told));

    // The next hook, over TLS, shows the mail.
    let mut shown = String::new();
    let delivered = wait_until(Duration::from_secs(10), || {
        let out = daemon.hook(Some(&pin), "prompt-submit", "tls-session-1");
        shown = text(&out);
        shown.contains("hello over TLS")
    });
    assert!(delivered, "the mail never showed in the hook: {shown}");
}

#[test]
fn a_wrong_or_missing_pin_is_refused() {
    let Some(_) = daemon_bin() else {
        return;
    };
    let daemon = Daemon::start();
    std::fs::create_dir_all(daemon.dir.path().join("work")).unwrap();
    let wrong = "0".repeat(64);
    let right = daemon.fingerprint.clone();

    // myko, through the tunnel: the handshake fails, so the roster can't be read.
    let out = daemon.shim(Some(&wrong), &["roster"]).output().unwrap();
    assert!(
        !out.status.success(),
        "a wrong pin read the roster: {}",
        text(&out)
    );
    let out = daemon.shim(None, &["roster"]).output().unwrap();
    assert!(
        !out.status.success(),
        "no pin read the roster: {}",
        text(&out)
    );

    // The hooks: each refusal says why, and nothing joins.
    let out = daemon.hook(Some(&wrong), "session-start", "tls-session-wrong");
    assert!(
        text(&out).contains("TLS to the hook listener at"),
        "a wrong pin's hook didn't say TLS failed: {}",
        text(&out)
    );
    let out = daemon.hook(None, "session-start", "tls-session-nopin");
    assert!(
        text(&out).contains("needs the daemon's TLS fingerprint"),
        "a missing pin's hook didn't ask for the fingerprint: {}",
        text(&out)
    );
    // A join that did get through would show within the time the positive
    // test allows, so wait that long before saying neither did.
    std::thread::sleep(Duration::from_secs(3));
    let roster = daemon.shim(Some(&right), &["roster"]).output().unwrap();
    assert!(roster.status.success(), "roster: {}", text(&roster));
    let seen = text(&roster);
    assert!(
        !seen.contains("tls-session-wrong") && !seen.contains("tls-session-nopin"),
        "a session joined without the right pin: {seen}"
    );
}

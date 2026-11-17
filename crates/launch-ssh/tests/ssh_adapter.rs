//! Runs `marshal-launch-ssh` end to end, with `tests/fixtures/fake-ssh`
//! standing in for ssh so the adapter's real remote scripts run on this
//! machine. tmux runs on a private socket under a temp dir.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        std::fs::create_dir_all(dir.path().join("tmux")).unwrap();
        std::fs::create_dir_all(dir.path().join("work")).unwrap();
        Env { dir }
    }

    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }

    fn bin(&self, name: &str, body: &str) {
        let p = self.path("bin").join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn run(&self, verb: &str, req: &Value, extra: &[(&str, &str)]) -> Output {
        let path = format!(
            "{}:{}",
            self.path("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut c = Command::new(env!("CARGO_BIN_EXE_marshal-launch-ssh"));
        c.arg(verb)
            .env("MARSHAL_LAUNCH_SSH", fixture("fake-ssh"))
            .env("HOME", self.path("home"))
            .env("TMUX_TMPDIR", self.path("tmux"))
            .env_remove("TMUX")
            .env_remove("MARSHAL_BASE_URL")
            // The "target's" login shell: bash, unless a test says otherwise.
            .env("SHELL", which("bash"))
            .env("PATH", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in extra {
            c.env(k, v);
        }
        let mut child = c.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(req.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn request(&self, argv: &[&str]) -> Value {
        json!({
            "version": 1,
            "harness": "claude",
            "argv": argv,
            "cwd": self.path("work"),
            "name": "mar94-test",
            "agent_id": "launch-test-1",
            "marshal_version": marshal_launch_ssh::VERSION,
            "env": {
                "MARSHAL_AGENT_ID": "launch-test-1",
                "MARSHAL_DAEMON_ADDRESS": "ws://marshal.invalid:6155",
            },
            "options": ["--host", "target-1"],
        })
    }

    fn tmux(&self, args: &[&str]) -> Output {
        Command::new("tmux")
            .args(args)
            .env("TMUX_TMPDIR", self.path("tmux"))
            .env_remove("TMUX")
            .output()
            .unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.tmux(&["kill-server"]);
    }
}

fn which(p: &str) -> String {
    let o = Command::new("sh")
        .args(["-c", &format!("command -v {p}")])
        .output()
        .unwrap();
    String::from_utf8(o.stdout).unwrap().trim().to_string()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn stdout_json(o: &Output) -> Value {
    assert!(o.status.success(), "exit {:?}: {}", o.status, stderr(o));
    serde_json::from_slice(&o.stdout).unwrap()
}

fn have_tmux() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn launch_runs_argv_in_cwd_with_env_then_status_and_stop() {
    if !have_tmux() {
        eprintln!("skipping: no tmux");
        return;
    }
    let e = Env::new();
    // Prints what it was given, so the test sees argv, env and cwd unchanged.
    let req = e.request(&[
        "sh",
        "-c",
        "printf '%s|%s|%s|%s\\n' \"$MARSHAL_AGENT_ID\" \"$MARSHAL_DAEMON_ADDRESS\" \"$(pwd -P)\" \"$1\" > out; sleep 60",
        "sh",
        "it's one arg",
    ]);
    let l = stdout_json(&e.run("launch", &req, &[]));
    let work = std::fs::canonicalize(e.path("work")).unwrap();
    let host = String::from_utf8(Command::new("uname").arg("-n").output().unwrap().stdout).unwrap();
    assert_eq!(l["version"], 1);
    assert_eq!(l["handle"], "target-1 mar94-test");
    assert_eq!(l["host"], host.trim());
    assert_eq!(l["cwd"], work.display().to_string());
    assert_eq!(l["where"], format!("{} tmux:mar94-test", host.trim()));
    assert_eq!(l["attach"], "ssh -t target-1 tmux attach -t '=mar94-test'");
    // The response parses as the launcher's type.
    let _: marshal_launch::Launched = serde_json::from_value(l.clone()).unwrap();

    let out = work.join("out");
    for _ in 0..100 {
        if std::fs::read_to_string(&out).is_ok_and(|s| s.ends_with('\n')) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        format!(
            "launch-test-1|ws://marshal.invalid:6155|{}|it's one arg\n",
            work.display()
        )
    );

    // A second launch under the same name starts nothing.
    let again = e.run("launch", &req, &[]);
    assert!(!again.status.success());
    assert!(
        stderr(&again).contains("already has a tmux session named mar94-test"),
        "{}",
        stderr(&again)
    );

    let h = json!({"version": 1, "handle": "target-1 mar94-test"});
    assert_eq!(stdout_json(&e.run("status", &h, &[]))["running"], true);
    stdout_json(&e.run("stop", &h, &[]));
    assert_eq!(stdout_json(&e.run("status", &h, &[]))["running"], false);
    // Stopping what's gone is not an error.
    let gone = e.run("stop", &h, &[]);
    assert!(gone.status.success());
    assert!(stderr(&gone).contains("nothing to stop"));
}

#[test]
fn launch_refuses_loopback_and_starts_nothing() {
    let e = Env::new();
    let mut req = e.request(&["true"]);
    req["env"]["MARSHAL_DAEMON_ADDRESS"] = json!("ws://localhost:6155");
    for verb in ["check", "launch"] {
        let o = e.run(verb, &req, &[]);
        assert_eq!(o.status.code(), Some(1));
        assert!(stderr(&o).contains("loopback"), "{}", stderr(&o));
        assert!(o.stdout.is_empty());
    }
    assert!(
        !e.tmux(&["has-session", "-t", "=mar94-test"])
            .status
            .success()
    );
}

#[test]
fn an_unreachable_target_starts_nothing_and_says_so() {
    let e = Env::new();
    let o = e.run(
        "launch",
        &e.request(&["true"]),
        &[("FAKE_SSH_UNREACHABLE", "1")],
    );
    assert_eq!(o.status.code(), Some(1));
    let err = stderr(&o);
    assert!(err.contains("No route to host"), "{err}");
    assert!(
        err.contains("can't reach target-1 over ssh; nothing was started"),
        "{err}"
    );
}

#[test]
fn check_runs_its_probes_on_the_target_and_names_each_failure() {
    let e = Env::new();
    e.bin("marshal-shim", "echo 'marshal-shim 0.1.0'");
    e.bin("claude", "echo '[]'");
    let mut req = e.request(&["claude"]);
    req["cwd"] = json!("~/no-such-folder");
    let o = e.run("check", &req, &[]);
    assert_eq!(o.status.code(), Some(1));
    let err = stderr(&o);
    for want in [
        "can't reach the daemon at marshal.invalid:6155",
        "can't reach the daemon's hook listener at marshal.invalid:6156",
        "MARSHAL_HOOK_BIND",
        "has no folder",
        "no-such-folder",
        "the target's marshal-shim is 0.1.0, but this launcher is",
        "has no MCP server that runs marshal-shim",
        "don't run `marshal-shim claude-hook`",
    ] {
        assert!(err.contains(want), "missing `{want}` in:\n{err}");
    }
    // `~` is the target's home.
    assert!(
        err.contains(&e.path("home").join("no-such-folder").display().to_string()),
        "{err}"
    );
}

#[test]
fn check_reads_a_wired_claude_plugin_on_the_target() {
    let e = Env::new();
    let plugin = e.path("home/.claude/plugins/cache/marshal/marshal-shim/0.60.2");
    std::fs::create_dir_all(plugin.join("hooks")).unwrap();
    let repo_plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/marshal-shim");
    std::fs::copy(
        repo_plugin.join("hooks/hooks.json"),
        plugin.join("hooks/hooks.json"),
    )
    .unwrap();
    std::fs::copy(repo_plugin.join(".mcp.json"), plugin.join(".mcp.json")).unwrap();
    e.bin(
        "marshal-shim",
        &format!("echo 'marshal-shim {}'", marshal_launch_ssh::VERSION),
    );
    e.bin(
        "claude",
        &format!(
            "echo '[{{\"id\":\"marshal-shim@marshal\",\"enabled\":true,\"installPath\":\"{}\"}}]'",
            plugin.display()
        ),
    );
    let o = e.run("check", &e.request(&["claude"]), &[]);
    let err = stderr(&o);
    // Only the ports fail: marshal.invalid doesn't resolve. The plugin
    // shipped in this repo counts as wired.
    assert!(err.contains("can't reach the daemon at"), "{err}");
    assert!(!err.contains("plugin"), "{err}");
    assert!(!err.contains("Claude's hooks"), "{err}");
    assert!(!err.contains("MCP"), "{err}");
    assert!(!err.contains("the target's marshal-shim is"), "{err}");
}

#[test]
fn check_fails_when_the_targets_daemon_address_file_names_another_daemon() {
    let e = Env::new();
    let write = |dir: PathBuf, addr: &str| {
        std::fs::create_dir_all(dir.join("marshal")).unwrap();
        std::fs::write(dir.join("marshal/daemon-address"), format!("  {addr}\n")).unwrap();
    };
    // The target's own ~/.config.
    let home_cfg = e.path("home/.config");
    write(home_cfg.clone(), "ws://elsewhere:6155");
    let err = stderr(&e.run("check", &e.request(&["claude"]), &[]));
    let file = home_cfg.join("marshal/daemon-address");
    assert!(
        err.contains(&format!(
            "has {} naming ws://elsewhere:6155",
            file.display()
        )),
        "{err}"
    );
    assert!(err.contains("ahead of MARSHAL_DAEMON_ADDRESS"), "{err}");

    // A file naming the launch address is fine.
    write(home_cfg.clone(), "ws://marshal.invalid:6155");
    let err = stderr(&e.run("check", &e.request(&["claude"]), &[]));
    assert!(!err.contains("daemon-address"), "{err}");

    // The request's XDG_CONFIG_HOME is the agent's, so its file wins.
    let xdg = e.path("xdg");
    write(xdg.clone(), "ws://other:7000");
    let mut req = e.request(&["claude"]);
    req["env"]["XDG_CONFIG_HOME"] = serde_json::json!(xdg);
    let err = stderr(&e.run("check", &req, &[]));
    assert!(err.contains("naming ws://other:7000"), "{err}");
}

fn have_json_reader() -> bool {
    ["python3", "jq"].iter().any(|p| {
        Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// Wired by hand, as upstream's README documents: the MCP server in
/// ~/.claude.json and the hooks in ~/.claude/settings.json.
#[test]
fn check_accepts_claude_wired_by_hand_once_it_has_the_wait_hook() {
    if !have_json_reader() {
        eprintln!("skipping: neither python3 nor jq");
        return;
    }
    let e = Env::new();
    e.bin(
        "marshal-shim",
        &format!("echo 'marshal-shim {}'", marshal_launch_ssh::VERSION),
    );
    e.bin("claude", "echo '[]'");
    std::fs::write(
        e.path("home/.claude.json"),
        r#"{"numStartups": 3, "mcpServers": {"marshal": {"command": "marshal-shim"}},
            "projects": {"/elsewhere": {"history": ["kept on the target"]}}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(e.path("home/.claude")).unwrap();
    let settings = |extra: &str| {
        std::fs::write(
            e.path("home/.claude/settings.json"),
            format!(
                r#"{{"env": {{"SOME_TOKEN": "stays-on-the-target"}}, "hooks": {{
                  "SessionStart": [{{"hooks": [{{"type": "command", "command": "marshal-shim claude-hook session-start", "timeout": 15}}]}}],
                  "UserPromptSubmit": [{{"hooks": [{{"type": "command", "command": "marshal-shim claude-hook prompt-submit", "timeout": 15}}]}}]{extra}
                }}}}"#
            ),
        )
        .unwrap()
    };

    // The README's hooks alone: the only Claude reason is the missing waiter.
    settings("");
    let err = stderr(&e.run("check", &e.request(&["claude"]), &[]));
    assert!(err.contains("not `marshal-shim claude-hook wait`"), "{err}");
    assert!(!err.contains("MCP server"), "{err}");
    assert!(!err.contains("don't run"), "{err}");

    settings(
        r#", "Stop": [{"hooks": [{"type": "command", "command": "marshal-shim claude-hook wait", "asyncRewake": true}]}]"#,
    );
    let err = stderr(&e.run("check", &e.request(&["claude"]), &[]));
    assert!(err.contains("can't reach the daemon at"), "{err}");
    assert!(!err.contains("Claude"), "{err}");
}

/// The shim there takes MARSHAL_BASE_URL from the target's own environment
/// when the request sets none, so the check probes that hook listener.
#[test]
fn check_probes_the_hook_listener_the_targets_marshal_base_url_names() {
    let e = Env::new();
    for (base, want) in [
        (
            "http://hooks.invalid:7001/",
            "hook listener at hooks.invalid:7001",
        ),
        ("http://[fd00::1]:7002", "hook listener at [fd00::1]:7002"),
        (
            "https://hooks.invalid",
            "hook listener at hooks.invalid:443",
        ),
    ] {
        let err = stderr(&e.run(
            "check",
            &e.request(&["claude"]),
            &[("MARSHAL_BASE_URL", base)],
        ));
        assert!(err.contains(want), "{base}: {err}");
    }
    // The request's own MARSHAL_BASE_URL wins over the target's.
    let mut req = e.request(&["claude"]);
    req["env"]["MARSHAL_BASE_URL"] = json!("http://chosen.invalid:7003");
    let err = stderr(&e.run(
        "check",
        &req,
        &[("MARSHAL_BASE_URL", "http://hooks.invalid:7001")],
    ));
    assert!(
        err.contains("hook listener at chosen.invalid:7003"),
        "{err}"
    );
}

/// The check's probes connect: listeners on this machine's non-loopback
/// address are reported reachable (loopback launch addresses are refused).
#[test]
fn check_reaches_listening_ports() {
    let Some(ip) = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("10.255.255.255:9").map(|_| s))
        .and_then(|s| s.local_addr())
        .ok()
        .map(|a| a.ip())
        .filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
    else {
        eprintln!("skipping: no non-loopback address");
        return;
    };
    let daemon = std::net::TcpListener::bind((ip, 0)).unwrap();
    let hook = std::net::TcpListener::bind((ip, 0)).unwrap();
    let e = Env::new();
    let mut req = e.request(&["claude"]);
    req["env"]["MARSHAL_DAEMON_ADDRESS"] =
        json!(format!("ws://{ip}:{}", daemon.local_addr().unwrap().port()));
    req["env"]["MARSHAL_BASE_URL"] =
        json!(format!("http://{ip}:{}", hook.local_addr().unwrap().port()));
    let err = stderr(&e.run("check", &req, &[]));
    assert!(err.contains("can't run this launch"), "{err}");
    assert!(!err.contains("can't reach the daemon"), "{err}");
}

/// Rule 2: check brings back facts about the wiring, never the target's
/// secrets. Other MCP servers' env, settings env, hook arguments and
/// history stay on the target.
#[test]
fn check_brings_no_secrets_back_from_the_target() {
    if !have_json_reader() {
        eprintln!("skipping: neither python3 nor jq");
        return;
    }
    let e = Env::new();
    e.bin(
        "marshal-shim",
        &format!("echo 'marshal-shim {}'", marshal_launch_ssh::VERSION),
    );
    e.bin(
        "claude",
        "echo '[{\"id\":\"other@x\",\"enabled\":true,\"installPath\":\"/p/SECRET-PLUGIN-PATH\"}]'",
    );
    std::fs::write(
        e.path("home/.claude.json"),
        r#"{"oauthAccount": {"accessToken": "SECRET-OAUTH-TOKEN"},
            "mcpServers": {
              "marshal": {"command": "marshal-shim", "env": {"API_KEY": "SECRET-MARSHAL-ENV"}},
              "github": {"command": "gh-mcp", "env": {"GITHUB_TOKEN": "SECRET-GITHUB-TOKEN"}}},
            "projects": {"/elsewhere": {"history": ["SECRET-HISTORY"]}}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(e.path("home/.claude")).unwrap();
    std::fs::write(
        e.path("home/.claude/settings.json"),
        r#"{"env": {"ANTHROPIC_API_KEY": "SECRET-ANTHROPIC-KEY"}, "hooks": {
              "SessionStart": [{"hooks": [
                {"type": "command", "command": "marshal-shim claude-hook session-start --token SECRET-HOOK-ARG"},
                {"type": "command", "command": "TOKEN=SECRET-OTHER-HOOK other-tool run"}]}]}}"#,
    )
    .unwrap();
    let transcript = e.path("transcript");
    let o = e.run(
        "check",
        &e.request(&["claude"]),
        &[("FAKE_SSH_TRANSCRIPT", transcript.to_str().unwrap())],
    );
    let wire = std::fs::read_to_string(&transcript).unwrap();
    // The facts check needs did come back.
    assert!(
        wire.contains("@@claude-hook session-start\tmarshal-shim\t"),
        "{wire}"
    );
    assert!(
        wire.contains("@@claude-mcp user\tmarshal\tmarshal-shim\t\t~/.claude.json"),
        "{wire}"
    );
    let shown = format!("{wire}{}{}", String::from_utf8_lossy(&o.stdout), stderr(&o));
    assert!(
        !shown.contains("SECRET"),
        "a secret left the target:\n{shown}"
    );
}

/// A login shell other than bash (zsh on macOS) sets PATH in its profile.
/// The scripts take PATH from it, so what's only on that PATH is found.
#[test]
fn the_targets_login_shell_supplies_path() {
    let e = Env::new();
    let login_bin = e.path("login-bin");
    std::fs::create_dir_all(&login_bin).unwrap();
    let shim = login_bin.join("marshal-shim");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\necho 'marshal-shim {}'\n",
            marshal_launch_ssh::VERSION
        ),
    )
    .unwrap();
    let login_sh = e.path("login-sh");
    std::fs::write(
        &login_sh,
        format!(
            "#!/bin/sh\n[ \"$1\" = -lc ] || exit 1\nPATH={}:$PATH\nexport PATH\nexec sh -c \"$2\"\n",
            login_bin.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    for f in [&shim, &login_sh] {
        std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let req = e.request(&["claude"]);
    // Only the test's own bin and the system dirs: no machine install of
    // marshal-shim (~/.cargo/bin, ~/.local/bin) leaks in.
    let path = format!("{}:/usr/bin:/bin", e.path("bin").display());
    // With bash as the login shell, the shim isn't on PATH.
    let err = stderr(&e.run("check", &req, &[("PATH", &path)]));
    assert!(err.contains("no marshal-shim on its login PATH"), "{err}");
    // With the other shell, its PATH has it.
    let err = stderr(&e.run(
        "check",
        &req,
        &[("PATH", &path), ("SHELL", login_sh.to_str().unwrap())],
    ));
    assert!(!err.contains("marshal-shim on its login PATH"), "{err}");
    assert!(!err.contains("the target's marshal-shim is"), "{err}");
}

/// `--tmux <path>` runs that tmux for every verb, through the handle.
#[test]
fn tmux_option_is_used_by_check_launch_status_and_stop() {
    if !have_tmux() {
        eprintln!("skipping: no tmux");
        return;
    }
    let e = Env::new();
    let mut req = e.request(&["sh", "-c", "sleep 60"]);
    req["options"] = json!(["--host", "target-1", "--tmux", "/nonexistent/tmux"]);
    let err = stderr(&e.run("check", &req, &[]));
    assert!(
        err.contains("has no tmux at /nonexistent/tmux (--tmux)"),
        "{err}"
    );
    let o = e.run("launch", &req, &[]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("has no tmux (/nonexistent/tmux)"),
        "{}",
        stderr(&o)
    );

    let tmux = which("tmux");
    req["options"] = json!(["--host", "target-1", "--tmux", tmux]);
    let l = stdout_json(&e.run("launch", &req, &[]));
    assert_eq!(l["handle"], format!("target-1 mar94-test tmux={tmux}"));
    assert_eq!(
        l["attach"],
        format!("ssh -t target-1 {tmux} attach -t '=mar94-test'")
    );
    let h = json!({"version": 1, "handle": l["handle"]});
    assert_eq!(stdout_json(&e.run("status", &h, &[]))["running"], true);
    stdout_json(&e.run("stop", &h, &[]));
    assert_eq!(stdout_json(&e.run("status", &h, &[]))["running"], false);
}

/// `--xdg-config-home` is where the harness's shim will read
/// `marshal/daemon-address`, so check reads it there and not in ~/.config.
#[test]
fn xdg_config_home_option_steers_the_daemon_address_check() {
    let e = Env::new();
    // ~/.config names the live daemon: not this launch's, but not read either.
    std::fs::create_dir_all(e.path("home/.config/marshal")).unwrap();
    std::fs::write(
        e.path("home/.config/marshal/daemon-address"),
        "ws://live:6155\n",
    )
    .unwrap();
    let xdg = e.path("xdg");
    std::fs::create_dir_all(xdg.join("marshal")).unwrap();
    std::fs::write(
        xdg.join("marshal/daemon-address"),
        "ws://marshal.invalid:6155\n",
    )
    .unwrap();
    let mut req = e.request(&["claude"]);
    // Without the option, ~/.config's file is the one the shim would read.
    let err = stderr(&e.run("check", &req, &[]));
    assert!(err.contains("naming ws://live:6155"), "{err}");
    req["options"] = json!(["--host", "target-1", "--xdg-config-home", xdg]);
    let err = stderr(&e.run("check", &req, &[]));
    assert!(!err.contains("daemon-address"), "{err}");
    // Its file naming another daemon is caught.
    std::fs::write(xdg.join("marshal/daemon-address"), "ws://other:6155\n").unwrap();
    let err = stderr(&e.run("check", &req, &[]));
    assert!(
        err.contains(&format!(
            "{} naming ws://other:6155",
            xdg.join("marshal/daemon-address").display()
        )),
        "{err}"
    );
}

/// Exactly a1's layout: a user-scope `marshal` on an old shim (one that
/// rejects --version) fixed to the live daemon, and the launch folder's own
/// .mcp.json `marshal` plus hooks with an `XDG_CONFIG_HOME=…` prefix, both
/// on the new shim by path. The folder's server is the effective one.
#[test]
fn check_passes_a1s_layout() {
    if !have_json_reader() {
        eprintln!("skipping: neither python3 nor jq");
        return;
    }
    let e = Env::new();
    e.bin(
        "marshal-shim",
        "echo 'Error: unknown argument: --version' >&2; exit 1",
    );
    e.bin("claude", "echo '[]'");
    let new_dir = e.path("a1-test/bin");
    std::fs::create_dir_all(&new_dir).unwrap();
    let new_shim = new_dir.join("marshal-shim");
    std::fs::write(
        &new_shim,
        format!(
            "#!/bin/sh\necho 'marshal-shim {}'\n",
            marshal_launch_ssh::VERSION
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&new_shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    let xdg = e.path("a1-test/xdg");
    std::fs::write(
        e.path("home/.claude.json"),
        r#"{"mcpServers": {"marshal": {"command": "marshal-shim",
              "env": {"MARSHAL_DAEMON_ADDRESS": "ws://192.168.1.241:6155"}}}}"#,
    )
    .unwrap();
    let work = e.path("work");
    std::fs::write(
        work.join(".mcp.json"),
        format!(
            r#"{{"mcpServers": {{"marshal": {{"command": "{}"}}}}}}"#,
            new_shim.display()
        ),
    )
    .unwrap();
    std::fs::create_dir_all(work.join(".claude")).unwrap();
    let hook = |verb: &str| {
        format!(
            r#"{{"type": "command", "command": "XDG_CONFIG_HOME={} {} claude-hook {verb}"}}"#,
            xdg.display(),
            new_shim.display()
        )
    };
    std::fs::write(
        work.join(".claude/settings.json"),
        format!(
            r#"{{"hooks": {{
              "SessionStart": [{{"hooks": [{}, {}]}}],
              "UserPromptSubmit": [{{"hooks": [{}]}}],
              "Stop": [{{"hooks": [{}]}}]}}}}"#,
            hook("session-start"),
            hook("wait"),
            hook("prompt-submit"),
            hook("wait")
        ),
    )
    .unwrap();
    let mut req = e.request(&["claude"]);
    req["options"] = json!(["--host", "target-1", "--xdg-config-home", xdg]);
    let err = stderr(&e.run("check", &req, &[]));
    // Only the ports fail here (marshal.invalid doesn't resolve).
    assert!(err.contains("can't reach the daemon at"), "{err}");
    for not in [
        "Claude",
        "marshal-shim is",
        "more than one",
        "has no version",
        "fixed to",
    ] {
        assert!(!err.contains(not), "`{not}` in:\n{err}");
    }
}

/// a1's login shell is zsh. Real ssh hands the remote command to the login
/// shell, and the user's attach command goes the same way. zsh expands an
/// unquoted `=word` to a command path (EQUALS), so `-t =<name>` breaks there.
/// This runs the adapter's scripts through `zsh -c` (as sshd would) with zsh
/// as the login shell, on a live session.
///
/// Ignored by default so a machine without zsh or tmux can't count it as a
/// pass; run it with `--ignored`, and it fails if either is missing.
#[test]
#[ignore = "needs zsh and tmux; run with --ignored"]
fn status_stop_and_attach_work_when_the_login_shell_is_zsh() {
    let zsh = which("zsh");
    assert!(
        !zsh.is_empty(),
        "no zsh on this machine, so the zsh login-shell case can't run"
    );
    assert!(
        have_tmux(),
        "no tmux on this machine, so the zsh login-shell case can't run"
    );
    let e = Env::new();
    let shell = [
        ("SHELL", zsh.as_str()),
        ("FAKE_SSH_LOGIN_SHELL", zsh.as_str()),
    ];
    let l = stdout_json(&e.run("launch", &e.request(&["sh", "-c", "sleep 60"]), &shell));
    let h = json!({"version": 1, "handle": l["handle"]});
    assert_eq!(stdout_json(&e.run("status", &h, &shell))["running"], true);

    // The attach command, minus `ssh -t <dest>`, is what the target's zsh
    // runs. Checked with has-session, which needs no terminal.
    let remote = l["attach"]
        .as_str()
        .unwrap()
        .strip_prefix("ssh -t target-1 ")
        .unwrap()
        .replace(" attach ", " has-session ");
    let in_zsh = |cmd: &str| {
        Command::new(&zsh)
            .args(["-c", cmd])
            .env("TMUX_TMPDIR", e.path("tmux"))
            .env_remove("TMUX")
            .output()
            .unwrap()
    };
    let o = in_zsh(&remote);
    assert!(o.status.success(), "{remote}: {}", stderr(&o));
    // The unquoted form is the bug: zsh fails it before tmux runs.
    let o = in_zsh("tmux has-session -t =mar94-test");
    assert!(!o.status.success());
    assert!(stderr(&o).contains("not found"), "{}", stderr(&o));

    stdout_json(&e.run("stop", &h, &shell));
    assert_eq!(stdout_json(&e.run("status", &h, &shell))["running"], false);
}

//! MAR-56: a shim started under test with no daemon address refuses, rather
//! than falling back to `localhost:6155`/`:6156`, which on a developer's machine
//! is often a live daemon.
//!
//! Each run has a deadline, so a shim that connects and starts serving fails
//! the test instead of hanging it.

mod support;

use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(20);

/// A shim under `MARSHAL_TEST` with a temp home and no address of any kind.
fn unaddressed(args: &[&str]) -> Command {
    let home = support::shared_home();
    let mut cmd = support::shim(home);
    cmd.args(args)
        .env_remove("MARSHAL_DAEMON_ADDRESS")
        .env_remove("MYKO_ADDRESS")
        .env_remove("MARSHAL_BASE_URL");
    cmd
}

fn run(mut cmd: Command, stdin: &str) -> Output {
    use std::io::Write as _;
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn marshal-shim");
    let _ = child.stdin.take().unwrap().write_all(stdin.as_bytes());
    let started = Instant::now();
    while child.try_wait().expect("poll marshal-shim").is_none() {
        if started.elapsed() > DEADLINE {
            let _ = child.kill();
            panic!("marshal-shim kept running instead of refusing");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child.wait_with_output().expect("collect output")
}

fn assert_refused(out: &Output, what: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(2),
        "{what}: {:?} {stderr}",
        out.status
    );
    assert!(
        stderr.contains("MARSHAL_TEST is set but no daemon address was given"),
        "{what}: {stderr}"
    );
}

#[test]
fn the_mcp_shim_refuses_without_an_address() {
    assert_refused(&run(unaddressed(&[]), ""), "mcp shim");
}

#[test]
fn a_claude_hook_refuses_without_an_address() {
    let input = r#"{"session_id":"isolation-test","cwd":"/tmp","hook_event_name":"UserPromptSubmit","prompt":"hi"}"#;
    assert_refused(
        &run(unaddressed(&["claude-hook", "prompt-submit"]), input),
        "claude-hook",
    );
}

#[test]
fn a_blank_address_is_no_address() {
    let mut cmd = unaddressed(&[]);
    cmd.env("MARSHAL_DAEMON_ADDRESS", "  ");
    assert_refused(&run(cmd, ""), "blank MARSHAL_DAEMON_ADDRESS");
}

#[test]
fn help_and_version_still_answer_under_test() {
    for flag in ["--help", "--version"] {
        let out = run(unaddressed(&[flag]), "");
        assert!(out.status.success(), "{flag}: {:?}", out.status);
    }
}

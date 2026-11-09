//! Shared by every shim integration test (MAR-56): a shim a test starts never
//! reads or writes the operator's real config dir, and never reaches the
//! default `localhost:6155`/`:6156`, which on a developer's machine is often a
//! live daemon.
//!
//! [`shim`] gives a command that already has a temp `HOME` and config dir,
//! `MARSHAL_TEST=1` (so the shim itself refuses the default address), and an
//! address that never resolves. A test that has its own daemon or home sets
//! them after this, and its settings win.
#![allow(dead_code)]

use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

pub const SHIM: &str = env!("CARGO_BIN_EXE_marshal-shim");

/// A daemon address that can't reach anything: `.invalid` never resolves
/// (RFC 6761). It stands in until a test names its own daemon.
pub const NO_DAEMON: &str = "ws://marshal-test.invalid:1";

/// One temp home for the shims in this test binary that don't need their own.
/// It lives for the whole run.
pub fn shared_home() -> &'static Path {
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    HOME.get_or_init(|| {
        tempfile::Builder::new()
            .prefix("marshal-shim-test-")
            .tempdir()
            .expect("temp home for shim tests")
    })
    .path()
}

/// Point `cmd` at `home` and away from any live daemon.
pub fn isolate<'a>(cmd: &'a mut Command, home: &Path) -> &'a mut Command {
    cmd.env("MARSHAL_TEST", "1")
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("APPDATA", home.join("appdata"))
        .env("MARSHAL_DAEMON_ADDRESS", NO_DAEMON)
        .env_remove("MYKO_ADDRESS")
        .env_remove("MARSHAL_BASE_URL")
}

/// A `marshal-shim` command isolated in `home`.
pub fn shim(home: &Path) -> Command {
    let mut cmd = Command::new(SHIM);
    isolate(&mut cmd, home);
    cmd
}

//! `marshal-shim doctor`: report what's wrong with this machine's Marshal
//! setup. It only reads: it registers no session, changes no file, and prints
//! one line per check, with a fix for each problem it finds. The exit status
//! is non-zero if any check fails.
//!
//! Checks, in order: this binary is the `marshal-shim` on `PATH`; the daemon
//! accepts a WebSocket handshake; and Claude Code is wired to Marshal, with the
//! MCP server and every hook in `plugins/marshal-shim/hooks/hooks.json`, once
//! each.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;

/// How long the daemon may take to answer the handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Finding {
    pub level: Level,
    pub check: &'static str,
    pub detail: String,
    pub fix: Option<String>,
}

impl Finding {
    fn ok(check: &'static str, detail: impl Into<String>) -> Self {
        Finding {
            level: Level::Ok,
            check,
            detail: detail.into(),
            fix: None,
        }
    }
    fn warn(check: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Finding {
            level: Level::Warn,
            check,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }
    fn fail(check: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Finding {
            level: Level::Fail,
            check,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }
}

/// The Claude Code hooks Marshal needs: (event, command, whether it must be
/// `asyncRewake`). The same set as `plugins/marshal-shim/hooks/hooks.json`; a
/// test fails if the two drift apart.
const CLAUDE_HOOKS: &[(&str, &str, bool)] = &[
    (
        "SessionStart",
        "marshal-shim claude-hook session-start",
        false,
    ),
    ("SessionStart", "marshal-shim claude-hook wait", true),
    ("Stop", "marshal-shim claude-hook wait", true),
    (
        "UserPromptSubmit",
        "marshal-shim claude-hook prompt-submit",
        false,
    ),
    (
        "PostToolUse",
        "marshal-shim claude-hook post-tool-use",
        false,
    ),
];

/// The plugin that supplies the hooks and the MCP server when it's installed
/// and enabled: `<plugin>@<marketplace>`.
const PLUGIN_ID: &str = "marshal-shim@marshal";

/// Every `(command, asyncRewake)` under `event` in a settings file's `hooks`.
fn hook_commands<'a>(settings: &'a Value, event: &str) -> Vec<(&'a str, bool)> {
    settings["hooks"][event]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
        .filter_map(|h| {
            let cmd = h["command"].as_str()?;
            Some((cmd.trim(), h["asyncRewake"].as_bool().unwrap_or(false)))
        })
        .collect()
}

fn plugin_enabled(settings: Option<&Value>) -> bool {
    settings.is_some_and(|s| s["enabledPlugins"][PLUGIN_ID].as_bool() == Some(true))
}

/// Claude Code's wiring, from `settings.json` and `.claude.json` (either may be
/// missing). Pure, so it's tested without a home directory.
pub fn claude_wiring(settings: Option<&Value>, claude_json: Option<&Value>) -> Vec<Finding> {
    let mut out = Vec::new();
    let plugin = plugin_enabled(settings);

    // The MCP server: the plugin, or a user-scope `mcpServers` entry.
    let has_server = claude_json
        .and_then(|c| c["mcpServers"].as_object())
        .is_some_and(|m| {
            m.values().any(|v| {
                v["command"]
                    .as_str()
                    .is_some_and(|c| c.ends_with("marshal-shim"))
            })
        });
    out.push(match (plugin, has_server) {
        (true, true) => Finding::warn(
            "claude mcp",
            "the marshal MCP server is configured twice: by the plugin and in ~/.claude.json",
            "keep one: `claude mcp remove -s user marshal`, or uninstall the plugin",
        ),
        (true, false) => Finding::ok("claude mcp", format!("from the {PLUGIN_ID} plugin")),
        (false, true) => Finding::ok("claude mcp", "marshal-shim in ~/.claude.json"),
        (false, false) => Finding::fail(
            "claude mcp",
            "no marshal MCP server for Claude Code",
            "install the plugin (`/plugin install marshal-shim@marshal`), or \
             `claude mcp add -s user marshal marshal-shim`",
        ),
    });

    // The hooks. The plugin supplies them, so with the plugin on, any copy in
    // settings.json runs twice.
    let mut hook_problem = false;
    for &(event, command, rewake) in CLAUDE_HOOKS {
        let found: Vec<bool> = settings
            .map(|s| hook_commands(s, event))
            .unwrap_or_default()
            .into_iter()
            .filter(|(c, _)| *c == command)
            .map(|(_, r)| r)
            .collect();
        let check = "claude hook";
        let name = format!("{event}: {command}");
        let finding = match (plugin, found.as_slice()) {
            (true, []) => continue,
            (false, [r]) if *r == rewake => continue,
            (true, _) => Finding::warn(
                check,
                format!("{name} is in settings.json and also comes from the plugin"),
                "remove it from ~/.claude/settings.json; the plugin runs it",
            ),
            (false, []) => Finding::fail(
                check,
                format!("{name} is missing"),
                "install the plugin, or add it to ~/.claude/settings.json as \
                 plugins/marshal-shim/hooks/hooks.json has it",
            ),
            (false, [r]) => Finding::fail(
                check,
                format!("{name} has asyncRewake {r}, needs {rewake}"),
                "fix it to match plugins/marshal-shim/hooks/hooks.json",
            ),
            (false, more) => Finding::warn(
                check,
                format!("{name} is there {} times", more.len()),
                "keep one in ~/.claude/settings.json",
            ),
        };
        hook_problem = true;
        out.push(finding);
    }
    if !hook_problem {
        out.push(Finding::ok(
            "claude hook",
            if plugin {
                "from the plugin"
            } else {
                "all five in ~/.claude/settings.json"
            },
        ));
    }
    out
}

/// Whether this binary is the `marshal-shim` that Claude Code will run.
pub fn path_check(exe: Option<&Path>, on_path: Option<&Path>) -> Finding {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    match (exe, on_path) {
        (_, None) => Finding::fail(
            "binary",
            "no marshal-shim on PATH, and Claude Code runs it by name",
            "install marshal-shim (see the README), and put its directory on PATH",
        ),
        (Some(exe), Some(found)) if canon(exe) != canon(found) => Finding::warn(
            "binary",
            format!(
                "this is {}, but PATH finds {} first",
                exe.display(),
                found.display()
            ),
            "Claude Code runs the one on PATH; check that's the one you meant",
        ),
        (_, Some(found)) => Finding::ok("binary", found.display().to_string()),
    }
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{name}.exe"), name.to_string()]
    } else {
        vec![name.to_string()]
    };
    std::env::split_paths(&path).find_map(|d| names.iter().map(|n| d.join(n)).find(|p| p.is_file()))
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Claude Code's config files, honouring `CLAUDE_CONFIG_DIR`.
fn claude_files() -> (Option<PathBuf>, Option<PathBuf>) {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        let dir = PathBuf::from(dir);
        return (
            Some(dir.join("settings.json")),
            Some(dir.join(".claude.json")),
        );
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    (
        home.as_ref()
            .map(|h| h.join(".claude").join("settings.json")),
        home.map(|h| h.join(".claude.json")),
    )
}

async fn daemon_check(url: &str) -> Finding {
    let outcome =
        tokio::time::timeout(HANDSHAKE_TIMEOUT, tokio_tungstenite::connect_async(url)).await;
    match outcome {
        Ok(Ok((mut socket, _))) => {
            let _ = socket.close(None).await;
            Finding::ok("daemon", format!("accepted a handshake at {url}"))
        }
        // Something listens, but it isn't a WebSocket server on this path:
        // another program on the port, or the address names the wrong thing.
        Ok(Err(tokio_tungstenite::tungstenite::Error::Http(r))) => Finding::fail(
            "daemon",
            format!(
                "{url} answered HTTP {}, not a WebSocket handshake, so it isn't a Marshal daemon",
                r.status()
            ),
            "check what's listening on that port, and the daemon address \
             (`marshal-shim --help` says where it comes from)",
        ),
        Ok(Err(e)) => Finding::fail(
            "daemon",
            format!("no handshake at {url}: {e}"),
            "start the daemon (`marshal-daemon`), or check the address (`marshal-shim --help` \
             says where it comes from) and the firewall",
        ),
        Err(_) => Finding::fail(
            "daemon",
            format!("no answer from {url} within {HANDSHAKE_TIMEOUT:?}"),
            "check the daemon is running and reachable from here",
        ),
    }
}

fn render(f: &Finding) -> String {
    let mark = match f.level {
        Level::Ok => "ok  ",
        Level::Warn => "warn",
        Level::Fail => "FAIL",
    };
    match &f.fix {
        Some(fix) => format!(
            "{mark} {:<12} {}\n     {:<12} fix: {fix}",
            f.check, f.detail, ""
        ),
        None => format!("{mark} {:<12} {}", f.check, f.detail),
    }
}

pub async fn run() -> Result<()> {
    let mut findings = vec![path_check(
        std::env::current_exe().ok().as_deref(),
        which("marshal-shim").as_deref(),
    )];
    findings.push(daemon_check(&crate::daemon_address()).await);
    let (settings, claude_json) = claude_files();
    findings.extend(claude_wiring(
        settings.as_deref().and_then(read_json).as_ref(),
        claude_json.as_deref().and_then(read_json).as_ref(),
    ));
    for f in &findings {
        println!("{}", render(f));
    }
    let failed = findings.iter().filter(|f| f.level == Level::Fail).count();
    if failed > 0 {
        anyhow::bail!("{failed} check(s) failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hooks_json() -> Value {
        let text = include_str!("../../../plugins/marshal-shim/hooks/hooks.json");
        serde_json::from_str(text).unwrap()
    }

    fn server() -> Value {
        json!({ "mcpServers": { "marshal": { "command": "marshal-shim" } } })
    }

    fn levels(f: &[Finding]) -> Vec<(Level, &str)> {
        f.iter().map(|f| (f.level, f.detail.as_str())).collect()
    }

    #[test]
    fn the_plugins_hooks_are_exactly_what_doctor_wants() {
        // If hooks.json changes, CLAUDE_HOOKS must change with it.
        let plugin = hooks_json();
        let mut found = Vec::new();
        for event in plugin["hooks"].as_object().unwrap().keys() {
            for (cmd, rewake) in hook_commands(&plugin, event) {
                found.push((event.clone(), cmd.to_string(), rewake));
            }
        }
        let mut want: Vec<_> = CLAUDE_HOOKS
            .iter()
            .map(|(e, c, r)| (e.to_string(), c.to_string(), *r))
            .collect();
        found.sort();
        want.sort();
        assert_eq!(found, want);
    }

    #[test]
    fn the_plugin_id_names_this_repos_marketplace_and_plugin() {
        let market: Value =
            serde_json::from_str(include_str!("../../../.claude-plugin/marketplace.json")).unwrap();
        let plugin: Value = serde_json::from_str(include_str!(
            "../../../plugins/marshal-shim/.claude-plugin/plugin.json"
        ))
        .unwrap();
        let id = format!(
            "{}@{}",
            plugin["name"].as_str().unwrap(),
            market["name"].as_str().unwrap()
        );
        assert_eq!(id, PLUGIN_ID);
    }

    #[test]
    fn hooks_copied_from_the_plugin_pass() {
        let f = claude_wiring(Some(&hooks_json()), Some(&server()));
        assert!(f.iter().all(|f| f.level == Level::Ok), "{:?}", levels(&f));
        assert_eq!(f.len(), 2, "{:?}", levels(&f));
    }

    #[test]
    fn a_missing_hook_and_server_fail() {
        let f = claude_wiring(None, None);
        let fails = f.iter().filter(|f| f.level == Level::Fail).count();
        assert_eq!(fails, 1 + CLAUDE_HOOKS.len(), "{:?}", levels(&f));
        assert!(f.iter().all(|f| f.level != Level::Ok), "{:?}", levels(&f));
    }

    #[test]
    fn a_wait_hook_without_async_rewake_fails() {
        let mut settings = hooks_json();
        settings["hooks"]["Stop"][0]["hooks"][0]["asyncRewake"] = json!(false);
        let f = claude_wiring(Some(&settings), Some(&server()));
        assert!(
            f.iter()
                .any(|f| f.level == Level::Fail && f.detail.starts_with("Stop:")),
            "{:?}",
            levels(&f)
        );
        assert!(
            f.iter()
                .all(|f| f.detail != "all five in ~/.claude/settings.json")
        );
    }

    #[test]
    fn a_hook_twice_warns() {
        let mut settings = hooks_json();
        let group = settings["hooks"]["UserPromptSubmit"][0].clone();
        settings["hooks"]["UserPromptSubmit"]
            .as_array_mut()
            .unwrap()
            .push(group);
        let f = claude_wiring(Some(&settings), Some(&server()));
        assert!(f.iter().all(|f| f.level != Level::Fail), "{:?}", levels(&f));
        assert!(
            f.iter()
                .any(|f| f.level == Level::Warn && f.detail.contains("2 times")),
            "{:?}",
            levels(&f)
        );
    }

    #[test]
    fn the_plugin_beside_copies_warns_twice_over() {
        let mut settings = hooks_json();
        settings["enabledPlugins"] = json!({ PLUGIN_ID: true });
        let f = claude_wiring(Some(&settings), Some(&server()));
        assert!(f.iter().all(|f| f.level != Level::Fail), "{:?}", levels(&f));
        assert!(f.iter().any(|f| f.detail.contains("configured twice")));
        assert_eq!(
            f.iter()
                .filter(|f| f.detail.contains("also comes from the plugin"))
                .count(),
            CLAUDE_HOOKS.len()
        );
    }

    #[test]
    fn the_plugin_alone_passes() {
        let settings = json!({ "enabledPlugins": { PLUGIN_ID: true } });
        let f = claude_wiring(Some(&settings), Some(&json!({})));
        assert!(f.iter().all(|f| f.level == Level::Ok), "{:?}", levels(&f));
    }

    #[test]
    fn a_disabled_plugin_counts_as_absent() {
        let settings = json!({ "enabledPlugins": { PLUGIN_ID: false } });
        let f = claude_wiring(Some(&settings), Some(&json!({})));
        assert!(f.iter().any(|f| f.level == Level::Fail), "{:?}", levels(&f));
    }

    #[test]
    fn path_check_needs_marshal_shim_on_path() {
        assert_eq!(path_check(None, None).level, Level::Fail);
        let a = Path::new("/nonexistent/a/marshal-shim");
        let b = Path::new("/nonexistent/b/marshal-shim");
        assert_eq!(path_check(Some(a), Some(b)).level, Level::Warn);
        assert_eq!(path_check(Some(a), Some(a)).level, Level::Ok);
    }

    #[tokio::test]
    async fn the_daemon_check_fails_when_nothing_listens() {
        // Bind and drop a listener, so the port is free and nothing accepts.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let f = daemon_check(&format!("ws://127.0.0.1:{port}")).await;
        assert_eq!(f.level, Level::Fail, "{f:?}");
    }

    #[tokio::test]
    async fn the_daemon_check_passes_when_a_websocket_server_accepts() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let _ = tokio_tungstenite::accept_async(tcp).await;
        });
        let f = daemon_check(&format!("ws://127.0.0.1:{port}")).await;
        assert_eq!(f.level, Level::Ok, "{f:?}");
        server.abort();
    }

    #[tokio::test]
    async fn the_daemon_check_names_a_plain_http_server() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = tcp.read(&mut buf).await;
            let _ = tcp
                .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n")
                .await;
        });
        let f = daemon_check(&format!("ws://127.0.0.1:{port}")).await;
        assert_eq!(f.level, Level::Fail, "{f:?}");
        assert!(f.detail.contains("HTTP 404"), "{f:?}");
        server.abort();
    }
}

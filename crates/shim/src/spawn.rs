//! `marshal-shim spawn`: start a Claude Code or Codex session in a detached
//! tmux session, and wait for it to join the roster (MAR-26).
//!
//! It adds no permission or channels flags. The harness starts in its default
//! permission mode and gets live delivery from its hooks, as any session does.
//! There's also no way to pass flags through to the harness, so an agent can't
//! use spawn to start a session with more permissions than the defaults.
//!
//! Each launch gets a fresh token, `spawn-<uuid>`, passed to the harness as
//! `MARSHAL_AGENT_ID` (MAR-27). Both harnesses' hooks report that as the
//! session's `agent_id`, so the session that joined is the roster row carrying
//! the token, and no other session, in that directory or anywhere, can be
//! taken for it. The token is a correlation id, not a credential: it's visible
//! on the roster and on tmux's command line, and grants nothing.
//!
//! tmux runs the harness with no shell in between: it hands a lone command
//! argument to `sh -c`, so the harness always goes as two or more arguments.
//!
//! When a tmux server is already running, a new session gets that server's
//! environment, not spawn's. So spawn passes the settings that decide which
//! daemon the harness joins and as whom ([`PASSED_ENV`]) with `new-session -e`.
//! None of them are secrets, which matters because `-e` puts them on tmux's
//! command line. The spawner's own identity is never passed: the new session
//! gets its own token as `MARSHAL_AGENT_ID`, never the spawner's, and the Codex
//! app-server endpoint isn't passed at all.
//!
//! With `--task`, once the session has joined, spawn sends it the task as a
//! direct message (MAR-28), the way `tell` does from a terminal: under a
//! short-lived terminal session, labelled as from a terminal, addressed to the
//! joined session's full id. Direct mail wakes an idle session through its
//! hooks, so it acts on the task in whatever permission mode it started in.
//!
//! Apart from that send, spawn only reads the roster and registers no session.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use hyphae::Gettable;
use marshal_entities::{GetAllSessionNicknames, GetAllSessions, Session, SessionNickname};
use myko::client::{ConnectionStatus, MykoClient};

const USAGE: &str = "usage: marshal-shim spawn <claude|codex> [--cwd <dir>] [--name <tmux session>] [--timeout <secs>] [--task <text>]";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const POLL: Duration = Duration::from_millis(200);
pub(crate) const MAX_NAME: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Harness {
    Claude,
    Codex,
}

impl Harness {
    pub(crate) fn parse(s: &str) -> Result<Self> {
        match s {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            other => bail!("unknown harness {other:?}: spawn starts claude or codex\n{USAGE}"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Args {
    pub harness: Harness,
    pub cwd: Option<PathBuf>,
    pub name: Option<String>,
    pub timeout: Duration,
    /// Sent to the new session as a direct message once it joins (MAR-28).
    pub task: Option<String>,
}

pub(crate) fn parse_args(args: &[String]) -> Result<Args> {
    let mut it = args.iter();
    let harness = Harness::parse(it.next().context(USAGE)?)?;
    let mut parsed = Args {
        harness,
        cwd: None,
        name: None,
        timeout: DEFAULT_TIMEOUT,
        task: None,
    };
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .with_context(|| format!("{flag} needs a value\n{USAGE}"))
        };
        match flag.as_str() {
            "--cwd" => parsed.cwd = Some(PathBuf::from(value()?)),
            "--name" => parsed.name = Some(value()?.clone()),
            "--timeout" => {
                let secs: u64 = value()?
                    .parse()
                    .with_context(|| format!("--timeout takes whole seconds\n{USAGE}"))?;
                ensure!(secs > 0, "--timeout must be at least 1 second");
                parsed.timeout = Duration::from_secs(secs);
            }
            "--task" => {
                let task = value()?;
                ensure!(!task.trim().is_empty(), "--task needs some text\n{USAGE}");
                parsed.task = Some(task.clone());
            }
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    Ok(parsed)
}

/// tmux takes `.` and `:` in a target as window and pane separators, so a name
/// is kept to letters, digits, `-` and `_`.
pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `<harness>-<directory name>`, with anything tmux can't take turned into `-`.
fn default_name(harness: Harness, cwd: &Path) -> String {
    let base = cwd
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let base: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut name = format!("{}-{}", harness.as_str(), base.trim_matches('-'));
    name.truncate(MAX_NAME);
    name.trim_end_matches('-').to_string()
}

/// The program and arguments tmux runs, with no shell in between. Codex goes
/// through `codex-run`, which is what gives an idle Codex thread live wakes.
///
/// `claude` alone would be one argument, which tmux runs through `sh -c`, so
/// it goes through `env --` to make it two and keep the shell out.
pub(crate) fn harness_command(harness: Harness, shim: &Path) -> Vec<String> {
    match harness {
        Harness::Claude => vec![
            "env".into(),
            "--".into(),
            std::env::var("MARSHAL_CLAUDE_BIN").unwrap_or_else(|_| "claude".into()),
        ],
        Harness::Codex => vec![shim.display().to_string(), "codex-run".into()],
    }
}

/// Settings passed from spawn's environment to the harness's, because a
/// running tmux server would otherwise give it the server's. Each decides
/// which daemon the harness joins, or what it reports about itself, and none
/// is a secret.
const PASSED_ENV: &[&str] = &[
    "MARSHAL_DAEMON_ADDRESS",
    "MYKO_ADDRESS",
    "MARSHAL_BASE_URL",
    "MARSHAL_HOST",
    "MARSHAL_OPERATOR",
    "MARSHAL_BIN_DIR",
    "MARSHAL_CODEX_BIN",
    "MARSHAL_WAITER_LOCK_WAIT_SECS",
    // Where the daemon-address file is read from.
    "XDG_CONFIG_HOME",
];

fn passed_env(get: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    PASSED_ENV
        .iter()
        .filter_map(|k| get(k).map(|v| (k.to_string(), v)))
        .collect()
}

pub(crate) fn tmux_new_session_args(
    name: &str,
    cwd: &Path,
    env: &[(String, String)],
    command: &[String],
) -> Vec<String> {
    let mut args = vec![
        "new-session".to_string(),
        "-d".into(),
        "-s".into(),
        name.into(),
        "-c".into(),
        cwd.display().to_string(),
    ];
    for (k, v) in env {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    // Two or more arguments after this, so tmux runs the command directly
    // rather than through a shell, and nothing in it is interpreted.
    args.push("--".into());
    args.extend(command.iter().cloned());
    args
}

pub(crate) fn tmux() -> Command {
    Command::new(std::env::var("MARSHAL_TMUX_BIN").unwrap_or_else(|_| "tmux".into()))
}

/// `=name` matches that session exactly, not as a prefix of a longer name.
pub(crate) fn tmux_has_session(name: &str) -> Result<bool> {
    let status = tmux()
        .args(["has-session", "-t", &format!("={name}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("running tmux: is it installed?")?;
    Ok(status.success())
}

/// A token for one launch. Fresh each time, so no earlier row can carry it.
pub(crate) fn new_token() -> String {
    format!("spawn-{}", uuid::Uuid::new_v4().simple())
}

/// The session carrying this launch's token.
pub(crate) fn joined<'a>(sessions: &'a [Arc<Session>], token: &str) -> Option<&'a Session> {
    sessions
        .iter()
        .map(|s| s.as_ref())
        .find(|s| s.agent_id.as_deref() == Some(token))
}

/// What the harness is given: the listed settings from spawn's environment,
/// and this launch's token as its `MARSHAL_AGENT_ID`, in place of any the
/// spawner has.
pub(crate) fn harness_env(
    get: impl Fn(&str) -> Option<String>,
    token: &str,
) -> Vec<(String, String)> {
    let mut env = passed_env(get);
    env.push(("MARSHAL_AGENT_ID".into(), token.into()));
    env
}

pub(crate) fn run(rest: &[String]) -> Result<()> {
    // `spawn list | attach | stop` manage what spawn started (MAR-30).
    if let Some(sub @ ("list" | "attach" | "stop")) = rest.first().map(String::as_str) {
        return crate::spawn_manage::run(sub, &rest[1..]);
    }
    let args = parse_args(rest)?;
    let cwd = match &args.cwd {
        Some(dir) => dir.clone(),
        None => std::env::current_dir().context("reading the current directory")?,
    };
    let cwd = std::fs::canonicalize(&cwd)
        .with_context(|| format!("{} isn't a directory spawn can use", cwd.display()))?;
    ensure!(cwd.is_dir(), "{} isn't a directory", cwd.display());
    let name = match args.name.clone() {
        Some(name) => name,
        None => default_name(args.harness, &cwd),
    };
    ensure!(
        valid_name(&name),
        "{name:?} can't name a tmux session: use up to {MAX_NAME} letters, digits, - or _"
    );
    ensure!(
        !tmux_has_session(&name)?,
        "a tmux session named {name} already exists: attach with `tmux attach -t ={name}`, or pick another --name"
    );
    let shim = std::env::current_exe().context("finding marshal-shim's own path")?;
    let command = harness_command(args.harness, &shim);

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?
        .block_on(async {
            let session = launch_and_wait(&name, &cwd, &command, args.timeout).await?;
            match args.task {
                Some(task) => send_task(&name, &session, task).await,
                None => Ok(()),
            }
        })
}

/// Send the first task as a direct message, the way `tell` does from a
/// terminal, to the joined session's full id, so it reaches that session and
/// no other. Direct mail wakes an idle session through its hooks.
pub(crate) async fn send_task(name: &str, session_id: &str, task: String) -> Result<()> {
    crate::terminal_send::run(
        crate::terminal_send::Target::Named(session_id.to_string()),
        task,
    )
    .await
    .with_context(|| {
        format!(
            "the session joined, but its first task wasn't sent. It's still running in tmux session {name}; \
             send the task with `marshal-shim tell {session_id} <task>`"
        )
    })
}

async fn launch_and_wait(
    name: &str,
    cwd: &Path,
    command: &[String],
    timeout: Duration,
) -> Result<String> {
    marshal_entities::link();
    let client = MykoClient::new();
    let sessions = client.watch_query::<GetAllSessions>(GetAllSessions {});
    let nicknames = client.watch_query::<GetAllSessionNicknames>(GetAllSessionNicknames {});
    let address = crate::daemon_address();
    client.set_address(Some(address.clone()));

    // Start nothing unless the daemon can be reached, so a failure to reach it
    // is said plainly rather than as a session that never joined.
    let started = Instant::now();
    while !matches!(
        client.connection_status_sync(),
        ConnectionStatus::Connected(_)
    ) {
        if started.elapsed() > CONNECT_TIMEOUT {
            bail!(
                "couldn't reach the Marshal daemon at {address} within {CONNECT_TIMEOUT:?}, so nothing was started"
            );
        }
        tokio::time::sleep(POLL).await;
    }
    let token = new_token();

    let out = tmux()
        .args(tmux_new_session_args(
            name,
            cwd,
            &harness_env(|k| std::env::var(k).ok(), &token),
            command,
        ))
        .output()
        .context("running tmux: is it installed?")?;
    ensure!(
        out.status.success(),
        "tmux couldn't start {}: {}",
        command.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    eprintln!(
        "started `{}` in tmux session {name} in {}; waiting up to {}s for it to join",
        command.join(" "),
        cwd.display(),
        timeout.as_secs()
    );

    let launched = Instant::now();
    loop {
        if let Some(s) = joined(&sessions.get(), &token) {
            let nickname = nickname_of(&nicknames.get(), s);
            println!(
                "{nickname} ({}) joined from tmux session {name}. Attach: tmux attach -t ={name}",
                s.id.0
            );
            return Ok(s.id.0.to_string());
        }
        if !tmux_has_session(name)? {
            bail!(
                "`{}` exited before joining the roster, and its tmux session {name} is gone",
                command.join(" ")
            );
        }
        if launched.elapsed() > timeout {
            bail!(
                "no session carrying this launch's token joined the roster from {} within {}s. The tmux session {name} is left running: \
                 `tmux attach -t ={name}` to see it (a first-run prompt such as trusting the folder \
                 holds a session back until it's answered), or `tmux kill-session -t ={name}`",
                cwd.display(),
                timeout.as_secs()
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

pub(crate) fn nickname_of(nicknames: &[Arc<SessionNickname>], s: &Session) -> String {
    nicknames
        .iter()
        .find(|n| n.id.0 == s.id.0)
        .map(|n| n.nickname.clone())
        .unwrap_or_else(|| marshal_entities::nickname(&s.id.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use marshal_entities::SessionId;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_a_harness_and_its_options() {
        let a = parse_args(&args(&[
            "codex",
            "--cwd",
            "/w",
            "--name",
            "dev",
            "--timeout",
            "5",
        ]))
        .unwrap();
        assert_eq!(a.harness, Harness::Codex);
        assert_eq!(a.cwd.as_deref(), Some(Path::new("/w")));
        assert_eq!(a.name.as_deref(), Some("dev"));
        assert_eq!(a.timeout, Duration::from_secs(5));
        assert_eq!(
            parse_args(&args(&["claude"])).unwrap().timeout,
            DEFAULT_TIMEOUT
        );
        assert_eq!(parse_args(&args(&["claude"])).unwrap().task, None);
        let a = parse_args(&args(&["claude", "--task", "say ready"])).unwrap();
        assert_eq!(a.task.as_deref(), Some("say ready"));
        assert!(parse_args(&args(&["claude", "--task", "  "])).is_err());
        assert!(parse_args(&args(&["claude", "--task"])).is_err());
    }

    #[test]
    fn refuses_anything_it_doesnt_know_including_harness_flags() {
        assert!(parse_args(&args(&[])).is_err());
        assert!(parse_args(&args(&["bash"])).is_err());
        assert!(parse_args(&args(&["claude", "--cwd"])).is_err());
        assert!(parse_args(&args(&["claude", "--timeout", "0"])).is_err());
        assert!(parse_args(&args(&["claude", "--dangerously-skip-permissions"])).is_err());
        assert!(
            parse_args(&args(&[
                "claude",
                "--",
                "--permission-mode",
                "bypassPermissions"
            ]))
            .is_err()
        );
    }

    #[test]
    fn tmux_names_are_kept_to_what_a_target_can_take() {
        assert!(valid_name("claude-marshal_2"));
        assert!(!valid_name(""));
        assert!(!valid_name("a.b"));
        assert!(!valid_name("a:b"));
        assert!(!valid_name("a b"));
        assert!(!valid_name(&"x".repeat(MAX_NAME + 1)));
        assert_eq!(
            default_name(Harness::Claude, Path::new("/home/j/my repo.v2")),
            "claude-my-repo-v2"
        );
        assert_eq!(default_name(Harness::Codex, Path::new("/")), "codex");
        assert!(valid_name(&default_name(
            Harness::Codex,
            Path::new(&format!("/{}", "y".repeat(200)))
        )));
    }

    #[test]
    fn the_harness_runs_with_no_flags_and_no_shell() {
        let shim = Path::new("/opt/marshal-shim");
        assert_eq!(
            harness_command(Harness::Codex, shim),
            ["/opt/marshal-shim", "codex-run"]
        );
        // tmux hands a lone argument to `sh -c`, so there are always two or more.
        let claude = harness_command(Harness::Claude, shim);
        assert!(claude.len() >= 2, "{claude:?}");
        assert_eq!(claude[..2], ["env", "--"]);
        let env = [("MARSHAL_DAEMON_ADDRESS".to_string(), "ws://h:1".to_string())];
        let tmux = tmux_new_session_args("n", Path::new("/w"), &env, &claude[..]);
        assert_eq!(
            tmux[..9],
            [
                "new-session",
                "-d",
                "-s",
                "n",
                "-c",
                "/w",
                "-e",
                "MARSHAL_DAEMON_ADDRESS=ws://h:1",
                "--"
            ]
        );
        assert_eq!(tmux[9..], claude[..]);
    }

    #[test]
    fn only_the_listed_settings_reach_the_harness_and_never_an_identity() {
        let env = |k: &str| match k {
            "MARSHAL_DAEMON_ADDRESS"
            | "MARSHAL_AGENT_ID"
            | "MARSHAL_CODEX_APP_SERVER_SOCKET"
            | "XDG_CONFIG_HOME" => Some(format!("{k}-value")),
            _ => None,
        };
        let passed = passed_env(env);
        let keys: Vec<&str> = passed.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["MARSHAL_DAEMON_ADDRESS", "XDG_CONFIG_HOME"]);
        assert!(
            !PASSED_ENV
                .iter()
                .any(|k| k.contains("AGENT_ID") || k.contains("APP_SERVER"))
        );
    }

    #[test]
    fn the_harness_gets_this_launchs_token_and_never_the_spawners() {
        let env = |k: &str| match k {
            "MARSHAL_AGENT_ID" => Some("the-spawner".to_string()),
            "MARSHAL_DAEMON_ADDRESS" => Some("ws://h:1".to_string()),
            _ => None,
        };
        let token = new_token();
        let given = harness_env(env, &token);
        let ids: Vec<&str> = given
            .iter()
            .filter(|(k, _)| k == "MARSHAL_AGENT_ID")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(ids, [token.as_str()]);
        assert!(token.starts_with("spawn-") && token.len() == "spawn-".len() + 32);
        assert_ne!(new_token(), token, "a token is fresh for each launch");
    }

    fn row(id: &str, cwd: &str, agent_id: Option<&str>) -> Arc<Session> {
        Arc::new(Session {
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
            connected_at: 100,
            last_activity_at: None,
            last_tool: None,
            last_tool_at: None,
            operator: None,
            host: None,
            project: None,
            channels_enabled: None,
            acks_pushes: None,
        })
    }

    #[test]
    fn only_the_session_carrying_the_token_has_joined() {
        // Another new session in the same directory, with no token or another's.
        let rows = [
            row("same-dir", "/w", None),
            row("other-launch", "/w", Some("spawn-other")),
        ];
        assert!(joined(&rows, "spawn-mine").is_none());
        let rows = [
            row("same-dir", "/w", None),
            row("mine", "/elsewhere", Some("spawn-mine")),
        ];
        assert_eq!(joined(&rows, "spawn-mine").unwrap().id.0.as_ref(), "mine");
    }
}

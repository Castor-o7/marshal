//! `marshal-shim spawn`: start a Claude Code or Codex session in a detached
//! tmux session, and wait for it to join the roster (MAR-26).
//!
//! It adds no permission or channels flags. The harness starts in its default
//! permission mode and gets live delivery from its hooks, as any session does.
//! There's also no way to pass flags through to the harness, so an agent can't
//! use spawn to start a session with more permissions than the defaults.
//!
//! A session is taken to have joined when a session id that wasn't on the
//! roster before the launch appears with the launch directory as its cwd. That
//! can be fooled by another session starting in the same directory at the same
//! time. MAR-27 replaces it with a one-time token.
//!
//! Like `roster`, spawn only reads the roster and registers no session itself.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use hyphae::Gettable;
use marshal_entities::{GetAllSessionNicknames, GetAllSessions, Session, SessionNickname};
use myko::client::{ConnectionStatus, MykoClient};

const USAGE: &str = "usage: marshal-shim spawn <claude|codex> [--cwd <dir>] [--name <tmux session>] [--timeout <secs>]";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to let the roster arrive before taking the snapshot. It may really
/// be empty, so this can't wait for rows to appear.
const ROSTER_SETTLE: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(200);
const MAX_NAME: usize = 64;
/// How far the daemon's clock may run behind this one.
const CLOCK_SLACK_MS: i64 = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Harness {
    Claude,
    Codex,
}

impl Harness {
    fn parse(s: &str) -> Result<Self> {
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
}

pub(crate) fn parse_args(args: &[String]) -> Result<Args> {
    let mut it = args.iter();
    let harness = Harness::parse(it.next().context(USAGE)?)?;
    let mut parsed = Args {
        harness,
        cwd: None,
        name: None,
        timeout: DEFAULT_TIMEOUT,
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
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    Ok(parsed)
}

/// tmux takes `.` and `:` in a target as window and pane separators, so a name
/// is kept to letters, digits, `-` and `_`.
fn valid_name(name: &str) -> bool {
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
fn harness_command(harness: Harness, shim: &Path) -> Vec<String> {
    match harness {
        Harness::Claude => {
            vec![std::env::var("MARSHAL_CLAUDE_BIN").unwrap_or_else(|_| "claude".into())]
        }
        Harness::Codex => vec![shim.display().to_string(), "codex-run".into()],
    }
}

fn tmux_new_session_args(name: &str, cwd: &Path, command: &[String]) -> Vec<String> {
    let mut args = vec![
        "new-session".to_string(),
        "-d".into(),
        "-s".into(),
        name.into(),
        "-c".into(),
        cwd.display().to_string(),
        // Given as separate arguments, tmux runs the command directly rather
        // than through a shell, so nothing in it is interpreted.
        "--".into(),
    ];
    args.extend(command.iter().cloned());
    args
}

fn tmux() -> Command {
    Command::new(std::env::var("MARSHAL_TMUX_BIN").unwrap_or_else(|_| "tmux".into()))
}

/// `=name` matches that session exactly, not as a prefix of a longer name.
fn tmux_has_session(name: &str) -> Result<bool> {
    let status = tmux()
        .args(["has-session", "-t", &format!("={name}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("running tmux: is it installed?")?;
    Ok(status.success())
}

fn same_dir(a: &str, b: &Path) -> bool {
    let a = Path::new(a);
    a == b || std::fs::canonicalize(a).is_ok_and(|a| a == b)
}

/// The first session that wasn't on the roster before the launch, registered
/// no earlier than `not_before_ms`, and is working in the launch directory.
pub(crate) fn joined<'a>(
    before: &HashSet<String>,
    not_before_ms: i64,
    sessions: &'a [Arc<Session>],
    cwd: &Path,
) -> Option<&'a Session> {
    sessions.iter().map(|s| s.as_ref()).find(|s| {
        !before.contains(s.id.0.as_ref())
            && s.connected_at >= not_before_ms
            && same_dir(&s.cwd, cwd)
    })
}

pub(crate) fn run(rest: &[String]) -> Result<()> {
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
        .block_on(launch_and_wait(&name, &cwd, &command, args.timeout))
}

async fn launch_and_wait(
    name: &str,
    cwd: &Path,
    command: &[String],
    timeout: Duration,
) -> Result<()> {
    marshal_entities::link();
    let client = MykoClient::new();
    let sessions = client.watch_query::<GetAllSessions>(GetAllSessions {});
    let nicknames = client.watch_query::<GetAllSessionNicknames>(GetAllSessionNicknames {});
    let address = crate::daemon_address();
    client.set_address(Some(address.clone()));

    // The snapshot has to come from a connected roster, or every session on it
    // would look new. An empty roster can't be told from one that hasn't
    // arrived, so once connected, give the rows a moment to come.
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
    let connected = Instant::now();
    while sessions.get().is_empty() && connected.elapsed() < ROSTER_SETTLE {
        tokio::time::sleep(POLL).await;
    }
    let before: HashSet<String> = sessions.get().iter().map(|s| s.id.0.to_string()).collect();
    // A row that arrived late wouldn't be in the snapshot, so a joined session
    // must also have registered after this, give or take clock skew.
    let not_before_ms = chrono::Utc::now().timestamp_millis() - CLOCK_SLACK_MS;

    let out = tmux()
        .args(tmux_new_session_args(name, cwd, command))
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
        if let Some(s) = joined(&before, not_before_ms, &sessions.get(), cwd) {
            let nickname = nickname_of(&nicknames.get(), s);
            println!(
                "{nickname} ({}) joined from tmux session {name}. Attach: tmux attach -t ={name}",
                s.id.0
            );
            return Ok(());
        }
        if !tmux_has_session(name)? {
            bail!(
                "`{}` exited before joining the roster, and its tmux session {name} is gone",
                command.join(" ")
            );
        }
        if launched.elapsed() > timeout {
            bail!(
                "nothing joined the roster from {} within {}s. The tmux session {name} is left running: \
                 `tmux attach -t ={name}` to see it (a first-run prompt such as trusting the folder \
                 holds a session back until it's answered), or `tmux kill-session -t ={name}`",
                cwd.display(),
                timeout.as_secs()
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

fn nickname_of(nicknames: &[Arc<SessionNickname>], s: &Session) -> String {
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
        let tmux = tmux_new_session_args("n", Path::new("/w"), &["claude".into()]);
        assert_eq!(
            tmux,
            ["new-session", "-d", "-s", "n", "-c", "/w", "--", "claude"]
        );
    }

    fn row(id: &str, cwd: &str) -> Arc<Session> {
        row_at(id, cwd, 100)
    }

    fn row_at(id: &str, cwd: &str, connected_at: i64) -> Arc<Session> {
        Arc::new(Session {
            id: SessionId(Arc::from(id)),
            client_id: None,
            agent_id: None,
            pid: 0,
            cwd: cwd.into(),
            git_branch: None,
            current_task: None,
            session_name: None,
            activity: None,
            kind: None,
            connected_at,
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
    fn only_a_new_session_in_the_launch_directory_counts_as_joined() {
        let before: HashSet<String> = ["old".to_string()].into();
        let dir = Path::new("/nonexistent/w");
        let rows = [
            row("old", "/nonexistent/w"),
            row("elsewhere", "/nonexistent/x"),
        ];
        assert!(joined(&before, 50, &rows, dir).is_none());
        let rows = [row("old", "/nonexistent/w"), row("new", "/nonexistent/w")];
        assert_eq!(
            joined(&before, 50, &rows, dir).unwrap().id.0.as_ref(),
            "new"
        );
        // One the snapshot missed, but which registered long before the launch.
        let rows = [row_at("late", "/nonexistent/w", 10)];
        assert!(joined(&before, 50, &rows, dir).is_none());
    }
}

//! `marshal-shim spawn-team <team.toml>`: start a team of sessions from one
//! file, each in its own tmux session, and give each its first task (MAR-32).
//!
//! ```toml
//! team = "app"            # optional; the file's name otherwise
//!
//! [[member]]
//! role    = "backend"
//! harness = "claude"      # or "codex"
//! cwd     = "~/src/app"   # `~/`, absolute, or relative to the team file
//! task    = "Take the next story on the roadmap"   # optional
//! ```
//!
//! Every member is launched the way `spawn` launches one: no flags added, no
//! shell, the listed settings passed with `new-session -e`, and its own
//! one-time token as `MARSHAL_AGENT_ID`. Each joins when the row carrying its
//! token appears. Its tmux session is `<team>-<role>`.
//!
//! Marshal has no role field on a session yet (MAR-9), so the role shows in
//! two places: the tmux session's name, and the start of the first task, which
//! is sent as "You are the <role> member of team <team>. <task>". A member
//! with no task is sent nothing.
//!
//! The whole file is checked before anything starts: a bad harness, a missing
//! directory, a repeated role or a tmux name already taken starts no one. Once
//! started, a member that doesn't join is reported and the rest carry on.
//! Members that did start are left running either way.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use hyphae::Gettable;
use marshal_entities::{GetAllSessionNicknames, GetAllSessions};
use myko::client::{ConnectionStatus, MykoClient};
use serde::Deserialize;

use crate::spawn::{self, Harness};

const USAGE: &str = "usage: marshal-shim spawn-team <team.toml> [--timeout <secs>]";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// A team file that starts more sessions than this is more likely a mistake
/// than a team.
const MAX_MEMBERS: usize = 16;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TeamFile {
    team: Option<String>,
    #[serde(default)]
    member: Vec<MemberFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemberFile {
    role: String,
    harness: String,
    cwd: String,
    task: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Member {
    pub role: String,
    pub harness: Harness,
    pub cwd: PathBuf,
    pub task: Option<String>,
    /// `<team>-<role>`.
    pub tmux_name: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Team {
    pub name: String,
    pub members: Vec<Member>,
}

fn expand(cwd: &str, base: &Path, home: Option<&Path>) -> Result<PathBuf> {
    let path = if cwd == "~" {
        home.context("cwd uses ~ but HOME isn't set")?.to_path_buf()
    } else if let Some(rest) = cwd.strip_prefix("~/") {
        home.context("cwd uses ~ but HOME isn't set")?.join(rest)
    } else {
        base.join(cwd)
    };
    Ok(path)
}

/// Read and check a team file. Nothing about the machine is checked here
/// except that each directory exists, so the rules can be tested.
pub(crate) fn parse_team(text: &str, file: &Path, home: Option<&Path>) -> Result<Team> {
    let parsed: TeamFile = toml::from_str(text)
        .with_context(|| format!("{} isn't a valid team file", file.display()))?;
    let name = match parsed.team {
        Some(n) => n,
        None => file
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    ensure!(
        spawn::valid_name(&name),
        "team name {name:?} can't start a tmux name: use letters, digits, - or _"
    );
    ensure!(
        !parsed.member.is_empty(),
        "{} has no [[member]]",
        file.display()
    );
    ensure!(
        parsed.member.len() <= MAX_MEMBERS,
        "{} has {} members; spawn-team starts at most {MAX_MEMBERS}",
        file.display(),
        parsed.member.len()
    );
    let base = file.parent().unwrap_or(Path::new("."));
    let mut roles = HashSet::new();
    let mut members = Vec::new();
    for m in parsed.member {
        let role = m.role;
        ensure!(roles.insert(role.clone()), "role {role:?} appears twice");
        let tmux_name = format!("{name}-{role}");
        ensure!(
            spawn::valid_name(&role) && spawn::valid_name(&tmux_name),
            "role {role:?} can't name a tmux session: use letters, digits, - or _, \
             and keep {name}-<role> to {} characters",
            spawn::MAX_NAME
        );
        let harness = Harness::parse(&m.harness).with_context(|| format!("member {role}"))?;
        let cwd = expand(&m.cwd, base, home)?;
        let cwd = std::fs::canonicalize(&cwd)
            .ok()
            .filter(|p| p.is_dir())
            .with_context(|| format!("member {role}: {} isn't a directory", cwd.display()))?;
        let task = match m.task {
            Some(t) if t.trim().is_empty() => bail!("member {role}: task is empty"),
            t => t,
        };
        members.push(Member {
            role,
            harness,
            cwd,
            task,
            tmux_name,
        });
    }
    Ok(Team { name, members })
}

/// The first task as sent, with the role in front.
pub(crate) fn task_message(team: &str, role: &str, task: &str) -> String {
    format!("You are the {role} member of team {team}. {task}")
}

fn parse_args(args: &[String]) -> Result<(PathBuf, Duration)> {
    let mut it = args.iter();
    let file = PathBuf::from(it.next().context(USAGE)?);
    let mut timeout = DEFAULT_TIMEOUT;
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--timeout" => {
                let secs: u64 = it
                    .next()
                    .with_context(|| format!("--timeout needs a value\n{USAGE}"))?
                    .parse()
                    .with_context(|| format!("--timeout takes whole seconds\n{USAGE}"))?;
                ensure!(secs > 0, "--timeout must be at least 1 second");
                timeout = Duration::from_secs(secs);
            }
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    Ok((file, timeout))
}

/// How one member's launch went.
#[derive(Debug)]
enum Outcome {
    Joined {
        session_id: String,
        nickname: String,
    },
    NotStarted(String),
    Exited,
    TimedOut,
}

pub(crate) fn run(rest: &[String]) -> Result<()> {
    let (file, timeout) = parse_args(rest)?;
    let text =
        std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    let file = std::fs::canonicalize(&file).unwrap_or(file);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let team = parse_team(&text, &file, home.as_deref())?;
    for m in &team.members {
        ensure!(
            !spawn::tmux_has_session(&m.tmux_name)?,
            "a tmux session named {} already exists, so no one was started: \
             attach with `tmux attach -t ={}`, or stop it first",
            m.tmux_name,
            m.tmux_name
        );
    }
    let shim = std::env::current_exe().context("finding marshal-shim's own path")?;

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?
        .block_on(start_team(&team, &shim, timeout))
}

async fn start_team(team: &Team, shim: &Path, timeout: Duration) -> Result<()> {
    marshal_entities::link();
    let client = MykoClient::new();
    let sessions = client.watch_query::<GetAllSessions>(GetAllSessions {});
    let nicknames = client.watch_query::<GetAllSessionNicknames>(GetAllSessionNicknames {});
    let address = crate::daemon_address();
    client.set_address(Some(address.clone()));
    let started = Instant::now();
    while !matches!(
        client.connection_status_sync(),
        ConnectionStatus::Connected(_)
    ) {
        if started.elapsed() > spawn::CONNECT_TIMEOUT {
            bail!(
                "couldn't reach the Marshal daemon at {address} within {:?}, so no one was started",
                spawn::CONNECT_TIMEOUT
            );
        }
        tokio::time::sleep(spawn::POLL).await;
    }

    // Start everyone, then wait for them together.
    let mut tokens = Vec::new();
    let mut outcomes: Vec<Option<Outcome>> = Vec::new();
    for m in &team.members {
        let token = spawn::new_token();
        let command = spawn::harness_command(m.harness, shim);
        let out = spawn::tmux()
            .args(spawn::tmux_new_session_args(
                &m.tmux_name,
                &m.cwd,
                &spawn::harness_env(|k| std::env::var(k).ok(), &token),
                &command,
            ))
            .output();
        let outcome = match out {
            Ok(o) if o.status.success() => {
                eprintln!(
                    "{}: started `{}` in tmux session {} in {}",
                    m.role,
                    command.join(" "),
                    m.tmux_name,
                    m.cwd.display()
                );
                None
            }
            Ok(o) => Some(Outcome::NotStarted(
                String::from_utf8_lossy(&o.stderr).trim().to_string(),
            )),
            Err(e) => Some(Outcome::NotStarted(format!("running tmux: {e}"))),
        };
        tokens.push(token);
        outcomes.push(outcome);
    }
    eprintln!(
        "waiting up to {}s for {} member(s) to join",
        timeout.as_secs(),
        outcomes.iter().filter(|o| o.is_none()).count()
    );

    let launched = Instant::now();
    while outcomes.iter().any(Option::is_none) {
        let rows = sessions.get();
        let names = nicknames.get();
        for (i, m) in team.members.iter().enumerate() {
            if outcomes[i].is_some() {
                continue;
            }
            if let Some(s) = spawn::joined(&rows, &tokens[i]) {
                outcomes[i] = Some(Outcome::Joined {
                    session_id: s.id.0.to_string(),
                    nickname: spawn::nickname_of(&names, s),
                });
            } else if !spawn::tmux_has_session(&m.tmux_name)? {
                outcomes[i] = Some(Outcome::Exited);
            } else if launched.elapsed() > timeout {
                outcomes[i] = Some(Outcome::TimedOut);
            }
        }
        if outcomes.iter().any(Option::is_none) {
            tokio::time::sleep(spawn::POLL).await;
        }
    }

    // Send tasks only to those who joined, and report everyone.
    let mut failed = 0;
    let mut lines = Vec::new();
    for (m, outcome) in team.members.iter().zip(outcomes) {
        let line = match outcome.expect("every member has an outcome") {
            Outcome::Joined {
                session_id,
                nickname,
            } => {
                let task = match &m.task {
                    None => "no task".to_string(),
                    Some(task) => {
                        let body = task_message(&team.name, &m.role, task);
                        match spawn::send_task(&m.tmux_name, &session_id, body).await {
                            Ok(()) => "task sent".to_string(),
                            Err(e) => {
                                failed += 1;
                                format!("TASK NOT SENT: {e:#}")
                            }
                        }
                    }
                };
                format!(
                    "{}: joined as {nickname} ({session_id}) in tmux {}; {task}",
                    m.role, m.tmux_name
                )
            }
            Outcome::NotStarted(why) => {
                failed += 1;
                format!("{}: NOT STARTED: tmux said {why:?}", m.role)
            }
            Outcome::Exited => {
                failed += 1;
                format!(
                    "{}: DIDN'T JOIN: the harness exited, and tmux session {} is gone",
                    m.role, m.tmux_name
                )
            }
            Outcome::TimedOut => {
                failed += 1;
                format!(
                    "{}: DIDN'T JOIN within {}s: tmux session {} is left running \
                     (a folder Claude doesn't trust yet waits at its trust prompt); \
                     `tmux attach -t ={}` or `tmux kill-session -t ={}`",
                    m.role,
                    timeout.as_secs(),
                    m.tmux_name,
                    m.tmux_name,
                    m.tmux_name
                )
            }
        };
        lines.push(line);
    }
    println!("--- team {}", team.name);
    for line in &lines {
        println!("{line}");
    }
    let n = team.members.len();
    println!("--- {} of {n} joined and set up, {failed} not", n - failed);
    if failed > 0 {
        bail!(
            "{failed} of {n} members of team {} didn't join or didn't get their task",
            team.name
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for sub in ["home/src/app", "team/api"] {
            std::fs::create_dir_all(d.path().join(sub)).unwrap();
        }
        d
    }

    fn team(text: &str, d: &tempfile::TempDir) -> Result<Team> {
        parse_team(
            text,
            &d.path().join("team/app-team.toml"),
            Some(&d.path().join("home")),
        )
    }

    #[test]
    fn reads_members_and_resolves_their_directories() {
        let d = dirs();
        let t = team(
            r#"
            team = "app"
            [[member]]
            role = "backend"
            harness = "claude"
            cwd = "~/src/app"
            task = "take the next story"
            [[member]]
            role = "reviewer"
            harness = "codex"
            cwd = "api"
            "#,
            &d,
        )
        .unwrap();
        assert_eq!(t.name, "app");
        let canon = |p: &str| std::fs::canonicalize(d.path().join(p)).unwrap();
        assert_eq!(t.members[0].cwd, canon("home/src/app"));
        assert_eq!(t.members[0].tmux_name, "app-backend");
        assert_eq!(t.members[0].task.as_deref(), Some("take the next story"));
        assert_eq!(t.members[1].harness, Harness::Codex);
        assert_eq!(t.members[1].cwd, canon("team/api"));
        assert_eq!(t.members[1].task, None);
    }

    #[test]
    fn the_team_name_defaults_to_the_files() {
        let d = dirs();
        let t = team(
            "[[member]]\nrole = \"a\"\nharness = \"claude\"\ncwd = \"api\"\n",
            &d,
        )
        .unwrap();
        assert_eq!(t.name, "app-team");
        assert_eq!(t.members[0].tmux_name, "app-team-a");
    }

    #[test]
    fn a_bad_member_refuses_the_whole_team() {
        let d = dirs();
        let one = |m: &str| team(&format!("team = \"t\"\n[[member]]\n{m}\n"), &d);
        let ok = "role = \"a\"\nharness = \"claude\"\ncwd = \"api\"";
        assert!(one(ok).is_ok());
        assert!(one("role = \"a\"\nharness = \"bash\"\ncwd = \"api\"").is_err());
        assert!(one("role = \"a\"\nharness = \"claude\"\ncwd = \"missing\"").is_err());
        assert!(one("role = \"a b\"\nharness = \"claude\"\ncwd = \"api\"").is_err());
        assert!(one("role = \"a\"\nharness = \"claude\"\ncwd = \"api\"\ntask = \" \"").is_err());
        // Unknown keys, such as a try at passing flags, are refused.
        assert!(
            one("role = \"a\"\nharness = \"claude\"\ncwd = \"api\"\nargs = [\"--dangerously-skip-permissions\"]")
                .is_err()
        );
        assert!(
            team(
                &format!("team = \"t\"\n[[member]]\n{ok}\n[[member]]\n{ok}\n"),
                &d
            )
            .is_err()
        );
        assert!(team("team = \"t\"\n", &d).is_err());
        assert!(team(&format!("team = \"a.b\"\n[[member]]\n{ok}\n"), &d).is_err());
        let many: String = (0..=MAX_MEMBERS)
            .map(|i| format!("[[member]]\nrole = \"r{i}\"\nharness = \"claude\"\ncwd = \"api\"\n"))
            .collect();
        assert!(team(&many, &d).is_err());
    }

    #[test]
    fn the_task_says_the_role_and_team_first() {
        assert_eq!(
            task_message("app", "backend", "Take MAR-40."),
            "You are the backend member of team app. Take MAR-40."
        );
    }
}

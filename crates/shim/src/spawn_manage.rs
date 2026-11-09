//! `marshal-shim spawn list | attach <name> | stop <name>`: the sessions spawn
//! started (MAR-30).
//!
//! A tmux session counts as spawned when its environment carries a spawn token,
//! `MARSHAL_AGENT_ID=spawn-<uuid>`, which `spawn` and `spawn-team` set with
//! `new-session -e`. Only those are listed, attached to or stopped, so `stop`
//! can't end any other tmux session or any agent spawn didn't start. Josh chose
//! (2026-11-06) that anyone, him or an agent, may stop a spawned session.
//!
//! `stop` kills the tmux session, then ends the session at the daemon the way a
//! SessionEnd hook does, because Claude's plugin has none: its roster row goes,
//! its room memberships with it, and its waits end. It then checks that the row
//! has gone and stayed gone.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use hyphae::Gettable;
use marshal_entities::{
    GetAllSessionNicknames, GetAllSessions, GetAllWakeWatchers, Session, SessionNickname,
    WakeWatcher, wake_state,
};
use myko::client::{ConnectionStatus, MykoClient};
use std::sync::Arc;

use crate::spawn;

const TOKEN_PREFIX: &str = "spawn-";
/// How long a stopped session's row has to stay gone to count as cleared.
const STAYS_GONE: Duration = Duration::from_secs(3);
const CLEAR_TIMEOUT: Duration = Duration::from_secs(15);
/// How long to let the roster arrive once connected. It may really be empty.
const ROSTER_SETTLE: Duration = Duration::from_secs(2);

/// A tmux session spawn started: its name, directory and token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Launched {
    pub name: String,
    pub path: String,
    pub token: String,
}

/// The token in `tmux show-environment` output for one variable, if it's a
/// spawn token. tmux prints `NAME=value`, or `-NAME` when it's unset.
pub(crate) fn token_from_env_line(line: &str) -> Option<String> {
    let value = line.trim().strip_prefix("MARSHAL_AGENT_ID=")?;
    (value.starts_with(TOKEN_PREFIX) && value.len() > TOKEN_PREFIX.len()).then(|| value.to_string())
}

/// Every tmux session spawn started. No tmux server running means none.
fn launched() -> Result<Vec<Launched>> {
    let out = spawn::tmux()
        .args(["list-sessions", "-F", "#{session_name}\t#{session_path}"])
        .output()
        .context("running tmux: is it installed?")?;
    if !out.status.success() {
        // tmux says "no server running" when there are no sessions at all.
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some((name, path)) = line.split_once('\t') else {
            continue;
        };
        if let Some(token) = session_token(name)? {
            found.push(Launched {
                name: name.to_string(),
                path: path.to_string(),
                token,
            });
        }
    }
    Ok(found)
}

fn session_token(name: &str) -> Result<Option<String>> {
    let out = spawn::tmux()
        .args([
            "show-environment",
            "-t",
            &format!("={name}"),
            "MARSHAL_AGENT_ID",
        ])
        .output()
        .context("running tmux: is it installed?")?;
    Ok(out
        .status
        .success()
        .then(|| token_from_env_line(&String::from_utf8_lossy(&out.stdout)))
        .flatten())
}

/// The spawned session named `name`, or why it can't be acted on.
fn find(name: &str) -> Result<Launched> {
    ensure!(
        spawn::valid_name(name),
        "{name:?} isn't a tmux session name spawn uses"
    );
    if let Some(l) = launched()?.into_iter().find(|l| l.name == name) {
        return Ok(l);
    }
    if spawn::tmux_has_session(name)? {
        bail!("tmux session {name} wasn't started by spawn, so it's left alone");
    }
    bail!(
        "there's no tmux session named {name}; `marshal-shim spawn list` shows the ones spawn started"
    )
}

pub(crate) fn run(sub: &str, rest: &[String]) -> Result<()> {
    let rt = || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("building tokio runtime")
    };
    match (sub, rest) {
        ("list", []) => rt()?.block_on(list()),
        ("attach", [name]) => attach(&find(name)?),
        ("stop", [name]) => {
            let target = find(name)?;
            rt()?.block_on(stop(&target))
        }
        _ => bail!("usage: marshal-shim spawn list | attach <name> | stop <name>"),
    }
}

/// Connect and let the roster arrive, or say the daemon couldn't be reached.
async fn roster() -> Result<(
    MykoClient,
    impl Gettable<Vec<Arc<Session>>>,
    impl Gettable<Vec<Arc<SessionNickname>>>,
    impl Gettable<Vec<Arc<WakeWatcher>>>,
)> {
    marshal_entities::link();
    let client = MykoClient::new();
    let sessions = client.watch_query::<GetAllSessions>(GetAllSessions {});
    let nicknames = client.watch_query::<GetAllSessionNicknames>(GetAllSessionNicknames {});
    let watchers = client.watch_query::<GetAllWakeWatchers>(GetAllWakeWatchers {});
    let address = crate::daemon_address();
    crate::tls_link::connect(&client, address.clone());
    let started = Instant::now();
    while !matches!(
        client.connection_status_sync(),
        ConnectionStatus::Connected(_)
    ) {
        if started.elapsed() > spawn::CONNECT_TIMEOUT {
            bail!(
                "couldn't reach the Marshal daemon at {address} within {:?}",
                spawn::CONNECT_TIMEOUT
            );
        }
        tokio::time::sleep(spawn::POLL).await;
    }
    let connected = Instant::now();
    while sessions.get().is_empty() && connected.elapsed() < ROSTER_SETTLE {
        tokio::time::sleep(spawn::POLL).await;
    }
    Ok((client, sessions, nicknames, watchers))
}

async fn list() -> Result<()> {
    let found = launched()?;
    if found.is_empty() {
        println!("no tmux sessions started by spawn");
        return Ok(());
    }
    // The tmux side is known without the daemon, so list it even if the
    // daemon can't be reached, and say so.
    let joined = match roster().await {
        Ok((_client, sessions, nicknames, watchers)) => {
            // Wake watchers may be none at all, so give them a moment.
            let settle = Instant::now();
            while watchers.get().is_empty() && settle.elapsed() < ROSTER_SETTLE {
                tokio::time::sleep(spawn::POLL).await;
            }
            Some((sessions.get(), nicknames.get(), watchers.get()))
        }
        Err(e) => {
            eprintln!("{e:#}; listing tmux only");
            None
        }
    };
    let now = chrono::Utc::now().timestamp_millis();
    for l in &found {
        let on_roster = match &joined {
            None => "roster unknown".to_string(),
            Some((rows, names, watchers)) => match spawn::joined(rows, &l.token) {
                Some(s) => format!(
                    "{} ({}), wake {}",
                    spawn::nickname_of(names, s),
                    s.id.0,
                    wake_state(s, watchers, now).as_str()
                ),
                None => "not on the roster".to_string(),
            },
        };
        println!("{}\t{on_roster}\t{}", l.name, l.path);
    }
    Ok(())
}

fn attach(target: &Launched) -> Result<()> {
    let to = format!("={}", target.name);
    // Inside tmux, attaching would nest; switch this client instead.
    let args: Vec<&str> = if std::env::var_os("TMUX").is_some() {
        vec!["switch-client", "-t", &to]
    } else {
        vec!["attach-session", "-t", &to]
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = spawn::tmux().args(&args).exec();
        Err(err).context("running tmux")
    }
    #[cfg(not(unix))]
    {
        let status = spawn::tmux().args(&args).status().context("running tmux")?;
        ensure!(status.success(), "tmux couldn't attach to {}", target.name);
        Ok(())
    }
}

async fn stop(target: &Launched) -> Result<()> {
    let (client, sessions, nicknames, _watchers) = roster().await?;
    let row = spawn::joined(&sessions.get(), &target.token).cloned();

    let out = spawn::tmux()
        .args(["kill-session", "-t", &format!("={}", target.name)])
        .output()
        .context("running tmux")?;
    ensure!(
        out.status.success(),
        "tmux couldn't stop {}: {}",
        target.name,
        String::from_utf8_lossy(&out.stderr).trim()
    );

    let Some(row) = row else {
        println!(
            "stopped tmux session {}; it had no row on the roster, so there was nothing to clear",
            target.name
        );
        return Ok(());
    };
    let id = row.id.0.to_string();
    let nickname = spawn::nickname_of(&nicknames.get(), &row);
    let base = crate::codex_hook::registration_base(&crate::daemon_address());
    ensure!(
        crate::codex_hook::end_session(&base, &id),
        "stopped tmux session {}, but the daemon's hook listener at {base} didn't answer, so \
         {nickname} ({id}) is still on the roster until the daemon's sweep reaps it",
        target.name
    );

    // Gone, and still gone a moment later: a hook from the dying harness
    // mustn't have put the row back.
    let started = Instant::now();
    let mut gone_since: Option<Instant> = None;
    loop {
        let present = sessions.get().iter().any(|s| s.id.0.as_ref() == id);
        match (present, gone_since) {
            (false, None) => gone_since = Some(Instant::now()),
            (false, Some(t)) if t.elapsed() >= STAYS_GONE => break,
            (true, Some(_)) => gone_since = None,
            _ => {}
        }
        if started.elapsed() > CLEAR_TIMEOUT {
            bail!(
                "stopped tmux session {}, but {nickname} ({id}) is still on the roster after {}s",
                target.name,
                CLEAR_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(spawn::POLL).await;
    }
    drop(client);
    println!(
        "stopped {nickname} ({id}): tmux session {} is gone, and its roster row and rooms are cleared",
        target.name
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_spawn_token_marks_a_session_as_spawned() {
        assert_eq!(
            token_from_env_line("MARSHAL_AGENT_ID=spawn-abc\n").as_deref(),
            Some("spawn-abc")
        );
        assert_eq!(token_from_env_line("MARSHAL_AGENT_ID=spawn-"), None);
        assert_eq!(token_from_env_line("MARSHAL_AGENT_ID=my-embedder"), None);
        assert_eq!(token_from_env_line("-MARSHAL_AGENT_ID"), None);
        assert_eq!(token_from_env_line(""), None);
    }
}

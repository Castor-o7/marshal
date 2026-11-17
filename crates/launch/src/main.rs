//! `marshal-launch <adapter> <harness>`: start an agent and wait for it to join.
//! See docs/agent-launcher.md.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use marshal_launch::{Adapter, ExternalAdapter, Plan, address, record, tmux};

const USAGE: &str =
    "usage: marshal-launch <adapter> <claude|codex> [--cwd <dir>] [--name <label>] \
[--task <text>] [--daemon <url>] [--timeout <secs>] [--no-check] [-- <adapter options>]
       marshal-launch list
       marshal-launch stop <name|agent_id>";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, PartialEq, Eq)]
struct Args {
    adapter: String,
    harness: String,
    cwd: Option<String>,
    name: Option<String>,
    daemon: Option<String>,
    timeout: Duration,
    check: bool,
    options: Vec<String>,
    task: Option<String>,
}

fn parse_args(args: &[String]) -> Result<Args> {
    let mut it = args.iter();
    let adapter = it.next().context(USAGE)?.clone();
    let harness = it.next().context(USAGE)?.clone();
    marshal_launch::harness_argv(&harness).context(USAGE)?;
    let mut a = Args {
        adapter,
        harness,
        cwd: None,
        name: None,
        daemon: None,
        timeout: DEFAULT_TIMEOUT,
        check: true,
        options: vec![],
        task: None,
    };
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .with_context(|| format!("{flag} needs a value\n{USAGE}"))
        };
        match flag.as_str() {
            "--cwd" => a.cwd = Some(value()?),
            "--name" => a.name = Some(value()?),
            "--daemon" => a.daemon = Some(value()?),
            "--timeout" => {
                let secs: u64 = value()?
                    .parse()
                    .with_context(|| format!("--timeout takes whole seconds\n{USAGE}"))?;
                ensure!(secs > 0, "--timeout must be at least 1 second");
                a.timeout = Duration::from_secs(secs);
            }
            "--no-check" => a.check = false,
            "--task" => {
                let task = value()?;
                ensure!(!task.trim().is_empty(), "--task needs some text\n{USAGE}");
                a.task = Some(task);
            }
            // Everything after `--` is the adapter's, never the harness's.
            "--" => {
                a.options = it.by_ref().cloned().collect();
            }
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    Ok(a)
}

/// The built-in tmux adapter, or `marshal-launch-<name>` found on `PATH`.
fn adapter_for(name: &str) -> Result<Box<dyn Adapter>> {
    match name {
        address::LOCAL_ADAPTER => Ok(Box::new(tmux::Tmux::default())),
        other => Ok(Box::new(ExternalAdapter::find(other)?)),
    }
}

fn run(args: Args) -> Result<()> {
    let adapter = adapter_for(&args.adapter)?;
    let local = adapter.name() == address::LOCAL_ADAPTER;
    let cwd = match args.cwd {
        Some(c) => c,
        None if local => std::env::current_dir()
            .context("reading the current directory")?
            .display()
            .to_string(),
        None => bail!("--cwd is needed for the {} adapter", args.adapter),
    };
    let name = match args.name {
        Some(n) => n,
        None => tmux::default_name(&args.harness, &PathBuf::from(&cwd)),
    };
    let plan = Plan {
        harness: args.harness,
        cwd,
        name,
        launch_address: address::from_environment(args.daemon),
        daemon_address: address::from_environment(None),
        timeout: args.timeout,
        check: args.check,
        options: args.options,
        task: args.task,
        records: Some(record::default_path()?),
    };
    let outcome = runtime()?.block_on(marshal_launch::launch(adapter.as_ref(), &plan))?;
    let attach = outcome
        .launched
        .attach
        .as_deref()
        .map(|a| format!(". Attach: {a}"))
        .unwrap_or_default();
    println!(
        "{} ({}) joined from {}{attach}",
        outcome.joined.nickname, outcome.joined.session_id, outcome.launched.r#where
    );
    if let Some(id) = outcome.task_message_id {
        println!("first task sent as message {id}");
    }
    Ok(())
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")
}

/// What `list` says of one launch's process, from its adapter.
fn process_state(l: &record::Launch) -> String {
    match adapter_for(&l.adapter).and_then(|a| a.status(&l.handle)) {
        Ok(Some(true)) => "running".into(),
        Ok(Some(false)) => "exited".into(),
        Ok(None) => "unknown".into(),
        Err(e) => format!("unknown ({e})"),
    }
}

fn list() -> Result<()> {
    let store = record::Store::open_default()?;
    let launches = store.list()?;
    if launches.is_empty() {
        println!("no launches recorded in {}", store.path().display());
        return Ok(());
    }
    let address = address::from_environment(None);
    let rt = runtime()?;
    let roster = rt.block_on(marshal_launch::join::Roster::connect(&address));
    if let Err(e) = &roster {
        eprintln!("roster state unknown: {e:#}");
    }
    for l in &launches {
        let on_roster = match &roster {
            Ok(r) => {
                let sessions = r.sessions();
                let launched = marshal_launch::Launched {
                    handle: l.handle.clone(),
                    host: l.host.clone(),
                    cwd: l.cwd.clone(),
                    r#where: l.r#where.clone(),
                    attach: None,
                };
                match marshal_launch::join::find(&sessions, &l.agent_id, &launched) {
                    marshal_launch::join::Join::Joined(s) => format!("on the roster as {}", s.id.0),
                    _ if l.session_id.is_some() => "joined, now off the roster".into(),
                    _ => "not on the roster".into(),
                }
            }
            Err(_) => "roster unknown".into(),
        };
        println!(
            "{}\t{}\t{} {}\t{}\t{}\t{}",
            l.name,
            l.agent_id,
            l.adapter,
            l.harness,
            l.r#where,
            process_state(l),
            on_roster
        );
    }
    Ok(())
}

fn stop(key: &str) -> Result<()> {
    let store = record::Store::open_default()?;
    let launches = store.list()?;
    let l = record::find(&launches, key)?;
    adapter_for(&l.adapter)?
        .stop(&l.handle)
        .with_context(|| format!("stopping {} at {}", l.name, l.r#where))?;
    store.remove(&l.agent_id)?;
    println!(
        "stopped {} ({}) at {}; its roster row clears when the harness exits",
        l.name, l.agent_id, l.r#where
    );
    Ok(())
}

fn dispatch(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("list") => {
            ensure!(args.len() == 1, "list takes no arguments\n{USAGE}");
            list()
        }
        Some("stop") => match &args[1..] {
            [key] => stop(key),
            _ => bail!("stop takes one name or agent id\n{USAGE}"),
        },
        _ => parse_args(args).and_then(run),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = dispatch(&args) {
        eprintln!("marshal-launch: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_an_adapter_a_harness_and_its_options() {
        let a = parse_args(&v(&[
            "ssh",
            "codex",
            "--cwd",
            "~/w",
            "--name",
            "dev",
            "--daemon",
            "ws://m:6155",
            "--timeout",
            "5",
            "--no-check",
            "--",
            "--host",
            "build-03",
        ]))
        .unwrap();
        assert_eq!(a.adapter, "ssh");
        assert_eq!(a.harness, "codex");
        assert_eq!(a.cwd.as_deref(), Some("~/w"));
        assert_eq!(a.name.as_deref(), Some("dev"));
        assert_eq!(a.daemon.as_deref(), Some("ws://m:6155"));
        assert_eq!(a.timeout, Duration::from_secs(5));
        assert!(!a.check);
        assert_eq!(a.options, ["--host", "build-03"]);
        let a = parse_args(&v(&["tmux", "claude", "--task", "say ready"])).unwrap();
        assert_eq!(a.task.as_deref(), Some("say ready"));
        assert!(parse_args(&v(&["tmux", "claude", "--task", " "])).is_err());
        let a = parse_args(&v(&["tmux", "claude"])).unwrap();
        assert_eq!((a.timeout, a.check), (DEFAULT_TIMEOUT, true));
    }

    #[test]
    fn refuses_anything_it_doesnt_know_including_harness_flags() {
        assert!(parse_args(&v(&[])).is_err());
        assert!(parse_args(&v(&["tmux"])).is_err());
        assert!(parse_args(&v(&["tmux", "bash"])).is_err());
        assert!(parse_args(&v(&["tmux", "claude", "--cwd"])).is_err());
        assert!(parse_args(&v(&["tmux", "claude", "--timeout", "0"])).is_err());
        assert!(parse_args(&v(&["tmux", "claude", "--dangerously-skip-permissions"])).is_err());
        let missing = adapter_for("no-such-adapter-here")
            .err()
            .unwrap()
            .to_string();
        assert!(
            missing.contains("marshal-launch-no-such-adapter-here"),
            "{missing}"
        );
        assert!(adapter_for("../evil").is_err());
        assert!(dispatch(&v(&["list", "x"])).is_err());
        assert!(dispatch(&v(&["stop"])).is_err());
    }
}

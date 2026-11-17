//! `marshal-launch <adapter> <harness>`: start an agent and wait for it to join.
//! See docs/agent-launcher.md.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use marshal_launch::{Adapter, Plan, address, tmux};

const USAGE: &str = "usage: marshal-launch <adapter> <claude|codex> [--cwd <dir>] [--name <label>] \
[--daemon <url>] [--timeout <secs>] [--no-check] [-- <adapter options>]";
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
            // Everything after `--` is the adapter's, never the harness's.
            "--" => {
                a.options = it.by_ref().cloned().collect();
            }
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    Ok(a)
}

fn adapter_for(name: &str) -> Result<Box<dyn Adapter>> {
    match name {
        address::LOCAL_ADAPTER => Ok(Box::new(tmux::Tmux::default())),
        other => bail!("no adapter named {other:?}: this build has the built-in tmux adapter"),
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
    };
    let outcome = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?
        .block_on(marshal_launch::launch(adapter.as_ref(), &plan))?;
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
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = parse_args(&args).and_then(run) {
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
        assert!(adapter_for("ssh").is_err());
    }
}

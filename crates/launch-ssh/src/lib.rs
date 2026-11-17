//! `marshal-launch-ssh`: the reference external adapter for `marshal-launch`.
//!
//! It starts an agent in a detached tmux session on another machine, over ssh.
//! It speaks adapter contract v1 (see `docs/agent-launcher.md`, "The adapter
//! contract"): `marshal-launch-ssh <verb>` reads one JSON object on stdin and
//! writes one on stdout. Anything on stderr is shown to the user.
//!
//! Every verb runs one `bash -l -s` on the target, with a script this module
//! writes on its stdin. Values reach the script single-quoted, never through
//! the ssh command line, so nothing in a request is parsed by a remote shell.
//! The script reports back in `@@<key> <value>` lines, which [`Probe::parse`]
//! reads; anything else on stdout (a chatty login profile) is ignored.
//!
//! Options (after `--` on the `marshal-launch` command line):
//! `--host <ssh destination>`, required.
//!
//! `MARSHAL_LAUNCH_SSH` replaces the `ssh` program (split on whitespace), for
//! a site wrapper or for tests.

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use marshal_launch::{LaunchRequest, Launched, PROTOCOL_VERSION};
use serde::Deserialize;
use serde_json::{Value, json};

/// The daemon's websocket port, when the launch address names none.
pub const DAEMON_PORT: u16 = 6155;
/// The daemon's hook port. The shim derives it from the launch address the
/// same way (`ws://host:6155` → `http://host:6156`).
pub const HOOK_PORT: u16 = 6156;
/// This adapter's version. The workspace releases the shim, the daemon and the
/// launcher at one version, so a target's shim is accepted when it matches
/// this one's release line (see [`shim_compatible`]).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Begin/end markers `marshal-shim codex-setup` writes around its block in
/// Codex's `config.toml`.
const CODEX_BEGIN: &str = "# >>> marshal (managed by marshal-shim codex-setup) >>>";
const CODEX_END: &str = "# <<< marshal (managed by marshal-shim codex-setup) <<<";

/// `stop` and `status` requests.
#[derive(Debug, Deserialize)]
pub struct HandleRequest {
    pub version: u32,
    pub handle: String,
}

/// Run one verb: `input` is the JSON read from stdin, the result is the JSON
/// to write to stdout.
pub fn run(verb: &str, input: &str) -> Result<Value> {
    match verb {
        "check" => {
            let req = parse_launch_request(input)?;
            let t = Target::from_request(&req)?;
            check(&t, &req)?;
            Ok(json!({ "version": PROTOCOL_VERSION }))
        }
        "launch" => {
            let req = parse_launch_request(input)?;
            let t = Target::from_request(&req)?;
            let l = launch(&t, &req)?;
            let mut v = serde_json::to_value(&l)?;
            v["version"] = json!(PROTOCOL_VERSION);
            Ok(v)
        }
        "stop" | "status" => {
            let req: HandleRequest =
                serde_json::from_str(input).context("reading the request on stdin")?;
            check_version(req.version)?;
            let (dest, name) = parse_handle(&req.handle)?;
            if verb == "stop" {
                stop(&dest, &name)?;
                Ok(json!({ "version": PROTOCOL_VERSION }))
            } else {
                let running = status(&dest, &name)?;
                Ok(json!({ "version": PROTOCOL_VERSION, "running": running }))
            }
        }
        other => bail!("unknown verb `{other}` (expected check, launch, stop or status)"),
    }
}

fn check_version(v: u32) -> Result<()> {
    if v != PROTOCOL_VERSION {
        bail!("request is contract version {v}; this adapter speaks version {PROTOCOL_VERSION}");
    }
    Ok(())
}

pub fn parse_launch_request(input: &str) -> Result<LaunchRequest> {
    let req: LaunchRequest = serde_json::from_str(input).context("reading the request on stdin")?;
    check_version(req.version)?;
    Ok(req)
}

/// Where a request goes, and the addresses it must reach from there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The ssh destination (`host` or `user@host`).
    pub dest: String,
    /// The tmux session name.
    pub name: String,
    pub daemon: HostPort,
    pub hook: HostPort,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPort {
    pub host: String,
    pub port: u16,
}

impl std::fmt::Display for HostPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

impl Target {
    /// Validate a request and work out where it goes. Refuses a loopback
    /// launch address: the agent runs on another machine, where `localhost`
    /// is not the daemon.
    pub fn from_request(req: &LaunchRequest) -> Result<Target> {
        let dest = parse_options(&req.options)?;
        validate_name(&req.name)?;
        if req.argv.is_empty() {
            bail!("the request's argv is empty");
        }
        for k in req.env.keys() {
            validate_env_key(k)?;
        }
        let addr = req.env.get("MARSHAL_DAEMON_ADDRESS").ok_or_else(|| {
            anyhow!("the request's env has no MARSHAL_DAEMON_ADDRESS (the launch address)")
        })?;
        let daemon = parse_address(addr, &["ws://", "wss://"], DAEMON_PORT)
            .ok_or_else(|| anyhow!("can't read the launch address `{addr}`"))?;
        if is_loopback(&daemon.host) {
            bail!(
                "the launch address {addr} is a loopback address, which on {dest} would not be \
                 this daemon; launch with an address the target can reach (a mesh name or IP)"
            );
        }
        // The shim lets MARSHAL_BASE_URL override the hook base; otherwise it
        // is the launch address's host on the hook port.
        let hook = match req.env.get("MARSHAL_BASE_URL").filter(|s| !s.is_empty()) {
            Some(base) => {
                let hp = parse_address(base, &["http://", "https://"], HOOK_PORT)
                    .ok_or_else(|| anyhow!("can't read MARSHAL_BASE_URL `{base}`"))?;
                if is_loopback(&hp.host) {
                    bail!("MARSHAL_BASE_URL {base} is a loopback address; {dest} can't reach it");
                }
                hp
            }
            None => HostPort {
                host: daemon.host.clone(),
                port: HOOK_PORT,
            },
        };
        Ok(Target {
            dest,
            name: req.name.clone(),
            daemon,
            hook,
        })
    }
}

/// Read `--host <dest>` (or `--host=<dest>`) from the adapter options.
pub fn parse_options(options: &[String]) -> Result<String> {
    let mut host = None;
    let mut it = options.iter();
    while let Some(o) = it.next() {
        if o == "--host" {
            host = Some(
                it.next()
                    .ok_or_else(|| anyhow!("--host needs a value"))?
                    .clone(),
            );
        } else if let Some(h) = o.strip_prefix("--host=") {
            host = Some(h.to_string());
        } else {
            bail!("unknown option `{o}` (the ssh adapter takes --host <destination>)");
        }
    }
    let host = host.ok_or_else(|| anyhow!("the ssh adapter needs --host <destination>"))?;
    validate_dest(&host)?;
    Ok(host)
}

/// An ssh destination must not be read as an ssh option, and must be one word.
pub fn validate_dest(d: &str) -> Result<()> {
    if d.is_empty() || d.starts_with('-') || d.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        bail!("`{d}` is not an ssh destination");
    }
    Ok(())
}

/// tmux session names: letters, digits, `-` and `_` (tmux rewrites `.` and
/// `:`, which would make the session hard to find again).
pub fn validate_name(n: &str) -> Result<()> {
    if n.is_empty()
        || n.starts_with('-')
        || !n
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("launch name `{n}` must be letters, digits, `-` and `_`");
    }
    Ok(())
}

fn validate_env_key(k: &str) -> Result<()> {
    let mut cs = k.chars();
    let ok = matches!(cs.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        bail!("`{k}` is not an environment variable name");
    }
    Ok(())
}

/// Parse `scheme://host[:port][/path]`. IPv6 hosts are bracketed.
pub fn parse_address(addr: &str, schemes: &[&str], default_port: u16) -> Option<HostPort> {
    let rest = schemes.iter().find_map(|s| addr.strip_prefix(s))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let (host, port) = if let Some(r) = authority.strip_prefix('[') {
        let end = r.find(']')?;
        let host = &r[..end];
        let port = match &r[end + 1..] {
            "" => default_port,
            p => p.strip_prefix(':')?.parse().ok()?,
        };
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().ok()?),
            None => (authority, default_port),
        }
    };
    if host.is_empty() {
        return None;
    }
    Some(HostPort {
        host: host.to_string(),
        port,
    })
}

/// `localhost`, `*.localhost`, 127/8, `::1`, and the unspecified addresses.
pub fn is_loopback(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".localhost") {
        return true;
    }
    match h.parse::<std::net::IpAddr>() {
        Ok(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || matches!(ip, std::net::IpAddr::V6(v6)
                    if v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback() || v4.is_unspecified()))
        }
        Err(_) => false,
    }
}

/// Shell single-quote.
pub fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'"'"'"#))
}

/// Shared prelude: `say` reports a line back; `cwd` is expanded and resolved
/// the way the shim will see it (`getcwd`, so symlinks resolved).
const PRELUDE: &str = r#"set -u
say() { printf '@@%s\n' "$*"; }
case "$cwd" in "~") cwd=$HOME ;; "~/"*) cwd=$HOME/${cwd#"~/"} ;; esac
"#;

/// The `check` script: reports each probe, starts nothing.
pub fn check_script(t: &Target, req: &LaunchRequest) -> String {
    let mut s = format!(
        "daemon_host={}\ndaemon_port={}\nhook_host={}\nhook_port={}\ncwd={}\nharness={}\nargv0={}\n",
        sq(&t.daemon.host),
        t.daemon.port,
        sq(&t.hook.host),
        t.hook.port,
        sq(&req.cwd),
        sq(&req.harness),
        sq(&req.argv[0]),
    );
    s.push_str(PRELUDE);
    s.push_str(&format!(
        r#"probe() {{
  if command -v timeout >/dev/null 2>&1; then
    timeout 5 bash -c ': </dev/tcp/"$1"/"$2"' 2>/dev/null
  else
    bash -c ': </dev/tcp/"$1"/"$2"' 2>/dev/null
  fi
}}
say "host $(uname -n)"
if probe "$daemon_host" "$daemon_port"; then say "daemon-port ok"; else say "daemon-port fail"; fi
if probe "$hook_host" "$hook_port"; then say "hook-port ok"; else say "hook-port fail"; fi
if cd "$cwd" 2>/dev/null; then say "cwd $(pwd -P)"; else say "cwd-missing $cwd"; fi
if command -v tmux >/dev/null 2>&1; then say "tmux ok"; else say "tmux missing"; fi
if command -v "$argv0" >/dev/null 2>&1; then say "harness ok"; else say "harness missing"; fi
if command -v marshal-shim >/dev/null 2>&1; then
  say "shim $(marshal-shim --version 2>&1 | head -n 1)"
else
  say "shim-missing"
fi
case "$harness" in
claude)
  if command -v claude >/dev/null 2>&1; then
    say "begin claude-plugins"
    claude plugin list --json 2>/dev/null
    say "end"
    claude plugin list --json 2>/dev/null \
      | grep -o '"installPath": *"[^"]*"' | sed 's/^.*"\([^"]*\)"$/\1/' \
      | while IFS= read -r p; do
          case "$p" in */marshal-shim/*|*/marshal-shim)
            say "begin claude-plugin-hooks $p"; cat "$p/hooks/hooks.json" 2>/dev/null; echo; say "end"
            say "begin claude-plugin-mcp $p"; cat "$p/.mcp.json" 2>/dev/null; echo; say "end"
          ;; esac
        done
  fi
  ;;
codex)
  cfg="${{CODEX_HOME:-$HOME/.codex}}/config.toml"
  if [ -f "$cfg" ]; then
    say "begin codex-managed"
    sed -n '\|^{begin}$|,\|^{end}$|p' "$cfg"
    say "end"
  else
    say "codex-config-missing $cfg"
  fi
  ;;
esac
say "done"
"#,
        begin = CODEX_BEGIN,
        end = CODEX_END,
    ));
    s
}

/// What the `check` script reported.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Probe {
    pub host: Option<String>,
    pub daemon_port_ok: Option<bool>,
    pub hook_port_ok: Option<bool>,
    pub cwd: Option<String>,
    pub cwd_missing: Option<String>,
    pub tmux: Option<bool>,
    pub harness: Option<bool>,
    /// `marshal-shim --version` output; `None` if not on PATH.
    pub shim: Option<String>,
    /// Multi-line blocks, keyed by their `begin` line (`claude-plugins`,
    /// `claude-plugin-hooks <path>`, `codex-managed`, ...).
    pub blocks: BTreeMap<String, String>,
    pub codex_config_missing: Option<String>,
    pub done: bool,
}

impl Probe {
    pub fn parse(out: &str) -> Probe {
        let mut p = Probe::default();
        let mut block: Option<(String, String)> = None;
        for line in out.lines() {
            if let Some((key, body)) = &mut block {
                if line == "@@end" {
                    p.blocks.insert(std::mem::take(key), std::mem::take(body));
                    block = None;
                } else {
                    body.push_str(line);
                    body.push('\n');
                }
                continue;
            }
            let Some(l) = line.strip_prefix("@@") else {
                continue;
            };
            let (k, v) = l.split_once(' ').unwrap_or((l, ""));
            match k {
                "host" => p.host = Some(v.to_string()),
                "daemon-port" => p.daemon_port_ok = Some(v == "ok"),
                "hook-port" => p.hook_port_ok = Some(v == "ok"),
                "cwd" => p.cwd = Some(v.to_string()),
                "cwd-missing" => p.cwd_missing = Some(v.to_string()),
                "tmux" => p.tmux = Some(v == "ok"),
                "harness" => p.harness = Some(v == "ok"),
                "shim" => p.shim = Some(v.to_string()),
                "codex-config-missing" => p.codex_config_missing = Some(v.to_string()),
                "begin" => block = Some((v.to_string(), String::new())),
                "done" => p.done = true,
                _ => {}
            }
        }
        p
    }

    /// Every reason this target can't run `req`, in the order a person would
    /// fix them. Empty means it can.
    pub fn failures(&self, t: &Target, req: &LaunchRequest) -> Vec<String> {
        let host = self.host.as_deref().unwrap_or(&t.dest);
        let mut f = Vec::new();
        if !self.done {
            f.push(format!("the check on {host} stopped before it finished"));
            return f;
        }
        if self.daemon_port_ok != Some(true) {
            f.push(format!(
                "{host} can't reach the daemon at {} (the launch address); the agent could not join",
                t.daemon
            ));
        }
        if self.hook_port_ok != Some(true) {
            f.push(format!(
                "{host} can't reach the daemon's hook port at {}; the agent's hooks could not \
                 register it or deliver its mail",
                t.hook
            ));
        }
        if let Some(c) = &self.cwd_missing {
            f.push(format!("{host} has no folder {c}"));
        }
        if self.tmux != Some(true) {
            f.push(format!("{host} has no tmux on PATH"));
        }
        if self.harness != Some(true) {
            f.push(format!(
                "{host} has no `{}` on PATH (in a login shell)",
                req.argv[0]
            ));
        }
        match &self.shim {
            None => f.push(format!(
                "{host} has no marshal-shim on PATH (in a login shell)"
            )),
            Some(v) => match shim_version(v) {
                Some(found) if shim_compatible(found, VERSION) => {}
                Some(found) => f.push(format!(
                    "marshal-shim on {host} is {found}; this launcher accepts {}",
                    accepted_range(VERSION)
                )),
                None => f.push(format!(
                    "marshal-shim --version on {host} printed `{v}`, not a marshal-shim version"
                )),
            },
        }
        match req.harness.as_str() {
            "claude" => f.extend(self.claude_wiring(host, t)),
            "codex" => f.extend(self.codex_wiring(host, t)),
            other => f.push(format!(
                "the ssh adapter can't check the wiring of harness `{other}` (it knows claude and codex)"
            )),
        }
        f
    }

    fn claude_wiring(&self, host: &str, t: &Target) -> Vec<String> {
        let Some(list) = self.blocks.get("claude-plugins") else {
            return vec![format!(
                "{host} has no `claude` on PATH to list its plugins"
            )];
        };
        let plugins: Vec<Value> = serde_json::from_str(list).unwrap_or_default();
        let shim = plugins.iter().find(|p| {
            p["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("marshal-shim@"))
        });
        let Some(shim) = shim else {
            return vec![format!(
                "Claude on {host} has no marshal-shim plugin installed, so neither its hooks nor \
                 its MCP server point at marshal-shim"
            )];
        };
        if shim["enabled"] != Value::Bool(true) {
            return vec![format!(
                "the marshal-shim plugin on {host} is installed but not enabled"
            )];
        }
        let path = shim["installPath"].as_str().unwrap_or("");
        let mut f = Vec::new();
        let hooks = self.blocks.get(&format!("claude-plugin-hooks {path}"));
        if !hooks.is_some_and(|h| h.contains("marshal-shim claude-hook")) {
            f.push(format!(
                "Claude's hooks on {host} don't run `marshal-shim claude-hook` \
                 ({path}/hooks/hooks.json); a launched agent would not get its mail"
            ));
        }
        let mcp = self
            .blocks
            .get(&format!("claude-plugin-mcp {path}"))
            .and_then(|m| serde_json::from_str::<Value>(m).ok());
        match mcp
            .as_ref()
            .and_then(|m| m["mcpServers"].as_object())
            .and_then(|servers| {
                servers
                    .values()
                    .find(|s| s["command"].as_str().is_some_and(is_shim_command))
            }) {
            None => f.push(format!(
                "Claude's MCP server on {host} is not marshal-shim ({path}/.mcp.json)"
            )),
            Some(server) => {
                // The plugin reads the launch address from the environment;
                // a fixed address elsewhere would send the agent to another daemon.
                if let Some(a) = server["env"]["MARSHAL_DAEMON_ADDRESS"].as_str()
                    && !a.contains("${MARSHAL_DAEMON_ADDRESS")
                    && parse_address(a, &["ws://", "wss://"], DAEMON_PORT).as_ref()
                        != Some(&t.daemon)
                {
                    f.push(format!(
                        "Claude's marshal-shim MCP server on {host} is fixed to {a}, not the launch \
                         address {}",
                        t.daemon
                    ));
                }
            }
        }
        f
    }

    fn codex_wiring(&self, host: &str, t: &Target) -> Vec<String> {
        if let Some(cfg) = &self.codex_config_missing {
            return vec![format!(
                "Codex on {host} has no {cfg}; run `marshal-shim codex-setup` there"
            )];
        }
        let block = self
            .blocks
            .get("codex-managed")
            .map(String::as_str)
            .unwrap_or("");
        if !block.contains(CODEX_BEGIN) {
            return vec![format!(
                "Codex on {host} has no marshal block in its config.toml; run \
                 `marshal-shim codex-setup` there"
            )];
        }
        let mut f = Vec::new();
        let mcp_ok = block.contains("[mcp_servers.marshal]")
            && block
                .lines()
                .any(|l| l.trim_start().starts_with("command") && l.contains("marshal-shim"));
        if !mcp_ok {
            f.push(format!(
                "Codex's marshal MCP server on {host} is not marshal-shim"
            ));
        }
        if !block.contains("codex-hook") {
            f.push(format!(
                "Codex's hooks on {host} don't run `marshal-shim codex-hook`"
            ));
        }
        // codex-setup writes the daemon address into the block; it must be
        // this launch's, or the agent joins some other daemon.
        if let Some(a) = toml_string_after(block, "MARSHAL_DAEMON_ADDRESS")
            && parse_address(&a, &["ws://", "wss://"], DAEMON_PORT).as_ref() != Some(&t.daemon)
        {
            f.push(format!(
                "Codex on {host} is set up for the daemon at {a}, not the launch address {}; run \
                 `marshal-shim codex-setup` there with this address",
                t.daemon
            ));
        }
        f
    }
}

/// `"KEY = \"value\""` → `value`, for the first `KEY` in `s`.
fn toml_string_after(s: &str, key: &str) -> Option<String> {
    let i = s.find(key)?;
    let rest = s[i + key.len()..]
        .trim_start()
        .strip_prefix('=')?
        .trim_start();
    let rest = rest.strip_prefix('"')?;
    Some(rest[..rest.find('"')?].to_string())
}

fn is_shim_command(c: &str) -> bool {
    let base = c.rsplit(['/', '\\']).next().unwrap_or(c);
    base == "marshal-shim" || base == "marshal-shim.exe"
}

/// `marshal-shim 0.60.2` → `0.60.2`.
pub fn shim_version(out: &str) -> Option<&str> {
    let v = out.trim().strip_prefix("marshal-shim ")?.trim();
    semver_triple(v).map(|_| v)
}

fn semver_triple(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    let t = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(t)
}

/// Same release line: same major, and for `0.x` the same minor too.
pub fn shim_compatible(found: &str, ours: &str) -> bool {
    match (semver_triple(found), semver_triple(ours)) {
        (Some((fm, fn_, _)), Some((om, on, _))) => fm == om && (om != 0 || fn_ == on),
        _ => false,
    }
}

fn accepted_range(ours: &str) -> String {
    match semver_triple(ours) {
        Some((0, m, _)) => format!("0.{m}.x"),
        Some((m, _, _)) => format!("{m}.x"),
        None => ours.to_string(),
    }
}

/// The `launch` script: start `argv` in a detached tmux session, in `cwd`,
/// with `env` exported, through a login shell so PATH is the target's own.
/// Adds nothing to `argv`.
pub fn launch_script(t: &Target, req: &LaunchRequest) -> String {
    let mut inner = String::new();
    for (k, v) in &req.env {
        inner.push_str(&format!("export {k}={}\n", sq(v)));
    }
    inner.push_str("exec");
    for a in &req.argv {
        inner.push(' ');
        inner.push_str(&sq(a));
    }
    inner.push('\n');
    let mut s = format!(
        "name={}\ncwd={}\ninner={}\n",
        sq(&t.name),
        sq(&req.cwd),
        sq(&inner)
    );
    s.push_str(PRELUDE);
    s.push_str(
        r#"h=$(uname -n)
cd "$cwd" 2>/dev/null || { echo "$h has no folder $cwd" >&2; exit 1; }
cwd=$(pwd -P)
command -v tmux >/dev/null 2>&1 || { echo "$h has no tmux on PATH" >&2; exit 1; }
if tmux has-session -t "=$name" 2>/dev/null; then
  echo "$h already has a tmux session named $name" >&2; exit 1
fi
tmux new-session -d -s "$name" -c "$cwd" bash -lc "$inner" || exit 1
say "host $h"
say "cwd $cwd"
say "done"
"#,
    );
    s
}

pub fn stop_script(name: &str) -> String {
    format!(
        "name={}\n{}",
        sq(name),
        r#"if tmux has-session -t "=$name" 2>/dev/null; then
  tmux kill-session -t "=$name" || exit 1
  printf '@@stopped\n'
else
  echo "no tmux session named $name on $(uname -n); nothing to stop" >&2
  printf '@@gone\n'
fi
"#
    )
}

pub fn status_script(name: &str) -> String {
    format!(
        "name={}\n{}",
        sq(name),
        r#"if tmux has-session -t "=$name" 2>/dev/null; then printf '@@running true\n'; else printf '@@running false\n'; fi
"#
    )
}

/// Handles are `<destination> <session>`: both are single words.
pub fn handle(dest: &str, name: &str) -> String {
    format!("{dest} {name}")
}

pub fn parse_handle(h: &str) -> Result<(String, String)> {
    let (d, n) = h
        .split_once(' ')
        .ok_or_else(|| anyhow!("`{h}` is not an ssh adapter handle"))?;
    validate_dest(d)?;
    validate_name(n)?;
    Ok((d.to_string(), n.to_string()))
}

/// Run `script` with `bash -l -s` on `dest`. stderr passes through to the
/// user; stdout is returned. Fails if ssh can't reach `dest` (exit 255) or the
/// script exits nonzero.
pub fn remote(dest: &str, script: &str) -> Result<String> {
    let ssh = std::env::var("MARSHAL_LAUNCH_SSH").unwrap_or_else(|_| "ssh".to_string());
    let mut words = ssh.split_whitespace();
    let prog = words
        .next()
        .ok_or_else(|| anyhow!("MARSHAL_LAUNCH_SSH is empty"))?;
    let mut child = Command::new(prog)
        .args(words)
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "--",
            dest,
            "bash -l -s",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("running `{prog}`"))?;
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(script.as_bytes())
        .context("writing the script to ssh")?;
    let out = child.wait_with_output().context("waiting for ssh")?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    match out.status.code() {
        Some(0) => Ok(stdout),
        Some(255) => bail!("can't reach {dest} over ssh; nothing was started"),
        Some(c) => bail!("the script on {dest} failed (exit {c})"),
        None => bail!("ssh to {dest} was killed by a signal"),
    }
}

pub fn check(t: &Target, req: &LaunchRequest) -> Result<()> {
    let out = remote(&t.dest, &check_script(t, req))?;
    let failures = Probe::parse(&out).failures(t, req);
    if failures.is_empty() {
        return Ok(());
    }
    let mut msg = format!("{} can't run this launch:", t.dest);
    for f in &failures {
        msg.push_str("\n  - ");
        msg.push_str(f);
    }
    Err(anyhow!(msg))
}

pub fn launch(t: &Target, req: &LaunchRequest) -> Result<Launched> {
    let out = remote(&t.dest, &launch_script(t, req))?;
    let p = Probe::parse(&out);
    let (Some(host), Some(cwd), true) = (p.host, p.cwd, p.done) else {
        bail!(
            "the launch script on {} did not report where it started the agent",
            t.dest
        );
    };
    Ok(Launched {
        handle: handle(&t.dest, &t.name),
        r#where: format!("{host} tmux:{}", t.name),
        attach: Some(format!("ssh -t {} tmux attach -t ={}", t.dest, t.name)),
        host,
        cwd,
    })
}

pub fn stop(dest: &str, name: &str) -> Result<()> {
    remote(dest, &stop_script(name)).map(|_| ())
}

pub fn status(dest: &str, name: &str) -> Result<bool> {
    let out = remote(dest, &status_script(name))?;
    out.lines()
        .find_map(|l| l.strip_prefix("@@running "))
        .map(|v| v == "true")
        .ok_or_else(|| anyhow!("the status script on {dest} did not answer"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(addr: &str) -> LaunchRequest {
        LaunchRequest {
            version: 1,
            harness: "claude".into(),
            argv: vec!["claude".into()],
            cwd: "~/work/app".into(),
            name: "claude-app".into(),
            agent_id: "launch-1".into(),
            env: BTreeMap::from([
                ("MARSHAL_AGENT_ID".into(), "launch-1".into()),
                ("MARSHAL_DAEMON_ADDRESS".into(), addr.into()),
            ]),
            options: vec!["--host".into(), "build-03".into()],
        }
    }

    #[test]
    fn refuses_a_loopback_launch_address() {
        for a in [
            "ws://localhost:6155",
            "ws://127.0.0.1:6155",
            "ws://127.9.9.9",
            "ws://[::1]:6155",
            "ws://0.0.0.0:6155",
            "ws://foo.localhost:6155",
            "ws://[::ffff:127.0.0.1]:6155",
        ] {
            let e = Target::from_request(&req(a)).unwrap_err().to_string();
            assert!(e.contains("loopback"), "{a}: {e}");
        }
        let t = Target::from_request(&req("ws://marshal.mesh:6155")).unwrap();
        assert_eq!(t.dest, "build-03");
        assert_eq!(
            t.daemon,
            HostPort {
                host: "marshal.mesh".into(),
                port: 6155
            }
        );
        assert_eq!(
            t.hook,
            HostPort {
                host: "marshal.mesh".into(),
                port: 6156
            }
        );
    }

    #[test]
    fn hook_port_follows_marshal_base_url_and_refuses_its_loopback() {
        let mut r = req("ws://100.86.1.2");
        r.env
            .insert("MARSHAL_BASE_URL".into(), "http://hooks.mesh:7000/".into());
        let t = Target::from_request(&r).unwrap();
        assert_eq!(t.daemon.port, 6155);
        assert_eq!(
            t.hook,
            HostPort {
                host: "hooks.mesh".into(),
                port: 7000
            }
        );
        r.env
            .insert("MARSHAL_BASE_URL".into(), "http://localhost:6156".into());
        assert!(
            Target::from_request(&r)
                .unwrap_err()
                .to_string()
                .contains("loopback")
        );
    }

    #[test]
    fn refuses_bad_requests() {
        let mut r = req("ws://m:6155");
        r.env.remove("MARSHAL_DAEMON_ADDRESS");
        assert!(
            Target::from_request(&r)
                .unwrap_err()
                .to_string()
                .contains("MARSHAL_DAEMON_ADDRESS")
        );
        let mut r = req("ws://m:6155");
        r.options = vec![];
        assert!(
            Target::from_request(&r)
                .unwrap_err()
                .to_string()
                .contains("--host")
        );
        r.options = vec!["--host".into(), "-oProxyCommand=x".into()];
        assert!(Target::from_request(&r).is_err());
        r.options = vec!["--host=a b".into()];
        assert!(Target::from_request(&r).is_err());
        let mut r = req("ws://m:6155");
        r.name = "a.b".into();
        assert!(Target::from_request(&r).is_err());
        let mut r = req("ws://m:6155");
        r.env.insert("A;B".into(), "x".into());
        assert!(Target::from_request(&r).is_err());
        assert!(parse_launch_request(r#"{"version":2}"#).is_err());
    }

    #[test]
    fn single_quotes_survive() {
        assert_eq!(sq("it's"), r#"'it'"'"'s'"#);
    }

    #[test]
    fn shim_versions() {
        assert_eq!(shim_version("marshal-shim 0.60.2\n"), Some("0.60.2"));
        assert_eq!(shim_version("bash: marshal-shim: not found"), None);
        assert!(shim_compatible("0.60.9", "0.60.2"));
        assert!(!shim_compatible("0.59.0", "0.60.2"));
        assert!(shim_compatible("1.4.0", "1.0.0"));
        assert!(!shim_compatible("2.0.0", "1.9.0"));
        assert_eq!(accepted_range("0.60.2"), "0.60.x");
    }

    #[test]
    fn handles_round_trip() {
        let h = handle("u@build-03", "claude-app");
        assert_eq!(
            parse_handle(&h).unwrap(),
            ("u@build-03".into(), "claude-app".into())
        );
        assert!(parse_handle("nospace").is_err());
    }

    fn good_claude_probe(path: &str) -> String {
        format!(
            "profile noise\n@@host build-03\n@@daemon-port ok\n@@hook-port ok\n@@cwd /home/u/work/app\n\
             @@tmux ok\n@@harness ok\n@@shim marshal-shim {VERSION}\n\
             @@begin claude-plugins\n[{{\"id\":\"marshal-shim@marshal\",\"enabled\":true,\"installPath\":\"{path}\"}}]\n@@end\n\
             @@begin claude-plugin-hooks {path}\n{{\"hooks\":{{\"Stop\":[{{\"hooks\":[{{\"command\":\"marshal-shim claude-hook wait\"}}]}}]}}}}\n@@end\n\
             @@begin claude-plugin-mcp {path}\n{{\"mcpServers\":{{\"marshal\":{{\"command\":\"marshal-shim\",\"env\":{{\"MARSHAL_DAEMON_ADDRESS\":\"${{MARSHAL_DAEMON_ADDRESS:-ws://localhost:6155}}\"}}}}}}}}\n@@end\n\
             @@done\n"
        )
    }

    #[test]
    fn a_wired_claude_target_passes() {
        let r = req("ws://marshal.mesh:6155");
        let t = Target::from_request(&r).unwrap();
        let p = Probe::parse(&good_claude_probe("/p/marshal-shim/0.60.2"));
        assert_eq!(p.failures(&t, &r), Vec::<String>::new());
    }

    fn one_failure(probe: &str, r: &LaunchRequest) -> String {
        let t = Target::from_request(r).unwrap();
        let f = Probe::parse(probe).failures(&t, r);
        assert_eq!(f.len(), 1, "{f:#?}");
        f[0].clone()
    }

    #[test]
    fn each_failure_says_why() {
        let r = req("ws://marshal.mesh:6155");
        let good = good_claude_probe("/p/marshal-shim/0.60.2");
        let cases = [
            (
                "@@daemon-port ok",
                "@@daemon-port fail",
                "can't reach the daemon at marshal.mesh:6155",
            ),
            (
                "@@hook-port ok",
                "@@hook-port fail",
                "hook port at marshal.mesh:6156",
            ),
            (
                "@@shim marshal-shim",
                "@@shim marshal-shim 0.1.0 x",
                "not a marshal-shim version",
            ),
            (
                "@@cwd /home/u/work/app",
                "@@cwd-missing /home/u/work/app",
                "has no folder",
            ),
            ("@@tmux ok", "@@tmux missing", "no tmux"),
            ("@@harness ok", "@@harness missing", "no `claude`"),
            (
                "marshal-shim claude-hook wait",
                "other",
                "hooks on build-03 don't run",
            ),
            (
                "\"command\":\"marshal-shim\"",
                "\"command\":\"other\"",
                "MCP server on build-03 is not",
            ),
            ("\"enabled\":true", "\"enabled\":false", "not enabled"),
            (
                "marshal-shim@marshal",
                "other@marshal",
                "no marshal-shim plugin",
            ),
            ("@@done\n", "", "stopped before it finished"),
        ];
        for (from, to, want) in cases {
            let f = one_failure(&good.replacen(from, to, 1), &r);
            assert!(f.contains(want), "{from} → {to}: {f}");
        }
        let old = good.replace(&format!("marshal-shim {VERSION}"), "marshal-shim 0.1.0");
        assert!(one_failure(&old, &r).contains("is 0.1.0; this launcher accepts"));
        let none = good.replace(&format!("@@shim marshal-shim {VERSION}"), "@@shim-missing");
        assert!(one_failure(&none, &r).contains("no marshal-shim on PATH"));
        let fixed = good.replace(
            "${MARSHAL_DAEMON_ADDRESS:-ws://localhost:6155}",
            "ws://other:6155",
        );
        assert!(one_failure(&fixed, &r).contains("fixed to ws://other:6155"));
    }

    #[test]
    fn codex_wiring() {
        let mut r = req("ws://marshal.mesh:6155");
        r.harness = "codex".into();
        r.argv = vec!["codex".into()];
        let t = Target::from_request(&r).unwrap();
        let base = format!(
            "@@host b\n@@daemon-port ok\n@@hook-port ok\n@@cwd /w\n@@tmux ok\n@@harness ok\n\
             @@shim marshal-shim {VERSION}\n@@begin codex-managed\n{CODEX_BEGIN}\n[mcp_servers.marshal]\n\
             command = \"/home/u/.local/bin/marshal-shim\"\n\
             env = {{ MARSHAL_DAEMON_ADDRESS = \"ws://marshal.mesh:6155\", MARSHAL_HARNESS = \"codex\" }}\n\
             command = \"'/home/u/.local/bin/marshal-shim' codex-hook session-start http://marshal.mesh:6156\"\n\
             {CODEX_END}\n@@end\n@@done\n"
        );
        assert_eq!(Probe::parse(&base).failures(&t, &r), Vec::<String>::new());
        let other = base.replace("\"ws://marshal.mesh:6155\"", "\"ws://elsewhere:6155\"");
        assert!(one_failure(&other, &r).contains("set up for the daemon at ws://elsewhere:6155"));
        let nohooks = base.replace("codex-hook", "x");
        assert!(one_failure(&nohooks, &r).contains("don't run `marshal-shim codex-hook`"));
        let missing = base.replace(
            &base[base.find("@@begin").unwrap()..base.find("@@done").unwrap()],
            "@@codex-config-missing /home/u/.codex/config.toml\n",
        );
        assert!(one_failure(&missing, &r).contains("codex-setup"));
    }
}

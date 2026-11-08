//! marshal-daemon — myko coordination server.
//!
//! Single binary: spins up a Myko server over WebSocket and registers
//! the entities defined in the `entities` crate. Persistence is Postgres via
//! myko when `MYKO_POSTGRES_URL` is set (durable, latest-state-per-entity —
//! like pulse-cluster / pulse-ctx); otherwise the daemon runs ephemeral. Bind
//! address is configurable so the server can be hosted remotely; clients
//! (shims, TUIs, web UIs) point their `MykoClient` at it.

use anyhow::{Context, Result};
use myko_server::mcp::dispatch::ServerInfo;
use myko_server::postgres::PostgresConfig;
use myko_server::{BlackholePersister, MykoServer};
use std::{
    net::{SocketAddr, ToSocketAddrs},
    sync::Arc,
};

/// Default MCP/WS bind. Loopback by default: marshal carries no auth and
/// its `asSession` self-identify is trust-on-assert, so it must not be
/// reachable off-box unless deliberately exposed. Production binds the
/// NetBird mesh hostname via `MARSHAL_BIND` — making exposure an explicit
/// act, not a default. Port 6155 is distinct from myko's default 5155.
const DEFAULT_BIND: &str = "127.0.0.1:6155";

/// Default plain-HTTP hook listener bind (the `/hook/*` endpoints). Same
/// loopback-by-default posture; override with `MARSHAL_HOOK_BIND`. A
/// second port because it speaks plain HTTP, not MCP — see `http_listener`.
const DEFAULT_HOOK_BIND: &str = "127.0.0.1:6156";

#[tokio::main]
async fn main() -> Result<()> {
    // Answer `--help` / `--version` before anything starts, and refuse any
    // other argument: configuration comes from the environment only, so an
    // argument here is a mistake, and ignoring it would start a real daemon.
    // `args_os`, so an argument that isn't UTF-8 is refused, not a panic.
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    match args.first().and_then(|arg| arg.to_str()) {
        Some("-h" | "--help") => {
            print!("{}", usage());
            return Ok(());
        }
        Some("-V" | "--version") => {
            println!("marshal-daemon {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("forget") => {
            let target = match args.get(1).and_then(|a| a.to_str()) {
                Some(t) if args.len() == 2 && !t.trim().is_empty() => t.trim().to_string(),
                _ => {
                    eprintln!(
                        "usage: marshal-daemon forget <session id | 8+ char id prefix | nickname>"
                    );
                    std::process::exit(2);
                }
            };
            std::process::exit(forget(&target));
        }
        _ => {}
    }
    // The daemon always runs in the foreground. `--foreground` stays accepted,
    // alone, because earlier docs told people to pass it, so a unit file
    // written from them keeps working.
    let unknown = match args.first() {
        Some(first) if first.as_os_str() == "--foreground" => args.get(1),
        first => first,
    };
    if let Some(arg) = unknown {
        eprintln!(
            "marshal-daemon: unknown argument: {}",
            arg.to_string_lossy()
        );
        eprintln!("Try 'marshal-daemon --help'.");
        std::process::exit(2);
    }

    init_logging();

    // Resolve via the system resolver (not bare `parse`) so a mesh DNS
    // name like `marshal-01.lucid.host:6155` works and survives the peer's
    // mesh IP being reassigned (next start re-resolves).
    let bind_addr = resolve_bind("MARSHAL_BIND", DEFAULT_BIND)?;
    let hook_bind = resolve_bind("MARSHAL_HOOK_BIND", DEFAULT_HOOK_BIND)?;

    // Force-link entities + sagas so their `inventory` registrations aren't
    // dead-code-eliminated.
    marshal_entities::link();
    daemon::link();

    // Client/Server entities are WS-bound and always transient — blackhole them
    // so connection bookkeeping never lands in durable storage (a replayed
    // Client would reference a WS connection that no longer exists).
    let blackhole: Arc<dyn myko::server::Persister> = Arc::new(BlackholePersister);
    // Session is durable but its liveness fields churn on a seconds-scale
    // heartbeat; the filter drops liveness-only SETs from the durable path so
    // the event table grows with session lifecycle, not with heartbeats
    // (lv-6731). Late-bound to the Postgres producer after build below.
    let session_filter = Arc::new(daemon::liveness_filter::SessionLivenessFilter::new());
    let mut builder = MykoServer::builder()
        .with_bind_addr(bind_addr)
        .with_persister_override("Client", blackhole.clone())
        .with_persister_override("Server", blackhole.clone())
        // A wait dies with the daemon, so there's nothing to replay, and a held
        // wait's refreshes shouldn't grow the event table.
        .with_persister_override("WakeWatcher", blackhole.clone())
        .with_persister_override("Session", session_filter.clone())
        .with_server_info(marshal_server_info());

    // Persistence: Postgres via myko when `MYKO_POSTGRES_URL` is set — durable,
    // latest-state-per-entity (no append-forever event log and no full-file
    // replay on boot, which the old bespoke JSONL persister had), matching
    // pulse-cluster / pulse-ctx. Without it, run EPHEMERAL (blackhole): the
    // daemon is still usable in dev / CI without a database, and the roster,
    // rooms, and messages regenerate as sessions reconnect. myko-server handles
    // Postgres replay + live-notify internally — no manual replay/watcher here.
    builder = match PostgresConfig::from_env() {
        Some(pg) => {
            log::info!(
                "marshal-daemon persistence: postgres (events table `{}`)",
                pg.table
            );
            builder.with_postgres(pg)
        }
        None => {
            log::warn!(
                "marshal-daemon persistence: EPHEMERAL — no MYKO_POSTGRES_URL set; \
                 roster/rooms/messages will NOT survive a restart"
            );
            builder.with_default_persister(blackhole)
        }
    };

    let server = builder.build();

    // Bind the Session filter to the real durable persister now that the
    // builder has constructed it. In ephemeral mode there is none and the
    // filter keeps dropping, matching the blackhole default.
    if let Some(handle) = server.postgres_producer.clone() {
        session_filter.bind(Arc::new(handle));
    }

    // The periodic sweeper starts below, once myko has caught up with Postgres
    // (MAR-19).

    // The operator's control files (MAR-46): `marshal-daemon forget` writes
    // them in the state directory, and this applies them every 2 s.
    match daemon::controls::state_dir() {
        Some(dir) => match daemon::controls::ensure_state_dir(&dir) {
            Ok(()) => {
                log::info!("[controls] state directory {}", dir.display());
                tokio::spawn(daemon::controls::run(dir, server.ctx()));
            }
            Err(e) => log::warn!(
                "[controls] can't use state directory {}: {e}; `marshal-daemon forget` won't work",
                dir.display()
            ),
        },
        None => log::warn!(
            "[controls] no state directory (set MARSHAL_STATE_DIR); `marshal-daemon forget` won't work"
        ),
    }

    // Spawn marshal's own plain-HTTP listener for the Claude Code `/hook/*`
    // endpoints. Shares the server ctx (same registry + event log); stock
    // myko's MCP listener is left untouched on its own port.
    //
    // Set on SIGTERM or Ctrl-C, so held waits end cleanly before the daemon exits.
    // A listener that can't bind or stops is logged, and the daemon keeps
    // running, as it always has: the hooks fail quietly within their timeout
    // when nothing answers.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    {
        let hook_ctx = Arc::new(server.ctx());
        tokio::spawn(async move {
            if let Err(e) = daemon::http_listener::run_until(hook_bind, hook_ctx, stop_rx).await {
                log::error!("[hook] listener exited: {e}");
            }
        });
    }

    log::info!("marshal-daemon listening on ws://{bind_addr}, hooks on http://{hook_bind}");
    // The sweeper starts only once myko has replayed Postgres (MAR-19, from
    // MUS-58). `build` starts the replay in the background and only `run`
    // waits for it. See `cleanup::run_sweeper_when_ready`.
    let sweeper = async {
        daemon::cleanup::run_sweeper_when_ready(server.ctx(), || server.is_ready()).await;
        std::future::pending::<()>().await;
    };
    tokio::select! {
        r = server.run() => r.map_err(|e| anyhow::anyhow!(e))?,
        () = sweeper => {}
        signal = stop_signal() => {
            log::info!("marshal-daemon: {signal}; ending held waits, then stopping");
            let _ = stop_tx.send(true);
            // Long enough for each held wait to write its reply.
            tokio::time::sleep(STOP_DRAIN).await;
        }
    }
    Ok(())
}
/// How long a stopping daemon gives held waits to write their replies.
const STOP_DRAIN: std::time::Duration = std::time::Duration::from_millis(500);

/// Waits for SIGTERM, which systemd sends on stop and restart, or Ctrl-C.
async fn stop_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => tokio::select! {
                _ = term.recv() => "SIGTERM",
                _ = tokio::signal::ctrl_c() => "Ctrl-C",
            },
            Err(e) => {
                log::warn!("marshal-daemon: can't watch for SIGTERM: {e}");
                let _ = tokio::signal::ctrl_c().await;
                "Ctrl-C"
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "Ctrl-C"
    }
}

/// The `--help` text. The daemon takes no other arguments; it reads its
/// configuration from the environment.
fn usage() -> String {
    format!(
        "marshal-daemon {version}
Marshal coordination daemon: serves the session roster, rooms and messages
to marshal-shim, marshal-tui and the agent plugins.

Usage: marshal-daemon [OPTIONS]
       marshal-daemon forget <SESSION>

Commands:
  forget SESSION  Ask the running daemon to remove a session from the roster
                  (its id, an 8+ char id prefix, or its nickname) and refuse its
                  hooks for a while, so a stale waiter can't put it back

Options:
  -h, --help     Print this help and exit
  -V, --version  Print the version and exit

Environment:
  MARSHAL_BIND       WebSocket and MCP listen address (default {DEFAULT_BIND})
  MARSHAL_HOOK_BIND  HTTP hook listen address (default {DEFAULT_HOOK_BIND})
  MYKO_POSTGRES_URL  Postgres URL for durable state; without it the daemon
                     runs ephemeral and keeps nothing across a restart
  MARSHAL_STATE_DIR  The daemon's state directory, for the operator's control
                     files (default $XDG_STATE_HOME/marshal, else
                     ~/.local/state/marshal)
  MARSHAL_FORGET_HOLD_SECS  How long `forget` refuses a session's hooks
                     (default 600)
  RUST_LOG           Log filter (default info)
",
        version = env!("CARGO_PKG_VERSION"),
    )
}

/// Resolve a bind spec from `env_var` (or `default`) via the system
/// resolver, so hostnames — not just numeric IPs — are accepted.
fn resolve_bind(env_var: &str, default: &str) -> Result<SocketAddr> {
    let spec = std::env::var(env_var).unwrap_or_else(|_| default.to_string());
    spec.to_socket_addrs()
        .with_context(|| format!("resolving {env_var} '{spec}'"))?
        .next()
        .with_context(|| format!("{env_var} '{spec}' resolved to no addresses"))
}

/// MCP `ServerInfo` for the `/myko/mcp` initialize response.
///
/// marshal runs on *stock* myko 4.24, whose MCP surface is `search` +
/// `execute` (no per-op tool carries a discoverable arg schema). So this
/// blob is load-bearing: it's the operation-id + arg reference the model
/// needs to build `execute` calls, not just flavour text.
///
/// Only a client that speaks to `/myko/mcp` DIRECTLY reads this. Fleet
/// agents go through the marshal-shim, which exposes its own curated tool
/// surface (send_message/broadcast/…) with its own instructions.
fn marshal_server_info() -> ServerInfo {
    ServerInfo {
        name: "marshal".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        instructions: Some(format!(
            "Marshal coordinates sibling coding-agent sessions on this fleet.\n\
             \n\
             This is stock myko 4.24: the MCP surface is `search` (discover operations) + \
             `execute` (run JS — `await myko.command(id, args)` for writes, \
             `await myko.query(id, args)` for reads). The operation ids and args below are \
             what you pass to `execute`.\n\
             \n\
             Your own marshal session_id is injected at session start (in a \
             <marshal_session> block). This HTTP-MCP transport carries no connection \
             identity, so pass it as the `asSession` arg on every command below so peers \
             know who sent the message.\n\
             \n\
             READS (queries):\n\
             - GetAllSessions → the roster: every live session — its id, host, operator, \
             cwd, project, current_task. Recipient ids come from here (session_id uuids). \
             Compose a human label from `host.name` + cwd basename when displaying.\n\
             - GetAllRooms → every room and its id.\n\
             \n\
             WRITES (commands) — args are camelCase JSON:\n\
             - SendMessage      {{ toSessionId, body, asSession }}\n\
             - BroadcastMessage {{ toRoomId, body, asSession }}\n\
             - JoinRoom         {{ name, description?, asSession }}\n\
             - LeaveRoom        {{ room, asSession }}   (room = id or name)\n\
             - AckMessages      {{ messageIds, asSession }}   (rarely needed: inbound \
             messages are auto-acked when surfaced to you at turn start)\n\
             e.g. execute `await myko.command('SendMessage', {{toSessionId, body, asSession}})`.\n\
             \n\
             Inbound peer messages are delivered to you automatically at the start of each \
             turn (a <marshal_inbox> block), pulled by a session hook — you do not poll. \
             {coordination}",
            coordination = marshal_entities::COORDINATION_GUIDANCE,
        )),
        // 4.24 tool-search index — built from the registered operations
        // (no I/O); powers the `search` MCP tool.
        operation_index: Arc::new(myko::operation_index::build_operation_index()),
    }
}

fn init_logging() {
    let mut b = env_logger::Builder::from_default_env();
    if std::env::var("RUST_LOG").is_err() {
        b.filter_level(log::LevelFilter::Info);
    }
    b.init();
}

/// `marshal-daemon forget <target>`: ask the running daemon to forget a session
/// (MAR-46), wait for its answer, and say what it did. The exit status is 0
/// when it forgot one, 1 when it refused, and 3 when no daemon answered.
fn forget(target: &str) -> i32 {
    use daemon::controls;
    let Some(dir) = controls::state_dir() else {
        eprintln!(
            "marshal-daemon forget: no state directory: set MARSHAL_STATE_DIR, as the daemon has it"
        );
        return 2;
    };
    let token = match controls::request_forget(&dir, target) {
        Ok(t) => t,
        Err(e) => {
            eprintln!(
                "marshal-daemon forget: couldn't write to {}: {e}",
                dir.display()
            );
            return 2;
        }
    };
    let deadline = std::time::Instant::now() + controls::REFRESH * 3;
    while std::time::Instant::now() < deadline {
        if let Some((ok, text)) = controls::forget_result(&dir, &token) {
            println!("{text}");
            return if ok { 0 } else { 1 };
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    eprintln!(
        "marshal-daemon forget: no daemon answered within {} s. Is it running, with state directory {}? \
         The request stays there, and a daemon that starts within {} min applies it.",
        (controls::REFRESH * 3).as_secs(),
        dir.display(),
        controls::forget_hold().as_secs() / 60
    );
    3
}

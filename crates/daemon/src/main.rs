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

/// With `--tls`, myko's own listener moves here, behind the TLS listener on
/// `MARSHAL_BIND` (MAR-23). Loopback only.
const DEFAULT_MYKO_BIND: &str = "127.0.0.1:6157";

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
        Some("pair-code") => std::process::exit(pair_code(&args[1..])),
        Some("unpair") => std::process::exit(unpair(&args[1..])),
        Some("paired") if args.len() == 1 => std::process::exit(paired()),
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
    // The daemon always runs in the foreground. `--foreground` stays accepted
    // because earlier docs told people to pass it, so a unit file written from
    // them keeps working. `--tls` turns on TLS (MAR-23); `--plain` says plainly
    // that it's off, which is the default.
    let mut tls = None;
    for arg in &args {
        match arg.to_str() {
            Some("--foreground") => {}
            Some("--tls") if tls != Some(false) => tls = Some(true),
            Some("--plain") if tls != Some(true) => tls = Some(false),
            Some("--tls" | "--plain") => {
                eprintln!("marshal-daemon: --tls and --plain can't both be given");
                std::process::exit(2);
            }
            _ => {
                eprintln!(
                    "marshal-daemon: unknown argument: {}",
                    arg.to_string_lossy()
                );
                eprintln!("Try 'marshal-daemon --help'.");
                std::process::exit(2);
            }
        }
    }
    let tls_flag = tls;
    let tls = tls.unwrap_or(false);
    // Before anything builds a rustls config (see `tls::install_provider`).
    daemon::tls::install_provider();

    init_logging();

    // Resolve via the system resolver (not bare `parse`) so a mesh DNS
    // name like `marshal-01.lucid.host:6155` works and survives the peer's
    // mesh IP being reassigned (next start re-resolves).
    let bind_addr = resolve_bind("MARSHAL_BIND", DEFAULT_BIND)?;
    let hook_bind = resolve_bind("MARSHAL_HOOK_BIND", DEFAULT_HOOK_BIND)?;

    // With `--tls` (MAR-23), the public addresses get TLS listeners, bound now
    // so a taken port stops startup, and myko and the hook listener move to
    // loopback behind them. Without it, nothing here runs.
    let tls_front = if tls {
        Some(TlsFront::start(bind_addr, hook_bind).await?)
    } else {
        None
    };
    let myko_bind = tls_front.as_ref().map_or(bind_addr, |front| front.myko);

    // Pairing (MAR-24). With machines paired, running open would hand their
    // sessions to anyone, so the daemon needs to be told which way to run.
    // With --tls, every connection needs a paired machine's token or the
    // host's own local token, checked by myko#129's provider.
    let state_dir = daemon::controls::state_dir();
    let pairing_store = state_dir.as_deref().map(daemon::pairing::Store::open);
    match pairing_store
        .as_ref()
        .map(daemon::pairing::Store::any_pairings)
    {
        Some(Err(e)) => {
            eprintln!(
                "marshal-daemon: can't read the pairings in {}: {e}; refusing to start",
                state_dir
                    .as_deref()
                    .map(|d| d.display().to_string())
                    .unwrap_or_default()
            );
            std::process::exit(2);
        }
        Some(Ok(true)) if tls_flag.is_none() => {
            eprintln!(
                "marshal-daemon: machines are paired with this daemon, so it won't start \
                 without being told how: --tls to serve them, or --plain to run open"
            );
            std::process::exit(2);
        }
        Some(Ok(true)) if !tls => {
            log::warn!(
                "marshal-daemon: machines are paired, but --plain was given: running open, and \
                 the paired machines can't connect"
            );
        }
        _ => {}
    }
    let auth_provider = if tls {
        let config_dir = daemon::pairing::config_dir().ok_or_else(|| {
            anyhow::anyhow!(
                "marshal-daemon --tls: no config directory for the local token (set HOME)"
            )
        })?;
        let local = daemon::pairing::ensure_local_token(&config_dir)
            .context("marshal-daemon --tls: the local token")?;
        let store = pairing_store
            .clone()
            .ok_or_else(|| anyhow::anyhow!("marshal-daemon --tls: no state directory (set HOME)"))?
            .with_local_token(&local);
        daemon::pairing::install(store.clone());
        // MAR-80: a connection acts only as its own sessions, and sessions
        // registered through the hook listener belong to the daemon host.
        let local = myko_auth::Principal {
            issuer: daemon::pairing::ISSUER.into(),
            subject: daemon::pairing::LOCAL_NAME.into(),
            kind: myko_auth::PrincipalKind::Agent,
        };
        let binding = Arc::new(daemon::auth::session_binding::SessionBinding::new(
            daemon::pairing::AllowAll,
            Some(&local),
        ));
        binding.install();
        let provider = myko_auth::MykoAuthProvider::new(
            Arc::new(daemon::pairing::PairingAuthenticator::new(store)),
            binding,
        );
        Some(Arc::new(provider))
    } else {
        None
    };

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
        .with_bind_addr(myko_bind)
        .with_persister_override("Client", blackhole.clone())
        .with_persister_override("Server", blackhole.clone())
        // A wait dies with the daemon, so there's nothing to replay, and a held
        // wait's refreshes shouldn't grow the event table.
        .with_persister_override("WakeWatcher", blackhole.clone())
        .with_persister_override("Session", session_filter.clone())
        .with_auth_provider(auth_provider)
        .with_server_info(marshal_server_info());

    // Persistence: Postgres via myko when `MYKO_POSTGRES_URL` is set — durable,
    // latest-state-per-entity (no append-forever event log and no full-file
    // replay on boot, which the old bespoke JSONL persister had), matching
    // pulse-cluster / pulse-ctx. Without it, run EPHEMERAL (blackhole): the
    // daemon is still usable in dev / CI without a database, and the roster,
    // rooms, and messages regenerate as sessions reconnect. myko-server handles
    // Postgres replay + live-notify internally — no manual replay/watcher here.
    //
    // Without Postgres, `MARSHAL_PERSIST=file` keeps the store in a file in the
    // state directory instead (MAR-53, from muster's MUS-17; opt-in, so the
    // default is unchanged).
    use daemon::file_persister::{FilePersister, PersistMode, resolve_mode};
    let postgres = PostgresConfig::from_env();
    let (mode, note) = resolve_mode(
        postgres.is_some(),
        std::env::var(daemon::file_persister::ENV).ok().as_deref(),
    )
    .map_err(|why| anyhow::anyhow!("marshal-daemon: {why}"))?;
    if let Some(note) = note {
        log::warn!("marshal-daemon persistence: {note}");
    }
    let mut file_store = None;
    builder = match (mode, postgres) {
        (PersistMode::Postgres, Some(pg)) => {
            log::info!(
                "marshal-daemon persistence: postgres (events table `{}`)",
                pg.table
            );
            builder.with_postgres(pg)
        }
        (PersistMode::File, _) => {
            // The one state directory, shared with the operator's controls (MAR-46).
            let dir = daemon::controls::state_dir().ok_or_else(|| {
                anyhow::anyhow!(
                    "marshal-daemon: the state directory: HOME is not set; set MARSHAL_STATE_DIR"
                )
            })?;
            let opened = FilePersister::open(&dir)
                .map_err(|e| anyhow::anyhow!("marshal-daemon: the store file: {e}"))?;
            let persister: Arc<dyn myko::server::Persister> = opened.persister.clone();
            file_store = Some(opened);
            builder.with_default_persister(persister)
        }
        _ => {
            log::warn!(
                "marshal-daemon persistence: EPHEMERAL — no MYKO_POSTGRES_URL set; \
                 roster/rooms/messages will NOT survive a restart (set {}=file to keep them in \
                 the state directory)",
                daemon::file_persister::ENV
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
    // With the store in a file, load it before anything starts, the sweeper
    // included, so nothing sees it half-loaded (MAR-53).
    if let Some(opened) = file_store {
        session_filter.bind(opened.persister.clone());
        let (loaded, unparsed) = load_into_stores(&server, opened.folded.events);
        log::info!(
            "marshal-daemon persistence: file ({}, {loaded} entities loaded{}){}",
            opened.path.display(),
            opened.backup,
            if opened.folded.torn_tail {
                "; skipped a last line cut off by a crash"
            } else {
                ""
            }
        );
        log::info!(
            "marshal-daemon persistence: loaded {:?}",
            opened.folded.counts
        );
        if unparsed > 0 {
            log::error!(
                "marshal-daemon persistence: {unparsed} stored entities no longer parse and \
                 weren't loaded; they stay in the file"
            );
        }
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
        let hook_inner = tls_front.as_ref().map(|front| front.take_hook_listener());
        tokio::spawn(async move {
            let result = match hook_inner {
                Some(listener) => {
                    daemon::http_listener::serve_until(listener, hook_ctx, stop_rx).await
                }
                None => daemon::http_listener::run_until(hook_bind, hook_ctx, stop_rx).await,
            };
            if let Err(e) = result {
                log::error!("[hook] listener exited: {e}");
            }
        });
    }

    match &tls_front {
        Some(front) => log::info!(
            "marshal-daemon listening on wss://{}, hooks on https://{} (TLS, fingerprint {}; \
             myko on ws://{} and hooks on http://{}, loopback only)",
            front.public,
            front.public_hooks,
            front.fingerprint,
            front.myko,
            front.hooks_inner,
        ),
        None => {
            log::info!("marshal-daemon listening on ws://{bind_addr}, hooks on http://{hook_bind}");
        }
    }
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
/// The TLS listeners in front of myko and the hook listener (MAR-23).
struct TlsFront {
    /// The public WebSocket and MCP address, as bound.
    public: SocketAddr,
    /// The public hook address, as bound.
    public_hooks: SocketAddr,
    /// myko's loopback listener, behind `public`.
    myko: SocketAddr,
    /// The hook listener's loopback address, behind `public_hooks`.
    hooks_inner: SocketAddr,
    fingerprint: String,
    hook_listener: std::sync::Mutex<Option<tokio::net::TcpListener>>,
}

impl TlsFront {
    async fn start(bind: SocketAddr, hook_bind: SocketAddr) -> Result<Self> {
        let myko = resolve_bind("MARSHAL_MYKO_BIND", DEFAULT_MYKO_BIND)?;
        daemon::tls::require_loopback("MARSHAL_MYKO_BIND", myko)?;
        let dir = daemon::controls::state_dir().ok_or_else(|| {
            anyhow::anyhow!(
                "marshal-daemon --tls: the state directory: HOME is not set; set MARSHAL_STATE_DIR"
            )
        })?;
        daemon::controls::ensure_state_dir(&dir)
            .with_context(|| format!("marshal-daemon --tls: state directory {}", dir.display()))?;
        let identity = daemon::tls::load_or_create_identity(&daemon::tls::identity_dir(&dir))
            .context("marshal-daemon --tls: the TLS certificate")?;
        let acceptor = daemon::tls::acceptor(&identity)?;
        // The hook listener is the daemon's own, so it can take a listener
        // bound here on any loopback port. myko binds its own, on `myko`.
        let loopback = if hook_bind.is_ipv6() {
            "[::1]:0"
        } else {
            "127.0.0.1:0"
        };
        let hook_listener = tokio::net::TcpListener::bind(loopback)
            .await
            .with_context(|| {
                format!("marshal-daemon --tls: binding the loopback hook listener on {loopback}")
            })?;
        let hooks_inner = hook_listener.local_addr()?;
        let public_listener = tokio::net::TcpListener::bind(bind)
            .await
            .with_context(|| format!("marshal-daemon --tls: binding {bind}"))?;
        let public_hooks_listener = tokio::net::TcpListener::bind(hook_bind)
            .await
            .with_context(|| format!("marshal-daemon --tls: binding {hook_bind}"))?;
        let public = public_listener.local_addr()?;
        let public_hooks = public_hooks_listener.local_addr()?;
        // The public listener is bound now, so a taken port stops startup, but
        // it forwards only once myko answers on its loopback port.
        let myko_acceptor = acceptor.clone();
        tokio::spawn(async move {
            daemon::tls::wait_for_upstream(myko).await;
            daemon::tls::serve(public_listener, myko_acceptor, myko).await;
        });
        tokio::spawn(daemon::tls::serve(
            public_hooks_listener,
            acceptor,
            hooks_inner,
        ));
        Ok(Self {
            public,
            public_hooks,
            myko,
            hooks_inner,
            fingerprint: identity.fingerprint(),
            hook_listener: std::sync::Mutex::new(Some(hook_listener)),
        })
    }

    /// The loopback hook listener, once.
    fn take_hook_listener(&self) -> tokio::net::TcpListener {
        self.hook_listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("the hook listener is taken once")
    }
}

/// Put stored entities straight into myko's stores, as myko's own Postgres
/// consumer does at boot (`apply_remote_event`): no cascades, which already
/// ran when they were written, and nothing written back to the store file.
fn load_into_stores(server: &MykoServer, events: Vec<myko::wire::MEvent>) -> (usize, usize) {
    let handlers = server.handler_registry();
    let registry = server.registry();
    let (mut loaded, mut unparsed) = (0, 0);
    for event in events {
        let parsed = handlers
            .item_parser(&event.item_type)
            .and_then(|parse| parse(event.item).ok());
        match parsed {
            Some(item) => {
                registry
                    .get_or_create(item.entity_type())
                    .insert(item.id(), item);
                loaded += 1;
            }
            None => {
                log::error!(
                    "marshal-daemon persistence: a stored {} doesn't parse",
                    event.item_type
                );
                unparsed += 1;
            }
        }
    }
    (loaded, unparsed)
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
       marshal-daemon pair-code <NAME> [--ttl SECS] [--address HOST:PORT] [--hooks HOST:PORT]
       marshal-daemon unpair <NAME>
       marshal-daemon paired

Commands:
  forget SESSION  Ask the running daemon to remove a session from the roster
                  (its id, an 8+ char id prefix, or its nickname) and refuse its
                  hooks for a while, so a stale waiter can't put it back
  pair-code NAME  Print a one-time code to pair a machine as NAME with a daemon
                  run with --tls (good for 15 minutes unless --ttl says
                  otherwise). --address is the daemon as that machine dials it;
                  else MARSHAL_ADVERTISE, else MARSHAL_BIND. --hooks is its
                  hook listener; else that host on MARSHAL_HOOK_BIND's port
  unpair NAME     Forget a paired machine's token; its connections close
  paired          List the paired machines

Options:
  --tls          Serve TLS on MARSHAL_BIND and MARSHAL_HOOK_BIND with a
                 self-signed certificate kept in the state directory; clients
                 pin its fingerprint (state directory's tls/fingerprint).
                 myko and the hook listener move to loopback behind it
  --plain        No TLS (the default)
  -h, --help     Print this help and exit
  -V, --version  Print the version and exit

Environment:
  MARSHAL_BIND       WebSocket and MCP listen address (default {DEFAULT_BIND})
  MARSHAL_HOOK_BIND  HTTP hook listen address (default {DEFAULT_HOOK_BIND})
  MARSHAL_MYKO_BIND  With --tls, myko's own listener behind MARSHAL_BIND;
                     loopback only (default {DEFAULT_MYKO_BIND})
  MARSHAL_ADVERTISE  The host:port paired machines dial, for pair-code
  MYKO_POSTGRES_URL  Postgres URL for durable state; without it the daemon
                     runs ephemeral and keeps nothing across a restart
  MARSHAL_PERSIST    Set to `file`, with no Postgres, to keep messages, read
                     state and rooms in the state directory across a restart
  MARSHAL_STATE_DIR  The daemon's state directory, for the operator's control
                     files and MARSHAL_PERSIST=file's store (default
                     $XDG_STATE_HOME/marshal, else ~/.local/state/marshal)
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

/// `marshal-daemon pair-code <name> [--ttl SECS] [--address HOST:PORT]`.
fn pair_code(args: &[std::ffi::OsString]) -> i32 {
    let mut name = None;
    let mut ttl = daemon::pairing::DEFAULT_INVITE_TTL;
    let mut address = None;
    let mut hooks = None;
    let mut it = args.iter().map(|a| a.to_string_lossy().into_owned());
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--ttl" => match it.next().and_then(|v| v.parse::<u64>().ok()) {
                Some(secs) if secs > 0 => ttl = std::time::Duration::from_secs(secs),
                _ => return usage_error("--ttl takes a number of seconds"),
            },
            "--address" => match it.next() {
                Some(a) if !a.trim().is_empty() => address = Some(a.trim().to_string()),
                _ => return usage_error("--address takes host:port"),
            },
            "--hooks" => match it.next() {
                Some(a) if !a.trim().is_empty() => hooks = Some(a.trim().to_string()),
                _ => return usage_error("--hooks takes host:port"),
            },
            a if a.starts_with('-') => return usage_error(&format!("unknown option {a}")),
            a if name.is_none() => name = Some(a.to_string()),
            _ => return usage_error("one name"),
        }
    }
    let Some(name) = name else {
        return usage_error("a machine name");
    };
    let Some(state) = daemon::controls::state_dir() else {
        eprintln!("marshal-daemon pair-code: no state directory (set MARSHAL_STATE_DIR)");
        return 2;
    };
    let fingerprint = match std::fs::read_to_string(
        daemon::tls::identity_dir(&state).join(daemon::tls::FINGERPRINT_FILE),
    ) {
        Ok(text) => text.trim().to_string(),
        Err(_) => {
            eprintln!(
                "marshal-daemon pair-code: no TLS certificate yet in {}: start the daemon with \
                 --tls once first",
                state.display()
            );
            return 2;
        }
    };
    let address = address
        .or_else(|| {
            std::env::var("MARSHAL_ADVERTISE")
                .ok()
                .filter(|a| !a.trim().is_empty())
        })
        .or_else(|| {
            std::env::var("MARSHAL_BIND")
                .ok()
                .filter(|a| !a.trim().is_empty())
        })
        .unwrap_or_else(|| DEFAULT_BIND.to_string());
    if address.starts_with("127.")
        || address.starts_with("[::1]")
        || address.starts_with("localhost")
    {
        eprintln!(
            "marshal-daemon pair-code: the code names {address}, which another machine can't \
             reach; give --address or MARSHAL_ADVERTISE"
        );
    }
    // The hook listener is on the same host as the daemon, on
    // MARSHAL_HOOK_BIND's port unless --hooks says otherwise.
    let hooks = hooks.unwrap_or_else(|| {
        let host = address
            .rsplit_once(':')
            .map_or(address.as_str(), |(h, _)| h);
        let port = std::env::var("MARSHAL_HOOK_BIND")
            .ok()
            .and_then(|b| b.rsplit_once(':').map(|(_, p)| p.to_string()))
            .unwrap_or_else(|| DEFAULT_HOOK_BIND.rsplit_once(':').unwrap().1.to_string());
        format!("{host}:{port}")
    });
    let store = daemon::pairing::Store::open(&state);
    let secret = match store.add_invite(&name, ttl) {
        Ok(secret) => secret,
        Err(e) => {
            eprintln!("marshal-daemon pair-code: {e}");
            return 2;
        }
    };
    let invite = marshal_entities::pairing::Invite {
        address,
        hooks,
        fingerprint,
        invite: secret,
        name: name.clone(),
    };
    println!("{}", invite.to_code());
    eprintln!(
        "On {name}: marshal-shim pair <that code>   (good for {} s, once)",
        ttl.as_secs()
    );
    0
}

fn unpair(args: &[std::ffi::OsString]) -> i32 {
    let name = match args {
        [name] => name.to_string_lossy().into_owned(),
        _ => return usage_error("marshal-daemon unpair <NAME>"),
    };
    let Some(state) = daemon::controls::state_dir() else {
        eprintln!("marshal-daemon unpair: no state directory (set MARSHAL_STATE_DIR)");
        return 2;
    };
    match daemon::pairing::Store::open(&state).remove(&name) {
        Ok(true) => {
            println!("unpaired {name}; its connections close within a second");
            0
        }
        Ok(false) => {
            println!("{name} isn't paired");
            1
        }
        Err(e) => {
            eprintln!("marshal-daemon unpair: {e}");
            2
        }
    }
}

fn paired() -> i32 {
    let Some(state) = daemon::controls::state_dir() else {
        eprintln!("marshal-daemon paired: no state directory (set MARSHAL_STATE_DIR)");
        return 2;
    };
    match daemon::pairing::Store::open(&state).names() {
        Ok(names) if names.is_empty() => {
            println!("no machines are paired");
            0
        }
        Ok(names) => {
            for name in names {
                println!("{name}");
            }
            0
        }
        Err(e) => {
            eprintln!("marshal-daemon paired: {e}");
            2
        }
    }
}

fn usage_error(what: &str) -> i32 {
    eprintln!("marshal-daemon: expected {what}");
    eprintln!("Try 'marshal-daemon --help'.");
    2
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

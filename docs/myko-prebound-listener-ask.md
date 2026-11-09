# Ask for myko-server: serve on a socket the caller bound (MAR-40)

Fork-only. This is a draft of an additive ask for ignition-is-go/myko. Josh sends it if he wants to,
after changing it however he likes. Nothing has been posted, and no PR has been opened there.

Checked against myko-server 6.11.5 (the version Marshal pins) and myko main at `bd43bea9`
(v6.11.6, 2026-11-09). `MykoServer::run` and `run_ws_loop` are identical in both
(`libs/myko/server/src/lib.rs`).

## The problem

`MykoServer` takes only an address (`MykoServerBuilder::with_bind_addr`) and binds it itself inside
`run()` and `run_ws_loop()`. A test can't ask for port 0, because nothing tells it which port the
server got. So every Marshal test that runs a myko server, in process or as the daemon subprocess,
picks a free port, lets go of it, and hands the number to the server. Between the pick and the
server's bind, any other test running in parallel can take the port.

Marshal has paid for that race three times:

- **MAR-21** (Castor #28). A test's myko server reports a lost bind as `run()`'s error, and the test
  starts it again on a new port. That needs a timing assumption: when another test's server already
  holds the port, a connect to it succeeds. So the test can only tell that the bind failed by
  waiting 200 ms to see whether `run()` returns an error (for example `crates/shim/tests/spawn_e2e.rs`,
  `try_spawn`).
- **MAR-67** (Castor #73). Every test that needs a free or dead port now holds the socket for the
  whole test, and every listener in Marshal's own code takes the bound socket from the test. That
  fixed every port race except the myko server's. The server is the one place the pattern can't
  reach.
- **MAR-69** (Castor #74). A conformance cell's `restart()` starts the daemon again on the same
  ports, because the restart cells test a reconnect to the same address. A lost bind there panics,
  so the harness reruns the whole cell on fresh ports, up to 3 times.

All three are retries around a window the server could simply not have, if it would serve on a
socket the caller has already bound.

## The smallest additive API

Two methods, next to the existing two:

```rust
impl MykoServer {
    /// Like [`run`](Self::run), but serves on `socket`, which the caller has already bound,
    /// instead of binding `bind_addr`. The server calls `listen` on it at the same point
    /// `run` binds, after startup and `after_init`, so a connect during startup is still
    /// refused rather than queued.
    pub async fn run_with_socket(&self, socket: tokio::net::TcpSocket)
        -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Like [`run_ws_loop`](Self::run_ws_loop), but serves on `socket`.
    pub async fn run_ws_loop_with_socket(&self, socket: tokio::net::TcpSocket)
        -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}
```

The methods take a `tokio::net::TcpSocket` (bound but not yet listening) rather than a
`TcpListener` on purpose. myko#33 ("bind listener after startup hooks, not before") moved the bind
to the end of `run()`. Before that, a connect during a slow `after_init` was accepted into the
backlog and then hung, never answered. A listener the caller made would bring that window back. A
socket that's only bound holds the port, so nothing else can take it, and a connect to it is
refused until the server calls `listen`. That's the same refusal clients already retry today.

With it, a test does:

```rust
let socket = tokio::net::TcpSocket::new_v4()?;
socket.bind("127.0.0.1:0".parse()?)?;
let addr = socket.local_addr()?; // known before the server starts, and held
tokio::spawn(async move { server.run_with_socket(socket).await });
// connect to `addr`, retrying on a refusal while the server starts
```

That replaces the pick, the retry and the 200 ms wait.

## Why it's backward compatible

- `run()`, `run_ws_loop()`, `with_bind_addr`, `MykoServerConfig` and the builder are unchanged. No
  public type gains a field, so code that builds `MykoServerConfig` by hand still compiles.
- `run()` keeps its startup order exactly. The only change is that the bind at the end goes
  through a private helper that either binds `bind_addr`, as now, or calls `listen` on the given
  socket.
- Nothing new is required. A server that never calls `run_with_socket` behaves byte for byte as it
  does today.
- `tokio::net::TcpSocket` is already available: myko-server depends on tokio with `full`.

One behaviour to state in the docs: with `run_with_socket`, `bind_addr` isn't used for the
listener, so the "listening on" logs should print `listener.local_addr()`, not `bind_addr`.
`PeerRegistryConfig` carries its own address and port, so peer publication isn't affected.

## Sketch of the patch (against 6.11.5 / 6.11.6)

```rust
use tokio::net::{TcpListener, TcpSocket};

/// Where the accept loop's listener comes from.
enum Listen {
    /// Bind this address, as `run` always has.
    Bind(SocketAddr),
    /// Listen on a socket the caller already bound.
    Socket(TcpSocket),
}

impl Listen {
    async fn listen(self) -> std::io::Result<TcpListener> {
        match self {
            Self::Bind(addr) => TcpListener::bind(addr).await,
            // Use the backlog `TcpListener::bind` uses (mio follows std's per-platform value).
            Self::Socket(socket) => socket.listen(BACKLOG),
        }
    }
}

impl MykoServer {
    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.run_on(Listen::Bind(self.config.bind_addr)).await
    }

    pub async fn run_with_socket(
        &self,
        socket: TcpSocket,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.run_on(Listen::Socket(socket)).await
    }

    async fn run_on(
        &self,
        listen: Listen,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // ... the body of today's `run()`, unchanged, down to the bind ...

        let listener = listen.listen().await?;
        let local = listener.local_addr()?;
        tracing::info!("MykoServer listening on {local}");
        tracing::info!(
            "Myko gateway: ws://{local}{} | MCP: {} (POST + WS + SSE)",
            self.websocket_path,
            self.mcp_path
        );

        // ... peer registry start and accept loop, unchanged ...
        self.run_ws_accept_loop(listener).await
    }

    pub async fn run_ws_loop(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.run_ws_loop_on(Listen::Bind(self.config.bind_addr)).await
    }

    pub async fn run_ws_loop_with_socket(
        &self,
        socket: TcpSocket,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.run_ws_loop_on(Listen::Socket(socket)).await
    }

    async fn run_ws_loop_on(
        &self,
        listen: Listen,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.validate_endpoint_paths()?;
        let listener = listen.listen().await?;
        // ... logs from `listener.local_addr()`, then the accept loop, as above ...
        self.run_ws_accept_loop(listener).await
    }
}
```

Tests to go with it, in myko-server's own suite:

- A socket bound to port 0 and passed to `run_with_socket` serves a WebSocket client at its
  `local_addr`.
- While `after_init` is blocked, a connect to that address is refused, not left hanging. This keeps
  myko#33's guarantee.
- `run()` with `with_bind_addr` still binds and serves as before.

Caller's note for the docs: `TcpListener::bind` sets `SO_REUSEADDR` on Unix. A caller that wants
the same should call `socket.set_reuseaddr(true)` before `bind`.

## What Marshal does once it ships

Fork-side, after a myko release with the methods (the second half of MAR-40's done-when):

- The in-process test servers (`spawn_e2e`, `codex_bridge_e2e`, `terminal_send_e2e`,
  `claude_push_ack_e2e`, `send_message_e2e`, `session_del_inbox_e2e`, `ws_reconnect_subscription`,
  `hook_inbox_safety`) bind port 0 and call `run_with_socket`. Their retry loops and the 200 ms wait
  go.
- The daemon subprocess suites (conformance and the e2e daemons) need the daemon to take an
  inherited socket, for example a `--listen-fd` flag, which is Marshal's own change. Until then,
  MAR-69's rerun stays for the restart cells.

## Draft issue text

> **Title:** myko-server: serve on a socket the caller has bound
>
> `MykoServer` only binds an address it's given, so a caller can't ask for port 0 and learn the
> port. Tests that run a server in parallel therefore pick a free port and release it before the
> server binds it, and another test can take it in between. We retry around this in Marshal's
> tests today. Detecting the loss needs a timing assumption, because a connect to a port another
> test's server holds succeeds.
>
> Would you take an additive `MykoServer::run_with_socket(tokio::net::TcpSocket)` (and
> `run_ws_loop_with_socket`)? `run()` would be unchanged: the server calls `listen` on the given
> socket at the point `run()` binds today, after startup and `after_init`, so #33's guarantee
> holds (a connect during startup is refused, not queued). It takes a bound socket rather than a
> listener for exactly that reason. Logs would print the listener's `local_addr`.
>
> Happy to send the PR with tests (serves on a port-0 socket; refuses during a blocked
> `after_init`; `run()` unchanged) if the shape is right.

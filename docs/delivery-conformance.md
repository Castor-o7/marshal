# Delivery conformance

MAR-14, ported from muster's MUS-69. One scenario table, run against every
harness Marshal delivers to, with the real `marshal-daemon` and `marshal-shim`
binaries and no TUI or model. Each harness is a fake that drives the real
binaries the way its TUI does. The suite is `crates/shim/tests/conformance.rs`.

**Stage 1 (MAR-14) runs the `claude` and `codex` columns.** Every other cell
shows `n/a` in the grid with its reason, so a cell left out never reads as one
that ran:

- the `claude-tls` and `codex-tls` columns: Marshal has no TLS gate yet (MAR-23);
- the `pi`, `opencode` and `prime-agent` columns: stage 2, with MAR-10 and MAR-11;
- `operator-pause-stops-wakes` and `resume-wakes-held-mail`: Marshal has no
  operator pause.

The sections below describe the whole suite as muster built it, including the
columns stage 1 doesn't run.

```sh
cargo build --workspace
cargo test -p marshal-shim --test conformance -- --ignored --nocapture
```

In stage 1 the plugin columns don't run, so bun isn't needed.

`CONFORMANCE_ONLY=<text>` runs only the cells whose harness or scenario name
contains it. `CONFORMANCE_GRID=<file>` also writes the grid to a file. CI runs
the whole suite after `cargo build --workspace`.

## How a cell runs

Each cell gets a fresh daemon in throwaway dirs, on free loopback ports, with
no Postgres and a 2 s ack lease (`MARSHAL_ACK_LEASE_SECS`). Its hook traffic goes
through a small proxy that can drop `/hook/ack`. A second session sends, through
a real `marshal-shim` speaking MCP, so the send result (`live_push`, `wake`) is
checked as well as what the recipient sees.

## The scenarios

| # | Scenario | Passes when |
|---|---|---|
| 1 | `idle-wakes-on-dm` | An idle session's wake path fires with the message. |
| 2 | `busy-gets-it-at-next-hook` | A busy session's next hook or turn carries the message, and the one after doesn't. |
| 3 | `dropped-ack-redelivers` | When a delivery's ack is lost, the message comes again at the session's next delivery point once the lease lapses, and not after an ack that arrives. Claude's waiter acks with its own `/hook/ack`, which the proxy drops. Codex has no ack request: the daemon acks a hook's reply once it has flushed it, so the proxy loses the reply instead. |
| 3b | `lost-hook-reply-redelivers` | When the reply to a busy session's prompt hook is lost after the daemon sent it, as when a harness kills a hook at its timeout, the message comes again at the next hook. |
| 4 | `duplicate-shown-at-most-once` | A send retried with the same idempotency key is one message, shown once. A wake shows its message once, not once from the push and again from the woken turn's drain. Mail a wake delivered isn't injected again by the next hook. |
| 4b | `burst-both-delivered` | Two messages sent back to back to an idle session both reach the model, between the wake and the next delivery point. A harness that drains the inbox while handling the first must show what it drains. |
| 5 | `daemon-restart-mid-wait` | The daemon restarts (SIGTERM, as systemd stops it) while the session waits, in memory, so the roster empties. The session still wakes on mail sent after the restart. |
| 5 (pg) | `daemon-restart-mid-wait-pg` | The same, with the daemon persisting to Postgres. |
| 5b (pg) | `restart-keeps-unread-mail` | Mail sent while the session is busy, then a restart against Postgres: the mail is kept, and wakes the session once it goes idle. n/a for the push plugins, which take the push into their running turn. |
| 5c (pg) | `restart-burst-shown-once-pg` | After a restart against Postgres, 14 messages sent 350 ms apart, spanning the plugin's reconnect: some while it's away, some racing its reconnect pull, some pushed live after. Each is shown exactly once, including past the hold lease (MUS-90). |
| 5d (pg) | `restart-without-mail-stays-idle-pg` | A restart against Postgres with no mail doesn't wake an idle session: the reconnect's pull of an empty inbox stays quiet (MUS-90). |
| 5e (file) | `restart-keeps-unread-mail-file` | The same with no Postgres and `MARSHAL_PERSIST=file` (MUS-17, MAR-53 on Marshal): the store is the file in the daemon's state directory. Needs no database, so it always runs. n/a for the push plugins, as 5b. |
| 6 | `operator-pause-stops-wakes` | With `marshal-daemon pause`, mail doesn't wake an idle session but still arrives at the next hook. |
| 6b | `resume-wakes-held-mail` | Mail sent while wakes are paused doesn't wake the session, and resuming alone wakes it with that mail, with no new message. |
| 7 | `broadcast-never-wakes` | A room broadcast neither wakes the session nor is injected. |
| 8 | `wake-none-not-expected` | A session the roster calls `wake: none` isn't reported to the sender as `wake: expected`. |
| 9 | `mail-waiting-at-reconnect` | Mail sent while a push plugin's connection is down is shown once at the next turn after it reconnects, and not again after that, even when another message is pushed live first. A plugin that drains the inbox after a push acks the waiting mail there without showing it (upstream #133). |
| 10 | `hung-handover-released` | When the harness's API hangs taking a push, a turn's pull doesn't show the message while the plugin holds it, the next turn shows it once the hold's lease lapses, and no pull shows it again after the hung handover goes through. That late handover can show it a second time, which the design accepts ([push-ack-design.md](push-ack-design.md)). |

| 11 | `restart-keeps-identity` | A session that ends as its TUI ends it, followed by a new session in the same working directory, hands the new one its nickname (MUS-3). Mail to the old nickname and the old id reaches the new one. If the new one first got a fresh nickname, mail to that still reaches it, and its next hook says its name changed. |
| 12 | `live-sibling-keeps-its-name` | A second session in the same working directory, started while the first is live and idle past the quiet time, gets a nickname of its own, and the first keeps its own. For Codex the first is a hook-owned row with no client, which mustn't be taken over until MUS-77. |
| 13 | `unread-mail-follows-a-restart` | Mail sent to a session while no hook runs, so it's still unread when the session ends, is shown once by the session that takes over its nickname, and not at the hook after (MUS-47). |

Scenarios 9 and 10 are about a live push, so they're `n/a` for the hook
harnesses (Claude and Codex). Scenarios 11 to 13 are `n/a` for the plugins,
which don't report a harness yet. The suite's daemon runs with a 1 s
succession grace and a 2 s quiet time.

A cell expected to fail is listed in `KNOWN_FAILURES` against the story that
fixes it, and shows as `known (MAR-nn)` in the grid. If one starts passing, the
suite fails until it's taken off the list. In stage 1 the list is empty: every
cell that runs passes. Muster's entries were all in the plugin columns.

## The harnesses

| Column | Harness | Mode | Status |
|---|---|---|---|
| `claude` | Claude Code | plain | runs (stage 1) |
| `claude-tls` | Claude Code | TLS, through the gate | `n/a` until the gate (MAR-23) |
| `codex` | Codex, through `marshal-shim codex-bridge` and the app-server | plain | runs (stage 1) |
| `codex-tls` | Codex | TLS, through the gate | `n/a` until the gate (MAR-23) |
| `opencode` | opencode plugin (`plugins/marshal-opencode`) | plain | `n/a` until stage 2 |
| `pi` | Pi plugin (`plugins/marshal-pi`) | plain | `n/a` until stage 2 |
| `prime-agent` | Prime Agent plugin (`plugins/marshal-prime-agent`) | plain | `n/a` until stage 2 |

### What the fake Claude assumes

Pinned to **Claude Code 2.1.318**. Its hook input comes from
`crates/shim/tests/fixtures/claude-code-2.1.318/`. It drives the hooks exactly
as `plugins/marshal-shim/hooks/hooks.json` wires them, and it assumes these TUI
behaviours, which the suite can't check:

- **Command hooks get the hook JSON on stdin**, and Claude adds a
  `PostToolUse` or `UserPromptSubmit` hook's
  `hookSpecificOutput.additionalContext` to the turn.
- **An `asyncRewake` hook that exits 2 starts a turn** with its stderr, even
  in a session nobody has typed in. Any other exit starts nothing. The fake
  reads the waiter's exit code and stderr and treats exit 2 as the wake.
- **`SessionStart` runs `session-start`, then starts a waiter. `Stop` starts a
  waiter after every turn.** The fake arms a waiter for each idle period: with
  the SessionStart input the first time and the Stop input after that.
- **Claude runs the MCP shim for the session** with `CLAUDE_CODE_SESSION_ID`
  set, and without `--dangerously-load-development-channels`. So the roster
  row has a live client with channels off. Scenario 8 depends on this.
- **A busy session's next delivery point is `PostToolUse`.** The fake doesn't
  model a waiter that's still running while a turn is busy: it treats busy as
  no waiter, as when the waiter has just woken the session and `Stop` hasn't
  fired yet.

### What the fake Codex assumes

Pinned to **Codex 0.163.0**, the version `codex_bridge.rs` cites. Its
app-server frames and hook input come from
`crates/shim/tests/fixtures/codex-0.163.0/`. It runs the real
`marshal-shim codex-bridge` against a fake shared app-server that hosts one
thread, and the real `marshal-shim codex-hook` commands, as `codex-run` and
`codex_setup.rs` wire them. It assumes these behaviours, which the suite can't
check:

- **The app-server announces an attached TUI's thread** with
  `thread/started` to every client that initializes, and the bridge registers
  it from that.
- **`turn/start` is accepted on an idle thread and refused while a turn
  runs.** The fake models busy as "refused". The exact busy error is a guess.
  The bridge treats any error as "retry later".
- **A turn started by `turn/start` runs Codex's `UserPromptSubmit` hook**
  before the model, and that hook injects the inbox. The fake treats an
  accepted `turn/start` as the wake and then runs that hook.
- **Codex's MCP shim registers no session** (`MARSHAL_HARNESS=codex`), so the
  thread's roster row is hook-owned with no live client. The fake runs no MCP
  shim for it.
- **After a wake, the bridge won't wake the same thread again for 30 s.** So
  scenario 3 checks redelivery at the next hook, not with a second wake.

### What the fake Pi assumes

Pinned to **`@earendil-works/pi-coding-agent` 0.83.0**, the version in
`plugins/marshal-pi/package-lock.json`. The real marshal-pi extension runs
under bun, loaded by `plugins/marshal-pi/test/conformance-driver.ts` with a
stand-in for pi's `ExtensionAPI`. It connects to the daemon itself and takes
the live push, as it does inside pi. It assumes these behaviours of pi, which
the suite can't check:

- **`pi.sendMessage(…, { triggerTurn: true })` on an idle session starts a
  turn** with that message, and on a busy one steers it into the running
  turn. The fake records the first as the wake and the second as busy
  delivery.
- **A turn starts with `before_agent_start`**, and whatever message the
  handler returns is shown to the model. The fake fires it at each busy
  boundary and after a wake, which is where marshal-pi drains the inbox.
- **`session_start`, `agent_settled` and `session_shutdown` fire as their
  names say.** The session id comes from `ctx.sessionManager.getSessionFile()`.
- **A lost delivery** means `pi.sendMessage` rejects after the extension has
  received the push, as when pi fails to take the message. That's where the
  fake loses the ack in scenario 3.
- **A hung handover** (scenario 10) means `pi.sendMessage` doesn't resolve
  until the suite lets it go, and then pi takes the message after all.
- **A dropped connection** (scenario 9) is the extension's WebSocket closing.
  The driver wraps `WebSocket` before it loads the extension, closes the open
  socket, and points new ones at a closed port until the suite restores it.
  myko reconnects on its own, as it does after a real drop.

Two scenarios are `n/a` for Pi. A plugin has no prompt hooks, so a hook reply
can't be lost (3b). And a connected plugin renders pushes, so its roster row
says `channel`, never `none` (8).

### What the fake opencode assumes

Pinned to **`@opencode-ai/plugin` and `@opencode-ai/sdk` 1.17.11**, the
versions in `plugins/marshal-opencode/package-lock.json`. The real
marshal-opencode plugin runs under bun, loaded by
`plugins/marshal-opencode/test/conformance-driver.ts` with a stand-in for
opencode's plugin input: the SDK client (`session.prompt`, `tui.showToast`,
`app.log`) and bun's own `$`. It connects to the daemon itself and takes the
live push. It assumes these behaviours of opencode, which the suite can't
check:

- **`client.session.prompt` on an idle session starts a turn** with that
  text. On a busy one, opencode queues it behind the running turn, so the model
  sees it next. The fake records the first as the wake and the second as busy
  delivery.
- **A user turn runs the `chat.message` hook**, and parts the hook adds are
  shown to the model. marshal-opencode drains the inbox there, so the fake runs
  it at each busy boundary, and after a wake, because the plugin's
  `session.prompt` is itself a user turn.
- **The `event` hook gets `session.created` and `session.idle`** with the
  session id where `eventSessionId` looks for it.
- **A lost delivery** means `session.prompt` rejects after the plugin has
  received the push. A hung handover and a dropped connection work as for Pi.

Scenarios 3b and 8 are `n/a`, as for Pi.

### What the fake Prime Agent assumes

Prime Agent runs the pi extension API, so its fake is the fake Pi with Prime
Agent's session manager, in `plugins/marshal-prime-agent/test/conformance-driver.ts`.
It's pinned to **`@earendil-works/pi-coding-agent` 0.84.0**, the version in
`plugins/marshal-prime-agent/package-lock.json`, and makes Pi's assumptions,
plus these:

- **The session id comes from `ctx.sessionManager.getSessionId()`**, and
  `getHeader()` describes the conversation. The fake's header has no
  `rlmDepth`, so it's a top-level session, which the extension registers. An
  RLM sub-agent, which it skips, isn't covered.
- **`ctx.ui.setWidget`** exists alongside `notify` and `setStatus`, for the
  statusline widget.

Scenarios 3b and 8 are `n/a`, as for Pi.

## Gaps

These are listed so nobody reads the grid as more than it is.

- **The plugin columns need bun and the plugin's `node_modules`** once stage 2
  turns them on: put bun on `PATH` or set `MARSHAL_BUN`, and run
  `npm ci --workspaces=false` in the plugin's directory. The drivers are in
  `plugins/*/test/conformance-driver.ts` in muster and come over with stage 2.
- **The fixtures are constructed, not recorded.** The Claude fixtures follow
  Claude Code's documented hook input. The Codex fixtures follow the frames the
  bridge's own tests use, because Codex isn't installed where they were
  written. Recording both is below. The shim reads only a few fields from each
  (the ids and cwd), so the risk is small.
- **The Postgres cells need a database** (MUS-76): `CONFORMANCE_POSTGRES_URL`,
  in which each daemon gets its own events table (`conformance_…`) and leaves it
  behind. Without one they show `no postgres`. A local run therefore
  accumulates `conformance_*` tables; drop them with
  `psql "$CONFORMANCE_POSTGRES_URL" -Atc "select 'drop table ' || tablename || ';' from pg_tables where tablename like 'conformance\_%'" | psql "$CONFORMANCE_POSTGRES_URL"`. CI runs a `postgres:16` service
  and sets `CONFORMANCE_REQUIRE_POSTGRES`, so there a missing database fails.
- **No harness here uses channels.** The fakes run without the channels flag,
  so the live-push path into Claude isn't a column.
- **The two TUI behaviours above are assumed, not checked.** If Claude Code
  changes what exit 2 from an `asyncRewake` hook does, this suite still
  passes. Only a real-agent run (MUS-68) would show it.

## Re-recording fixtures

Do this when Claude Code changes its hook input, or to replace the constructed
fixtures with recorded ones.

1. In a scratch directory, not a real project, write
   `.claude/settings.json` with one command hook per event the plugin uses
   (`SessionStart`, `Stop`, `UserPromptSubmit`, `PostToolUse`), each running
   `cat > /tmp/hook-<event>.json`.
2. Run `claude --version` and note it. Start Claude there, send one prompt that
   makes one tool call (for example "run `true`"), and let the turn end. This
   runs a model once.
3. Make `crates/shim/tests/fixtures/claude-code-<version>/`. Copy each file in,
   replacing the session id with `{{session_id}}` and the working directory
   with `{{cwd}}`, and remove anything personal, like paths under your home.
4. Set `CLAUDE_CODE_VERSION` in `conformance.rs` to the new version, update the
   fixture README to say the files are recorded, and update the version above.
5. Run the suite. If a cell changes, the change is a finding: report it.

For Pi, the pinned thing is the extension API, not recorded data. When
`@earendil-works/pi-coding-agent` changes in `plugins/marshal-pi/package-lock.json`,
check its `ExtensionAPI` types against the stand-in in `conformance-driver.ts`
(`on`, `sendMessage`, `registerTool`, `registerCommand`, and the events the
extension handles), update the version above, and run the suite.

For opencode, the pinned thing is the plugin API, likewise. When
`@opencode-ai/plugin` changes in `plugins/marshal-opencode/package-lock.json`,
check its `Plugin` input and `Hooks` types against that plugin's
`conformance-driver.ts`, update the version above, and run the suite.

For Codex, with Codex installed:

1. Run `codex --version` and note it. In a scratch directory, start a Codex
   app-server listening on a loopback WebSocket and attach a TUI to it with
   `codex --remote`, as `marshal-shim codex-run` does. Check
   `codex app-server --help` for the listen flag in that version.
2. Connect a WebSocket client to the same port, for example
   `websocat ws://127.0.0.1:<port>`. Send `initialize` and `initialized`, and
   save the `initialize` result and the `thread/started` notification. Send a
   `turn/start` for the thread while a turn is running, and save the error.
   That turn runs a model.
3. For the hook input, add `[[hooks.UserPromptSubmit]]` and
   `[[hooks.PostToolUse]]` hooks that run `cat > /tmp/codex-<event>.json` to
   the scratch directory's Codex config, and run one turn that makes one tool
   call.
4. Make `crates/shim/tests/fixtures/codex-<version>/`, replace the thread id
   with `{{thread_id}}` and the cwd with `{{cwd}}`, set `CODEX_VERSION` in
   `conformance.rs`, and run the suite.

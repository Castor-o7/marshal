# Session binding: with auth on, a connection acts only as its own sessions (MAR-80)

Fork-only. From the watchman on 2026-11-09, out of wise-leopard's map of muster's gate onto myko#129's
hooks ([gate-on-myko-129.md](gate-on-myko-129.md), rows 17 and 18). It's the most direct defence against
attacker 2 of the threat model: one session, prompt-injected or not, claiming to be another.

## What it is

`SessionBinding` (`crates/daemon/src/auth/session_binding.rs`) is a myko-auth `Authorizer` that wraps
whatever authorizer the provider installs and adds one rule: **a principal may act only as the sessions
it registered.** A principal is what the authenticator makes of a connection's token: with MAR-24, a
paired machine.

It's a library type. Nothing in it touches the daemon's startup, flags or `with_auth_provider`. MAR-24
installs it with one line when it installs the provider, so today's plain mode (no provider) is
unchanged by construction.

## What it checks

Once the inner authorizer has allowed a request:

| Request | Rule |
|---|---|
| `RegisterSession`, or a raw `Session` SET (the shim's fallback for an old daemon) | The id is claimed for the caller. An id another principal holds is refused, so a session isn't taken over across principals. The same principal re-registering its id (a restart, MAR-9) is fine |
| `asSession` on SendMessage, BroadcastMessage, ReadMessages, AckMessages, JoinRoom, LeaveRoom | Must be one of the caller's sessions |
| `id` on every `SetSession*` setter (status, activity, current task and the rest) | Must be one of the caller's sessions |
| A client event on `Session` (DEL), `Message` (its sender), `MessageRead`, `RoomMember`, `WakeWatcher` | The session it names must be the caller's |
| A client event on `SessionIdentity`, `SessionNickname` or `Succession` | Refused to every client: they're the daemon's own |
| An anonymous caller naming any session | Refused |
| Everything else | Left to the inner authorizer |

A refusal is `AuthError::Forbidden`. myko answers a command with a `command-error` whose message is
`permission denied` and keeps the connection open; a refused event is logged and not applied. A
connection with no `asSession` still resolves its own session from its connection, as today.

## Sessions the hook listener registers

Codex threads, and Claude sessions on `/hook/session-start`, are registered through the hook listener
(port 6156), which is outside myko. So they'd be nobody's. The binding takes an optional **local
principal**, the daemon host's own token, and `bind_local(session)` in the hook listener binds each
hook-registered session to it. The Codex MCP shim, connecting with that token, acts for its threads; a
remote principal can't act for them, and the local principal can't act for a session a remote one
registered over myko.

**This is a loopback trust boundary, made explicit.** Any process on the daemon's machine that holds
the local token can act for every hook-registered session. That's the trust plain mode already gives
all of loopback, but now it's one file: on a shared machine the token file must be owner-only (0600).
Where that file lives is MAR-81's (the hook-listener token, which MAR-80 doesn't cover: the hook
listener itself still takes any loopback request).

With **no local principal configured**, hook-registered sessions stay unowned, and under auth nothing
can act for them. That fails closed, by design: trust-on-first-use (the first principal to name an
unowned session gets it) was considered and rejected.

## Cleanup

A binding goes when its session does. The hook listener's `session-end`, the cleanup sweep,
`marshal-daemon forget` and a client's own `Session` DEL all call `release`, so the table can't grow
without bound. A re-registered id after a release is free to whoever registers it.

## Limits

- **Reads and subscriptions aren't covered.** The binding checks commands and client events. What a
  principal may query, view, report on or subscribe to is the inner authorizer's rule. Revocation does
  pass through: `revision` and `session_revision` are the inner's, so an unpair or a policy edit still
  reaches open connections and live subscriptions.
- **A daemon restart under file persistence empties the table while the store keeps its rows.** Until
  a session's owner re-registers it (the shim does on reconnect), the id is unowned, and the first
  principal to claim it wins. A session-end or sweep of a row with no binding is harmless.
- **Succession isn't principal-aware.** A dead principal A's session can be succeeded by a session of
  principal B with the same identity key, which takes its nickname and unread mail (MAR-9, MAR-42).
  That's a follow-up story, reserved by the watchman.

## Tests

- `crates/daemon/src/auth/session_binding.rs`: unit tests of each rule against the authorizer alone.
- `crates/daemon/tests/session_binding.rs`: a myko server with Marshal's entities behind #129's
  provider. Plain mode unchanged; a peer trying another's `asSession` or setter gets `permission
  denied` and its connection stays open; a takeover across principals is refused by command and by raw
  event; the daemon's own entities are refused; a session's end releases its binding; Codex-style
  hook-registered sessions work under the local principal, a remote principal can't act for them, the
  local principal can't act for a remote one's session, and with no local principal they're refused.

## Open to upstream

Declaring `SessionIdentity`, `SessionNickname` and `Succession` `server_owned` (myko#129's
`client_events` registration) would refuse client writes in plain mode too, with no provider needed.
That's a change to plain mode, so it's a MAR-15 question rather than part of this story.

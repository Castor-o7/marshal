# Marshal threat model (Castor fork)

What the fork's opt-in security covers, as built on myko#129, and what it doesn't. Fork-only
(MAR-87); upstream Marshal has none of this yet, and gets it later as a draft PR (MAR-38).
Everything here is off unless the daemon is started with `--tls`: a daemon run as upstream ships
it behaves as upstream's does.

## What's protected

- **The roster.** Who is working, where, on what, and when they were last active.
- **Mail.** Direct messages and room broadcasts between sessions, including what a session is
  told to do. Since upstream #164, a message to an idle session wakes it and starts a turn, so a forged
  message is an instruction to an agent nobody is watching.
- **Identity.** Which session is which: a session's id, nickname, host and operator, and the
  succession of a restarted session to its predecessor's name and unread mail.
- **The daemon's own secrets.** The TLS private key, the pairing tokens (as hashes) and the
  local token.

## Who it defends against

Four attackers, all on a single operator's machines or network. These are muster's four, kept.

1. **A web page** open in a browser on the daemon's host or its network. Browsers don't apply
   CORS to WebSocket upgrades, so a page can open `ws://127.0.0.1:6155` and act as a session,
   and send simple requests to the hook listener on `:6156`.
2. **One prompt-injected session**, which tries to act as another session: read its mail, send
   as it, end it, or take its name.
3. **Another device on the LAN**, once the daemon is exposed off loopback so sessions on other
   machines can reach it.
4. **One lost or stolen machine** that was trusted, which must be removable on its own, without
   touching the others.

Out of scope: other accounts on the same machine (the local token's file is owner-only, which is
the whole of that defence), the open internet, and root on the daemon's host.

## Which mode this covers

| Mode | How it's started | Who may connect |
|---|---|---|
| Plain (upstream's behaviour) | `marshal-daemon`, or `--plain` | Anyone who can reach the ports |
| Gated | `marshal-daemon --tls` | Paired machines and the daemon host's own shims |

With machines paired, the daemon refuses to start without being told which: `--tls` or `--plain`.
A store that can't be read refuses to start too.

## The surfaces, gated

| Surface | Port | Gate |
|---|---|---|
| myko's WebSocket and MCP routes | `MARSHAL_BIND` (6155) | TLS in front (MAR-23); myko#129's provider admits only a bearer the pairing store knows (MAR-24); `SessionBinding` decides what a connection may do as whom (MAR-80) |
| The hook listener, `/hook/*` | `MARSHAL_HOOK_BIND` (6156) | TLS in front (MAR-23); every request carries a bearer the store knows, else 401; a hook for a session another principal registered gets 403 (MAR-81) |
| `/pair` on the hook listener | 6156 | TLS; a one-time, expiring invite secret, redeemed once (MAR-24) |
| myko's own listener | `MARSHAL_MYKO_BIND` (127.0.0.1:6157) | Loopback only, refused if not; reachable only through the TLS front or from the host |

## How each attacker is handled

**A web page.** The TLS front serves a self-signed certificate that clients pin; a page has no
pin and no token, so its upgrade is refused by myko with 401 before any frame, and its hook POST
is refused the same way. (`Origin` is not checked: with auth on, a page can't present a token,
which is the defence; in plain mode nothing stops it, as upstream.)

**A prompt-injected session.** `SessionBinding` (MAR-80) ties every session to the principal that
registered it for writes and actions (reads aren't scoped; see the gaps): a connection may register, SET and act (`asSession`, `SetSession*`, `Message` as
sender, `MessageRead`, `RoomMember`, `WakeWatcher`) only for its own sessions, and may not write the
daemon's own entities (`SessionIdentity`, `SessionNickname`, `Succession`). The hook listener binds
a hook-registered session to the token's owner and refuses other principals' hooks for it, before
anything is written (MAR-81). Succession stays within a principal (MAR-83). A paired machine's
sessions carry the pairing's name as their host, whatever the client declares (MAR-85). A pasted
pairing code or token is redacted before a message is stored (MAR-86). The daemon host's own
sessions share one principal, `(local)`: on that host, one session can still act as another, as
in plain mode.

**Another LAN device.** Without a paired token it gets 401 on every route; with a guessed pin its
TLS handshake fails. The certificate's fingerprint, not a CA, is what clients check. A token is
never sent in clear: the shim presents it only over TLS or to loopback.

**A stolen machine.** `marshal-daemon unpair <name>` forgets its token; its open connections close
within about a second (the token cache plus myko's recheck) and its next connection gets 401.
Pairing the same name again does the same to the old token. Only hashes of tokens are stored, so
the daemon's files can't be used to connect.

## Known gaps

Honest, and each with where it closes.

- **Reads aren't scoped.** `SessionBinding` governs what a connection may write and act as, not
  what it may read: the `GetAllMessages` and `GetAllSessions` queries and subscriptions give every
  valid principal every inbox and the whole roster (MAR-80's stated limit). On a one-operator
  network that's the operator's own traffic; on anything else it isn't acceptable. MAR-90:
  per-principal read scoping, off unless configured.

- **After a daemon restart, who holds which session is forgotten** while the sessions persist (the
  binding table is in memory). Until each owner's next hook, any paired token could claim a
  surviving id. MAR-83's succession work is where this closes.
- **No connection limits and no send allowance.** One paired machine can open as many connections
  and send as many messages as it likes. MAR-88, off unless configured.
- **No sender allow-list.** Every paired machine may send to every session. MAR-89, off unless
  configured.
- **The daemon host's own sessions are one principal.** A prompt-injected session on the host
  can act as another host session. Per-session tokens on the host would close it; not planned.
- **myko's loopback port can be taken before myko binds it** (it binds the port itself and says
  nothing); the TLS front waits for it to answer, but can't tell myko from a squatter. MAR-40.
- **A paired client's raw `Session` SET may blank its attested host** (not forge one); the
  command and hook paths fill it again on the next registration. Accepted.
- **TLS is terminated by a forwarder, not by myko.** The forwarded connection to myko is plain, on
  loopback. Anyone with loopback access to the host is already inside the local trust boundary.
- **The local token is a loopback trust boundary.** Its file is owner-only; a shared host where
  another account can read the operator's config directory has no defence here.
- **Not yet run outside loopback tests**: no IPv6 run, no Windows run of TLS or pairing, not yet on
  Murph with a real paired machine.

# The gate stories on myko#129's hooks (MAR-76)

Fork-only, docs only. For each piece of muster's gate, this says which hook in ignition-is-go/myko#129
(at `f6028125`, read 2026-11-09) it would use, what Marshal would still write itself, and which of
MAR-38's questions it waits on. The point is that MAR-23, MAR-24, MAR-25 and MAR-8 can be revised the
day upstream answers [the auth issue](upstream-auth-issue-draft.md). Nothing here changes code or
defaults, and none of it is a design upstream has agreed to.

The muster pieces come from muster's [upstream-inventory.md](https://github.com/Castor-o7/muster/blob/main/docs/upstream-inventory.md)
(its "Gate" rows), and the tested behaviour of #129 comes from
[myko-129-review-notes.md](myko-129-review-notes.md) (draft #84's four tests).

## The hooks #129 offers

| Hook | Where | What it does |
|---|---|---|
| `MykoServer::builder().with_auth_provider(..)` | `server/src/lib.rs:127` | Turns auth on for the WebSocket and MCP routes. With none, behaviour is unchanged |
| `Authenticator::authenticate` | `auth/src/lib.rs:117` | Turns a bearer (an `Authorization` header, or a `myko.bearer.<base64url>` WebSocket subprotocol) into an identity, before the upgrade. A refusal gets 401 |
| `Authenticator::validate_identity` | `auth/src/lib.rs:134` | Rechecked on the hot path and every 100 ms on an open socket. `Revoked` ends that one session |
| `Authorizer::authorize` | `auth/src/lib.rs:144` | Sees the identity and each request: query, view, report or command with its arguments, a command result, a client event with its proposed item |
| `Authorizer::authorize_anonymous`, `with_anonymous_access` | `auth/src/lib.rs:153, 216` | Admission without a token. Off unless asked for, or unless a linked crate registers a public item |
| `Authorizer::revision` / `session_revision` | `auth/src/lib.rs:158, 168` | A policy change invalidates only the identities whose revision changed |
| `with_live_permissions` | `auth/src/lib.rs:208` | Rechecks live subscriptions after a policy change instead of closing the socket |
| `OAuthResource` (MCP OAuth discovery) | `auth/src/oauth.rs` | Protected-resource metadata for MCP HTTP clients. When it's set, MCP never admits anonymously |
| `CredentialProvider` and `MykoClient::set_credential_provider` | `auth/src/lib.rs:408`, `core/src/client/mod.rs:1746` | The client side: the shim gets a token at each connection attempt |

What #129 doesn't have: TLS (the router takes a plain `TcpStream`, `server/src/router.rs:238`), an
`Origin` check, connection or rate limits, a credential store other than Auth0's, and anything for
Marshal's hook listener (`crates/daemon/src/http_listener.rs`), which is outside myko.

## Muster's gate, piece by piece

The MAR-38 questions are numbered as in the draft: **Q1** whether upstream wants opt-in auth at all,
**Q2** whether to build it on #129 and whether #129's API is settled, **Q3** whether a daemon with
paired machines refuses plain mode or only warns, **Q4** whether refusing a browser `Origin` is
acceptable as a default on its own. Every row waits on Q1 and Q2. Only the extra ones are listed.

| Muster piece (inventory row) | #129 hook | What Marshal still writes | Waits on |
|---|---|---|---|
| TLS gate, `--tls` / `--plain` (4, MUS-64 #89, #92) | None. TLS sits in front of myko's listener | A TLS listener with a pinned certificate that forwards to myko on loopback, the `--tls` and `--plain` flags, and the shim's pin. Or a myko change so the router serves any stream it's handed, not only a `TcpStream` | Q3; whether upstream wants TLS in the daemon or left to deployment |
| Per-machine pairing, single-use expiring codes (4, #73) | `Authenticator::authenticate` admits a paired machine's token; `validate_identity` refuses one that's unpaired | The pairing store (`~/.config/marshal/pairing`), code issue and redeem, `pair`/`unpair` in the shim, and a `CredentialProvider` in the shim that reads its token | Q3 |
| Fail closed when pairings can't be read (MUS-85 #119) | `authenticate` returns `Unavailable` (503) | Reading the store at start-up and refusing plain mode when pairings exist | Q3 |
| Session binding: a connection acts only as its own sessions (17, 18) | `Authorizer::authorize` on `Execute { Command }`, comparing `asSession` with the token's sessions; tested in #84 | The table of which sessions each token registered, filled at `RegisterSession` | — |
| WebSocket frame filter for paired machines (19) | `authorize` on `ClientEvent` sees each proposed item; on `Execute` each query, view and report | The rules themselves (which entity types a machine may write, and which sessions it may read as). Most of the filter becomes an `Authorizer` | — |
| Attest a machine's host and operator; close gate bypasses (22, 30) | The identity's `principal` and `claims` carry what pairing recorded; `authorize` can reject a `Session` event whose host or operator disagrees | Setting `recorded` host and operator from the identity rather than the client. The mention-banner fix stands alone | — |
| Gate e2e after unpair (31, 32) | `validate_identity` → `Revoked` closes the socket (#84's revoke test) | The e2e for Marshal's own pairing, and clearing the unpaired machine's sessions | — |
| Local token on the hook listener; local gate in front of myko (73) | Nothing for the hook listener, which is outside myko. In front of myko the local gate isn't needed: myko checks the token itself | A token check in `http_listener.rs`, which can call the same `Authenticator`; local tools present the local token as a bearer | — |
| Myko's listener admits only what a gate forwards (77, the vendored `RequestFilter`) | `with_auth_provider` replaces it: every connection, forwarded or not, must carry a token myko checks | Nothing, unless TLS is done by forwarding. Then forwarded connections all come from loopback, so per-IP limits must happen in the TLS listener | — |
| Keep pairing secrets out of sessions (74) | None | The redaction in send and broadcast, as in muster | — |
| Rate limits per session, connection limits per listener (79, 80) | `authorize` can refuse a command over its allowance; connection limits need the listener | The allowance (per machine, from the identity) and the limits on each listener | Whether upstream wants limits only with auth on |
| Operator controls: sender allow-list (81) | `authorize` on `SendMessage` and `BroadcastMessage` | The allow-list and its control file. The wake pause was dropped with MAR-11 | — |
| Say when the local token is refused (MUS-83 #117) | A 401 with `WWW-Authenticate: Bearer` comes back before the upgrade | The shim's message naming the token file | — |
| Refuse browser `Origin`s | None | A check before the upgrade, for the WebSocket and the hook listener. With auth on, it's a second line, since a page has no token | Q4. As a default on its own it's a MAR-15 item |
| Threat model, `SECURITY.md` (51, 83) | — | The documents, rewritten for #129 | — |

## What this means for the gate stories

Proposals only. They're applied when MAR-38 is answered (its done-when says the gate stories are
revised to match the maintainers' answers).

- **MAR-23 (opt-in TLS)** stays, and stays the first piece. #129 doesn't touch it. Its open
  question is now where TLS lives: a TLS listener in the daemon that forwards to myko on loopback (as
  muster did), or a myko change so its router serves a TLS stream it's handed. MAR-40's ask (a
  listener the caller binds) is related, but it's still TCP, so it wouldn't be enough by itself.
- **MAR-24 (pairing)** becomes a Marshal `Authenticator` with a pairing store, plus a
  `CredentialProvider` in the shim. Its refusals and revocation come from #129 (tested in #84), so the
  story shrinks to the store, the codes and the commands.
- **MAR-25 (myko's listener behind the gate)** is mostly superseded. muster needed a patched
  myko-server so its listener would admit only forwarded requests. With `with_auth_provider`, myko
  checks the token itself, so nothing needs filtering. What's left belongs in MAR-23 if TLS forwards
  to loopback: bind myko to loopback only, and keep per-IP limits in front. Proposal: fold MAR-25 into
  MAR-23 and drop it ("Dropped:"), once upstream answers Q2.
- **MAR-8 (TLS restart takeover)** is unchanged. It's about TLS mode's restart timing, and it waits
  on MAR-23.
- **New, once upstream says yes:** session binding as an `Authorizer` (the `asSession` check #84
  prototyped), and the hook listener's token. Both are small, and both are the most direct defence
  against a prompt-injected session, attacker 2.

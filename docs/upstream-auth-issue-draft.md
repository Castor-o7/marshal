# Draft: upstream design issue for opt-in authentication (MAR-38)

Fork-only. This is the text Josh may file on ignition-is-go/marshal, after he has read and changed
it. Nothing here has been sent. Title and body follow the line.

---

**Title:** Design discussion: opt-in authentication for the daemon, built on myko#129

## Summary

I'd like to help Marshal be safe to run when agents work unattended, without changing anything for
someone who runs it the way it runs today. Before writing any code, I'd like to agree on the shape
with you. Here's the shape I'd propose: **opt-in authentication for the daemon, built on myko's own
auth provider and machine credentials from myko#129**, rather than a separate gate in front of it.

## Why

Today, any client that can reach the daemon's WebSocket can act as any session. That's fine on a
trusted machine with a person watching. It matters more once a message can wake an idle agent and
start a turn (#164), because a forged message is then an instruction to an agent nobody is
watching.

Four attackers I think are worth defending against, all on a single operator's machines:

1. **A web page open in a browser** on the daemon's host or the LAN. Browsers don't apply CORS to
   WebSocket upgrades, so a page can open `ws://127.0.0.1:6155` and act as a session. It can also
   send simple requests to the hook listener on `127.0.0.1:6156`.
2. **One prompt-injected session**, which can claim to be another session.
3. **Another device on the LAN.** The daemon binds loopback by default, but sessions on other
   machines only work once `MARSHAL_BIND` exposes it, and then anything on that network can act as
   a session. (The README says the default is `0.0.0.0:6155`. The code says `127.0.0.1:6155`. I've
   sent a small README fix separately.)
4. **One lost or stolen machine** that was trusted, which should be removable on its own.

Out of scope: other users with accounts on the same machine, the open internet, and root on the
daemon's host.

## Proposal

- **Off by default.** Plain mode stays exactly as it is. A flag (say `marshal-daemon --auth`)
  turns it on.
- **Built on myko#129.** Plug into its `with_auth_provider`, with a Marshal `Authenticator` and
  `Authorizer`, so there's no vendored myko-server. As I read #129 (at `fcdbc52b`), it gives the
  hooks for this: bearer admission, an authorizer that sees each command's arguments, and revoking
  one credential through `validate_identity`. The credentials, the session rule and TLS would be
  Marshal's to write.
- **Pairing a machine.** A one-time code on the daemon's host yields a local token for that
  machine. Each token can be revoked by itself, without disconnecting everyone else.
- **Sessions bound to their machine.** A connection may act only as the sessions its token
  registered: the authorizer checks `asSession` against the token's sessions. The shim and local
  tools present a token too, and so does the hook listener, which is outside myko.
- **TLS for the network port** when auth is on, with a pinned certificate, so a token can't be
  sniffed on the LAN. #129 leaves TLS to whoever deploys it, so this would sit in front of myko's
  listener or wrap it.
- **Refuse browser origins** on the WebSocket and hook listener when auth is on.
- **Failing safe.** Once machines are paired, the daemon shouldn't quietly come up open because a
  flag was forgotten. It could refuse to start without an explicit `--plain`. That's your call.

## What I can bring

I've run a working version of this on my own machines for a few weeks, in a fork (a TLS gate,
per-machine pairing, a session frame filter and a written threat model). It was built on a patched
myko-server, which is the part I don't want to bring here. So I'd port the behaviour and its tests
onto myko#129 instead, in small PRs: first tests and docs, then the opt-in pieces one at a time.

I've read myko#129 against these four cases and left notes on the PR, and I'm glad to help test it.

## Questions for you

1. Is opt-in authentication something you want in Marshal at all, or would you rather it stayed out?
2. Is building it on myko#129 the direction you'd choose, and is that PR's API settled enough to build on?
3. Should a daemon with paired machines refuse to start in plain mode, or only warn?
4. Would refusing a browser `Origin` on the WebSocket be acceptable as a default, separately from
   the rest? It's the one change that would help people who never turn auth on.

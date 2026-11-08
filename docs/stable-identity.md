# Stable identity across restarts

MAR-9, ported from muster's MUS-3 (88ab365) and MUS-92 (551a8c7). A session's nickname used to belong to its session id. A Claude
session started again in the same directory, or a new Codex thread, gets a new
id, so it got a new nickname. The old row lingered with the old one, and mail
to the old nickname went nowhere. Now a new session takes over a dead
predecessor's nickname. The code is `crates/daemon/src/succession.rs`.

## Which sessions match

Each session has an identity key, recorded when it's first named:

- **By default**, its harness and working directory, on the same machine for
  the same operator: `cwd:claude|/home/me/project|me@example.com|a1`.
- **`MARSHAL_IDENTITY=<role>`** declares a role instead, such as `watchman`.
  A role matches across working directories.
- **`MARSHAL_IDENTITY=none`** opts out. The session gets a fresh nickname and
  is never taken over.

The shim's hooks and MCP shim send `MARSHAL_IDENTITY` from the session's
environment. Only Claude and Codex report a harness so far, so the plugins
have no inferred identity yet.

## When a predecessor counts as dead

A wrong takeover hands a live session's mail to another session, while a
missed one only costs a fresh nickname. So the rules lean towards missing.

| The predecessor's row | Dead when |
|---|---|
| Had a client (a Claude MCP shim) | The client left at least the grace ago (`MARSHAL_SUCCESSION_GRACE_SECS`, 30 s), it has no waiter, and it hasn't hooked since the client dropped. |
| Never had a client (hooks only) | It has no waiter, and nothing for the quiet time (`MARSHAL_SUCCESSION_QUIET_SECS`, 600 s). |
| A Codex row still on the roster | Never, until MUS-77 gives the daemon a way to see the bridge's threads. |
| Gone | This daemon saw it go: Codex's SessionEnd, or a reap. After a daemon restart every row is gone, and none counts as dead until it's seen again. |

A role is never taken over while any session holding it is live.

## Late takeovers

Claude starts again within seconds, while its predecessor's client is still
inside the grace. Then the new session gets a fresh nickname first. For its
first 10 minutes (`MARSHAL_SUCCESSION_WINDOW_SECS`), the daemon checks again
on every sweep. Once the predecessor counts as dead:

- the new session takes its nickname;
- its fresh nickname keeps reaching it for 10 minutes
  (`MARSHAL_SUCCESSION_ALIAS_SECS`), since peers may have used it;
- its next prompt or tool hook says `Your Marshal nickname is now …`, because
  the session block it printed at start named the fresh one.

A predecessor that still looks live only because its client hasn't closed
yet, or its waiter's record hasn't ended, is checked again too, but only for
60 seconds from the new session's registration (`MARSHAL_SUCCESSION_SETTLE_SECS`,
MUS-92). That catches a new session that starts before the old one's socket
has closed. It still has to turn dead by the rules above. A sibling that's
still live after 60 seconds keeps its name.

## What a takeover does

In one step, the daemon:

- re-keys the nickname to the new session;
- moves the predecessor's ad hoc room memberships to it, keeping when the
  predecessor joined;
- retires the predecessor's row (its mail is kept, MUS-46);
- writes a `Succession` record: who took over from whom, the nickname, the
  key, why the predecessor counted as dead, and any alias.

It also hands the predecessor's unread direct mail to the successor (MAR-42,
from muster's MUS-47): each such message is readdressed to the successor, so
its next hook shows it once, as its own, and the record lists the ids. Mail
the predecessor had read stays in its history. Mail sent afterwards to the
old id, the old nickname or a late takeover's alias reaches the successor
through addressing, below.

One case can still show a message twice. The predecessor's waiter printed
it, then its ack was lost, and then it died. The mail is still unread, so the
successor gets it as well. That's the at-least-once rule every Marshal
delivery follows.

## Default change

Taking over by harness and working directory is on by default, which makes it
a default change: it's listed in the defaults issue (MAR-15), not offered
upstream as a PR. `MARSHAL_IDENTITY=none` turns it off for a session.

## Addressing

`send_message` resolves a name in this order:

1. session id;
2. nickname;
3. an alias from a late takeover;
4. an old session id, through its succession records;
5. a declared role, if exactly one session claims it (two claimants are
   refused, and the sender is told to name one);
6. operator;
7. session id prefix.

The roster shows a session's harness and its `declared_role`. A role is a
claim, not attested: any session that can reach the daemon can declare one, so
the roster shows it as declared, and a role is never handed to a new session
while a session holding it is live.

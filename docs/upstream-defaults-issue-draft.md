# Draft: upstream issue listing the fork's default changes (MAR-15)

**Fork-only, and unsent.** Josh may file this text on ignition-is-go/marshal once he has read and
changed it. Nothing here has been sent upstream, and no PR goes with it: upstream decides each
change, and we send code only for the ones it asks for. The commissioner (cobalt-bream) agrees the
draft first, as MAR-15's "Done when" requires. The title and body follow the line.

Status of each change on the fork as of 2026-11-08: **merged** means on Castor-o7/marshal `main`.
**In review** means a PR is open and the change isn't on `main` yet; its line says which head was
read. Update the commits before filing.

---

**Title:** Proposal: default changes from running Marshal unattended, for you to take or leave

## Summary

We've been running Marshal on our own fork with agents left working unattended, many sessions at
once, across restarts. Most of what we changed is additive and we'll offer it as small PRs. The
changes below are different: each one changes what an unchanged install does. That's your call,
not ours, so here they are in one place before any code: what changes, why, how to opt out where
there's a way, and the risk we see for your users. Take any subset, or none. For each one you want,
we'll send a PR cut from upstream `main`.

## The changes

| # | Change | Fork status | Opt-out |
|---|---|---|---|
| 1 | A shim's first write waits until its session is registered, and one refused as unregistered is retried once | merged | none |
| 2 | A session holding a `/hook/wait` isn't reaped when its WebSocket drops | merged | none |
| 3 | A restarted session in the same harness and directory takes over its predecessor's nickname | merged | `MARSHAL_IDENTITY=none` |
| 4 | A restarted session also takes over its predecessor's unread mail | in review | `MARSHAL_IDENTITY=none` |
| 5 | Ended sessions are removed from rooms, and broadcasts count only members on the roster | merged | none |
| 6 | `current_task` follows a checkout while it still holds the branch's default | in review | set your own with `set_status` |
| 7 | A blank `MARSHAL_DAEMON_ADDRESS` or `MYKO_ADDRESS` counts as unset | merged | none needed |
| 8 | An unknown `MARSHAL_PERSIST` value stops startup | in review | don't set it |

Proposed and not built on the fork yet: a refused empty `to`, a per-session send rate limit,
keeping a session while its hooks fire, a larger hook body, and a resource read that waits for
registration (listed at the end).

### 1. Hold a first write until the session is registered, and retry an unregistered refusal once

- **Before:** an agent's first tool call can reach the daemon before the shim's own session
  registration does. The daemon finds a write's caller by the connection's bound `Session` row, so
  the write fails with `caller … has no session on the roster`. The same happens when the row is
  still bound to an earlier connection that died.
- **After:** each write (not under Codex, which names its caller with `asSession`) waits up to 5 s
  for this session's bound row. If the row isn't there after 1 s, the shim sends the session once
  more. A write the daemon refuses as unregistered stored nothing, so the shim sends the session
  again, waits 750 ms and retries it once. Any other refusal is returned unchanged.
- **Why:** the connect-time registration is fire-and-forget, and the background check only re-sends
  after missing the row twice at 5 s. So an agent's first send after a restart failed, and agents
  read that as "Marshal is broken".
- **Fork:** MAR-43, Castor-o7/marshal#38, merged at `6fb0bfa56cdd9df056a34d3934b2019b2d335a2b`.
  A follow-up, MAR-18 (#53, in review), gives the refusal a stable code, `[unregistered]`, so the
  retry doesn't depend on the message's wording.
- **Opt-out:** none.
- **Risk for upstream users:** a write to a daemon that's genuinely unreachable now takes up to 5 s
  longer to fail. The retry can't duplicate a message, because the daemon stores nothing for a
  write it refuses as unregistered.

### 2. Keep a session through a dropped connection while its waiter is held

- **Before:** the sweeper reaped a WebSocket session once its connection had been gone past the
  60 s grace, even while that session's Claude Code `/hook/wait` was still held.
- **After:** a disconnected WS session that holds a current wait (one whose `until` is in the future)
  is treated like a live one, and its grace starts again when the wait ends. A wake record further
  out than the hook listener ever writes doesn't count, so a forged record can't keep a dead
  session forever.
- **Why:** an idle Claude session's MCP connection can drop while its wait hook is still the thing
  that would wake it. Reaping it lost its nickname and rooms, and the next wake found no session.
- **Fork:** MAR-6, Castor-o7/marshal#27, merged at `1272636b4bc84ba417268952c8d90fa4fc46a8dc`.
- **Opt-out:** none.
- **Risk:** a session whose wait is held but whose agent is gone stays on the roster until the wait
  ends (the hook listener's horizon), rather than 60 s after its connection drops. Without auth, a
  connected client can keep refreshing a wake record within that horizon. That's no worse than
  today, where any client can already write a session row.

### 3. A restarted session keeps its nickname

- **Before:** every restart gets a new nickname, so peers and people lose track of who's who. Mail
  addressed by the old nickname finds nobody.
- **After:** a session's identity is its harness plus working directory (with operator and host),
  or a role named by `MARSHAL_IDENTITY=<role>`. When a new session's identity matches one whose
  predecessor is dead, it takes over that nickname. "Dead" is strict: the client has been gone past
  a grace with no waiter or hook, or has been quiet with no client, or was seen ending. A send
  reaches the successor by role (when only one session claims it), by the old session id, and by a
  late takeover's alias. The roster shows `declared_role` and `harness`.
- **Why:** with agents restarted for compaction, crashes and upgrades, a stable name is what lets
  the rest of a team keep working with them.
- **Fork:** MAR-9 (and the quick-restart fix from muster's MUS-92), Castor-o7/marshal#31, merged at
  `83f52026ab93b43c664a49d0377f329cc6fb9905`. Design in `docs/stable-identity.md`.
- **Opt-out:** `MARSHAL_IDENTITY=none` for a session that should never take over or be taken over.
- **Risk:** two sessions run deliberately in the same harness and directory share an identity, so
  only one at a time holds the nickname, and the second gets a fresh one while the first is alive.
  A send by role to a role two sessions claim is refused rather than guessed.

### 4. A restarted session takes over its predecessor's unread mail

- **Before (even with 3):** unread direct mail stays addressed to the dead session's id, where
  nobody will read it.
- **After:** on a takeover, the predecessor's unread direct messages are readdressed to the
  successor, which shows each once at its next hook, as its own. Mail the predecessor had read stays
  with it. The takeover record lists the ids it moved.
- **Why:** without it, a restart silently strands whatever arrived while the old session was dying.
- **Fork:** MAR-42 (from muster's MUS-47), Castor-o7/marshal#43, in review at head
  `17dd317f114c84f269a122d35b1bfa91ee3dbe04`. It depends on 3.
- **Opt-out:** `MARSHAL_IDENTITY=none`, since there's no takeover without an identity.
- **Risk:** at-least-once, not exactly-once. If the old session's waiter showed a message, lost the
  ack and then died, the successor shows it again.

### 5. Drop ended sessions from rooms, and count only live members in a broadcast

- **Before:** `del_by_id` doesn't cascade a `Session`'s `RoomMember` rows, so after a reap, or a
  session ending, its memberships stayed, `everyone` included. Broadcasts counted those members and
  reported each as "session vanished from roster mid-broadcast".
- **After:** the sweeper removes memberships whose session is no longer on the roster, and
  broadcasts count only members still on it. A session that comes back is put back in its auto-rooms
  (`everyone`, `project:`, `op:`) as today, but must rejoin ad hoc rooms itself, unless a takeover
  (3) within a day moves them for it. The sweeper now starts only once the store has loaded, so a
  restart with Postgres can't sweep memberships whose sessions haven't replayed yet.
- **Why:** stale members made every broadcast report failures, and `everyone` listed sessions that
  had been gone for days.
- **Fork:** MAR-19 (from muster's MUS-58), Castor-o7/marshal#30, merged at
  `db884b2efcf08a8b2cbb3e2cfbc1b6d8cad014ff`.
- **Opt-out:** none.
- **Risk:** this is the one users would notice most. A session that's reaped and comes back without
  a takeover is no longer in the ad hoc rooms it joined. Until a takeover or a day passes, a reaped
  predecessor can still be listed in an ad hoc room in `marshal://rooms`, though broadcasts skip it.

### 6. `current_task` follows the branch while it holds the default

- **Before:** the shim sets a session's default `current_task` from its branch at startup, and a
  later `git checkout` leaves it naming the old branch.
- **After:** on a checkout, the shim's HEAD watcher re-derives the task, but only while it still
  holds the default for the branch being left. Text set with `set_status`, an explicit clear, and a
  workspace-derived name are left alone.
- **Why:** the roster showed every long-lived session working on whatever it had started on.
- **Fork:** MAR-20 (from muster's MUS-26), Castor-o7/marshal#34, in review at head
  `5f9bcff5b7a0a2d1355f4fb3a66482053cca95b1`.
- **Opt-out:** set your own task with `set_status`, and it isn't touched.
- **Risk:** low. Text that happens to equal the old branch's default is taken for the default and
  changed.

### 7. A blank daemon address counts as unset

- **Before:** `MARSHAL_DAEMON_ADDRESS=` (set but empty), or the same for `MYKO_ADDRESS`, beat the
  fallback, and the hook URL then fell to `localhost:6156`.
- **After:** a blank value is treated as unset, so the address order carries on to the next source.
- **Why:** a plugin's `.mcp.json` substitution of an unset variable can produce an empty string.
- **Fork:** part of MAR-56, Castor-o7/marshal#50, merged at
  `d789b44679396c5e3aa6e6a2601fd45f8e6fb8b9`. The rest of that PR (a test-only guard against
  reaching a live daemon) is additive.
- **Opt-out:** none needed.
- **Risk:** none we can see, unless someone relied on an empty address meaning something.

### 8. An unknown `MARSHAL_PERSIST` stops startup

- **Before:** `MARSHAL_PERSIST` meant nothing.
- **After:** with the fork's opt-in file store (MAR-53, additive), `MARSHAL_PERSIST=file` keeps
  messages, read state and rooms across a restart without Postgres. Any other value stops startup
  with an error, so a typo can't silently run ephemeral and lose mail. With Postgres configured,
  the setting is ignored and the startup line says so.
- **Why:** the store is opt-in, and someone who misspells the opt-in would otherwise believe their
  mail survives a restart.
- **Fork:** MAR-53 (from muster's MUS-17), Castor-o7/marshal#55, in review at head
  `20367301e305b43b94c8770cfa7157c57c627db5`. Only the refusal is a default change. The file store
  itself is additive, and we'd offer it separately.
- **Opt-out:** don't set the variable.
- **Risk:** a user who already sets `MARSHAL_PERSIST` to some other value for their own reasons gets
  a daemon that won't start until they remove it.

## Proposed, not built on the fork yet

Each of these changes a default, so we're asking before building:

- **Refuse an empty `to`.** Today an empty recipient falls through to `resolve_recipient`'s id-prefix
  step, which every session matches. When the sender is the only session, the message goes to the
  sender itself; otherwise it's refused as ambiguous, with a confusing message. It would always be refused,
  with a message naming the ways to address a session (muster's MUS-73, `f4bf5c9`).
- **A per-session send rate limit** (muster's MUS-9): a burst of 20, then 30 a minute, so one stuck
  agent can't flood the bus.
- **Keep a session while its hooks are firing** (muster's MUS-54), alongside 2. Hook activity, not
  only a held wait, would count as liveness.
- **A larger hook body.** The hook path cuts a message at 2000 characters (`CONTEXT_BODY_MAX_CHARS`)
  and names where to read the rest. Raising that would change what every agent sees (muster's
  MUS-102).
- **A resource read right after connecting waits for registration**, as a write does under 1
  (muster's MUS-114).

## What we can bring

For each change you want, a PR cut from upstream `main` with its tests, and our conformance suite's
cell for it if there is one. If you'd rather have any of these behind a setting, off by default,
say so, and we'll do it that way.

Inbox quoting (also on our list) is left out here, since we don't have a concrete proposal yet.

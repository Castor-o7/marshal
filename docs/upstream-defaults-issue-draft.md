# Draft: upstream issue listing the fork's default changes (MAR-15)

**Fork-only, and unsent.** Josh may file this text on ignition-is-go/marshal once he has read and
changed it. Nothing here has been sent upstream, and no PR goes with it: upstream decides each
change, and we send code only for the ones it asks for. The commissioner (cobalt-bream) agrees the
draft first, as MAR-15's "Done when" requires. The title and body follow the line.

**File only once every listed change has merged on the fork.** The commissioner's rule (2026-11-08):
upstream shouldn't weigh a moving target. As of 2026-11-08, items 4 and 6 are still **in review**, and
their lines cite the head that was read. Before filing, replace those with the merge commits, or drop
those items.

This supersedes muster's `docs/upstream-issue-defaults.md` (2026-11-05, cobalt-bream's first draft).
Two of that draft's items are handled differently here. Codex threads re-registering after a restart
(MUS-76) is offered as a bug fix, below. Inbox quoting is dropped, as upstream #141 already covers it.

---

**Title:** Proposal: default changes from running Marshal unattended, for you to take or leave

## Summary

We've been running Marshal on our own fork with agents left working unattended, many sessions at
once, across restarts. Most of what we changed is additive and we'll offer it as small PRs. The
changes below are different: each one changes what an unchanged install does. That's your call,
not ours, so here they are in one place before any code: what changes, why, how to opt out where
there's a way, and the risk we see for your users. Take any subset, or none. For each one you want,
we'll send a PR cut from upstream `main`. It joins the queue we already keep of one small PR at a
time, after #168, so nothing here lands as a batch.

## The changes

| # | Change | Fork status | Opt-out |
|---|---|---|---|
| 1 | A shim's first write waits until its session is registered, and one refused as unregistered is retried once | merged | none |
| 2 | A session holding a `/hook/wait` isn't reaped when its WebSocket drops | merged | none |
| 3 | A restarted session in the same harness and directory takes over its predecessor's nickname | merged | `MARSHAL_IDENTITY=none`; could be opt-in instead |
| 4 | A restarted session also takes over its predecessor's unread mail | not final (in review) | as for 3 |
| 5 | Ended sessions are removed from rooms, and broadcasts count only members on the roster | merged | none |
| 6 | `current_task` follows a checkout while it still holds the branch's default | not final (in review) | set your own with `set_status` |

Two bug fixes and one piece of an additive feature aren't default changes, so they're offered as
small PRs, listed after the six. Proposed and not built on the fork yet: a refused empty `to`, a
per-session send rate limit, keeping a session while its hooks fire, a larger hook body, and a
resource read that waits for registration (listed at the end).

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
  A follow-up, MAR-18 (Castor-o7/marshal#53, merged at
  `efb1c6631a57d5da8a4dde8e02adfcbf358269d0`), gives the refusal a stable code, `[unregistered]`, so
  the retry doesn't depend on the message's wording.
- **Opt-out:** none.
- **Risk for upstream users:** the wait is per write, not just the first. A session that never
  registers (misconfigured, or its registration refused) pays it on **every** write: up to 5 s
  waiting for its row (with one resend at 1 s), then, if the daemon refuses the write as
  unregistered, 750 ms and a second attempt. That's up to about 6 s per write before the agent sees
  the error. A daemon that's unreachable fails the same way, up to 5 s later than today. The retry
  can't duplicate a message, because the daemon stores nothing for a write it refuses as
  unregistered.

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
- **Risk:** a session whose wait is held but whose agent is gone stays on the roster while the
  wait is held, plus up to 2 minutes, rather than 60 s after its connection drops. A wake record
  lasts at most 2 minutes past its last refresh (`WAKE_WATCHER_TTL_MS` = 120 000 ms; the listener
  refreshes it every 60 s while the wait holds), and then the usual 60 s grace runs. Without auth, a
  connected client can keep refreshing a record within those 2 minutes. That's no worse than today,
  where any client can already write a session row.

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
  **We'd happily make this opt-in instead**, off by default and turned on with something like
  `MARSHAL_IDENTITY=auto` (or a role), if you'd rather an unchanged install kept today's behaviour.
  Together with 4, this is the largest change in meaning here.
- **Risk:** this changes who receives mail. A new, unrelated session started in the same harness
  and directory after the old one died silently takes its nickname, so mail addressed to that
  nickname, or to the old id, now reaches an agent that may not share the old one's context. Two
  sessions run deliberately in the same harness and directory share an identity, so only one at a
  time holds the nickname, and the second gets a fresh one while the first is alive. A send by role
  to a role two sessions claim is refused rather than guessed.

### 4. A restarted session takes over its predecessor's unread mail

- **Before (even with 3):** unread direct mail stays addressed to the dead session's id, where
  nobody will read it.
- **After:** on a takeover, the predecessor's unread direct messages are readdressed to the
  successor, which shows each once at its next hook, as its own. Mail the predecessor had read stays
  with it. The takeover record lists the ids it moved.
- **Why:** without it, a restart silently strands whatever arrived while the old session was dying.
- **Fork:** MAR-42 (from muster's MUS-47), Castor-o7/marshal#43, in review at head
  `17dd317f114c84f269a122d35b1bfa91ee3dbe04`. It depends on 3.
- **Opt-out:** `MARSHAL_IDENTITY=none`, since there's no takeover without an identity. As with 3,
  we'd make this opt-in if you prefer.
- **Risk:** the largest of the six, with 3. The successor gets the predecessor's unread mail as its
  own, including mail written for context it doesn't have: a new, unrelated agent started in the
  same directory inherits instructions meant for the one that died. It's also at-least-once, not
  exactly-once: if the old session's waiter showed a message, lost the ack and then died, the
  successor shows it again.

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
- **Risk:** the one users would notice first, though 3 and 4 change more. A session that's reaped
  and comes back without a takeover is no longer in the ad hoc rooms it joined. Until a takeover or a day passes, a reaped
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

## Bug fixes and additive pieces, offered as small PRs

These aren't default changes, so they don't need your decision here. They're listed so the issue is
complete:

- **A blank `MARSHAL_DAEMON_ADDRESS` or `MYKO_ADDRESS` counts as unset.** Today an empty string beats
  the fallback, and the hook URL drops to `localhost:6156`. A plugin's `.mcp.json` substitution of an
  unset variable can produce one. Fork: part of MAR-56, Castor-o7/marshal#50, merged at
  `d789b44679396c5e3aa6e6a2601fd45f8e6fb8b9`.
- **Codex threads re-register after a daemon restart.** Upstream's bridge registers a thread once,
  on `thread/started`. After a restart without Postgres the roster is empty, and an idle Codex
  session never sends `thread/started` again, so it stays off the roster until its next hook. The
  fork's bridge keeps the threads it registered and re-registers any missing from the roster, with a
  5 s cooldown. Fork: MAR-7 (from muster's MUS-76), Castor-o7/marshal#5, merged at
  `4ebcf08222f08500fac3c62db37cf5fa41ccf8fd`.
- **An unknown `MARSHAL_PERSIST` value stops startup.** This only means something alongside the
  fork's opt-in file store (MAR-53, Castor-o7/marshal#55, in review), so it's part of that additive
  PR's contract, not a default change here.

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

Inbox quoting, from our first list, is dropped as covered by #141: upstream's hook path already
escapes peer mail and frames it as untrusted.

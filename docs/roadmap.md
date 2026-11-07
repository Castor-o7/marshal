# Marshal roadmap (Castor fork)

Goal: a direct message reaches its recipient once, live, and survives failures, on every harness,
with no launch flags. Then a stronger Marshal overall, and, as a stretch, launching new agent
sessions in their own terminals on demand.

This file lives on the Castor fork (Castor-o7/marshal) only, for Rally (project MAR); never send it
upstream. The operator's planning doc is
[Marshal Roadmap](https://claude.ai/artifact/3QkRvUo5Yxrwf3wt4Shuw1).

IDs are permanent and come from `rally new-story MAR`. Don't reuse or renumber them. A story without a
"Done when" line isn't ready to claim. A tick means the watchman verified the story and its work is
merged on the fork's `main`.

## How work lands

1. Branch from `upstream/main` (ignition-is-go/marshal) for anything that may go upstream; from the
   fork's `main` for fork-only work.
2. Open a PR on Castor-o7/marshal, with the story ID in the title and commits.
3. Ask the watchman for a read at the PR's head, and merge only after it and a green CI.
4. Once fork `main` is green, the lamplighter deploys it as Murph's live daemon.
5. Upstream, later, and only when the operator decides: one small additive PR at a time. Default
   changes go to upstream as one issue (MAR-15), not as PRs.

## Track 1: reliable live delivery

### D1: Claude delivery is exact

- [ ] **MAR-1: D1: Safe retry of a timed-out send (idempotency key).** An optional `idempotencyKey` on
  `SendMessage`; a retry returns the stored message and stores and pushes nothing new; the shim sends
  a key on every send and says how to retry on a timeout. Done when: a retry racing its first attempt
  stores and pushes once, a retry after the recipient changed or left still finds the first send, and
  it's merged on the fork (Castor #2).
- [ ] **MAR-2: D1: Run the fork's CI on GitHub-hosted runners.** Upstream's Linux jobs need
  self-hosted runners, which the fork has none of, so every fork run sat queued. Done when: `check` and
  `windows-cross-check` run on `ubuntu-latest` on the fork, Release stays untouched, and it's merged
  (Castor #3).
- [ ] **MAR-3: D1: Hook inbox never offers a message twice (lease race).** `surface_unread` read unread
  mail before taking the lease lock, so an ack landing between them showed a message twice. Done when:
  the fix from upstream #169 is on the fork's `main` (taken from upstream when it merges, or ported),
  and `hook_inbox_safety` passes repeatedly in CI.
- [ ] **MAR-4: D1: One wake path ending doesn't clear another's record.** From MUS-94. A session has
  one `WakeWatcher` row, and any path's end deletes it. Done when: each wake path keeps its own record,
  ending one leaves the others, and a test shows it.
- [ ] **MAR-5: D1: Idle overnight wake verified, at most 2 model turns an hour.** From MUS-1, on
  Marshal's hooks. Done when: a Claude session idle overnight on Murph's daemon wakes on a message, and
  its idle turns are counted at 2 an hour or fewer. Needs a run: overnight, on Murph.

### D2: survives restarts and drops

- [ ] **MAR-6: D2: Keep a session through a dropped connection while its waiter is held.** The WS reap
  on `STALE_AFTER` doesn't check `WakeWatcher`, so a session still waiting is reaped. A default change:
  it goes in the defaults issue (MAR-15) and is built on the fork. Done when: a session holding
  `/hook/wait` survives its WS dropping past the grace, a test shows it, and the change is listed in
  MAR-15.
- [ ] **MAR-7: D2: Codex threads re-register after a daemon restart.** From MUS-76. The bridge
  registers a thread only on `thread/started`, so an idle Codex session stays off the roster after a
  restart. Done when: the bridge re-registers its threads when its daemon connection comes back, and a
  test restarts the daemon to show it.
- [ ] **MAR-8: D2: TLS restart doesn't miss its takeover.** From MUS-93: an intermittent miss on CI's
  slower runner. Depends on MAR-23. Done when: the restart-keeps-identity cell passes in TLS mode on
  CI 10 times in a row.
- [ ] **MAR-9: D2: Stable nickname across a restart.** From MUS-3. Done when: a session restarted in
  the same harness and directory keeps its nickname, a role declared by environment can be reached
  after a restart, and a test shows both.

### D3: every harness

- [ ] **MAR-10: D3: Plugins ack a push only after the harness takes it.** From MUS-81. Pi, opencode
  and Prime Agent mark a push read as soon as it reaches the connection. Overlaps lobo235's upstream
  plugin PRs: coordinate, don't race. Done when: a push the harness didn't take stays unread for the
  next drain, in each of the three plugins, with a test each.
- [ ] **MAR-11: D3: Resuming wakes re-pushes held mail to push sessions.** From MUS-82. Done when: mail
  held while wakes were paused reaches a push session on resume, without waiting for its next turn,
  and a test shows it.
- [ ] **MAR-12: D3: Show a Codex bridge's wake path on the roster.** From MUS-77. Done when: a
  bridge-watched Codex thread reads `waiting` on the roster and a send to it says `expected`.
- [ ] **MAR-13: D3: OpenCode plugin declares its channel.** From MUS-89. Done when: the plugin reports
  its channel state when it registers, and the roster's `wake` for it matches what it can do.

### D4: proven

- [ ] **MAR-14: D4: Delivery conformance suite in Marshal CI, with restart cells on Postgres.** From
  MUS-69 and MUS-76. The real daemon and shim against a fake harness each, one scenario table. Done
  when: the harness × scenario grid runs in the fork's CI, restart cells run against Postgres, and known
  gaps are listed in the suite.

## Track 2: strengthen Marshal

- [ ] **MAR-15: Strengthen: File the defaults issue upstream.** One issue listing the default changes
  upstream may choose: refuse an empty `to` (MUS-73, fix ready on a local branch), hold a shim's first
  write until it's registered (MUS-72, 78, 79), keep a waiting session through a drop (MAR-6), Codex
  re-register (MAR-7), and optional inbox quoting. Needs the operator: Josh files it. Done when: the
  draft is agreed with the commissioner and the operator has filed it.
- [ ] **MAR-16: Strengthen: cargo-deny dependency audit in CI.** Additive. Done when: `cargo deny check`
  runs in the fork's CI and passes, with each licence exception explained.
- [ ] **MAR-17: Strengthen: Terminal send and roster beyond upstream #168.** Done when: whatever of
  muster's terminal commands #168 doesn't cover is listed, and each item is ported or dropped with a
  reason.
- [ ] **MAR-18: Strengthen: Stable error code for the unregistered refusal.** From MUS-80. The shim
  matches the refusal's text. Done when: the refusal carries a code, the shim matches the code, and a
  test covers both.
- [ ] **MAR-19: Strengthen: Drop ended sessions from rooms.** From MUS-58. Done when: a session that
  ends leaves every room it was in, and `everyone` lists only live sessions, with a test.
- [ ] **MAR-20: Strengthen: Keep current_task fresh.** From MUS-26. Done when: `current_task` follows
  the branch unless the session set its own, with a test.
- [ ] **MAR-21: Strengthen: Tests pick ports without races.** From MUS-88. Done when: no test binds a
  port picked by asking the OS and releasing it, and the e2e suites pass run in parallel.
- [ ] **MAR-22: Strengthen: Keep windows-cross-check within its time limit.** From MUS-91. Done when:
  the job finishes under its limit on 10 consecutive fork runs.
- [ ] **MAR-23: Strengthen: Gate: opt-in TLS.** `marshal-daemon --tls`; plain stays the default.
  Depends on myko#129 (its auth provider). Done when: `--tls` serves pinned TLS, plain mode is
  unchanged, and the gate's e2e checks for TLS pass.
- [ ] **MAR-24: Strengthen: Gate: per-machine pairing.** Depends on MAR-23. Done when: a machine pairs
  with a one-time code, an unpaired machine is refused, and with pairings and neither `--tls` nor
  `--plain` the daemon refuses to start.
- [ ] **MAR-25: Strengthen: Gate: myko's own listener behind the gate.** Depends on MAR-24. Done when:
  myko's listener admits only requests forwarded by the gate, with no vendored myko-server.

## Track 3 (stretch): launch new agent sessions in terminals

- [ ] **MAR-26: Stretch: Launcher: spawn a session in tmux.** `marshal-shim spawn <harness> --cwd <dir>
  [--name <role>]` starts Claude Code or Codex in a detached tmux session. Waits on the operator's
  answers: which terminals and systems, who launches, which harnesses, which machines, and which
  permission mode. Done when: one command on Murph starts a session in tmux that appears on the roster
  within 30 seconds, with no permission-bypass or channels flags.
- [ ] **MAR-27: Stretch: Launcher: join handshake with a spawn token.** Depends on MAR-26. Done when:
  the launcher passes a one-time token, waits with a timeout for a roster row carrying it, and prints
  the new session's nickname or a clear failure.
- [ ] **MAR-28: Stretch: Launcher: send a first task once joined.** Depends on MAR-27. Done when:
  `--task "..."` reaches the new session as a direct message after it joins, and the session wakes and
  acts on it.
- [ ] **MAR-29: Stretch: Launcher: desktop terminal backends.** Depends on MAR-26. Done when: the
  launcher opens a visible window on Linux (`$TERMINAL`, konsole, gnome-terminal or kitty) and on macOS
  (Terminal or iTerm), with the same handshake as tmux.
- [ ] **MAR-30: Stretch: Launcher: list, reattach and stop launched sessions.** Depends on MAR-27.
  Done when: the launcher lists the sessions it started, reattaches to one, and stops one so its roster
  row and rooms are cleared.

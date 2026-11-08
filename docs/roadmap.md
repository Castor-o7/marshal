# Marshal roadmap (Castor fork)

Goal: bring the muster features the ignition-is-go team asked for into Marshal, in the operator's order
(2026-11-06):

1. **Codex live delivery:** an idle Codex session wakes reliably on a direct message, through the
   app-server bridge, as Claude Code already does through its waiting hook.
2. **Spin up agent teams:** start new agent sessions, alone or as a team, in their own terminals from
   one command, each joining the roster ready to work.
3. **Message an agent from a terminal:** `tell <agent> ...` and `all ...` reach a session as if typed
   into its own chat box, and `roster` shows who's there.

Everything else here supports those three, or follows them.

## Direction (operator, 2026-11-08)

Muster is frozen. Castor-o7/marshal is the only active line, and muster is kept as the archive the
ports come from. The ignition-is-go lead's invitation was to help Marshal rather than run a separate
project, and this roadmap takes him at his word.

The goals are security, durability and portability, and Marshal's core concept stays as it is: a
message bus with a roster, for many harnesses. Work sits in one of three layers:

1. **Upstream Marshal, the bus.** Additive or opt-in changes only, so a user who changes nothing
   sees no difference. Default changes go to upstream as one issue (MAR-15).
2. **This fork, where changes get tested.** Changes are proven on Murph before they're offered upstream.
3. **Outside Marshal.** Rally, the watchman and the team process. These never go upstream.

Most of Goals 1–3 and the conformance suite (MAR-14) are built and verified, waiting for their
ticks. Next comes security (MAR-37, MAR-38, then the gate stories), with durability and portability
stories filling the gaps. MAR-14 is test-only, which makes it the most useful thing to offer
upstream next.

This file lives on the Castor fork (Castor-o7/marshal) only, for Rally (project MAR); never send it
upstream. The operator's planning doc is
[Marshal Roadmap](https://claude.ai/artifact/3QkRvUo5Yxrwf3wt4Shuw1).

IDs are permanent and come from `rally new-story MAR`. Don't reuse or renumber them, so they aren't in
numeric order below. A story without a "Done when" line isn't ready to claim. A tick means the watchman
verified the story and its work is merged on the fork's `main`.

## How work lands

1. Branch from `upstream/main` (ignition-is-go/marshal) for anything that may go upstream; from the
   fork's `main` for fork-only work.
2. Open a PR on Castor-o7/marshal, with the story ID in the title and commits.
3. Ask the watchman for a read at the PR's head, and merge only after it and a green CI.
4. Once fork `main` is green, the lamplighter deploys it as Murph's live daemon.
5. Upstream, later, and only when the operator decides: one small additive PR at a time. Default
   changes go to upstream as one issue (MAR-15), not as PRs.

## In flight

- [ ] **MAR-1: Foundation: Safe retry of a timed-out send (idempotency key).** An optional
  `idempotencyKey` on `SendMessage`; a retry returns the stored message and stores and pushes nothing
  new; the shim sends a key on every send and says how to retry on a timeout. Done when: a retry racing
  its first attempt stores and pushes once, a retry after the recipient changed or left still finds the
  first send, and it's merged on the fork (Castor #2).
- [ ] **MAR-2: Foundation: Run the fork's CI on GitHub-hosted runners.** Upstream's Linux jobs need
  self-hosted runners, which the fork has none of, so every fork run sat queued. Done when: `check` and
  `windows-cross-check` run on `ubuntu-latest` on the fork, Release stays untouched, and it's merged
  (Castor #3).

## Goal 1: Codex live delivery

Marshal already wakes an idle Codex session through the app-server bridge's `turn/start`
([codex-live-delivery.md](codex-live-delivery.md)). Muster has since made that path hold up across
restarts and show on the roster; these stories bring that work over.

- [ ] **MAR-7: Goal 1: Codex threads re-register after a daemon restart.** From MUS-76 (muster
  `4425753`). The bridge registers a thread only on `thread/started`, so an idle Codex session stays off
  the roster after a restart, and mail to it can't be addressed. Done when: the bridge re-registers its
  threads when its daemon connection comes back, and a test restarts the daemon to show it.
- [ ] **MAR-12: Goal 1: Show a Codex bridge's wake path on the roster.** From MUS-77 (muster `2e0ae23`).
  The roster calls a bridge-watched Codex session `wake: none` and a send to it says `unobserved`,
  though the bridge will wake it. Done when: a bridge-watched Codex thread reads `waiting` on the roster
  and a send to it says `expected`, with a test.
- [ ] **MAR-4: Goal 1: One wake path ending doesn't clear another's record.** From MUS-94 (muster
  `2fcd2f9`). Matters once MAR-12 gives a Codex session a bridge record beside a hook waiter. Depends
  on MAR-12. Done when: each wake path keeps its own record, ending one leaves the others, and a test
  shows it.
- [ ] **MAR-14: Goal 1: Delivery conformance suite in Marshal CI, with restart cells on Postgres.**
  From MUS-69 and MUS-76. The real daemon and shim against a fake harness each, one scenario table,
  Codex and Claude first. Done when: the harness × scenario grid runs in the fork's CI, the Codex
  idle-wake and restart cells pass, and known gaps are listed in the suite.

## Goal 2: Spin up agent teams

Nothing in Marshal or muster launches a session today. Once one is running, hooks register it, the
waiter or the bridge wakes it, and `tell` (Goal 3) can hand it work, so the missing piece is the
launcher.

- [ ] **MAR-26: Goal 2: Launcher: spawn a session in tmux.** `marshal-shim spawn <harness> --cwd <dir>
  [--name <role>]` starts Claude Code or Codex in a detached tmux session. Operator's answers
  (2026-11-06): both harnesses (Codex through `codex-run`); the harness's default permission mode;
  Murph and tmux only for now; you or an agent may run it. Done when: one command on Murph starts a session in tmux that appears on the roster
  within 30 seconds, with no permission-bypass or channels flags.
- [ ] **MAR-27: Goal 2: Launcher: join handshake with a spawn token.** Depends on MAR-26. Done when:
  the launcher passes a one-time token, waits with a timeout for a roster row carrying it, and prints
  the new session's nickname or a clear failure.
- [ ] **MAR-28: Goal 2: Launcher: send a first task once joined.** Depends on MAR-27. Done when:
  `--task "..."` reaches the new session as a direct message after it joins, and the session wakes and
  acts on it.
- [ ] **MAR-32: Goal 2: Launcher: start a team of sessions from one command.** Depends on MAR-28. A
  team file names each member's harness, directory, role and first task. Done when: one command starts
  every member, each joins the roster and gets its task, and the command reports who joined and who
  didn't.
- [ ] **MAR-30: Goal 2: Launcher: list, reattach and stop launched sessions.** Depends on MAR-27.
  Done when: the launcher lists the sessions it started, reattaches to one, and stops one so its roster
  row and rooms are cleared.
- [ ] **MAR-29: Goal 2: Launcher: desktop terminal backends.** Depends on MAR-26. Josh chose
  (2026-11-06): `$TERMINAL`, then konsole, on Linux; the window attaches to the session's tmux, so
  closing it leaves the session running; macOS is MAR-35. Done when: `spawn --window` opens a visible
  window on Murph attached to the session, with the same handshake as tmux.
- [ ] **MAR-35: Goal 2: Launcher: macOS terminal windows (Terminal, iTerm).** Depends on MAR-29.
  Done when: `spawn --window` opens a Terminal or iTerm window attached to the session on a Mac, tested
  on one.

## Goal 3: Message an agent from a terminal

- [ ] **MAR-31: Goal 3: Terminal tell, all and roster on the fork.** Muster's `tell`, `all` and
  `roster`, as already cut for upstream on `port/terminal-cmds-v3` (upstream #168, open). A direct
  message from a terminal wakes an idle session like any other. Done when: `marshal-shim tell <name>
  <message>`, `marshal-shim all <message>` and `marshal-shim roster` work against the fork's daemon on
  Murph, and the branch is merged on the fork.
- [ ] **MAR-17: Goal 3: Terminal send and roster beyond upstream #168.** Done when: whatever of
  muster's terminal commands #168 doesn't cover is listed, and each item is ported or dropped with a
  reason. The list and Josh's choices (2026-11-06) are in `docs/muster-terminal-gap.md`: `doctor` is
  ported as MAR-36, and the rest is dropped or already covered.
- [ ] **MAR-36: Goal 3: `marshal-shim doctor`, a read-only health check.** From muster's `doctor`
  (MUS-15), without its credential check. Done when: `marshal-shim doctor` reports, one line per
  check with a fix for each problem, whether the shim on `PATH` is this binary, whether the daemon
  accepts a connection, and whether Claude Code has the MCP server and every hook in
  `plugins/marshal-shim/hooks/hooks.json` once each; it changes nothing, and exits non-zero on any
  failure.

## Supporting: Claude delivery

- [ ] **MAR-3: Supporting: Hook inbox never offers a message twice (lease race).** `surface_unread`
  read unread mail before taking the lease lock, so an ack landing between them showed a message twice.
  Done when: the fix from upstream #169 is on the fork's `main` (taken from upstream when it merges, or
  ported), and `hook_inbox_safety` passes repeatedly in CI.
- [ ] **MAR-5: Supporting: Idle overnight wake verified, at most 2 model turns an hour.** From MUS-1, on
  Marshal's hooks. Done when: a Claude session idle overnight on Murph's daemon wakes on a message, and
  its idle turns are counted at 2 an hour or fewer. Needs a run: overnight, on Murph.
- [ ] **MAR-9: Supporting: Stable nickname across a restart.** From MUS-3. Done when: a session
  restarted in the same harness and directory keeps its nickname, a role declared by environment can be
  reached after a restart, and a test shows both.
- [ ] **MAR-6: Supporting: Keep a session through a dropped connection while its waiter is held.** The
  WS reap on `STALE_AFTER` doesn't check `WakeWatcher`, so a session still waiting is reaped. A default
  change: it goes in the defaults issue (MAR-15) and is built on the fork. Done when: a session holding
  `/hook/wait` survives its WS dropping past the grace, a test shows it, and the change is listed in
  MAR-15.

- [ ] **MAR-33: Supporting: claude_push_ack_e2e agent_id read races the shim's roster view.** Found
  on Castor #2's hosted CI (2026-11-07): `ordinary_shim_preserves_hook_metadata_and_exposes_it_in_resources`
  read `marshal://whoami` once, right after the daemon's row showed the shim registered, and got a
  null `agent_id` because the shim's own subscribed view hadn't caught up. Passes 15 of 15 locally.
  Done when: the test reads again until the view catches up, within its bound, and passes on the
  fork's hosted CI.
- [ ] **MAR-34: Supporting: runner-bound workflows run only on upstream.** Every Release run on fork
  main since 8231de7 ended "cancelled" with no jobs, because it waits for upstream's self-hosted
  runners, so Rally's digest flagged each merge red. Josh chose (2026-11-06) to run them only upstream.
  Done when: Release's self-hosted jobs and the public CI jobs carry `if: github.repository ==
  'ignition-is-go/marshal'`, and the next merge's Release run on fork main ends skipped, not cancelled.
  Fork-only: never send this hunk upstream as-is.

## Later: other harnesses

- [ ] **MAR-10: Later: Plugins ack a push only after the harness takes it.** From MUS-81. Pi, opencode
  and Prime Agent mark a push read as soon as it reaches the connection. Overlaps lobo235's upstream
  plugin PRs: coordinate, don't race. Done when: a push the harness didn't take stays unread for the
  next drain, in each of the three plugins, with a test each.
- [ ] **MAR-11: Later: Resuming wakes re-pushes held mail to push sessions.** From MUS-82. Done when:
  mail held while wakes were paused reaches a push session on resume, without waiting for its next
  turn, and a test shows it.
- [ ] **MAR-13: Later: OpenCode plugin declares its channel.** From MUS-89. Done when: the plugin
  reports its channel state when it registers, and the roster's `wake` for it matches what it can do.

## Security: agree the shape with upstream first

The gate stories below (MAR-23 to MAR-25) wait on myko#129, trevorsargent's PR that adds auth
providers, machine credentials and authenticated gateways to myko itself. So the gate is built on
that PR, not on a patched myko-server, and its shape is agreed upstream before any code.

- [ ] **MAR-37: Security: Help land myko#129 and check it against the threat model.** Done when:
  myko#129's auth provider and machine credentials have been tried against muster's four attackers
  (a web page, one compromised session, a LAN device, a stolen machine), and what it covers and
  doesn't is posted on myko#129 as a review or comment, by Josh or with his OK.
- [ ] **MAR-38: Security: Propose opt-in authentication upstream as a design issue.** Draft in
  [upstream-auth-issue-draft.md](upstream-auth-issue-draft.md). Needs the operator: Josh files it.
  Done when: the issue is filed on ignition-is-go/marshal, and the gate stories are revised to
  match the maintainers' answers.

## Later: strengthen Marshal

- [ ] **MAR-15: Later: File the defaults issue upstream.** One issue listing the default changes
  upstream may choose: refuse an empty `to` (MUS-73, fix ready on a local branch), hold a shim's first
  write until it's registered (MUS-72, 78, 79), keep a waiting session through a drop (MAR-6: the sweep spares a dropped WS session while its `/hook/wait` is held), take over a dead
  predecessor's nickname by harness and directory (MAR-9), and
  optional inbox quoting. Needs the operator: Josh files it. Done when: the draft is agreed with the
  commissioner and the operator has filed it.
- [ ] **MAR-16: Later: cargo-deny dependency audit in CI.** Additive. Done when: `cargo deny check`
  runs in the fork's CI and passes, with each licence exception explained.
- [ ] **MAR-18: Later: Stable error code for the unregistered refusal.** From MUS-80. The shim matches
  the refusal's text. Done when: the refusal carries a code, the shim matches the code, and a test
  covers both.
- [ ] **MAR-19: Later: Drop ended sessions from rooms.** From MUS-58. Done when: a session that ends
  leaves every room it was in, and `everyone` lists only live sessions, with a test.
- [ ] **MAR-20: Later: Keep current_task fresh.** From MUS-26. Done when: `current_task` follows the
  branch unless the session set its own, with a test.
- [ ] **MAR-21: Later: Tests pick ports without races.** From MUS-88. Done when: no test binds a port
  picked by asking the OS and releasing it, and the e2e suites pass run in parallel.
- [ ] **MAR-22: Later: Keep windows-cross-check within its time limit.** From MUS-91. Done when: the
  job finishes under its limit on 10 consecutive fork runs.
- [ ] **MAR-23: Later: Gate: opt-in TLS.** `marshal-daemon --tls`; plain stays the default. Waits on
  myko#129 (its auth provider) and MAR-38 (upstream's answers decide the flag and shape). Done when: `--tls` serves pinned TLS, plain mode is unchanged, and the
  gate's e2e checks for TLS pass.
- [ ] **MAR-24: Later: Gate: per-machine pairing.** Depends on MAR-23. Done when: a machine pairs with
  a one-time code, an unpaired machine is refused, and with pairings and neither `--tls` nor `--plain`
  the daemon refuses to start.
- [ ] **MAR-25: Later: Gate: myko's own listener behind the gate.** Depends on MAR-24. Done when:
  myko's listener admits only requests forwarded by the gate, with no vendored myko-server.
- [ ] **MAR-8: Later: TLS restart doesn't miss its takeover.** From MUS-93: an intermittent miss on
  CI's slower runner. Depends on MAR-23. Done when: the restart-keeps-identity cell passes in TLS mode
  on CI 10 times in a row.

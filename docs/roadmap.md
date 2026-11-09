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
numeric order below. A story without a "Done when" line isn't ready to claim. A tick means "Verified by the
watchman", Rally's tick meaning for MAR: the watchman read the story and its work is merged on the
fork's `main`.

## How work lands

1. Branch from `upstream/main` (ignition-is-go/marshal) for anything that may go upstream; from the
   fork's `main` for fork-only work.
2. Open a PR on Castor-o7/marshal, with the story ID in the title and commits.
3. Ask the watchman for a read at the PR's head, and merge only after it and a green CI.
4. Once fork `main` is green, the lamplighter deploys it as Murph's live daemon.
5. Upstream, later, and only when the operator decides: one small additive PR at a time. Default
   changes go to upstream as one issue (MAR-15), not as PRs.

## In flight

- [x] **MAR-1: Foundation: Safe retry of a timed-out send (idempotency key).** An optional
  `idempotencyKey` on `SendMessage`; a retry returns the stored message and stores and pushes nothing
  new; the shim sends a key on every send and says how to retry on a timeout. Done when: a retry racing
  its first attempt stores and pushes once, a retry after the recipient changed or left still finds the
  first send, and it's merged on the fork (Castor #2).
- [x] **MAR-2: Foundation: Run the fork's CI on GitHub-hosted runners.** Upstream's Linux jobs need
  self-hosted runners, which the fork has none of, so every fork run sat queued. Done when: `check` and
  `windows-cross-check` run on `ubuntu-latest` on the fork, Release stays untouched, and it's merged
  (Castor #3).

## Goal 1: Codex live delivery

Marshal already wakes an idle Codex session through the app-server bridge's `turn/start`
([codex-live-delivery.md](codex-live-delivery.md)). Muster has since made that path hold up across
restarts and show on the roster; these stories bring that work over.

- [x] **MAR-7: Goal 1: Codex threads re-register after a daemon restart.** From MUS-76 (muster
  `4425753`). The bridge registers a thread only on `thread/started`, so an idle Codex session stays off
  the roster after a restart, and mail to it can't be addressed. Done when: the bridge re-registers its
  threads when its daemon connection comes back, and a test restarts the daemon to show it.
- [x] **MAR-12: Goal 1: Show a Codex bridge's wake path on the roster.** From MUS-77 (muster `2e0ae23`).
  The roster calls a bridge-watched Codex session `wake: none` and a send to it says `unobserved`,
  though the bridge will wake it. Done when: a bridge-watched Codex thread reads `waiting` on the roster
  and a send to it says `expected`, with a test.
- [x] **MAR-4: Goal 1: One wake path ending doesn't clear another's record.** From MUS-94 (muster
  `2fcd2f9`). Matters once MAR-12 gives a Codex session a bridge record beside a hook waiter. Depends
  on MAR-12. Done when: each wake path keeps its own record, ending one leaves the others, and a test
  shows it.
- [x] **MAR-14: Goal 1: Delivery conformance suite in Marshal CI, with restart cells on Postgres.**
  From MUS-69 and MUS-76. The real daemon and shim against a fake harness each, one scenario table,
  Codex and Claude first. Done when: the harness × scenario grid runs in the fork's CI, the Codex
  idle-wake and restart cells pass, and known gaps are listed in the suite.

## Goal 2: Spin up agent teams

Nothing in Marshal or muster launches a session today. Once one is running, hooks register it, the
waiter or the bridge wakes it, and `tell` (Goal 3) can hand it work, so the missing piece is the
launcher.

- [x] **MAR-26: Goal 2: Launcher: spawn a session in tmux.** `marshal-shim spawn <harness> --cwd <dir>
  [--name <role>]` starts Claude Code or Codex in a detached tmux session. Operator's answers
  (2026-11-06): both harnesses (Codex through `codex-run`); the harness's default permission mode;
  Murph and tmux only for now; you or an agent may run it. Done when: one command on Murph starts a session in tmux that appears on the roster
  within 30 seconds, with no permission-bypass or channels flags.
- [x] **MAR-27: Goal 2: Launcher: join handshake with a spawn token.** Depends on MAR-26. Done when:
  the launcher passes a one-time token, waits with a timeout for a roster row carrying it, and prints
  the new session's nickname or a clear failure.
- [x] **MAR-28: Goal 2: Launcher: send a first task once joined.** Depends on MAR-27. Done when:
  `--task "..."` reaches the new session as a direct message after it joins, and the session wakes and
  acts on it.
- [x] **MAR-32: Goal 2: Launcher: start a team of sessions from one command.** Depends on MAR-28. A
  team file names each member's harness, directory, role and first task. Done when: one command starts
  every member, each joins the roster and gets its task, and the command reports who joined and who
  didn't.
- [x] **MAR-30: Goal 2: Launcher: list, reattach and stop launched sessions.** Depends on MAR-27.
  Done when: the launcher lists the sessions it started, reattaches to one, and stops one so its roster
  row and rooms are cleared.
- [x] **MAR-29: Goal 2: Launcher: desktop terminal backends.** Depends on MAR-26. Josh chose
  (2026-11-06): `$TERMINAL`, then konsole, on Linux; the window attaches to the session's tmux, so
  closing it leaves the session running; macOS is MAR-35. Done when: `spawn --window` opens a visible
  window on Murph attached to the session, with the same handshake as tmux.
- [ ] **MAR-35: Goal 2: Launcher: macOS terminal windows (Terminal, iTerm).** Depends on MAR-29.
  Done when: `spawn --window` opens a Terminal or iTerm window attached to the session on a Mac, tested
  on one.

## Goal 3: Message an agent from a terminal

- [x] **MAR-31: Goal 3: Terminal tell, all and roster on the fork.** Muster's `tell`, `all` and
  `roster`, as already cut for upstream on `port/terminal-cmds-v3` (upstream #168, open). A direct
  message from a terminal wakes an idle session like any other. Done when: `marshal-shim tell <name>
  <message>`, `marshal-shim all <message>` and `marshal-shim roster` work against the fork's daemon on
  Murph, and the branch is merged on the fork.
- [x] **MAR-17: Goal 3: Terminal send and roster beyond upstream #168.** Done when: whatever of
  muster's terminal commands #168 doesn't cover is listed, and each item is ported or dropped with a
  reason. The list and Josh's choices (2026-11-06) are in `docs/muster-terminal-gap.md`: `doctor` is
  ported as MAR-36, and the rest is dropped or already covered.
- [x] **MAR-36: Goal 3: `marshal-shim doctor`, a read-only health check.** From muster's `doctor`
  (MUS-15), without its credential check. Done when: `marshal-shim doctor` reports, one line per
  check with a fix for each problem, whether the shim on `PATH` is this binary, whether the daemon
  accepts a connection, and whether Claude Code has the MCP server and every hook in
  `plugins/marshal-shim/hooks/hooks.json` once each; it changes nothing, and exits non-zero on any
  failure.

## Supporting: Claude delivery

- [x] **MAR-3: Supporting: Hook inbox never offers a message twice (lease race).** `surface_unread`
  read unread mail before taking the lease lock, so an ack landing between them showed a message twice.
  Done when: the fix from upstream #169 is on the fork's `main` (taken from upstream when it merges, or
  ported), and `hook_inbox_safety` passes repeatedly in CI.
- [ ] **MAR-5: Supporting: Idle overnight wake verified, at most 2 model turns an hour.** From MUS-1, on
  Marshal's hooks. Done when: a Claude session idle overnight on Murph's daemon wakes on a message, and
  its idle turns are counted at 2 an hour or fewer. Needs a run: overnight, on Murph.
- [x] **MAR-9: Supporting: Stable nickname across a restart.** From MUS-3. Done when: a session
  restarted in the same harness and directory keeps its nickname, a role declared by environment can be
  reached after a restart, and a test shows both.
- [x] **MAR-42: Supporting: A restarted session's unread mail follows it.** From MUS-47, built on MAR-9.
  A takeover readdresses the predecessor's unread direct mail to the successor; mail it had read stays
  with it. Done when: the conformance cell `unread-mail-follows-a-restart` passes on claude and codex, a
  daemon test shows unread mail moving and read mail staying, and the change is listed in MAR-15.
  Done (#43): `unread-mail-follows-a-restart` is off `MARSHAL_NOT_APPLICABLE` and passes on claude
  and codex in CI; `crates/daemon/tests/identity_succession.rs` shows the mail moving.
- [x] **MAR-54: Supporting: Codex rows on the roster can count as dead, from the bridge's signal.**
  Follow-up to MAR-9. Succession never treated a Codex row still on the roster as dead
  (`CODEX_PRESENT_ROWS_CAN_BE_DEAD`, "until MUS-77"), but MAR-12 has since given the roster the bridge's
  wake path. Scope agreed with the watchman (2026-11-08). Done when: a present Codex row counts as dead
  only once this daemon saw its bridge record end at least `MARSHAL_SUCCESSION_BRIDGE_GRACE_SECS` (120 s)
  ago, with no other waiter and no hook since, and a row never seen with one never does, with verdict
  tests including a 45 s bridge handover gap; succession finds waiters by `WakeWatcher::session()`, with
  a test; a daemon test shows a bridge-watched Codex sibling keeping its name and one whose record ended
  taken over with its unread mail; the codex conformance cells still pass; and `stable-identity.md`
  says so. Done (#48): verdict tests in `crates/daemon/src/succession.rs`, daemon tests in
  `crates/daemon/tests/identity_succession.rs`.
- [x] **MAR-6: Supporting: Keep a session through a dropped connection while its waiter is held.** The
  WS reap on `STALE_AFTER` doesn't check `WakeWatcher`, so a session still waiting is reaped. A default
  change: it goes in the defaults issue (MAR-15) and is built on the fork. Done when: a session holding
  `/hook/wait` survives its WS dropping past the grace, a test shows it, and the change is listed in
  MAR-15.

- [x] **MAR-33: Supporting: claude_push_ack_e2e agent_id read races the shim's roster view.** Found
  on Castor #2's hosted CI (2026-11-07): `ordinary_shim_preserves_hook_metadata_and_exposes_it_in_resources`
  read `marshal://whoami` once, right after the daemon's row showed the shim registered, and got a
  null `agent_id` because the shim's own subscribed view hadn't caught up. Passes 15 of 15 locally.
  Done when: the test reads again until the view catches up, within its bound, and passes on the
  fork's hosted CI.
- [x] **MAR-34: Supporting: runner-bound workflows run only on upstream.** Every Release run on fork
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
- [x] **MAR-13: Later: OpenCode plugin declares its channel.** From MUS-89. Done when: the plugin
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
- [x] **MAR-39: Docs: README says the daemon binds 0.0.0.0, but it binds 127.0.0.1.** Found while
  reading myko#129 for MAR-37: `DEFAULT_BIND` in `crates/daemon/src/main.rs` is `127.0.0.1:6155`,
  and the README says `0.0.0.0:6155` in two places. Additive and docs-only, so it can go upstream as
  it stands. Done when: the README matches the code, merged on the fork, and Josh has the branch to
  send upstream.

MAR-37's read and a draft comment for myko#129 are in [myko-129-review-notes.md](myko-129-review-notes.md).

## Cutover

- [ ] **MAR-59: Cutover: a kit to prove one session on Marshal.** Step 1 of the cutover runbook
  (rally's docs/cutover.md, RAL-119). Done when: [cutover-test-session.md](cutover-test-session.md)
  gives the project-local wiring, the checks with their expected output, and teardown, checked
  against the plugin's hooks.json and Claude Code's flags. Running it is the operator's call; the
  doc is marked not yet run.
- [ ] **MAR-60: Cutover: doctor checks a project's own Claude Code wiring.** Found writing MAR-59:
  `marshal-shim doctor` reads only `~/.claude/settings.json` and `~/.claude.json`, so it can't check a
  session wired by a project-local `.claude/settings.json` and `.mcp.json`, which is how the cutover
  moves sessions one at a time. Done when: run in a directory with its own `.claude/settings.json` or
  `.mcp.json`, doctor checks those too and says which file each finding came from; it stays read-only
  and its exit status unchanged, with tests.

## Parity with muster (gap survey, 2026-11-08)

Toward the operator's goal of bringing Marshal up to muster's capabilities. A survey of the 62 finished
muster stories no other entry here names found 23 already in Marshal, 11 portable, 8 default changes
(added to MAR-15), 11 held behind the gate stories, 6 about muster itself and 3 the operator dropped
([muster-terminal-gap.md](muster-terminal-gap.md)). The portable ones are below, ordered by what a
no-interruption cutover from muster's daemon needs first (rally#195's docs/cutover.md). MUS-92 is in
MAR-9, and MUS-47 is MAR-9's follow-up, so neither is repeated here.

- [ ] **MAR-44: Parity: A Claude waiter stands down when its session's id changes.** From MUS-104
  (muster `d96d678`). A `/clear` or compaction re-mints a Claude session's id, but a wait hook started
  before it keeps polling under the old id, so its row never ages out and mail to the old id still finds
  a waiter. Done when: each poll checks Claude's live manifest (`canonical_session_id`, as the shim's
  drift check does), a waiter whose id is no longer the session's exits, nothing changes with no
  manifest, and a test shows it.
- [ ] **MAR-45: Parity: Trust a Claude session manifest only if it's live.** From MUS-107 (muster
  `183179e`, `3f42b7a`, `a089b57`). Claude leaves `~/.claude/sessions/<pid>.json` behind when it dies,
  so after the pid is reused another process's shim or waiter takes a dead session's id. Done when:
  discovery and `canonical_session_id` trust a manifest only if it names a live process that started
  when it says (Linux: `procStart` against `/proc/<pid>/stat`; elsewhere `startedAt` with 2 s slack),
  what can't be read is trusted as today, with tests.
- [ ] **MAR-46: Parity: The operator can forget a session.** From MUS-105 (muster `69f6e53`, `073bec9`,
  `3c84895`). A ghost row has no way off the roster but the sweeper. Done when: `marshal-daemon forget
  <id | 8+ char prefix | nickname>` makes the running daemon delete that session's row and refuse its
  hooks for 10 minutes, a target naming no session or several removes nothing, a nickname is resolved
  once to its id, and tests cover each.
- [ ] **MAR-47: Parity: Say what a send with no live channel means.** From MUS-99 (muster `9c1a0ca`).
  Under hook delivery every send says `live_push: unavailable`, and nothing tells the sender a reply
  will come at the recipient's next turn. Done when: the first such send in a shim carries a `note`
  saying the message waits in the inbox, shows at the recipient's next tool call or prompt, and wakes
  an idle recipient only when `wake` is `expected`; it's said once per shim, with a test.
- [ ] **MAR-48: Parity: Push plugins get mail that waited out a reconnect.** From MUS-90 (muster
  `d839b97`, `10686af`). Pi, OpenCode and Prime Agent re-register on reconnect but don't pick up direct
  mail sent while they were away, which waits for the next user turn. Done when: after a reconnect each
  plugin shows mail that arrived while it was down, without a new message to trigger it, and a test per
  plugin shows it. After MAR-10, which touches the same files.
- [ ] **MAR-49: Parity: One error names every missing tool argument.** From MUS-100 (muster `ca3d6f9`,
  `2d997b3`). Write tools fail one missing field at a time. Done when: one error names every missing or
  mistyped field (Codex's `asSession` too) and the keys given, the common aliases muster accepts stand
  in, the canonical name wins over an alias, the advertised schemas are unchanged, with tests.
- [x] **MAR-50: Parity: A session leaves its old op: room when its operator changes.** From MUS-40
  (muster `1a4b670`). `auto_rooms` adds a session to `op:<operator>` but never removes it from the
  old one, so a broadcast to the old room reaches it. Done when: a session whose operator changes is
  only in its current `op:` room, with a test. Done (#42): each auto-rooms dispatch removes the
  session from other operators' `op:` rooms; a SET naming no operator leaves them.
  `crates/daemon/tests/auto_rooms_operator.rs` shows it.
- [ ] **MAR-51: Parity: A future lastActivityAt doesn't win operator mail.** The plain-mode part of
  MUS-33 (muster `9a0b61c`). Mail to an operator goes to their most active session, and a session can
  claim a far-future `lastActivityAt`. Done when: a `lastActivityAt` later than now doesn't outrank a
  real one when routing operator mail, with a test.
- [ ] **MAR-52: Parity: Run CI on macOS.** From MUS-13 (muster #163). Held until the official swap to
  Marshal, by the operator (2026-11-08: "we'll get macos tests ready when we perform the official swap");
  as for MAR-35, Rally hold 1418. Done when: a `macos-15` job runs the workspace tests on the fork's CI
  and passes. Keep the change to ci.yml small: every story edits it.
- [ ] **MAR-53: Parity: Keep messages across a daemon restart without Postgres (opt-in).** From MUS-17
  (muster `8b4b4c2`, `3dd12aa`, `d6b6cdf`; design in muster's local-persistence-design.md). Without
  Postgres a restart loses every message. Done when: with `MARSHAL_PERSIST=file` an unread message, read
  state and rooms survive a restart with no Postgres, the default is unchanged, and the conformance
  suite's file-restart cells pass.

## Later: strengthen Marshal

- [ ] **MAR-15: Later: File the defaults issue upstream.** One issue listing the default changes
  upstream may choose: refuse an empty `to` (MUS-73, fix ready on a local branch), hold a shim's first
  write until it's registered (MUS-72, 78, 79; built on the fork as MAR-43), keep a waiting session through a drop (MAR-6: the sweep spares a dropped WS session while its `/hook/wait` is held), take over a dead
  predecessor's nickname by harness and directory (MAR-9) and hand it the unread mail (MAR-42), a reaped session that comes back loses its adhoc rooms unless it takes the old one over (MAR-19: it rejoins its auto-rooms; a takeover within a day moves the adhoc ones, otherwise it rejoins them itself), have `current_task` follow the branch (MAR-20), a `MARSHAL_PERSIST` other than `file` stops startup (MAR-53: so a typo can't silently lose mail; before, the variable meant nothing), and
  optional inbox quoting. From the gap survey (2026-11-08), also: a per-session send rate limit
  (MUS-9), keeping a session while its hooks are firing (MUS-54), a larger hook body (MUS-102), and a
  resource read right after connecting waiting for registration (MUS-114). Needs the operator: Josh files it. The draft is
  [upstream-defaults-issue-draft.md](upstream-defaults-issue-draft.md), unsent. Done when: the draft is agreed with the
  commissioner and the operator has filed it.
- [x] **MAR-16: Later: cargo-deny dependency audit in CI.** Additive. Done when: `cargo deny check`
  runs in the fork's CI and passes, with each licence exception explained. Done (#32): CI's
  `dependency-audit` job runs cargo-deny 0.20.2 against `deny.toml`, ported from muster's MUS-11.
  Eight advisories in the lockfile (rustls, rustls-webpki, tokio, ctutils, webpki-roots) were fixed
  by semver-compatible updates, and yanked chacha20 was bumped. myko's crates are AGPL by design and
  are listed by name. The web UI's four git crates (mullion, css-styled and two of its parts)
  declare no licence and are excluded by name: a real gap, which needs a licence upstream or their
  replacement.
- [ ] **MAR-41: Later: The web UI's git crates carry a licence, or are replaced.** From MAR-16.
  mullion, css-styled, css-styled-derive and css-spec-data (ignition-is-go's git repositories, for
  marshal-ui) declare no licence, so there's no stated permission to use them; `deny.toml` excludes
  them by name. Waits on the operator: licensing is Josh's to raise with upstream. Done when: the
  four crates carry a licence upstream, or the UI drops or replaces them, and the exclusion comes
  out of `deny.toml`.
- [ ] **MAR-43: Later: Hold a shim's first write until it's registered, and retry one refused as
  unregistered.** From MUS-72, MUS-78 and MUS-79. A default change, so it's in MAR-15 for upstream and
  built on the fork first. An agent's first write can beat the shim's registration, or land on a row
  still bound to a dead connection, and the daemon refuses it as unregistered. Done when: a write waits
  up to 5 s for this session's bound row, sends the session again once after 1 s, and a write the
  daemon refuses as unregistered is retried once after sending the session again, with tests. MAR-18
  then gives that refusal a stable code.
- [ ] **MAR-18: Later: Stable error code for the unregistered refusal.** From MUS-80. The shim matches
  the refusal's text. Done when: the refusal carries a code, the shim matches the code, and a test
  covers both.
- [x] **MAR-19: Later: Drop ended sessions from rooms.** From MUS-58. Done when: a session that ends
  leaves every room it was in, and `everyone` lists only live sessions, with a test.
- [x] **MAR-20: Later: Keep current_task fresh.** From MUS-26. Done when: `current_task` follows the
  branch unless the session set its own, with a test. Done (#34), in `crates/shim/src/main.rs`.
- [ ] **MAR-58: Later: The roster says when a session's activity last changed.** The roster's
  `activity` (`busy`, `idle` or `shell`, from Claude's per-PID manifest) says what a session is doing
  but not since when, so "idle for a minute" and "idle for an hour" look the same, and
  `last_activity_at` moves only on Marshal traffic. Claude's manifest already records the change as
  `statusUpdatedAt`. Additive: a new optional `activity_at` field on `Session`, epoch milliseconds.
  Done when: the shim sets `activity_at` at registration and with every `activity` push, taking
  the manifest's `statusUpdatedAt` when it's there and its own clock when the change was seen
  but has no timestamp; a shim restart or a canonical-id re-register keeps the manifest's time
  rather than resetting it; a session with no `activity` (Codex, older Claude builds) has none;
  everywhere the shim shows `activity` shows `activity_at` beside it (`marshal://roster`, the
  `marshal-shim roster` command and `marshal://whoami`); and tests cover each of those cases.
- [x] **MAR-21: Later: Tests pick ports without races.** From MUS-88. Done when: every in-process
  hook listener in the tests takes a socket the test bound to port 0; every port a test's myko
  server or daemon still picks is retried on a lost bind instead of failing; and the e2e and
  conformance suites pass with two copies running at once. The rest, myko servers binding a listener
  they're given, is MAR-40. Done (#28). Hook listeners take the bound socket through
  `http_listener::serve_until`, so there's no window. A test's myko server reports a lost bind
  (`run()`'s error) and is started on a new port; a port another socket holds is refused, not
  mistaken for the server's own (`a_port_another_process_holds_is_reported`). The daemon subprocess
  suites start it again when it exits on a lost WebSocket bind, or logs `[hook] listener exited` for
  a lost hook bind, which it does without exiting.
- [ ] **MAR-40: Later: myko servers in tests bind a listener they're given.** Split from MAR-21.
  myko-server 6.11.5 binds its own port and takes no listener, so a test's myko server and the
  daemon subprocesses still bind a picked port and retry when they lose it. One weakness: when
  another test's myko server holds the port, a connect succeeds, and detection rests on the bind
  error arriving within 200 ms, a timing assumption. Waits on the operator and upstream: an additive
  ask that myko-server accept a pre-bound listener, which Josh sends. Done when: myko-server accepts
  a pre-bound listener, and the remaining port-0 picks in the tests use it.
- [x] **MAR-22: Later: Keep windows-cross-check within its time limit.** From MUS-91. Done when: the
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

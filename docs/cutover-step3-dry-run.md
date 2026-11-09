# Cutover step 3, dry run: moving the watchman and the lamplighter to Marshal

**A checklist. Nothing in it has been done.** It prepares step 3 of the cutover runbook (rally's
`docs/cutover.md`, RAL-119): "move the watchman and lamplighter first". The watchman (mellow-weevil)
asked for it on 2026-11-09 (MAR-72). Josh made the switch the rally's goal, to happen when there'll be no interruption, and asked not to be
asked when. So the watchman confirms readiness, the lamplighter runs the steps, and Josh restarts the
watchman's session, since only he can. Fork-only.

Read on Murph on 2026-11-09, read-only: Claude Code 2.1.319, `~/.claude/settings.json`,
`~/.claude.json`, the two working directories, and Muster's roster. Builds on Marshal's
[cutover-test-session.md](cutover-test-session.md) (MAR-59, step 1's kit) and rally's
`docs/mail-carryover.md` (RAL-122, option A).

## Who moves, from where

| Role | Session now (Muster) | Directory | Others in that directory |
|---|---|---|---|
| Watchman | mellow-weevil | `~/Documents/Caster` (not a git repository, no `.claude/`, no `.mcp.json`) | None on the roster |
| Lamplighter | eager-cobra | `~/src` (not a git repository; `.claude/` holds only `scheduled_tasks.lock`) | **Several**: silken-beetle and cobalt-glen started here too |

Session ids change with every restart, so read both sessions' current Muster ids from
`muster-shim roster` on the day, not from this table.

## Three things the step 1 kit doesn't have to face

Step 1's test session is fresh and throwaway. The watchman and the lamplighter are long-running and do
real work, which brings up three problems the kit didn't have.

### 1. `--setting-sources project` drops all of the user's settings, not just Muster's hooks

The kit launches with `--setting-sources project`, so the user-wide muster hooks don't run. But
`~/.claude/settings.json` holds more than hooks: `autoMode`, `permissions` (the `allow` rules, among
them Josh's standing merge and push rule), `model`, `modelSettings`, `enabledPlugins` and `theme`.
A watchman or lamplighter launched like the test session loses all of it. It would stop at every
merge, push and ledger write for want of a rule.

A project's settings **add** to the user's: there's no way to load the user file but not its hooks.
`disableAllHooks` would turn off Marshal's hooks too. So the user settings, minus the `hooks` key, have
to come back in another way. The proposal: one extra file, given with `--settings`:

```sh
python3 -I -c 'import json,sys; s=json.load(open(sys.argv[1])); s.pop("hooks",None); json.dump(s,sys.stdout,indent=2)' \
  ~/.claude/settings.json > ~/.config/marshal/cutover/user-settings-no-hooks.json
```

It's a copy, so it goes stale if the user file changes. Make it again before each launch until step
5, when the user-wide wiring itself moves to Marshal and the copy is no longer needed.

**To check in the dry run:** that `--settings` still applies under `--setting-sources project`. The
CLI's help says only that it loads "additional settings". In the dry-run session, `/permissions` must
show the `allow` rules and `/hooks` must show only the five `marshal-shim` hooks. If `--settings` is
ignored, the alternative is to put the same keys in the project's `.claude/settings.json` next to the
hooks, which is the same copy in a different place.

### 2. `~/src` is shared, so the lamplighter's wiring can't go in it

A `.claude/settings.json` or `.mcp.json` in `~/src` would be picked up by **every** session started
there. A session launched with plain `claude` loads the user's muster hooks too, so it would run both
sets and register on both daemons: the double wiring MAR-36's doctor exists to catch.

Two ways out:

- **(recommended) A directory of its own:** `~/src/lamplighter`, holding only the lamplighter's
  `.claude/settings.json` and `.mcp.json`. `~/src` isn't a git repository, so a session started in
  `~/src/lamplighter` reads only that directory's `.claude/`. `marshal-shim doctor`, run there, checks
  its wiring (MAR-60). The cost is that the lamplighter's project changes from `src` to `lamplighter`,
  so its project room changes too. The lamplighter's work is all by absolute path (systemd units,
  `~/.config`, `~/.local/state`), so nothing else should depend on its working directory. Confirm that
  with the lamplighter.
- **Flags only:** keep the cwd as `~/src`, and give both files by absolute path
  (`--settings <hooks file> --mcp-config <mcp file>`), with nothing written in `~/src`. That leaves no
  project file behind, but `marshal-shim doctor` can't check flag-only wiring, so check 1 below has to
  be done by hand.

`~/Documents/Caster` has no such problem: only the watchman runs there. Its wiring can be
project-local, as the kit's is.

### 3. A cross-daemon message has no reply path

During the side-by-side period, a team still on Muster reaches a moved session only with
`marshal-shim tell` from a shell, and the other way round with `muster-shim tell`. Such a message
arrives as `[from a terminal on Murph; it can't receive replies]`, so the answer has to be addressed by
hand to the sender's nickname on the other daemon. The watchman, once moved, answers every team that's
still on Muster this way. Every team needs to know this before the first move (see the announcements
below).

## Before the dry run

- [x] Step 1 is done and passed: MAR-70, cobalt-bream's run of the MAR-59 kit on 2026-11-09, verified at
      Rally record 1856 with all four checks passed.
- [ ] Step 2 is in place: `rally wake`, `rally digest` and `rally brief` read both rosters (RAL-126) and
      send through `marshal-shim` first (RAL-120). rally's `cutover.md` rows 5 and 6 say this is done.
- [x] A cutover hold is recorded in Rally (`rally hold`, step 0): record 1859.
- [ ] Marshal's daemon is the one built from fork main (MAR-46 `forget`, MAR-9 nickname, MAR-42 succession
      mail, MAR-61), and it's listening on `127.0.0.1:6155` and `:6156` only (the loopback drop-in).
- [ ] The watchman confirms readiness. The lamplighter runs the steps, and Josh restarts the watchman's
      session (only he can).

## The dry run: a stand-in for each role, nothing real moves

The dry run proves the wiring, not the move. It starts one stand-in session per role, in each role's
target directory, with the exact launch command the move will use. Neither stand-in takes on the role.

### Files

`~/.config/marshal/cutover/user-settings-no-hooks.json`, made as in problem 1 above.

`.claude/settings.json` in `~/Documents/Caster` and in `~/src/lamplighter` (or the hooks file, for the
flags-only option): the five hooks exactly as in MAR-59's section (a), by absolute path.

`.mcp.json` beside it, as in MAR-59:

```json
{
  "mcpServers": {
    "marshal": {
      "command": "/home/joshd/.cargo/bin/marshal-shim",
      "env": { "MARSHAL_DAEMON_ADDRESS": "ws://127.0.0.1:6155" }
    }
  }
}
```

**Writing these files is a change to the live directories.** While the watchman's real Muster session is
still running in `~/Documents/Caster`, the new files don't touch it, because a running session reads its
settings once at start. A watchman restarted there with plain `claude` would load them, though. So write
them just before the stand-in launches, and remove them in teardown if the move doesn't follow.

### Launch, one stand-in at a time

```sh
cd ~/Documents/Caster        # or ~/src/lamplighter
MARSHAL_DAEMON_ADDRESS=ws://127.0.0.1:6155 \
  claude -n cutover-dry-run-watchman \
    --setting-sources project \
    --settings ~/.config/marshal/cutover/user-settings-no-hooks.json \
    --strict-mcp-config --mcp-config .mcp.json
```

Give it one prompt: "Say hello, then wait."

### Checks, for each stand-in

- [ ] **1. Only Marshal is wired.** `/mcp` lists `marshal` and not `muster`. `/hooks` lists the five
      `marshal-shim claude-hook …` commands and no `muster-shim` ones. `marshal-shim doctor`, run in the
      directory, reports the project-local wiring once (skip it for the flags-only option).
- [ ] **2. The user's settings came through.** `/permissions` shows the user's `allow` rules, and the
      session is in auto mode with the user's model. Then show a rule is in force, not just listed: ask the
      stand-in to run a harmless command the rules cover, such as `rally show MAR-72` or
      `gh pr merge --help -R Castor-o7/marshal`, and it must run without a permission prompt. If not,
      stop: problem 1's `--settings` route doesn't work, and the move has to wait for the alternative.
- [ ] **3. On Marshal's roster, not Muster's.** As in MAR-59 check 2, with its `cwd` filter changed to
      `Documents/Caster` or `src/lamplighter`. `muster-shim roster` doesn't list it.
- [ ] **4. Idle wake.** MAR-59 check 3: `marshal-shim tell <nickname> "dry run: idle wake"` wakes it
      without anyone typing.
- [ ] **5. Rally sees it.** The viewer (127.0.0.1:7300) lists the stand-in, with its source as Marshal's
      roster (RAL-126). Rally's own SessionStart hook (`rally session-start`) isn't in the user-wide
      settings today, so the move neither adds nor loses it.
- [ ] **6. A cross-daemon round trip.** From a session still on Muster, `marshal-shim tell <stand-in>
      "dry run: from Muster"`. The stand-in receives it, and answers with `muster-shim tell <sender>
      "dry run: reply"`. The sender receives the reply. This is problem 3's path, which every team will use
      during the move.
- [ ] **7. Claude in Chrome still works.** If the watchman relies on Claude in Chrome or claude.ai
      connectors, check them in the stand-in: `--strict-mcp-config` may leave them out. Note what's
      missing.

### Teardown

- [ ] `/exit` each stand-in.
- [ ] `marshal-daemon forget <nickname>` for each (MAR-46), or wait for `STALE_AFTER`.
- [ ] If the move doesn't follow straight away: remove `~/Documents/Caster/.claude/` and `.mcp.json`, and
      `~/src/lamplighter`, so that a plain restart of the watchman doesn't pick them up.

## The move itself (step 3), once the dry run passes

For each role, one at a time. The lamplighter goes **first**. It relays for everyone, and `muster-shim`
and `marshal-shim tell` both work from its shell whichever daemon its session is on. The watchman goes
second, when the lamplighter has confirmed it can reach both rosters from its new session.

1. **Drain Muster mail (RAL-122, option A).** The session reads everything in its Muster inbox and acks
   it (`ack_messages`).
2. **The check.** The Marshal team runs `mail-carryover.md`'s read-only query against the `muster`
   database, with `psql "host=/run/user/1000/marshal-pg dbname=muster user=joshd" -X`. Expect no row for
   the session's current Muster id. Also look at the **predecessors' ids**: the watchman restarted as
   mellow-weevil on 2026-11-09, so mail to an earlier id may still be unread.
3. **Announce on Muster first.** The session broadcasts on Muster: its role, that it's moving to Marshal,
   when it'll be back, and how to reach it meanwhile (the lamplighter, or the other role, relays). Say how
   cross-daemon mail works (problem 3).
4. **Restart with Marshal's wiring.** For the watchman, Josh does this step. `/exit`, then the launch command above, from the role's directory.
   Give it the role in its first prompt (or `--resume <old session id>` with the same flags, if its context
   matters more than a clean start; test `--resume` in the dry run first if you plan to use it).
5. **Announce on Marshal after.** The new session broadcasts on Marshal: its new nickname and the old one.
   On Muster, the lamplighter (or the other role) posts the new nickname for anyone still there.
6. **Record the succession.** The watchman (or Josh, when the watchman is the one moving) runs
   `rally rally-session add <new nickname> --succeeds <old nickname> --note "cutover step 3"` (RAL-108).
7. **Run the check again** after a few minutes, for mail that reached the old identity during the move, and
   relay any with `marshal-shim tell <new nickname>`, quoting the sender and when it was sent.

## Rollback

The move changes only the two directories. To go back, `/exit` the moved session, remove its
`.claude/settings.json` and `.mcp.json` (or `~/src/lamplighter`), and start it again with plain `claude`. It
comes back on Muster with the user-wide wiring, which was never touched. Announce the rollback on both
daemons. Mail sent to it on Marshal stays in the `marshal` database. Read it with `marshal-shim`, or relay
it by hand.

## Open questions

- **For the lamplighter:** the watchman prefers a directory of its own (`~/src/lamplighter`). Is anything
  the lamplighter runs tied to `~/src` as its working directory?
- **For the watchman:** a clean start, or `--resume` to keep context?
- **For Josh, only if he wants to weigh in:** whether the `--settings` copy of the user settings is
  acceptable until step 5. The time of the move is the watchman's call, not a question for Josh.

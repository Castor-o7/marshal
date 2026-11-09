# Soak kit: load for the MAR-73 bar (MAR-75)

**Ready, not run.** Fork-only. This kit is load option 1 of [reliability-vs-muster.md](reliability-vs-muster.md)
(d): test sessions on Marshal, driven with direct mail and an hourly canary for a window, with one
planned restart with mail provably unread, then MAR-73's measures and teardown. **Josh hasn't accepted
the bar or chosen the load option, so don't start the 24 h window until he has.** It was dry-run on
2026-11-09 for a few minutes (below).

The scripts are in [`scripts/soak/`](../scripts/soak). They change nothing user-wide and nothing on
either daemon. The one restart is done by the lamplighter, not by the kit.

## What it does

| Step | Script | What happens |
|---|---|---|
| 1 | `setup.sh [N]` | Starts N test sessions (default 3), each in `~/src/marshal-soak/sN` with the project-local wiring copied from [cutover-test-session.md](cutover-test-session.md)'s own JSON blocks, so the two can't drift. Each one runs `marshal-shim doctor`, opens in a window of tmux session `marshal-soak` with the kit's launch flags, and gets a first prompt telling it to answer each test message with `ok` and use no tools. Setup waits until each session has answered `ready` and holds a waiter. Writes `state/sessions.tsv` |
| 2 | `drive.sh [hours] [load_min] [canary_min]` | For the window (default 24 h): every `load_min` (default 20), one `tell` to the next session in turn. Every `canary_min` (default 60), one `tell` to every session, each timed: **taken** is the db's first `MessageRead` for it, and **woke** is the session's roster `waiting_until` moving, meaning it finished a turn and holds a new waiter. Each is given 60 s. Logs `state/sends.tsv` and `state/canary.tsv` |
| 3 | `restart-check.sh [window]` | MAR-70 run 2's method on one session (default s1). It stops that session's waiters, sends a token, and checks the db (read-only) has no `MessageRead` for it. Then it **waits for the lamplighter** to restart `marshal-daemon`, checks the token is still unread, prompts the session, and checks the token is taken once, after the restart, with the nickname kept. Exits non-zero naming the step that failed. Logs `state/restart.tsv` |
| 4 | `measure.sh [hours]` | MAR-73's measures for both daemons over the window. It reads the journal script and the SQL from reliability-vs-muster.md at run time, so there's one copy of each, plus a summary of the canary and the restart. Read-only. Writes `state/measures-<time>/` |
| 5 | `teardown.sh` | `/exit`s each session, ends the tmux session, keeps `state/` in `~/.local/state/marshal-soak/<time>/`, and removes `~/src/marshal-soak` only if it holds the marker file setup writes (`.marshal-soak-kit`). Then it waits for Marshal's roster to drop the test sessions (about 60 s) |

Every setting can be overridden from the environment (`scripts/soak/lib.sh`): `SOAK_DIR`, `SOAK_TMUX`,
`SOAK_RESULTS`, `MARSHAL_DAEMON_ADDRESS` (default `ws://127.0.0.1:6155`), `SHIM`, `PG_SOCKET`.

## Running the 24 h window, once Josh says go

The kit runs its own tmux server (`tmux -L marshal-soak`, so `tmux -L marshal-soak attach` to watch),
apart from any other on the machine. Start it under a memory cap, so the three sessions and the driver
can't crowd out the daemons. For example, run setup inside a scope, and run the driver in a window of
the kit's own server, which is inside that scope:

```sh
systemd-run --user --scope -p MemoryMax=6G -p MemoryHigh=5G --unit=marshal-soak scripts/soak/setup.sh 3
tmux -L marshal-soak new-window -d -n drive "scripts/soak/drive.sh 24 2>&1 | tee -a ~/src/marshal-soak/state/drive.log"
```

1. Check `marshal-shim roster`, and tell whoever is on Marshal (the lamplighter, since step 3) that a
   soak is starting and one restart will come.
2. `scripts/soak/setup.sh 3`
3. In its own window of the kit's tmux server (above): `scripts/soak/drive.sh 24`. With the defaults that's 72 load and 72 canary
   messages, 144 direct messages in all, which clears the bar's 100. It's one short model turn per
   message, so about 144 turns across the three sessions.
4. Some hours in, while `drive.sh` runs: `scripts/soak/restart-check.sh s1`, and ask the lamplighter
   to restart `marshal-daemon` when it says so. The restart drops every Marshal session for a moment,
   the lamplighter included. drive.sh keeps going, and a canary sent in that moment will show it.
5. When `drive.sh` ends: `scripts/soak/measure.sh 24`, then `scripts/soak/teardown.sh`.
6. Record the result as run 2 in reliability-vs-muster.md, against the bar (d), with the measures
   directory's files.

How each part of the bar is shown:

| Bar (reliability-vs-muster.md (d)) | From |
|---|---|
| ≥3 agent sessions, ≥100 direct messages | `setup.sh 3`; `drive.sh` defaults (144); M5 and M3 in `marshal.txt` |
| ≥1 planned restart with mail waiting | `restart-check.sh`'s `PASS` line in `restart.tsv`; M4 in `marshal.txt` |
| M1–M5 at least as good as Muster's over the same window | `measure.sh`'s `marshal.txt` beside `muster.txt` |
| Every idle wake within 60 s | `canary.txt`: `woke_within_60s` equal to the canary sends |

## Dry run, 2026-11-09 16:05–16:11

Run on Murph by cobalt-bream: setup, one canary round, a two-minute drive, measures, teardown, but not
`restart-check.sh`, since a restart needs the lamplighter, and its method was run by hand in MAR-70 run
2. Marshal on 98da488. Marshal's roster had 1 session before setup (mauve-tortoise, the lamplighter).

| Step | Result |
|---|---|
| `setup.sh 3` | 16:05:57 to 16:06:11. doctor `ok` on all four lines for each. s1 trusty-plover, s2 chartreuse-ember, s3 ultra-wren, each answered `ready` |
| `drive.sh 0.03 1 60` | 16:06:59 to 16:08:50, 5 messages (3 canary, 2 load). Canary taken in 0.3, 0.6 and 0.5 s, and all three woke. `tell` said `wake expected but not observed` for each, as it should for a hook-delivered session |
| `measure.sh 1` | Both daemons measured. canary: `sends=3 taken_within_60s=3 woke_within_60s=3`, p50 0.5 s. Marshal over the hour: M1 1 planned start (MAR-70's 15:31:58), 0 errors; 8 direct messages, 0 unread after 30 min; M4 the 15:31:58 restart with 1 unread, 1 taken after; 0 duplicate nicknames. The first try stopped early when the window held no restart (a `grep` with no match under `pipefail`), fixed before this run |
| `teardown.sh` | 16:09:55 sessions ended and state kept in `~/.local/state/marshal-soak/20261109T160955`; directory removed; roster without the test sessions at 16:10:53 |
| Final scripts | setup waits for each `ready` now; it was changed after the run above, so setup and teardown were run again, 16:12:16 to 16:13:45, both clean. The sessions got the **same nicknames** as the first run, since Marshal keeps a nickname for a session in the same directory (MAR-9). So nicknames in a later run's logs can repeat an earlier run's |

# Cutover kit: prove one session on Marshal (MAR-59)

**Run twice on Murph, 2026-11-09 (MAR-70): every check passes.** Run 1's check 4 didn't test what it
said: the session's held wait hook took the token before the restart, so nothing was unread in the
daemon (see [(d) Run 1](#d-run-1-2026-11-09-mar-70)). Check 4 below is the method of
[(e) Run 2](#e-run-2-2026-11-09-mar-70-check-4-rerun), which showed the mail unread in the database across the restart. This kit is for step 1 of the cutover
runbook (rally's `docs/cutover.md`, RAL-119): "prove one session". Fork-only.

The goal is one fresh Claude Code session wired only to Marshal, with nothing user-wide changed. It
must show on Marshal's roster, wake when idle on `marshal-shim tell`, and get its unread mail after
`systemctl --user restart marshal-daemon`.

## What Murph has (read 2026-11-08, read-only)

- `marshal-daemon.service`: `MARSHAL_BIND=127.0.0.1:6155`, set by the loopback drop-in after Josh's
  decision (the base unit still says `0.0.0.0:6155`, and the drop-in wins). Postgres db `marshal` on
  the `marshal-pg` socket. It's listening on `127.0.0.1:6155` (WS) and `127.0.0.1:6156` (hooks).
- `~/.cargo/bin/marshal-shim` 0.58.1, which has `roster`, `tell` and `doctor`.
- No `~/.config/marshal/daemon-address` file, so the address comes from the environment, or else the
  default `ws://localhost:6155`.
- The user-wide wiring is muster's: five `muster-shim` hooks in `~/.claude/settings.json`, and the
  `muster` MCP server in `~/.claude.json`. The kit leaves both alone.

## Why the launch flags matter

A project's `.claude/settings.json` **adds** to the user's settings; it doesn't replace them. So a
session opened in the test directory with plain `claude` would run muster's hooks too, and load the
`muster` MCP server. Two flags keep it on Marshal only:

- `--setting-sources project` loads only the project's `.claude/settings.json`, so the user-wide
  muster hooks don't run.
- `--strict-mcp-config --mcp-config .mcp.json` loads only the `marshal` server.

Run `marshal-shim doctor` in the test directory too. Since MAR-60 it reads a project's own
`.claude/settings.json` and `.mcp.json` as well, and it should say `ok` for `claude mcp` and `claude hook`.
Until MAR-71 is deployed, its `daemon` line says FAIL ("answered HTTP 404") against a healthy daemon.
That's doctor's bug, not the daemon's: `roster` working shows the daemon is fine.

## (a) Setup

Pick an empty directory outside every repository. This kit uses `~/src/marshal-cutover-test`:

```sh
mkdir -p ~/src/marshal-cutover-test/.claude
cd ~/src/marshal-cutover-test
```

`.claude/settings.json`: the five hooks from `plugins/marshal-shim/hooks/hooks.json`, by absolute path,
with the same events, `asyncRewake` and timeouts:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          { "type": "command", "command": "/home/joshd/.cargo/bin/marshal-shim claude-hook session-start", "timeout": 10 },
          { "type": "command", "command": "/home/joshd/.cargo/bin/marshal-shim claude-hook wait", "asyncRewake": true, "timeout": 2592000 }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          { "type": "command", "command": "/home/joshd/.cargo/bin/marshal-shim claude-hook wait", "asyncRewake": true, "timeout": 2592000 }
        ]
      }
    ],
    "UserPromptSubmit": [
      {
        "hooks": [
          { "type": "command", "command": "/home/joshd/.cargo/bin/marshal-shim claude-hook prompt-submit", "timeout": 10 }
        ]
      }
    ],
    "PostToolUse": [
      {
        "hooks": [
          { "type": "command", "command": "/home/joshd/.cargo/bin/marshal-shim claude-hook post-tool-use", "timeout": 10 }
        ]
      }
    ]
  }
}
```

`.mcp.json`: the plugin's server, by absolute path, with the address given explicitly:

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

Launch it in its own terminal (or tmux window). The hooks inherit `MARSHAL_DAEMON_ADDRESS` from this
shell, so they reach `127.0.0.1:6156`, and the MCP server gets it from `.mcp.json`:

```sh
cd ~/src/marshal-cutover-test
MARSHAL_DAEMON_ADDRESS=ws://127.0.0.1:6155 \
  claude --setting-sources project --strict-mcp-config --mcp-config .mcp.json
```

Give it one prompt so it starts and then goes idle, for example: "Say hello, then wait."

## (b) Checks

Run these from another terminal on Murph. Use `MARSHAL_DAEMON_ADDRESS=ws://127.0.0.1:6155` for every
`marshal-shim` command, so a stray address can't point it elsewhere. In the session itself, `/mcp`
lists the connected servers.

**1. Only Marshal is wired.**
In the session, `/mcp` lists `marshal` and not `muster`. And `/hooks` lists the five
`marshal-shim claude-hook …` commands and no `muster-shim` ones.

**2. On Marshal's roster.**

```sh
MARSHAL_DAEMON_ADDRESS=ws://127.0.0.1:6155 ~/.cargo/bin/marshal-shim roster \
  | jq '.sessions[] | select(.declared.cwd // "" | test("marshal-cutover-test"))
        | {nickname, wake, bound: .recorded.bound_to_connection}'
```

Expected: one row, with `bound: true` and `wake: "waiting"` once the session is idle after its first
reply. Note the nickname for the next checks. It isn't on muster's roster: `muster-shim roster | grep
marshal-cutover-test` prints nothing.

If the row's `cwd` sits somewhere else, find it by its nickname instead: it's the newest `claude` row.

**3. Idle wake.**
With the session idle (it has replied, and nothing is typed):

```sh
MARSHAL_DAEMON_ADDRESS=ws://127.0.0.1:6155 ~/.cargo/bin/marshal-shim tell <nickname> "cutover check: idle wake"
```

Expected: `tell` prints the session with `in inbox, wake expected but not observed`. That's the
honest answer for a hook-delivered session: the waiter is held, but `tell` can't see the wake
happen. `in inbox, wake not observed` instead means no waiter was held, so stop and check the
`/hooks` list. Within a few seconds the session wakes by itself and shows the message, prefixed
`[from a terminal on Murph; it can't receive replies]`, without anyone typing into it. The sender is
shown under a fresh nickname, a different one for each `tell` (run 1 saw giant-serow, crimson-brill and
rapid-jackdaw). That nickname belongs to the short-lived `terminal` session each run registers and
removes on exit (`terminal_send.rs`). It isn't a peer, and the prefix is what tells the session it
can't reply.

**4. Mail survives a daemon restart.**

The token has to be **unread in the daemon** when it restarts, as the `marshal` database shows. A
session's `claude-hook wait` (started by SessionStart and Stop, with `asyncRewake`) runs in the
background even during a turn, and takes mail the moment it's sent. So keeping the session busy isn't
enough: run 1 did that, and the token was taken 19 s before the restart (see (d)). Instead, stop the
session's waiters so nothing takes the mail, and check the database on both sides of the restart.

This restarts the live Marshal daemon, which drops every Marshal session for a moment. Check
`marshal-shim roster` first, and tell anyone else on it. Restarting the daemon is the lamplighter's job:
ask them. With the waiters stopped the token stays unread, so the moment doesn't need agreeing.

1. With the session idle after its first reply, stop its waiters. They're the `marshal-shim
   claude-hook wait` children of the session's own `claude` process. Kill only those, not the MCP
   server (`marshal-shim` with no arguments):

   ```sh
   pstree -p "$(tmux list-panes -t <window> -F '#{pane_pid}')"   # find the session's claude PID
   ps -o pid,args --ppid <claude PID>   # two `marshal-shim claude-hook wait`, one bare `marshal-shim`
   kill -TERM <the two wait PIDs>
   ```

   `marshal-shim roster` then shows the session with `wake: "none"` and still `bound: true`.
2. Send a token. `tell` says `in inbox, wake not observed`, and the session doesn't wake:

   ```sh
   T="cutover check 4: unread across restart $(date +%s)"
   MARSHAL_DAEMON_ADDRESS=ws://127.0.0.1:6155 ~/.cargo/bin/marshal-shim tell <nickname> "$T"
   ```
3. Read the database, read-only. Find the token's message id, and check it has no `MessageRead`:

   ```sh
   q() { PGOPTIONS='-c default_transaction_read_only=on' psql -h /run/user/$UID/marshal-pg -d marshal -X -A -t -c "$1"; }
   q "select id, created_at, item_id from myko_events where item_type='Message' and event->'item'->>'body' like '%${T##* }%'"
   q "select count(*) from myko_events where item_type='MessageRead' and event->'item'->>'messageId'='<item_id>'"   # 0
   ```
4. Have the lamplighter run `systemctl --user restart marshal-daemon`. Once it's back, the count in
   step 3 is still 0, and the roster shows the session `bound: true` again.
5. Prompt the session, for example "Say hi, and tell me exactly what any peer or terminal messages you
   just received say". Its UserPromptSubmit hook takes the mail. Expected: the session shows the `$T`
   message once, the step-3 query now finds exactly one `MessageRead`, dated after the restart, and the
   roster shows the **same nickname** (MAR-9).

If the message doesn't show, that's a regression: MAR-14's restart cells on Postgres, merged and run in
CI, show unread mail surviving a restart. Stop there and report it to the watchman, with the token and
the time of the restart, rather than going on with the cutover.

## (c) Teardown

1. In the session, `/exit`.
2. Remove the test directory: `rm -rf ~/src/marshal-cutover-test`. Nothing outside it was changed.
3. The roster row goes on its own once its connection is gone (`STALE_AFTER`, 60 s after the shim
   disconnects and its wait ends). Once a daemon with MAR-46 is deployed, `marshal-daemon forget
   <nickname>` removes it at once. The daemon installed on 2026-11-08 predates MAR-46.
4. Mail the checks sent stays in Marshal's database, as any message does (14-day TTL).

Rollback is nothing more: the user-wide muster wiring was never touched.

## (d) Run 1, 2026-11-09 (MAR-70)

Run by cobalt-bream on Murph, for the watchman (mellow-weevil). Restarts were done by the lamplighter
(eager-cobra). Claude Code 2.1.319, marshal-shim 0.58.1, marshal-daemon on 15295bb. The test session was
started in a detached tmux window (`tmux new-session -d -s mar70 …` with the launch line in (a)) and
driven with `tmux send-keys`. Marshal's roster was empty before it started. Nothing user-wide was
changed, and muster-daemon wasn't touched.

| Check | Result |
|---|---|
| doctor in the test dir | `ok` binary, `ok` claude mcp ("marshal-shim in .mcp.json"), `ok` claude hook ("all five in .claude/settings.json"); **FAIL** daemon: `ws://127.0.0.1:6155 answered HTTP 404 … isn't a Marshal daemon`. The daemon serves its WebSocket at `/myko` only, and doctor probed `/`. That bug is MAR-71 (#79); with it, doctor says `ok daemon accepted a handshake at ws://127.0.0.1:6155/myko` |
| 1. Only Marshal wired | Pass. `/mcp`: `marshal` (6 tools) and Claude Code's built-in `claude-in-chrome`, and no `muster`. `/hooks`: "5 hooks on 4 events", all `[Project]` |
| 2. On Marshal's roster | Pass. `elder-caracal /home/joshd/src/marshal-cutover-test waiting bound=True`; not on muster's roster (`muster-shim roster \| grep -c marshal-cutover-test` → 0) |
| 3. Idle wake | Pass. 13:55:53, `tell` → `in inbox, wake expected but not observed`; the session woke by itself through its Stop hook in a few seconds, and said the message came from a terminal that can't receive replies |
| 4. Mail survives a restart | **Not shown** (corrected 2026-11-09, found in MAR-73). Token `cutover check: survives restart 1794250728` sent at 13:58:49 while the session was in the foreground loop; daemon restarted at 13:59:08 (PID 603362 → 1195712); after the loop ended at 14:05:01 the session showed the token once. But the `marshal` db has its `MessageRead` (event 1008) at 13:58:49.070, 0.2 s after the send (event 1004) and 19 s *before* the restart: the held wait hook had taken it. So it was never unread in the daemon at the restart. What this does show: taken mail wasn't lost or offered twice across the restart, and the roster afterwards had `elder-caracal … waiting bound=True`, the same nickname |
| 3 again | Pass. 14:05:55, the idle session woke on `tell` again |
| Teardown | 14:07, `/exit`, tmux killed, directory removed; no `marshal-shim` processes left; Marshal's roster empty by 14:08:25 |

What went wrong on the way, and is now in the steps above:

- **The first try at check 4 didn't count.** The `sleep` prompt didn't submit (a long line sent with
  `tmux send-keys … Enter` sat in the input box), so the session was idle and took the token at once.
  The restart for it (13:56:48) also came 4 s before the token. A second prompt to `sleep` was run in
  the background, and a foreground `sleep 480` was refused by Claude Code's harness. The foreground
  `for … date; sleep 10` loop worked. When driving the session through tmux, send the text and the
  `Enter` as two `send-keys` calls, and check the pane.
- **Each `tell` shows a new sender nickname.** That's by design (see check 3), but the kit didn't say,
  and the session first called each one "a peer".
- **The kit named the wrong lamplighter**, and said doctor can't read project-local wiring, which
  stopped being true at MAR-60.

## (e) Run 2, 2026-11-09 (MAR-70, check 4 rerun)

Run by cobalt-bream on Murph, with the restart by the lamplighter (eager-cobra). Claude Code 2.1.319,
marshal-shim and marshal-daemon on 98da488 (with MAR-71). The setup was as in (a), in a fresh
`~/src/marshal-cutover-test`, in tmux session `mar70b`. Marshal's roster was empty before it started.

| Step | Result |
|---|---|
| doctor | `ok` binary, `ok daemon accepted a handshake at ws://127.0.0.1:6155/myko`, `ok` claude mcp, `ok` claude hook |
| Session | `aurora-ibis /home/joshd/src/marshal-cutover-test waiting bound=True` after its first reply |
| 1. Waiters stopped | 15:13, the two `claude-hook wait` children of the session's `claude` (PID 1630348) sent SIGTERM, leaving the MCP server; roster: `aurora-ibis none True` |
| 2. Token sent | 15:13:10, `cutover check 4 rerun: unread across restart 1794255190` → `in inbox, wake not observed`; the session didn't wake |
| 3. Unread before | `Message` event 1189 (created 15:13:10.71), id `01a1c1f1-7ab6-7b93-b529-71d184728a5a`; `MessageRead` for it: **0** |
| 4. Restart | 15:31:58, PID 1581365 → 1693091. `MessageRead` for it at 15:32:13 and 15:32:15: still **0**; roster `aurora-ibis none True` |
| 5. Taken after | 15:32:27, prompt sent; `MessageRead` event 1230 at 15:32:27.766, the only one. The session quoted the token verbatim, once, from a sender shown as gusty-dorado (a `tell` terminal session) |
| Teardown | 15:33, tmux killed, directory removed |

So unread mail kept in the daemon's Postgres store survived a `marshal-daemon` restart, and reached
the session once, at its next hook, with its nickname kept. This is the check MAR-14's CI cells make,
now seen on Murph.


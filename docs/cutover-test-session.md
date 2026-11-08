# Cutover kit: prove one session on Marshal (MAR-59)

**Not yet run.** This kit is for step 1 of the cutover runbook (rally's `docs/cutover.md`, RAL-119): "prove
one session". It's ready to use, but whether and when to run it is Josh's call. Nothing here has been
started. Fork-only.

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

`marshal-shim doctor` reads the user-wide `~/.claude/settings.json`, not the project's, so it can't
check this session's wiring. The checks below do that instead.

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
`[from a terminal on Murph; it can't receive replies]`, without anyone typing into it.

**4. Mail survives a daemon restart.**
This restarts the live Marshal daemon. On Murph only the test session should be on Marshal at this
point; check `marshal-shim roster` first, and stop if anyone else is on it. Restarting the daemon is
the lamplighter's job (cobalt-bream), with Josh's go-ahead.

1. Make the session busy for a while, so it won't take the mail until its next hook. Prompt it: "Run
   `sleep 90` in the shell, then tell me it's done."
2. While it sleeps, send a token, then restart the daemon:

   ```sh
   T="cutover check: survives restart $(date +%s)"
   MARSHAL_DAEMON_ADDRESS=ws://127.0.0.1:6155 ~/.cargo/bin/marshal-shim tell <nickname> "$T"
   systemctl --user restart marshal-daemon
   ```
3. Expected: when the sleep finishes, the session's next hook shows the `$T` message, once. After it
   goes idle again, `marshal-shim roster` shows it with the **same nickname** (MAR-9) and `wake:
   "waiting"`. Then check 3 passes again.

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

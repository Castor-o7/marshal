# Muster's terminal commands that Marshal doesn't have (MAR-17)

MAR-17 lists what muster's terminal commands do beyond what upstream #168 brought to Marshal (`tell`,
`all` and `roster`), and records for each whether it's ported or dropped, and why. The comparison was
made against muster's `main` at a120efc and Marshal's fork `main` at ce6281a, on 2026-11-06. Josh made
the port-or-drop choices that day.

## `tell`, `all` and `roster`: nothing further

Marshal's `tell` and `all` already do everything muster's do. They also refuse a nickname two sessions
share (listing their ids) and an id prefix that's too short, and print one line per recipient saying
what the send did. Muster keeps one terminal session id per machine in a file (`terminal-session`).
Marshal registers a fresh id for each run instead, so two runs at once can't remove each other's
sender. That's a deliberate difference, not a gap.

## The rest

| Muster has | What it does | Decision | Why |
|---|---|---|---|
| `doctor` | Read-only checks: the shim on `PATH` is this binary, the daemon accepts a connection, and Claude Code is wired with the MCP server and every hook once each | **Port**, as MAR-36 | Additive and read-only, and it gives a one-command answer to "why isn't delivery working on this machine". Muster's credential check is left out, since Marshal has no pairing. |
| `pair`, `pairing`, `unpair` | Pair this machine with muster's gated daemon, show the pairing, remove it | **Drop** | They need muster's gate and credentials. Marshal's daemon has no gate, and adding one would change Marshal's defaults, which the fork doesn't do without upstream's say. |
| roster `observed` and `attested` | Split each session's facts by whether the gate vouches for them | **Drop** | Same reason: without a gate there's nothing to attest. Marshal's roster already keeps what the daemon recorded (`recorded`) apart from what the session claims (`declared`). |
| roster `role` | The role a session declares | **Already MAR-9** | Stable nickname and declared role are one story. Until then, `spawn-team` puts the role in the tmux name and the first task. |
| roster `harness` and `current_task_at` | Which harness a session runs, and when its task was last set | **Drop** | Josh chose not to port them now. The roster's `declared.kind` and `last_activity_at` cover most uses. |
| `setup` | Install the plugin through `claude plugin` in one command, dry run by default | **Drop** | Josh chose not to port it now. It changes Claude Code's configuration, and `codex-setup` already does the Codex side. |
| wake pause, roster `wakes_paused` | Pause every wake daemon-wide, for maintenance | **Drop** | Josh chose not to port it now. It's a daemon feature, not a terminal command, and the conformance suite already marks the pause cells as not applicable to Marshal. |

A dropped item can come back as its own story if it's wanted later.

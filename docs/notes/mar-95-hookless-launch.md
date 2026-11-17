# MAR-95: launching an agent that has no harness hooks

Design note, not a plan of record. Read against upstream `3d595ec`. Nothing here is to be PR'd upstream
until Josh says.

## The question

`marshal-launch` (MAR-93/94) does two things through the harness's hooks:

1. **Join.** The roster row it waits for is the one the agent's `SessionStart` hook registers
   (`marshal-shim claude-hook session-start`, which POSTs `/hook/session-start` with `MARSHAL_AGENT_ID`).
   The join proof matches that row on `agent_id`, `host.name` (first label) and `cwd`.
2. **First task.** It sends the task as one direct message. The `claude-hook wait` hook holds a long-poll
   (`/hook/wait`) and wakes the idle session (`asyncRewake`) when the message lands. The
   `UserPromptSubmit` hook then surfaces it as `<marshal_inbox>` and acks it.

An agent with no hooks configured loses both. This note compares three ways to replace them, against
those two tests: the join proof, and "acts on its first task with nobody typing".

## What upstream already does without hooks

**The Claude MCP shim registers itself.** When Claude starts its MCP servers, `marshal-shim` (no
subcommand) builds a `Session` and sends `RegisterSession` on every connect (`main.rs`, the
`connection_status` subscriber, plus a self-verifying re-SET loop). That row carries:

- `id`: Claude's own session id, from `CLAUDE_CODE_SESSION_ID`, which Claude puts in every child's
  environment at exec time (`session_discovery::resolve`). It is present before the first prompt, and
  there is no transcript race. The shim refuses to start rather than register under an invented id.
- `agent_id`: `MARSHAL_AGENT_ID` from its environment (`codex_hook::agent_id_from_env`), the same field
  the hooks send.
- `host`: `detect_host()`, the full `gethostname`. The hook path sends `short_host()`, the first label,
  and the join proof now compares first labels, so both match the adapter's `uname -n`.
- `cwd`: `std::env::current_dir()` of the shim, which is Claude's folder (physical path), the same value
  the adapters report.
- `channels_enabled`: read from the parent's command line.

So for Claude, **the MCP shim already produces a row that passes the join proof, with no hooks.**

**Codex is the exception.** Under `MARSHAL_HARNESS=codex` the shim registers nothing: Codex never tells
its MCP servers their session id (openai/codex#19937), so identity is owned by the `SessionStart` hook.
The one hook-free registration path for Codex is the `codex-run` bridge: it learns the thread id from the
app-server's `thread/started` and POSTs `/hook/session-register` (a daemon endpoint, not a harness hook).

**Mail without hooks.** A Claude session has three ways to be woken:

- `notifications/claude/channel`: the shim pushes it live, but Claude only acts on it when launched with
  `--dangerously-load-development-channels …marshal…` (`channels::channels_enabled_in`).
- `marshal-shim watch <session_id> [--once]`: it long-polls `/hook/wait` and prints each inbox block.
  Run under Claude's Monitor tool, or as a background Bash command with `--once`, its output wakes the
  session. But the agent has to start it itself, which takes a turn.
- The `UserPromptSubmit` and `PostToolUse` hooks: these are the hooks this note is replacing.

An MCP server alone cannot start a model turn. With no hooks and no channels flag, **nothing can wake an
idle Claude.** Some first turn has to come from outside.

## Option 1: registration by the agent's MCP shim

The launcher keeps doing what it does today (mint `agent_id`, pass it in `env`, wait for the row). The
row now comes from the MCP shim's `RegisterSession`, not from a hook.

- **Join proof:** passes as is. The row carries the launched `agent_id`, and its host and cwd are the
  ones the adapter reports. No launcher change is needed. `join::find` doesn't care which path
  registered the row.
- **Timing:** Claude starts MCP servers at launch and the session id is in the environment, so the row
  appears before any prompt. That's as early as `SessionStart`.
- **Prerequisite on the target:** the marshal MCP server must load with nobody there to approve it. A
  project `.mcp.json` asks for approval, so it has to be user scope (`~/.claude.json`), a local project
  entry, or the marshal-shim plugin, provisioned with the machine (as the design's "what a target must
  already have" says). `check` already reports this (MAR-94).
- **Codex:** does not work. The shim owns no identity there. A hookless Codex needs the `codex-run`
  bridge, which is a different `argv` (`marshal-shim codex-run …`) and a separate piece of work.
- **First task:** not addressed. It still needs a turn (see option 3).

## Option 2: a row the launcher registers for the agent

The launcher registers a `Session` itself, with the launched `agent_id`, host and cwd, and hands the
first task to that row.

- **Join proof:** defeated. The launcher would be proving its own row. The proof exists to show that
  *the launched process* came up, and a launcher-made row shows nothing about the agent.
- **Identity:** the launcher can't know the harness's session id (Claude mints it at start). So the row
  is not the agent's session. Mail to it reaches no process. When the agent's MCP shim starts, it
  registers a second row with the same `agent_id`, and `join::find` sees two candidates.
- **First task:** a message to the launcher's row is read by nobody.
- **Verdict:** reject. Its only legitimate use is a short-lived "launching" placeholder for `launch
  list`, which the launch record (`launches.json`) already provides.

## Option 3: delivering the first task without hooks

The task stays one direct Marshal message (durable, acked, auditable). What changes is how the idle
agent gets a turn to read it.

**3a. Channels flag.** Launch `claude --dangerously-load-development-channels plugin:marshal-shim@…`.
The shim's live push then wakes the session.
- Breaks rule 1 (no flags added to the harness). It is also a development flag, and it changes what the
  session can receive while unattended. Reject.

**3b. A launcher bootstrap prompt.** The launcher appends one fixed positional prompt to `argv`
(`claude "<bootstrap>"`). That starts the interactive session with a first turn. It's not a flag and
changes no permission mode. The bootstrap says, in effect: *"You were started by marshal-launch. Your
first task arrives as a Marshal direct message. Wait for it with `marshal-shim watch <your session id>
--once` as a background command (your id is in the marshal MCP server's instructions), act on it, then
ack it."*
- **Join proof:** unchanged (option 1 registers the row).
- **Nobody typing:** yes. The first turn is the bootstrap, and the watch's exit wakes the session when
  the task lands.
- **Order:** the bootstrap turn may run before the launcher has sent the task (it sends after the join).
  That's why it waits with `watch --once` rather than reading the inbox once. `/hook/wait` is a daemon
  endpoint, not a harness hook, so it's available. The task is acked by `watch` (`ack=client`) only
  once it has been printed.
- **Later mail:** the same `watch` can be re-armed, as the README describes for hookless sessions. The
  harness still has no live push, as today.
- **Cost:** rule 1 says "run `argv` as given". This needs a ruling that a launcher-owned positional
  prompt, with fixed text the launcher shows the user, is allowed and is not a flag. The prompt must not
  carry the task text itself, so peer-supplied content never becomes the agent's top-level instruction:
  it stays framed as a Marshal message.
- **Harness support:** Claude takes a positional prompt in interactive mode. Codex also does (`codex
  "<prompt>"`), but has no registering MCP shim (above).

**3c. The adapter types the task.** For example `tmux send-keys` after the join.
- Works only for adapters with a terminal (tmux, ssh). A VM, container or cloud runner has none. It
  types into whatever is on screen: a trust prompt, an update notice or an approval dialog would take
  the keystrokes. It's also "somebody typing" in all but name. Reject, except as a manual fallback.

**3d. MCP-only delivery.** Rely on the shim's resources and tools (`marshal://messages`, `messages`,
`ack_messages`).
- Fine for reading once a turn exists, but MCP cannot start a turn. On its own it fails "nobody typing".
  It is what 3b's turn uses.

## Recommendation

**Option 1 for the join, with 3b for the first task, for Claude.** Rule out 2, 3a and 3c.

- The join needs no launcher change: on a target with the marshal MCP server and no hooks, the shim's
  own row already passes the proof.
- The first task needs one launcher change: when the adapter reports (or `check` finds) that the
  target's harness has no marshal hooks, `marshal-launch` appends the fixed bootstrap prompt to `argv`.
  With hooks present it doesn't, and today's path stays as it is.
- Codex without hooks goes through the `codex-run` bridge (`/hook/session-register` plus `turn/start`),
  but `turn/start` would have to carry the inbox itself instead of leaving it to the `UserPromptSubmit`
  hook. That's a follow-up, not part of this.

## Questions for Josh

1. **Rule 1:** may the launcher add one fixed positional bootstrap prompt (not a flag, no permission
   change) when the target has no marshal hooks? If not, a hookless launch can register but can't act on
   a first task with nobody typing. That would be "joins; first task waits for the operator".
2. **When to use it:** automatically when `check` finds no hooks, or only with an explicit
   `--no-hooks` (or `--bootstrap`) option on `marshal-launch`?
3. **Codex:** in scope for MAR-95, or a separate story?

## Test for the done-when

An e2e like `first_task_e2e.rs`, with the folder's settings carrying **no hooks** and the marshal MCP
server loaded at user or local scope, using the real release daemon via `RealDaemon`. It asserts three
things: the row registered by the MCP shim (no `/hook/session-start` in the daemon log), the task
marked read, and the agent's answer in the pane. One open problem: on Murph, getting the MCP server
loaded without an approval prompt means writing user-scope config (`~/.claude.json`) or using a separate
`CLAUDE_CONFIG_DIR`, which has no login. Both need Josh.

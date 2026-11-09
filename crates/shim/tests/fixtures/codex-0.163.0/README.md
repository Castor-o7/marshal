# Codex 0.163.0: app-server frames and hook input

What the conformance suite's fake Codex sends the real `marshal-shim
codex-bridge` (the app-server's JSON-RPC over WebSocket) and the real
`marshal-shim codex-hook` (the hook JSON on stdin). `{{thread_id}}` and
`{{cwd}}` are filled in per cell.

- `initialize-result.json`: the result of `initialize`.
- `thread-started.json`: the notification an attached TUI's thread causes,
  which the bridge registers.
- `turn-start-result.json`: `turn/start` accepted on an idle thread.
- `turn-start-busy-error.json`: `turn/start` refused while a turn runs.
- `user-prompt-submit.json`, `post-tool-use.json`: hook input.

**Constructed, not recorded.** These follow the frames the bridge's own tests
use (`codex_bridge_e2e.rs`) and the Codex version the bridge pins
(`codex_bridge.rs`, 0.163.0). Codex isn't installed where they were written.
See `docs/delivery-conformance.md`, "Re-recording fixtures". The bridge reads
the thread id, session id and cwd, and the hooks read `session_id` and `cwd`.
The busy error's exact code and text are a guess; the bridge treats any error
as "retry later".

The hook inputs carry every field Codex 0.163.0 sends (`hooks/src/schema.rs`,
`UserPromptSubmitCommandInput` and the `PostToolUse` input, read for MAR-84):
`session_id`, `turn_id`, `transcript_path`, `cwd`, `hook_event_name`, `model`,
`permission_mode`, and the event's own. `transcript_path` is the session's
rollout file; it's `null` here, as Codex sends when there's no local rollout,
so these cells keep the hook's read-at-once path. The shim's two-phase path
for Codex (MAR-84) is tested in `claude_hook.rs` with a rollout file.

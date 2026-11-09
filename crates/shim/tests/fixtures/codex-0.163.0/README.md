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

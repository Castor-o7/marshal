# Claude Code 2.1.318 hook input

The JSON Claude Code writes to a command hook's stdin, one file per event the
Marshal plugin hooks (`plugins/marshal-shim/hooks/hooks.json`). The conformance
suite's fake Claude feeds these to `marshal-shim claude-hook`, filling in
`{{session_id}}` and `{{cwd}}`.

**Constructed, not recorded.** These follow Claude Code's documented hook
input for this version. They haven't been captured from a live run yet. See
`docs/delivery-conformance.md`, "Re-recording fixtures", to replace them with
recorded ones. The shim reads only `session_id` and `cwd`, so the other fields
matter only if it starts reading them.

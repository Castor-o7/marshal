# Two-phase delivery for Codex: which signal says the model has it (MAR-84)

**Research for the watchman's read. Nothing here is built yet.** MAR-82 made Claude sessions mark mail
read only once their transcript shows it reached the model ([two-phase-delivery.md](two-phase-delivery.md)).
This is the Codex half: which signal Codex gives that a hook's mail reached the model, read from
Codex's source at tag **`rust-v0.163.0`** (the version our Codex fixtures name). Paths below are under
`codex-rs/` in github.com/openai/codex at that tag.

**Records: pending.** Codex isn't installed on Murph, so no Codex records have been recorded yet. Whether
to install and log it in is Josh's call (the watchman has asked). The shapes below come from the source.
Done-when needs records recorded from a live Codex, as for Claude, before the matcher is trusted.

## How Codex delivers our mail today

Codex has no way to inject into a running turn from outside, so Marshal delivers at hook boundaries.
`marshal-shim codex-hook` runs as `UserPromptSubmit` and `PostToolUse`, and prints the inbox as
`hookSpecificOutput.additionalContext`. `codex-run`'s app-server bridge only *wakes* an idle thread
with `turn/start`. Its `UserPromptSubmit` hook then injects the mail (`crates/shim/src/codex_bridge.rs`,
module docs). The shim acks the mail read once it has written the hook output (`codex_hook.rs`,
`run_for`).

**The gap.** The hook's context goes into the turn's history before the model is called, and nothing
says the model was then called. If the turn stops first, the mail is marked read and the model never
had it. That happens on a blocked prompt, a compaction error, a cancelled MCP resolution, an interrupt,
or Codex being killed. It's narrower than Claude's (where a held rewake could wait a whole turn), but
it's the same kind of loss.

## What Codex gives (from source, 0.163.0)

**1. The hook input has a transcript path.** `UserPromptSubmit`'s stdin
(`hooks/src/schema.rs:566-583`, `UserPromptSubmitCommandInput`) is `session_id`, `turn_id`,
`agent_id`/`agent_type` (subagents only), **`transcript_path`**, `cwd`, `hook_event_name`, `model`,
`permission_mode`, `prompt`. `PostToolUse` (`schema.rs:320-342`) has the same first eight, plus
`tool_name`, `tool_input`, `tool_response` and `tool_use_id`. `transcript_path` is the session's rollout
file. It's null when there's no local rollout, and asking for it makes Codex write the file
(`core/src/session/mod.rs:5333-5345`). *Our constructed fixture
`crates/shim/tests/fixtures/codex-0.163.0/user-prompt-submit.json` leaves out `turn_id`,
`transcript_path`, `model` and `permission_mode`, and should gain them.*

**2. Hook context lands in the rollout file as a developer message.** Each `additionalContext` string
becomes its own history item: `ResponseItem::Message { role: "developer", content: [{ type:
"input_text", text: <the text, verbatim> }] }`, tagged `content_item_kinds: ["hooks.additional_context"]`,
with no wrapper (`core/src/context/hook_additional_context.rs:15-35`, `core/src/hook_runtime.rs:849-873`).
It's recorded to history and to the rollout (`core/src/session/mod.rs:3610-3753`) as a JSON line
`{"timestamp": …, "type": "response_item", "payload": { "type": "message", "role": "developer",
"content": [{ "type": "input_text", "text": "<marshal_inbox …>" }], … }}` (`history/src/lib.rs:369-375`,
`history/src/rollout_payload.rs:31-41`).

**3. But it's recorded before the model is called, and even when it isn't.** `UserPromptSubmit`'s
context is recorded right after the user's prompt, before the first sampling request
(`core/src/session/turn.rs:932-980`, called at `:377-387`). It's still recorded when the hook blocks
the prompt, and when the turn stops before sampling (compaction error, cancelled MCP resolution, step
abort: `turn.rs:196-226`, `:249-262`, `:278-286`; `tasks/regular.rs:84-93`). `PostToolUse`'s context is
recorded only when the tool succeeded, and goes out with the next sampling request
(`core/src/tools/registry.rs:810-843`). Rollout writes go through a background writer and aren't awaited
before the request (`rollout/src/recorder.rs:1048-1060`, `:1952-1962`), so the line can reach the disk a
little after the request.

**4. The app-server's notifications are weaker.** `turn/started` comes before the hooks run
(`tasks/regular.rs:63-70`). Developer messages map to no `item/*` notification
(`core/src/event_mapping.rs:181-200`). `hook/completed` (`app-server-protocol/…/v2/hook.rs:100-157`)
carries the context text, but it's sent before the text is recorded, and only for non-builtin
synchronous hooks. `rawResponseItem/completed` carries the developer message only with
`experimentalRawEvents`. `turn/completed`'s `turn.status` is `completed`, `interrupted`, `failed` or
`inProgress` (`v2/turn.rs:33-38`). And a plain `codex` (no `codex-run`) has no bridge at all, though it
writes the same rollout file (`tui/src/lib.rs:36-37`, `:619`; `rollout/src/recorder.rs:1743-1758`).

## Proposal: confirm from the rollout file, as Claude does from its transcript

Use the same offer-then-confirm as MAR-82, with the rollout file as the transcript:

1. **Offer.** `codex-hook` (harness `codex`) acks the mail it printed as `offered`, not read, and writes
   the `offered-<session>` marker, exactly as Claude's waiter does. The daemon side is MAR-82's,
   unchanged: `MessageOffer`, `/hook/offers`, the 6 h re-offer, and dropping on SessionEnd and reap.
2. **Confirm.** Each later Codex hook reads the tail of `transcript_path` and confirms an offered id
   only when:
   - a line `{"type":"response_item","payload":{"type":"message","role":"developer",…}}` holds the
     inbox's opening tag naming it (the same tag matcher as Claude's, ids exact, tool output skipped);
   - **and** a later line in the same file is model output: a `response_item` whose payload is an
     `assistant` message, a `reasoning` item, or a `function_call`. That's what says the model was
     called with the context in its history. The developer line alone isn't enough, because of (3).
3. **The next turn picks up the rest.** A turn that stopped before sampling leaves the context in
   history without model output after it. The mail stays offered. The next turn's model output, still
   with that context in history, confirms it then. Otherwise the 6 h re-offer sends it again: a
   duplicate is possible, but nothing is lost.

This works the same with `codex-run` or a plain `codex`, needs no bridge, and reuses MAR-82's daemon and
matcher. The new code is a Codex record filter beside Claude's, and `codex-hook` acking `offered` and
confirming.

**Failure modes, each failing safe (mail stays offered):**
- `transcript_path` is null (no local rollout): nothing can confirm, so the mail is re-offered after
  6 h. Alternatively, keep today's read for that case. Proposed: keep today's read, since it's no worse
  than now.
- The rollout line lands after the hook that would confirm it (background writer): a later hook
  confirms it.
- An idle rollout compressed to `.zst` (`rollout/src/compression.rs`; when that runs isn't traced):
  unreadable, so not confirmed, and re-offered at 6 h.
- Compaction rewrites history: the rollout still has the original lines, so this is unaffected.
- A retry (`error` with `willRetry`) resends the same history: a duplicate risk, not a loss.

**Not chosen:** the bridge's `hook/completed` plus `turn/completed`. It's sent before recording, it
needs the bridge (so a plain `codex` is out), and it only covers non-builtin hooks. It could be added
later to confirm faster under `codex-run`.

## Before building

- **Records** from a live Codex 0.163.0 (or whatever version Murph gets), from a disposable session:
  one prompt-submit delivery that ran, one blocked or interrupted before sampling, and one
  PostToolUse delivery, scrubbed to their shapes as for Claude. The matcher's tests use them, and a
  shape test fails when Codex changes.
- **The fixture fix** above (the real hook input's fields).
- **A live check**, as MAR-84's done-when says: kill Codex between its hook and the model, then see
  the mail arrive once.

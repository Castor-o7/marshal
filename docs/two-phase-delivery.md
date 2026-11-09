# Two-phase delivery: don't mark mail read until the model has it (MAR-79)

**A design for the watchman to take to Josh. Nothing here is built.** Fork-only for now; the same gap
is in Muster, so the shape may suit upstream too.

## The gap

A Claude session's mail is marked read when it's *handed to Claude Code*, not when the model has it.
In between, it can be lost.

`claude-hook wait` (the `asyncRewake` hook on SessionStart and Stop) holds a `/hook/wait` long-poll the
whole time the session lives, including while a turn is running. When mail arrives it writes the inbox
block to stderr, posts `/hook/ack` with the message ids, and exits 2 (`codex_hook.rs`, `wait_for_mail`).
The daemon then records a `MessageRead`. Claude Code doesn't show a rewake's output until the running
turn ends, and then starts a new turn with it.

So for a busy session the message is marked read at once, and the model sees it only when the turn
ends, which can be minutes or hours later. MAR-70's first check 4 showed it: a token's `MessageRead`
came 0.2 s after the send, while the session was mid-turn, and the session showed it six minutes later
(cutover-test-session.md (d)).

**Where it's lost.** Anything that ends the session, or drops the held output, between the ack and the
start of the rewake turn: a crash, a kill, `/exit` mid-turn, a terminal closed, Murph rebooting, or
Claude Code discarding a rewake (for example when two land together, or on `/clear` or compaction, which
we haven't characterised). The message is then read in the daemon and was never seen. Nothing retries
it, MAR-42 moves only *unread* mail to a successor, and the sender's `recipient.read` says it was read.

**What's already safe.** The daemon acks only after its own write succeeded and leases surfaced mail for
60 s, so a lost HTTP response re-offers it (`hooks.rs`, `HookOutcome`, `Leases`). The shim acks only
after its own stderr write succeeded. The gap is the step after that, inside Claude Code, which no ack
covers today.

## Proposal: offer, then confirm from the transcript

Split today's ack in two.

1. **Offer.** When the waiter hands mail on, it posts `/hook/ack` with `phase: "offered"`. The daemon
   records a durable `MessageOffer { session_id, message_id, offered_at }`, not a `MessageRead`. An
   offered message isn't surfaced again to that session while its offer stands. (Today's in-memory
   60 s lease still covers the time before the offer arrives.)
2. **Confirm.** Every later hook from that session (PostToolUse, Stop, UserPromptSubmit, SessionEnd)
   gets the session's `transcript_path` from Claude Code in its input. The shim reads the tail of that
   file and looks for the offered messages, which the inbox block names by id
   (`<marshal_inbox ids="…">`, a new attribute). An id that appears in the transcript has reached the
   model's context, so the shim posts `/hook/ack` with `phase: "seen"`, and only then does the daemon
   record the `MessageRead` and drop the offer.

The transcript is the one record of what the model was shown, and it's on the same machine as the
hook, so this checks the fact itself rather than inferring it from hook order. Inferring it is
fragile: the Stop that ends the turn the mail arrived *during* fires before the model has seen it.

**Which records count.** Claude Code writes a held rewake to the transcript *before* the model sees
it, as a `queue-operation` record with `operation: "enqueue"`, and writes it again, as a `user` record
or an `attachment` (`hook_success`, `hook_additional_context`, `queued_command`), when it goes to the
model. A count over one live session's transcript on Murph (2026-11-09, Claude Code 2.1.319) found
inbox blocks in 22 `enqueue` records, 8 `remove` records, 14 `user` records, 26 `attachment` records
and 31 `api-request-blob` records. So the confirm must match only records that went to the model,
never `queue-operation`. An `enqueue` is exactly the held state this design exists to tell apart. This
is Claude Code's internal format and could change, so the shim's matcher needs a test against a real
transcript captured from each Claude Code version we run, and the confirm timeout (below) is the
backstop if the format moves.

**If it's never confirmed.**
- **SessionEnd, or the session reaped** with offers outstanding: the offers are dropped, and the
  messages are unread again. A resumed session gets them on its next hook. A successor gets them by
  MAR-42, which moves unread mail. Nothing new is needed there, as long as an offer doesn't count as
  read.
- **A long-running session that never confirms** (a transcript the shim can't read, or a Claude Code
  change that stops writing the block): after a confirm timeout (proposed: 6 h, a setting) the daemon
  records the read anyway and logs `[hook] confirm timed out`. That way this can't turn into
  repeated re-offers, and the timeout count says when confirmation has stopped working.

**Duplicates.** Before handing on mail that was offered and never confirmed (after a restart, say),
the shim checks the transcript first. An id that's already there is confirmed, not shown again. So the
same session doesn't see a message twice, which keeps MAR-3's "never offered twice" for the model's
view. A successor in a new transcript can see mail its predecessor saw but never confirmed. That's
at-least-once, which is the right side to err on.

## Codex

Codex's MCP shim registers no session, and its hook injects the inbox as `additionalContext` in the
turn it runs in (`codex_hook.rs`). So the model has it in that turn, and the gap is smaller: only a
hook response lost after the shim's ack. The same split applies, with the confirm sent from Codex's next
hook (its turn has completed). The app-server bridge (`codex_bridge.rs`), which injects through
`turn/start`, can confirm on the turn's completion event. Codex can come second.

## What changes

| Where | Change |
|---|---|
| entities | `MessageOffer` (persisted, so a daemon restart keeps it). `AckMessages` takes a phase. Mail is unread until seen. Reads only for `seen`, or on the confirm timeout |
| daemon `hooks.rs` | `/hook/ack` with `offered` / `seen`. `surface_unread` leaves out mail with a standing offer to that session. The inbox block carries `ids="…"`, escaped like a handle. SessionEnd and the reaper drop that session's offers. A sweeper applies the confirm timeout |
| daemon `cleanup.rs` / succession | Offers drop with the session row. MAR-42 then moves the mail as unread |
| shim `codex_hook.rs` | The waiter acks `offered`. Every hook reads `transcript_path`'s tail (bounded, for example the last 2 MB) for outstanding ids and acks `seen`. Delivery checks the transcript first |
| roster / `read_messages` | A sender's `recipient` gains `offered` beside `read`, so "offered, not yet seen" shows |
| MAR-73 | M3's "taken" splits into offered and seen. Seen-but-late becomes a measure, and confirm timeouts a new M2 line |

## Tests it would need

- A daemon test: mail offered and never confirmed, then SessionEnd. The message is unread again, and
  the successor gets it (MAR-42's cell, with an offer outstanding).
- A shim test with a transcript captured from Claude Code (not a hand-written one): an id in a `user`
  or `attachment` record is acked `seen`; an id only in a `queue-operation` `enqueue` isn't; an id
  not there isn't; and mail already seen isn't delivered again.
- MAR-70's check 4 again, plus a new check 5: send while the session is busy, kill the session before
  the turn ends, start it again, and see the mail arrive once.
- A conformance cell for the confirm timeout.

## Questions for Josh

1. Is reading the transcript acceptable? It's the session's own file, read locally by its own hook,
   and only for the inbox's `ids` attribute, never sent anywhere. It also ties the shim to a Claude
   Code file format that isn't documented. The alternative is hook-order inference, which "Proposal"
   shows is wrong for a busy session.
2. The confirm timeout: 6 h, or another value? Or never time out, and alert instead?
3. Claude first and Codex after, or both together?
4. Fork-only, or written so it can go upstream (Muster has the same gap)?

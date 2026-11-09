# Claude Code 2.1.321 transcript records for a held rewake

`transcript-rewake.jsonl` holds the transcript records Claude Code 2.1.321
wrote for two Marshal inbox blocks delivered by `claude-hook wait`, recorded
from a disposable test session (MAR-59's kit wiring, nothing else in it) on
Murph, 2026-11-09, for MAR-82's transcript matcher (`src/transcript.rs`):

1. Mail sent while the session was busy in a foreground tool call:
   `queue-operation` `enqueue` (held), then `queue-operation` `remove`,
   `api-request-blob` and `attachment` (`queued_command`) when the next turn
   started.
2. Mail sent while it was idle: `queue-operation` `enqueue`, then
   `api-request-blob` and `user`.

**Recorded, then scrubbed.** Only the record shapes are kept: every key, and
the values that name a kind of record (`type`, `operation`, `role`,
`userType`, `entrypoint`, `version`). Every other string is `x`, every
number `0`, and each string holding an inbox block is replaced by a synthetic
block whose `ids` attribute is `{ID}`, which the tests fill in. Nothing from
the session itself is left.

To re-record for a newer Claude Code, run the same two deliveries in a fresh
kit session and scrub the same way.

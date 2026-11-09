# What "as reliable as Muster" means, measured on Murph (MAR-73)

Fork-only. Josh has asked that both daemons keep running, each as the other's backup, until Marshal is
as reliable as Muster. Step 3 of rally's
[cutover.md](https://github.com/Castor-o7/rally/blob/main/docs/cutover.md) moves the watchman and the
lamplighter. On Josh's direct go, the lamplighter began moving to Marshal at about 15:32 on
2026-11-09, just after this run, so Marshal's next run will have its first real agent traffic. This doc turns the phrase into a few measures that can be checked, gives
a read-only way to take each one from both daemons on Murph, records one run, and proposes a bar. **The
bar is a proposal for Josh to accept or change.** Nothing here changes either daemon.

**The headline from the first run:** neither daemon had an unplanned restart, an error or a panic in 7
days, and neither lost mail to a live session. But that tells us little about Marshal, because Marshal
carries almost no traffic. In the last 24 h Muster delivered 586 direct messages to 25 agent sessions,
and Marshal delivered 4, all of them cutover-test tokens. So the comparison can't be made until Marshal
carries real load, and the proposed bar (d) starts with that.

## (a) The measures

Each measure is taken for one daemon over a window, 24 h unless said otherwise.

| # | Measure | What counts | Source |
|---|---|---|---|
| M1 | **Restarts and crashes** | Starts, split into *planned* (systemd stopped it first: a deploy or an operator restart) and *unplanned* (no stop before it), plus any abnormal exit (`Main process exited`, `Failed with result`, `Scheduled restart`) | journal |
| M2 | **Errors and refusals** | `ERROR` lines, panics, and `WARN` lines split into the gate refusing callers with no token (Muster's TLS gate, which is expected) and everything else, each one listed | journal |
| M3 | **Mail taken, and how fast** | Direct messages sent; how many the recipient took; the time from send to take (p50, p95, max); and how many are still unread after 30 min, to a session that's live and to one that's gone | db |
| M4 | **Unread mail across a restart** | At each restart: how many direct messages were unread then, how many were taken afterwards, and how many are still unread to a live session | db + journal |
| M5 | **Sessions lost or duplicated** | Agent sessions seen, terminal senders seen (each `tell` registers one), sessions ended, takeovers (a restarted session keeping its nickname), and live sessions sharing a nickname | db |

**What "taken" means.** Both daemons record a `MessageRead` when the recipient's shim takes the
message: a hook's delivery, or its `claude-hook wait`, which runs in the background even while the
session is busy. So M3's latency is the time to the hook taking the message, not the time to the model
reading it. A session with no waiter held, such as one mid-turn without a background wait, takes its mail
at its next hook. That's why Muster's p95 is about a minute and its max is hours (a long-running turn).
MAR-70's first check 4 was wrong for this reason: the token had been taken before the restart (#89).

**Not measurable from the daemons today.**
- *Wakes expected vs observed.* `tell` prints the wake status (`wake expected but not observed`,
  `wake not observed`), but neither daemon stores it, and only the session sees the wake itself. M3's
  latency for mail to an idle session is the closest proxy. A canary, proposed in (d), would measure it
  directly.
- *Sends retried by the shim.* The shim retries a write made before the session is ready (MAR-43). The
  daemon only sees the attempt that succeeds, so a retry can't be counted from the daemon's side.
  Refusals are counted: they're M2's `WARN` lines.

## (b) How to take them, read-only

Both daemons use the same event store, one `myko_events` table, in Postgres databases `muster` and
`marshal` on the `marshal-pg` socket (`/run/user/$UID/marshal-pg`), so one query serves both. The
journal measures come from each unit's user journal.

**M1 and M2**, from the journal (read-only: `journalctl` and `systemctl show`):

```sh
#!/usr/bin/env bash
# Read-only: M1 restarts and M2 errors/refusals for one daemon unit, from the user journal.
# Usage: journal-measures.sh <muster-daemon|marshal-daemon> [since, default "24 hours ago"]
set -u
u=$1; since=${2:-24 hours ago}
j() { journalctl --user -u "$u" --since "$since" -o short-iso --no-pager; }
echo "== $u since '$since'"
systemctl --user show "$u" -p ActiveEnterTimestamp -p NRestarts -p Result | tr '\n' ' '; echo
# M1: each start is planned when the line before it is systemd's Stopping/Stopped,
# and unplanned otherwise (an exit, a crash, or systemd's own Restart=).
j | grep -E 'systemd\[[0-9]+\]: (Started|Stopping|Stopped)|Main process exited|Failed with result|Scheduled restart' |
  awk '/Main process exited|Failed with result|Scheduled restart/ {bad++; print "  EXIT  " $0}
       /Started/ { if (prev ~ /Stopp/) planned++; else { unplanned++; print "  START without a stop before it: " $0 } }
       { prev = $0 }
       END { printf "M1 starts planned=%d unplanned=%d abnormal_exits=%d\n", planned, unplanned, bad }'
# M2: ERROR lines, panics, and WARN lines split into the gate refusing callers
# without a token (expected, security) and everything else.
j | awk '/ ERROR /{e++} /panicked/{p++}
         / WARN / { if ($0 ~ /daemon::gate/) g++; else { w++; sub(/.*WARN +/, ""); print "  WARN  " substr($0, 1, 120) } }
         END { printf "M2 error=%d panic=%d warn_gate=%d warn_other=%d\n", e, p, g, w }'
```

**M3 to M5**, from each database. Read-only: `PGOPTIONS` makes every transaction read-only, which
refuses even a temporary view, and the file has only `SELECT`s. `:restarts` is the unit's start times
in epoch ms, read from the journal:

```sh
u=marshal-daemon db=marshal   # or: u=muster-daemon db=muster
R=$(journalctl --user -u $u --since "24 hours ago" -o short-unix --no-pager \
      | grep 'Started ' | awk '{printf "%s%d", (n++?",":""), $1*1000}')
PGOPTIONS='-c default_transaction_read_only=on' \
  psql -h /run/user/$UID/marshal-pg -d $db -X -A -v hours=24 -v restarts="'{$R}'" -f reliability.sql
```

`reliability.sql`:

```sql
-- reliability.sql (MAR-73). Read-only: SELECTs only, run with default_transaction_read_only on.
-- :restarts is a Postgres array literal of restart times in epoch ms, from the journal.

\set QUIET on
\pset footer off

-- M3 and M4 share one CTE, `direct`: direct messages sent in the window (latest SET of each message), with the
-- recipient's first read of it, and whether the recipient's session is still live.


\echo '-- M3 direct mail: sent, read, read latency (s), unread after 30 min (recipient live / gone)'
WITH direct AS (
WITH sent AS (
  SELECT DISTINCT ON (item_id) item_id,
         (event->'item'->>'sentAt')::bigint     AS sent_ms,
         event->'item'->>'toSessionId'           AS to_id
  FROM myko_events
  WHERE item_type = 'Message' AND change_type = 'SET'
    AND event->'item'->>'toSessionId' IS NOT NULL
    AND created_at > now() - make_interval(hours => :hours)
  ORDER BY item_id, id DESC
), reads AS (
  SELECT event->'item'->>'messageId' AS msg_id,
         event->'item'->>'sessionId' AS reader,
         min((event->'item'->>'readAt')::bigint) AS read_ms
  FROM myko_events
  WHERE item_type = 'MessageRead' AND change_type = 'SET'
  GROUP BY 1, 2
), live AS (
  SELECT DISTINCT ON (item_id) item_id, change_type = 'SET' AS live
  FROM myko_events WHERE item_type = 'Session'
  ORDER BY item_id, id DESC
)
SELECT s.item_id, s.sent_ms, s.to_id, r.read_ms, coalesce(l.live, false) AS to_live
FROM sent s
LEFT JOIN reads r ON r.msg_id = s.item_id AND r.reader = s.to_id
LEFT JOIN live l ON l.item_id = s.to_id
)
SELECT count(*)                                            AS sent,
       count(read_ms)                                      AS read,
       round(percentile_cont(0.5)  WITHIN GROUP (ORDER BY (read_ms - sent_ms) / 1000.0)::numeric, 1) AS p50_s,
       round(percentile_cont(0.95) WITHIN GROUP (ORDER BY (read_ms - sent_ms) / 1000.0)::numeric, 1) AS p95_s,
       round(max((read_ms - sent_ms) / 1000.0)::numeric, 1)                                       AS max_s,
       count(*) FILTER (WHERE read_ms IS NULL AND sent_ms < extract(epoch FROM now()) * 1000 - 1800000 AND to_live)     AS unread_live,
       count(*) FILTER (WHERE read_ms IS NULL AND sent_ms < extract(epoch FROM now()) * 1000 - 1800000 AND NOT to_live) AS unread_gone
FROM direct;

\echo '-- M4 mail unread at each restart: how much was waiting, and how much was read afterwards'
WITH direct AS (
WITH sent AS (
  SELECT DISTINCT ON (item_id) item_id,
         (event->'item'->>'sentAt')::bigint     AS sent_ms,
         event->'item'->>'toSessionId'           AS to_id
  FROM myko_events
  WHERE item_type = 'Message' AND change_type = 'SET'
    AND event->'item'->>'toSessionId' IS NOT NULL
    AND created_at > now() - make_interval(hours => :hours)
  ORDER BY item_id, id DESC
), reads AS (
  SELECT event->'item'->>'messageId' AS msg_id,
         event->'item'->>'sessionId' AS reader,
         min((event->'item'->>'readAt')::bigint) AS read_ms
  FROM myko_events
  WHERE item_type = 'MessageRead' AND change_type = 'SET'
  GROUP BY 1, 2
), live AS (
  SELECT DISTINCT ON (item_id) item_id, change_type = 'SET' AS live
  FROM myko_events WHERE item_type = 'Session'
  ORDER BY item_id, id DESC
)
SELECT s.item_id, s.sent_ms, s.to_id, r.read_ms, coalesce(l.live, false) AS to_live
FROM sent s
LEFT JOIN reads r ON r.msg_id = s.item_id AND r.reader = s.to_id
LEFT JOIN live l ON l.item_id = s.to_id
)
SELECT to_timestamp(r / 1000.0)                               AS restart,
       count(d.item_id)                                       AS unread_at_restart,
       count(d.item_id) FILTER (WHERE d.read_ms IS NOT NULL)  AS read_after,
       count(d.item_id) FILTER (WHERE d.read_ms IS NULL AND d.to_live) AS still_unread_live
FROM unnest(:restarts::bigint[]) AS r
LEFT JOIN direct d ON d.sent_ms < r AND (d.read_ms IS NULL OR d.read_ms > r)
GROUP BY r ORDER BY r;

\echo '-- M5 sessions: agent sessions and terminal senders seen, ended (DEL), takeovers (Succession), live duplicates by nickname'
WITH ev AS (
  SELECT * FROM myko_events WHERE created_at > now() - make_interval(hours => :hours)
), live AS (
  SELECT DISTINCT ON (item_id) item_id, change_type FROM myko_events
  WHERE item_type = 'Session' ORDER BY item_id, id DESC
), nick AS (
  SELECT DISTINCT ON (item_id) item_id, event->'item'->>'nickname' AS nickname FROM myko_events
  WHERE item_type = 'SessionNickname' AND change_type = 'SET' ORDER BY item_id, id DESC
)
SELECT (SELECT count(DISTINCT item_id) FROM ev WHERE item_type = 'Session' AND change_type = 'SET'
          AND coalesce(event->'item'->>'kind', '') <> 'terminal')                                  AS agent_sessions_seen,
       (SELECT count(DISTINCT item_id) FROM ev WHERE item_type = 'Session' AND change_type = 'SET'
          AND event->'item'->>'kind' = 'terminal')                                                 AS terminal_senders,
       (SELECT count(DISTINCT item_id) FROM ev WHERE item_type = 'Session' AND change_type = 'DEL') AS sessions_ended,
       (SELECT count(*) FROM ev WHERE item_type = 'Succession')                                    AS takeovers,
       (SELECT count(*) FROM (SELECT n.nickname FROM live l JOIN nick n USING (item_id)
                              WHERE l.change_type = 'SET' AND n.nickname <> ''
                              GROUP BY 1 HAVING count(*) > 1) d)                                   AS live_dup_nicknames;
```

It reads counts and times only. It never prints a message body, since those are other sessions' mail.

## (c) Run 1, 2026-11-09 15:09 EST

Run on Murph by cobalt-bream. Marshal on 98da488, Muster's daemon up since 2026-11-08 12:50:16.

### Journal (M1, M2)

| | Muster, 24 h | Marshal, 24 h | Muster, 7 d | Marshal, 7 d |
|---|---|---|---|---|
| Starts, planned / unplanned | 0 / 0 | 6 / 0 | 13 / 0 | 12 / 1 |
| Abnormal exits | 0 | 0 | 0 | 0 |
| `ERROR` / panics | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 |
| `WARN`, gate refusing callers without a token | 0 | 0 | 10 800 | 0 |
| `WARN`, other | 0 | 1 | 10 | 1 |

- Marshal's 6 starts in 24 h were all restarts by the lamplighter: three deploys (10:00, 12:09, 14:58),
  two for MAR-70's check 4 (13:56:49, 13:59:08), and 16:16 the day before. Its one other `WARN` is
  `forgot session … at the operator's request` (a `marshal-daemon forget`).
- Marshal's one "unplanned" start in 7 days, 2026-11-06 18:47:04, is the unit's first start after it
  was installed: it had been stopped since 2026-10-30, and a `systemctl daemon-reload` came just before.
  It isn't a crash, but the script can't tell the difference, so it's listed.
- Muster's 7-day gate `WARN`s are its TLS gate refusing connections with a missing or wrong token
  (9 204 on the network port and 1 578 locally, plus 18 refusals of other kinds, such as a browser origin), which is security working. Its 10 other `WARN`s are 9 hook calls
  refused for naming a session that wasn't the caller's own, and 1 link message from a sender that
  wasn't one of that daemon's sessions.

### Database (M3 to M5)

| | Muster, 24 h | Marshal, 24 h | Muster, 7 d | Marshal, 7 d |
|---|---|---|---|---|
| Direct messages sent / taken | 586 / 581 | 4 / 4 | 5 234 / 5 210 | 4 / 4 |
| Send to take, p50 / p95 / max (s) | 0.3 / 48.9 / 2 134.5 | 0.3 / 0.4 / 0.4 | 0.4 / 98.6 / 30 124.5 | 0.3 / 0.4 / 0.4 |
| Unread after 30 min, recipient live / gone | 0 / 0 | 0 / 0 | 0 / 19 | 0 / 0 |
| Restarts in the window, unread at a restart and still unread to a live session after it | none | 0 at all 6 | 0 at all 13 | 0 at all 13 |
| Agent sessions / terminal senders seen | 25 / 1 | 2 / 50 | 131 / 2 | 2 / 50 |
| Sessions ended / takeovers | 19 / 22 | 52 / 0 | 125 / 52 | 59 / 0 |
| Live sessions sharing a nickname | 0 | 0 | 0 | 0 |

- Muster's 5 untaken messages in 24 h were all under 30 min old at the time of the run.
- Muster's 7-day "unread at a restart" ran from 1 to 21 messages, and none is still unread to a live
  session. The ones never taken were to sessions that had ended. Its 19 messages unread to a gone
  session are mail to sessions that ended before taking it, which is the sender's loss, not a daemon
  fault, but worth knowing.
- Marshal's traffic is almost all synthetic. Its 4 direct messages are MAR-70's tokens. Its 50
  terminal senders are one-shot `tell` sessions: rally's wake and digest try `marshal-shim` first (RAL-120),
  and each attempt registers a sender, even when the recipient is on Muster.
  Its 2 agent sessions are MAR-70's test sessions. No Marshal restart has yet happened with mail
  waiting in the daemon. MAR-70's rerun of check 4 is the first (below).

### MAR-70's check 4, rerun with mail provably unread

The first time Marshal restarted with mail provably waiting in the daemon (MAR-70 run 2, recorded in
[cutover-test-session.md](cutover-test-session.md) (e)). A test session's waiters were stopped so that
nothing took its mail. The token (`Message` event 1189, sent 15:13:10) had no `MessageRead` before the
15:31:58 restart (PID 1581365 → 1693091), and none just after it. At the session's next prompt its
hook took it, once (`MessageRead` event 1230, 15:32:27.766), and the nickname was kept. That's one
sample of M4 on Marshal: 1 unread at the restart, 1 taken after, 0 lost. It was run after the queries
above, so their tables don't include it.

## (d) A proposed bar, for Josh

Marshal is "as reliable as Muster" for step 3 when, **under load**, it does at least as well as Muster
on each measure over the same window. Load is required, because Marshal has carried almost none so far.

**Load.** A 24 h window in which Marshal carries at least 3 agent sessions and at least 100 direct
messages, with at least one planned restart while mail is waiting. Two ways to get there, for Josh to
choose:
1. **A soak with test sessions.** Three kit sessions (cutover-test-session.md) on Marshal, plus a canary
   that sends each one a `tell` every hour and waits for its reply. That's about 72 model turns a day.
   The canary also measures wakes directly.
2. **One real team moves first.** A team between stories moves to Marshal (as step 4 would), while
   Muster stays up as the backup and the lamplighter relays. Real traffic and fewer token costs, but a
   problem would hit real work.

**The bar,** over that window:

| # | Marshal must show | Muster, run 1 (24 h) |
|---|---|---|
| M1 | 0 unplanned starts, 0 abnormal exits | 0, 0 |
| M2 | 0 `ERROR`, 0 panics, every other `WARN` explained | 0, 0, 0 |
| M3 | 0 unread after 30 min to a live session; p50 ≤ 1 s; p95 no worse than Muster's p95 over the same window | 0; 0.3 s; 48.9 s |
| M4 | at each restart, 0 still unread to a live session afterwards | no restart in the window; 0 at all 13 in 7 d |
| M5 | 0 live sessions sharing a nickname; every restarted agent session keeps its nickname | 0; 22 takeovers |
| Wake | (with the canary) every idle wake within 60 s of its `tell` | not measured |

If Josh accepts a bar, the Marshal team reruns this doc's measures at the end of the load window, and
records the result as run 2 here.

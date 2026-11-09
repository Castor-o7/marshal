# Muster's no-traffic check, before it's retired (MAR-65)

Fork-only. Step 7 of rally's [cutover.md](https://github.com/Castor-o7/rally/blob/main/docs/cutover.md)
stops `muster-daemon.service` "only after a full day with no traffic on Muster". This doc says what
that means and gives the query that checks it. The Marshal team runs it, with the mail check in rally's
`docs/mail-carryover.md`, before step 7.

## What counts as no traffic

Muster is quiet when all three hold:

1. **No message sent for 24 hours.** Every `Message` Muster still keeps was sent more than 24 hours ago.
   Direct and room mail both count.
2. **No session event for 24 hours.** No `Session` row was registered, updated or removed in the last
   24 hours. A connected session updates its row as it works, so this catches a session that's
   connected but sending nothing.
3. **No session registered.** Every `Session` row's latest event is a removal. A row that's still
   there is a session Muster still thinks it can reach, however quiet it is.

A database with no messages or no session events at all, as after Muster's 14-day sweep has taken
every message, passes that check: the query reads a missing time as `-infinity`.

If any of the three fails, Muster isn't quiet, and step 7 waits. Leftover rows from crashed sessions
fail check 3 until Muster's sweep removes them. Don't remove them by hand to pass the check.

## The query

Read-only: it turns on `default_transaction_read_only` first, and has only `SELECT`s. It never writes
to the `muster` database.

```sql
set default_transaction_read_only = on;
-- 1. The verdict, with the three facts it rests on.
with latest_session as (
  select distinct on (item_id) item_id, change_type
  from myko_events where item_type = 'Session'
  order by item_id, id desc),
facts as (
  select
    -- None at all (the 14-day sweep took every message) counts as long ago.
    (select coalesce(to_timestamp(max((event->'item'->>'sentAt')::bigint) / 1000), '-infinity')
       from myko_events where item_type = 'Message' and change_type = 'SET') as last_message,
    (select coalesce(max(created_at), '-infinity')
       from myko_events where item_type = 'Session') as last_session_event,
    (select count(*) from latest_session where change_type = 'SET') as sessions_registered)
select last_message, last_session_event, sessions_registered,
       case when last_message < now() - interval '24 hours'
             and last_session_event < now() - interval '24 hours'
             and sessions_registered = 0
            then 'quiet: no traffic for 24 h' else 'not quiet' end as verdict
from facts;
-- 2. The sessions still registered, if any.
with latest as (
  select distinct on (item_id) item_id, change_type, event->'item' as it
  from myko_events where item_type = 'Session'
  order by item_id, id desc)
select item_id as session_id, it->>'cwd' as cwd, it->>'kind' as kind,
       to_timestamp((it->>'lastActivityAt')::bigint / 1000) as last_activity
from latest where change_type = 'SET' order by last_activity desc nulls last;
```

Run it with `psql "host=/run/user/1000/marshal-pg dbname=muster user=joshd" -X -f <file>`, from the
query saved to a file. The first result is the verdict. The second lists any session still
registered, so you know who to move or wait for.

## First run

Run on Murph on 2026-11-09 at 16:42 UTC, while the cutover hadn't started:

```
      last_message      |      last_session_event       | sessions_registered |  verdict  
------------------------+-------------------------------+---------------------+-----------
 2026-11-09 11:42:04-05 | 2026-11-09 11:33:29.512294-05 |                   8 | not quiet
(1 row)


--------------------------------------+-------------------------------+-------------+------------------------
 4c68ae48-bd41-4d28-ae3a-195dadcc54ac | /home/joshd/src/marshal-mar58 | interactive | 2026-11-09 10:31:19-05
 6ddb232c-6851-4d72-bf1e-6a7e2f055fb4 | /home/joshd/src/marshal       | interactive | 2026-11-09 10:08:14-05
 fb9b19bc-389a-44c4-9102-b6311a544ad2 | /home/joshd/src               | interactive | 2026-11-09 10:00:51-05
 dd62da43-c33a-4e51-948b-37d78c33f8ee | /home/joshd/Documents/RustDAW | interactive | 2026-11-09 09:45:59-05
 f24f362c-efa8-4b0f-bf61-56b64936e085 | /home/joshd/Documents/RustDAW | interactive | 2026-11-09 09:44:33-05
 58aceacf-5cf1-4320-a444-dddd10340848 | /home/joshd/src               | interactive | 2026-11-08 20:43:37-05
 82fe7350-f7c2-4426-bb32-6c13e227b41e | /home/joshd/Documents/Caster  | interactive | 2026-11-08 19:24:30-05
 43b14e01-71aa-4443-91bd-2a566e912d43 | /home/joshd/src/muster        | interactive | 
(8 rows)

```

Not quiet, as expected before any session has moved: mail was sent within the minute, and eight
sessions are registered. The query ran cleanly against the live database, which is what this first
run had to show.
